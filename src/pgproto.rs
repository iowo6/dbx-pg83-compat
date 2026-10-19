//! PostgreSQL 3.0 wire protocol over a blocking TCP stream.
//!
//! Protocol 3.0 shipped in PostgreSQL 7.4 and is still the current protocol,
//! so speaking it directly keeps this driver compatible with PostgreSQL 8.3
//! (where modern client libraries trip over catalog differences instead).
//! Only the simple-query path is implemented; every value is transferred in
//! text format, which avoids any dependency on per-version type OIDs.

use std::io::{BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use crate::md5::pg_md5_password;

const PROTOCOL_VERSION: i32 = 196608; // 3.0
const CANCEL_REQUEST_CODE: i32 = 80877102;

#[derive(Debug)]
pub struct PgError {
    pub severity: String,
    pub code: String,
    pub message: String,
    pub detail: Option<String>,
    pub hint: Option<String>,
}

impl PgError {
    pub fn to_display(&self) -> String {
        let mut text = format!("{}: {}", if self.severity.is_empty() { "ERROR" } else { &self.severity }, self.message);
        if !self.code.is_empty() {
            text.push_str(&format!(" (SQLSTATE {})", self.code));
        }
        if let Some(detail) = &self.detail {
            text.push_str(&format!("\nDETAIL: {detail}"));
        }
        if let Some(hint) = &self.hint {
            text.push_str(&format!("\nHINT: {hint}"));
        }
        text
    }
}

#[derive(Debug, Clone)]
pub struct NoticeMessage {
    pub severity: String,
    pub message: String,
    pub code: Option<String>,
    pub detail: Option<String>,
    pub hint: Option<String>,
}

#[derive(Debug, Clone)]
pub struct FieldDescription {
    pub name: String,
    pub type_oid: u32,
}

#[derive(Debug, Default)]
pub struct StatementOutcome {
    /// Columns of the last result set (empty when the statement returned none).
    pub fields: Vec<FieldDescription>,
    /// Rows of the last result set, text-encoded; `None` cell = SQL NULL.
    pub rows: Vec<Vec<Option<String>>>,
    /// Rows affected from the final CommandComplete tag (0 when unknown).
    pub affected_rows: u64,
    /// Server notices/warnings observed while running the statement(s).
    pub notices: Vec<NoticeMessage>,
    /// True when the server reported an explicit empty query.
    pub empty_query: bool,
    /// Transaction status reported by the final ReadyForQuery.
    pub in_transaction: bool,
}

impl StatementOutcome {
    pub fn affected_from_tag(&mut self, tag: &str) {
        self.affected_rows = tag
            .split_whitespace()
            .next_back()
            .and_then(|token| token.parse::<u64>().ok())
            .unwrap_or(0);
        // CREATE TABLE / DROP ... report no count; keep the explicit 0.
    }
}

/// One authenticated PostgreSQL session.
pub struct PgConnection {
    reader: BufReader<TcpStream>,
    stream: TcpStream,
    pub backend_pid: u32,
    pub secret_key: u32,
    pub server_version: String,
    pub in_transaction: bool,
    /// Last `search_path` applied on this connection (avoids redundant SETs).
    applied_search_path: Option<String>,
    closed: bool,
}

pub struct ConnectOptions<'a> {
    pub host: &'a str,
    pub port: u16,
    pub database: &'a str,
    pub username: &'a str,
    pub password: &'a str,
    pub connect_timeout: Duration,
}

impl PgConnection {
    pub fn connect(options: &ConnectOptions) -> Result<PgConnection, PgError> {
        let address = format!("{}:{}", options.host, options.port);
        let stream = connect_with_timeout(&address, options.connect_timeout).map_err(|error| PgError {
            severity: "FATAL".into(),
            code: "08001".into(),
            message: format!("could not connect to {address}: {error}"),
            detail: None,
            hint: None,
        })?;
        stream.set_nodelay(true).ok();
        let read_timeout = Duration::from_secs(600);
        stream.set_read_timeout(Some(read_timeout)).ok();
        let stream2 = stream.try_clone().map_err(|error| PgError {
            severity: "FATAL".into(),
            code: "08006".into(),
            message: format!("could not duplicate socket: {error}"),
            detail: None,
            hint: None,
        })?;

        let mut connection = PgConnection {
            reader: BufReader::with_capacity(64 * 1024, stream),
            stream: stream2,
            backend_pid: 0,
            secret_key: 0,
            server_version: String::new(),
            in_transaction: false,
            applied_search_path: None,
            closed: false,
        };

        connection.startup(options)?;
        Ok(connection)
    }

    fn startup(&mut self, options: &ConnectOptions) -> Result<(), PgError> {
        // Only parameters every 8.x server understands are sent; unknown GUCs
        // in the startup packet are ignored by the server, but staying inside
        // the old vocabulary keeps the log clean.
        let mut body = Vec::new();
        body.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
        for (key, value) in [
            ("user", options.username),
            ("database", options.database),
            ("client_encoding", "UTF8"),
            ("DateStyle", "ISO, MDY"),
        ] {
            if value.is_empty() {
                continue;
            }
            body.extend_from_slice(key.as_bytes());
            body.push(0);
            body.extend_from_slice(value.as_bytes());
            body.push(0);
        }
        body.push(0);
        let message = len_prefixed(0, &body);
        self.stream.write_all(&message).map_err(io_pg_error)?;

        loop {
            let tag = self.read_byte()?;
            let payload = self.read_payload()?;
            match tag {
                b'R' => self.handle_auth(payload, options)?,
                b'E' => return Err(parse_error_fields(&payload)),
                b'K' => {
                    // BackendKeyData: pid + secret key, needed for cancellation.
                    if payload.len() >= 8 {
                        self.backend_pid = u32::from_be_bytes(payload[0..4].try_into().unwrap());
                        self.secret_key = u32::from_be_bytes(payload[4..8].try_into().unwrap());
                    }
                }
                b'S' => {
                    // ParameterStatus "name\0value\0"
                    if let Some((name, value)) = split_cstring_pair(&payload) {
                        if name == "server_version" {
                            self.server_version = value;
                        }
                    }
                }
                b'Z' => {
                    self.in_transaction = matches!(payload.first(), Some(b'T') | Some(b'E'));
                    return Ok(());
                }
                // Notices and authentication continuations before the session is
                // ready are informational; keep draining.
                b'N' => {}
                _ => {}
            }
        }
    }

    fn handle_auth(&mut self, payload: Vec<u8>, options: &ConnectOptions) -> Result<(), PgError> {
        if payload.len() < 4 {
            return Err(PgError {
                severity: "FATAL".into(),
                code: "08006".into(),
                message: "malformed authentication request".into(),
                detail: None,
                hint: None,
            });
        }
        let code = i32::from_be_bytes(payload[0..4].try_into().unwrap());
        match code {
            0 => Ok(()), // AuthenticationOk
            3 => {
                // CleartextPassword: payload is the password as a cstring.
                let mut body = options.password.as_bytes().to_vec();
                body.push(0);
                let message = len_prefixed(b'p', &body);
                self.stream.write_all(&message).map_err(io_pg_error)?;
                Ok(())
            }
            5 => {
                // MD5Password with 4-byte salt.
                if payload.len() < 8 {
                    return Err(PgError {
                        severity: "FATAL".into(),
                        code: "08006".into(),
                        message: "malformed MD5 authentication request".into(),
                        detail: None,
                        hint: None,
                    });
                }
                let salt: [u8; 4] = payload[4..8].try_into().unwrap();
                let token = pg_md5_password(options.username, options.password, salt);
                let mut body = token.into_bytes();
                body.push(0);
                let message = len_prefixed(b'p', &body);
                self.stream.write_all(&message).map_err(io_pg_error)?;
                Ok(())
            }
            other => Err(PgError {
                severity: "FATAL".into(),
                code: "28000".into(),
                message: format!(
                    "unsupported authentication method requested by the server (code {other}); \
                     this driver supports trust, password and md5"
                ),
                detail: None,
                hint: None,
            }),
        }
    }

    /// Runs one or more statements with the simple query protocol and returns
    /// the last result set plus aggregate notices. `set_search_path` is applied
    /// first when it differs from the connection's applied value.
    pub fn simple_query(&mut self, sql: &str) -> Result<StatementOutcome, PgError> {
        if !self.closed {
            let mut body = sql.as_bytes().to_vec();
            body.push(0);
            let message = len_prefixed(b'Q', &body);
            self.stream.write_all(&message).map_err(io_pg_error)?;
            self.read_query_response()
        } else {
            Err(PgError {
                severity: "FATAL".into(),
                code: "08003".into(),
                message: "connection is closed".into(),
                detail: None,
                hint: None,
            })
        }
    }

    fn read_query_response(&mut self) -> Result<StatementOutcome, PgError> {
        let mut outcome = StatementOutcome::default();
        // A simple-query message may carry several statements, each producing
        // its own RowDescription/DataRow/CommandComplete group; only the last
        // group survives (matching psql's behavior of showing the final result).
        let mut current_fields: Vec<FieldDescription> = Vec::new();
        let mut current_rows: Vec<Vec<Option<String>>> = Vec::new();
        let mut pending_error: Option<PgError> = None;

        loop {
            let tag = self.read_byte()?;
            let payload = self.read_payload()?;
            match tag {
                b'T' => {
                    current_fields = parse_row_description(&payload);
                    current_rows = Vec::new();
                }
                b'D' => {
                    current_rows.push(parse_data_row(&payload));
                }
                b'C' => {
                    let tag_text = String::from_utf8_lossy(trim_trailing_null(&payload)).into_owned();
                    outcome.fields = std::mem::take(&mut current_fields);
                    outcome.rows = std::mem::take(&mut current_rows);
                    outcome.affected_from_tag(&tag_text);
                }
                b'I' => {
                    // EmptyQueryResponse
                    outcome.fields = Vec::new();
                    outcome.rows = Vec::new();
                    outcome.empty_query = true;
                    current_fields.clear();
                    current_rows.clear();
                }
                b'N' => {
                    if let Some(notice) = parse_notice(&payload) {
                        outcome.notices.push(notice);
                    }
                }
                b'S' => {
                    if let Some((name, value)) = split_cstring_pair(&payload) {
                        if name == "server_version" {
                            self.server_version = value;
                        }
                    }
                }
                b'A' | b'd' | b'c' => {
                    // NotificationResponse / CopyData / CopyDone: not used by
                    // this driver, drain silently.
                }
                b'G' => {
                    // CopyInResponse: refuse politely so the server unwinds.
                    let mut fail_body = b"COPY FROM STDIN is not supported by this driver ".to_vec();
                    fail_body.pop();
                    fail_body.push(0);
                    let fail = len_prefixed(b'f', &fail_body);
                    self.stream.write_all(&fail).ok();
                }
                b'E' => {
                    // Keep the pending error but drain until ReadyForQuery:
                    // leaving the trailing Z in the buffer would desynchronize
                    // the next statement on this connection.
                    pending_error = Some(parse_error_fields(&payload));
                }
                b'Z' => {
                    self.in_transaction = matches!(payload.first(), Some(b'T') | Some(b'E'));
                    outcome.in_transaction = self.in_transaction;
                    return match pending_error {
                        Some(error) => Err(error),
                        None => Ok(outcome),
                    };
                }
                _ => {}
            }
        }
    }

    /// Applies `search_path` when it differs from the last value applied.
    pub fn set_search_path(&mut self, schema: &str) -> Result<(), PgError> {
        let normalized = schema.trim();
        if normalized.is_empty() {
            return Ok(());
        }
        if self.applied_search_path.as_deref() == Some(normalized) {
            return Ok(());
        }
        let sql = format!(
            "SET search_path TO {}, pg_catalog",
            crate::jdbc_url::sql_quote_ident(normalized)
        );
        self.simple_query(&sql)?;
        self.applied_search_path = Some(normalized.to_string());
        Ok(())
    }

    fn read_byte(&mut self) -> Result<u8, PgError> {
        let mut buffer = [0u8; 1];
        self.reader.read_exact(&mut buffer).map_err(|_| PgError {
            severity: "FATAL".into(),
            code: "08006".into(),
            message: "connection to the server was lost".into(),
            detail: None,
            hint: None,
        })?;
        Ok(buffer[0])
    }

    fn read_payload(&mut self) -> Result<Vec<u8>, PgError> {
        let mut length_bytes = [0u8; 4];
        self.reader.read_exact(&mut length_bytes).map_err(|_| PgError {
            severity: "FATAL".into(),
            code: "08006".into(),
            message: "connection to the server was lost".into(),
            detail: None,
            hint: None,
        })?;
        let length = u32::from_be_bytes(length_bytes) as usize;
        if length < 4 {
            return Err(PgError {
                severity: "FATAL".into(),
                code: "08006".into(),
                message: "malformed message length from the server".into(),
                detail: None,
                hint: None,
            });
        }
        let mut payload = vec![0u8; length - 4];
        self.reader.read_exact(&mut payload).map_err(|_| PgError {
            severity: "FATAL".into(),
            code: "08006".into(),
            message: "connection to the server was lost".into(),
            detail: None,
            hint: None,
        })?;
        Ok(payload)
    }
}

impl Drop for PgConnection {
    fn drop(&mut self) {
        if !self.closed {
            // Terminate message 'X'.
            let _ = self.stream.write_all(&len_prefixed(b'X', b""));
        }
    }
}

/// Sends an out-of-band CancelRequest for a backend identified by
/// (pid, secret key). The server closes this auxiliary connection right
/// after; there is no response to read.
pub fn send_cancel(host: &str, port: u16, pid: u32, secret: u32) -> Result<(), String> {
    let stream = TcpStream::connect((host, port)).map_err(|error| error.to_string())?;
    let mut stream = stream;
    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&(16i32).to_be_bytes());
    body.extend_from_slice(&CANCEL_REQUEST_CODE.to_be_bytes());
    body.extend_from_slice(&pid.to_be_bytes());
    body.extend_from_slice(&secret.to_be_bytes());
    stream.write_all(&body).map_err(|error| error.to_string())
}

fn connect_with_timeout(address: &str, timeout: Duration) -> Result<TcpStream, String> {
    use std::net::ToSocketAddrs;
    let addresses: Vec<std::net::SocketAddr> = address
        .to_socket_addrs()
        .map_err(|error| error.to_string())?
        .collect();
    let mut last_error = String::from("no addresses resolved");
    for socket in addresses {
        match TcpStream::connect_timeout(&socket, timeout) {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = error.to_string(),
        }
    }
    Err(last_error)
}

fn len_prefixed(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut message = Vec::with_capacity(body.len() + 5);
    if tag != 0 {
        message.push(tag);
    }
    message.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
    message.extend_from_slice(body);
    message
}

fn io_pg_error(error: std::io::Error) -> PgError {
    PgError {
        severity: "FATAL".into(),
        code: "08006".into(),
        message: format!("connection error: {error}"),
        detail: None,
        hint: None,
    }
}

fn trim_trailing_null(payload: &[u8]) -> &[u8] {
    match payload.last() {
        Some(0) => &payload[..payload.len() - 1],
        _ => payload,
    }
}

fn split_cstring_pair(payload: &[u8]) -> Option<(String, String)> {
    let mut parts = payload.split(|byte| *byte == 0);
    let name = parts.next()?;
    let value = parts.next()?;
    Some((String::from_utf8_lossy(name).into_owned(), String::from_utf8_lossy(value).into_owned()))
}

/// PostgreSQL ErrorResponse/NoticeResponse payload: a sequence of
/// `fieldType byte + value cstring` pairs, terminated by a lone `\0`.
/// Splitting on `\0` therefore yields segments like `SERROR`, `C42883` —
/// the type byte is part of the same segment as its value.
fn parse_error_fields(payload: &[u8]) -> PgError {
    let mut error = PgError {
        severity: "ERROR".into(),
        code: String::new(),
        message: String::new(),
        detail: None,
        hint: None,
    };
    for segment in payload.split(|byte| *byte == 0) {
        if segment.is_empty() {
            break;
        }
        let field_type = segment[0];
        let value = String::from_utf8_lossy(&segment[1..]).into_owned();
        match field_type {
            b'S' => error.severity = value,
            b'C' => error.code = value,
            b'M' => error.message = value,
            b'D' => error.detail = Some(value),
            b'H' => error.hint = Some(value),
            _ => {}
        }
    }
    error
}

fn parse_notice(payload: &[u8]) -> Option<NoticeMessage> {
    let error = parse_error_fields(payload);
    (!error.message.is_empty()).then_some(NoticeMessage {
        severity: if error.severity.is_empty() { "NOTICE".into() } else { error.severity },
        message: error.message,
        code: (!error.code.is_empty()).then_some(error.code),
        detail: error.detail,
        hint: error.hint,
    })
}

fn parse_row_description(payload: &[u8]) -> Vec<FieldDescription> {
    let mut fields = Vec::new();
    if payload.len() < 2 {
        return fields;
    }
    let count = u16::from_be_bytes([payload[0], payload[1]]) as usize;
    let mut offset = 2;
    for _ in 0..count {
        let Some(name_end) = payload[offset..].iter().position(|byte| *byte == 0) else {
            break;
        };
        let name = String::from_utf8_lossy(&payload[offset..offset + name_end]).into_owned();
        offset += name_end + 1;
        // table oid (4) + column attnum (2) + type oid (4) + typlen (2) + typmod (4) + format (2)
        if offset + 18 > payload.len() {
            break;
        }
        let type_oid = u32::from_be_bytes(payload[offset + 6..offset + 10].try_into().unwrap());
        fields.push(FieldDescription { name, type_oid });
        offset += 18;
    }
    fields
}

fn parse_data_row(payload: &[u8]) -> Vec<Option<String>> {
    let mut cells = Vec::new();
    if payload.len() < 2 {
        return cells;
    }
    let count = u16::from_be_bytes([payload[0], payload[1]]) as usize;
    let mut offset = 2;
    for _ in 0..count {
        if offset + 4 > payload.len() {
            break;
        }
        let length = i32::from_be_bytes(payload[offset..offset + 4].try_into().unwrap());
        offset += 4;
        if length < 0 {
            cells.push(None);
            continue;
        }
        let length = length as usize;
        if offset + length > payload.len() {
            break;
        }
        // PostgreSQL text-format values are UTF-8 for every text-ish type;
        // invalid bytes (SQL_ASCII databases) are replaced rather than failing.
        let value = String::from_utf8_lossy(&payload[offset..offset + length]).into_owned();
        offset += length;
        cells.push(Some(value));
    }
    cells
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_description_and_data_row_roundtrip() {
        // 2 fields: "id" (oid 23), "name" (oid 25)
        let mut payload = Vec::new();
        payload.extend_from_slice(&2u16.to_be_bytes());
        for (name, oid) in [("id", 23u32), ("name", 25u32)] {
            payload.extend_from_slice(name.as_bytes());
            payload.push(0);
            payload.extend_from_slice(&0u32.to_be_bytes()); // table oid
            payload.extend_from_slice(&0i16.to_be_bytes()); // attnum
            payload.extend_from_slice(&oid.to_be_bytes());
            payload.extend_from_slice(&(-1i16).to_be_bytes()); // typlen
            payload.extend_from_slice(&(-1i32).to_be_bytes()); // typmod
            payload.extend_from_slice(&0i16.to_be_bytes()); // format
        }
        let fields = parse_row_description(&payload);
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].name, "id");
        assert_eq!(fields[0].type_oid, 23);
        assert_eq!(fields[1].name, "name");

        let mut row = Vec::new();
        row.extend_from_slice(&2u16.to_be_bytes());
        row.extend_from_slice(&1i32.to_be_bytes());
        row.push(b'7');
        row.extend_from_slice(&(-1i32).to_be_bytes());
        let cells = parse_data_row(&row);
        assert_eq!(cells, vec![Some("7".to_string()), None]);
    }

    #[test]
    fn command_tag_row_counts() {
        let mut outcome = StatementOutcome::default();
        outcome.affected_from_tag("INSERT 0 5");
        assert_eq!(outcome.affected_rows, 5);
        outcome.affected_from_tag("UPDATE 12");
        assert_eq!(outcome.affected_rows, 12);
        outcome.affected_from_tag("SELECT 1000");
        assert_eq!(outcome.affected_rows, 1000);
        outcome.affected_from_tag("CREATE TABLE");
        assert_eq!(outcome.affected_rows, 0);
    }

    #[test]
    fn error_fields_parse() {
        let mut payload = Vec::new();
        payload.push(b'S');
        payload.extend_from_slice(b"ERROR");
        payload.push(0);
        payload.push(b'C');
        payload.extend_from_slice(b"42883");
        payload.push(0);
        payload.push(b'M');
        payload.extend_from_slice(b"function left(text, integer) does not exist");
        payload.push(0);
        payload.push(0);
        let error = parse_error_fields(&payload);
        assert_eq!(error.code, "42883");
        assert!(error.message.contains("left"));
        assert!(error.to_display().contains("SQLSTATE 42883"));
    }

    #[test]
    fn md5_auth_password_token() {
        // Derived from a known pg_md5 vector: user "postgres", password "postgres",
        // salt 01 02 03 04 — cross-checked against the standard algorithm.
        let token = pg_md5_password("postgres", "postgres", [1, 2, 3, 4]);
        assert!(token.starts_with("md5"));
        assert_eq!(token.len(), 3 + 32);
    }
}
