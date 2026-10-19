//! `jdbc:postgresql://` URL parsing.
//!
//! DBX sends the JDBC connection form fields as a snake_case `connection`
//! object; `connection_string` is the authoritative endpoint because the host
//! rewrites it when transport layers (SSH/SOCKS) replace host/port.

#[derive(Debug, Clone)]
pub struct PgEndpoint {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub username: String,
    pub password: String,
    pub options: Vec<(String, String)>,
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() + 1 && index + 2 < bytes.len() + 1 {
            if let (Some(high), Some(low)) =
                (bytes.get(index + 1).copied().and_then(hex_value), bytes.get(index + 2).copied().and_then(hex_value))
            {
                out.push(high * 16 + low);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parses `jdbc:postgresql://host:port/db?user=u&password=p&...`.
/// Returns `None` when the URL is not a PostgreSQL JDBC URL.
pub fn parse_postgres_url(url: &str) -> Option<PgEndpoint> {
    let rest = url.trim().strip_prefix("jdbc:postgresql:")?;
    if let Some(after) = rest.strip_prefix("//") {
        // `jdbc:postgresql://[authority]/db?...` — the first '/' or '?'
        // terminates the authority. A '/' *inside* the authority is a
        // Unix-socket directory (`jdbc:postgresql:///var/run/postgresql/db`).
        let authority_end = after.find(['/', '?']).unwrap_or(after.len());
        let (authority, remainder) = (&after[..authority_end], &after[authority_end..]);
        let (hostport, socket_dir) = match authority.rsplit_once('/') {
            Some((hosts, socket_dir)) => (hosts, Some(socket_dir)),
            None => (authority, None),
        };
        let hostport = hostport.split(',').next().unwrap_or_default().trim();
        let mut endpoint = parse_hostport(hostport);
        // The socket dir case: everything after the last '/' of the authority
        // is the database when the remainder is empty.
        if let Some(socket_dir) = socket_dir {
            if endpoint.database.is_empty() && !socket_dir.is_empty() {
                endpoint.database = socket_dir.to_string();
            }
        }
        return finish_endpoint(endpoint, remainder);
    }
    // `jdbc:postgresql:dbname` (local connection).
    let endpoint = parse_hostport("");
    finish_endpoint(endpoint, rest)
}

#[derive(Default)]
struct HostPort {
    host: String,
    port: u16,
    database: String,
}

fn parse_hostport(hostport: &str) -> HostPort {
    if hostport.is_empty() {
        return HostPort { host: "localhost".into(), port: 5432, database: String::new() };
    }
    if let Some(bracketed) = hostport.strip_prefix('[') {
        // `[ipv6]` or `[ipv6]:port` — the address itself contains colons, so
        // only a colon after the closing bracket can be the port.
        let (inner, port) = match bracketed.split_once(']') {
            Some((inner, port)) => (inner, port.strip_prefix(':').unwrap_or("")),
            None => (bracketed, ""),
        };
        return HostPort { host: inner.to_string(), port: port.parse().unwrap_or(5432), database: String::new() };
    }
    let (host, port) = hostport.rsplit_once(':').unwrap_or((hostport, ""));
    HostPort { host: host.to_string(), port: port.parse().unwrap_or(5432), database: String::new() }
}

fn finish_endpoint(mut endpoint: HostPort, remainder: &str) -> Option<PgEndpoint> {
    let mut database = String::new();
    let mut query = String::new();
    if let Some(rest) = remainder.strip_prefix('/') {
        let path_end = rest.find('?').unwrap_or(rest.len());
        database = rest[..path_end].to_string();
        query = rest[path_end..].to_string();
        // `jdbc:postgresql:///var/run/postgresql/db`: an authority-less path
        // is a Unix-socket directory in the JDBC driver. We dial TCP only, so
        // keep the final segment as the database and stay on localhost.
        if database.contains('/') {
            database = database.rsplit('/').next().unwrap_or_default().to_string();
        }
    } else if let Some(rest) = remainder.strip_prefix('?') {
        query = format!("?{rest}");
    } else if !remainder.is_empty() {
        database = remainder.to_string();
    }
    if database.is_empty() {
        database = endpoint.database.clone();
    }
    endpoint.database = database;

    let mut options: Vec<(String, String)> = Vec::new();
    if let Some(raw) = query.strip_prefix('?') {
        for pair in raw.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            options.push((percent_decode(key), percent_decode(value)));
        }
    }

    let (url_user, url_password) =
        options.iter().fold((None, None), |(user, password), (key, value)| {
            match key.to_ascii_lowercase().as_str() {
                "user" | "user name" | "username" => (Some(value.clone()), password),
                "password" => (user, Some(value.clone())),
                _ => (user, password),
            }
        });

    Some(PgEndpoint {
        host: endpoint.host,
        port: endpoint.port,
        database: endpoint.database,
        username: url_user.unwrap_or_default(),
        password: url_password.unwrap_or_default(),
        options,
    })
}

/// Resolves the effective endpoint from a DBX `connection` JSON object.
/// `connection.database` overrides the URL path database (the DBX JDBC plugin's
/// URL-binding behavior); `username`/`password` fill gaps left by the URL.
pub fn endpoint_from_connection(connection: &serde_json::Value) -> Option<PgEndpoint> {
    let url = connection.get("connection_string").and_then(|value| value.as_str())?;
    let mut endpoint = parse_postgres_url(url)?;
    if let Some(database) = connection.get("database").and_then(|value| value.as_str()) {
        if !database.trim().is_empty() {
            endpoint.database = database.to_string();
        }
    }
    if endpoint.username.is_empty() {
        if let Some(username) = connection.get("username").and_then(|value| value.as_str()) {
            endpoint.username = username.to_string();
        }
    }
    if endpoint.password.is_empty() {
        if let Some(password) = connection.get("password").and_then(|value| value.as_str()) {
            endpoint.password = password.to_string();
        }
    }
    // DBX `url_params` carries extra query options for JDBC URLs.
    if let Some(extra) = connection.get("url_params").and_then(|value| value.as_str()) {
        for pair in extra.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            if key == "user" || key == "password" {
                continue;
            }
            endpoint.options.push((percent_decode(key), percent_decode(value)));
        }
    }
    Some(endpoint)
}

#[allow(dead_code)] // kept for the SSL roadmap; the wire driver is plaintext-only today
pub fn ssl_mode(endpoint: &PgEndpoint) -> String {
    endpoint
        .options
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("sslmode") || key.eq_ignore_ascii_case("ssl"))
        .map(|(_, value)| value.to_ascii_lowercase())
        .unwrap_or_else(|| "prefer".to_string())
}

/// Escapes a string literal body for SQL (`'` doubled).
pub fn sql_quote_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Quotes an SQL identifier (doubling embedded `"`).
pub fn sql_quote_ident(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_host_port_database() {
        let endpoint = parse_postgres_url("jdbc:postgresql://db.example.com:5433/sales?sslmode=require").unwrap();
        assert_eq!(endpoint.host, "db.example.com");
        assert_eq!(endpoint.port, 5433);
        assert_eq!(endpoint.database, "sales");
        assert_eq!(ssl_mode(&endpoint), "require");
    }

    #[test]
    fn defaults_port_and_ipv6() {
        let endpoint = parse_postgres_url("jdbc:postgresql://[::1]/x").unwrap();
        assert_eq!(endpoint.host, "::1");
        assert_eq!(endpoint.port, 5432);
        assert_eq!(endpoint.database, "x");
    }

    #[test]
    fn url_credentials_and_encoding() {
        let endpoint =
            parse_postgres_url("jdbc:postgresql://h/db?user=u%40x&password=p%26x&ApplicationName=dbx").unwrap();
        assert_eq!(endpoint.username, "u@x");
        assert_eq!(endpoint.password, "p&x");
        assert!(endpoint.options.iter().any(|(key, _)| key == "ApplicationName"));
    }

    #[test]
    fn local_form_and_multi_host() {
        assert_eq!(parse_postgres_url("jdbc:postgresql:mydb").unwrap().database, "mydb");
        let endpoint = parse_postgres_url("jdbc:postgresql://h1:5432,h2:5433/db").unwrap();
        assert_eq!(endpoint.host, "h1");
        assert_eq!(endpoint.port, 5432);
    }

    #[test]
    fn unix_socket_directory() {
        let endpoint = parse_postgres_url("jdbc:postgresql:///var/run/postgresql/db").unwrap();
        assert_eq!(endpoint.database, "db");
    }

    #[test]
    fn connection_database_overrides_url_path() {
        let endpoint = endpoint_from_connection(&json!({
            "connection_string": "jdbc:postgresql://localhost:5432/old",
            "username": "u",
            "password": "p",
            "database": "newdb"
        }))
        .unwrap();
        assert_eq!(endpoint.username, "u");
        assert_eq!(endpoint.password, "p");
        assert_eq!(endpoint.database, "newdb");
    }

    #[test]
    fn connection_without_database_keeps_url() {
        let endpoint = endpoint_from_connection(&json!({
            "connection_string": "jdbc:postgresql://localhost:5432/urldb"
        }))
        .unwrap();
        assert_eq!(endpoint.database, "urldb");
    }

    #[test]
    fn rejects_other_jdbc_urls() {
        assert!(parse_postgres_url("jdbc:mysql://localhost/db").is_none());
        assert!(parse_postgres_url("jdbc:h2:mem:test").is_none());
    }
}
