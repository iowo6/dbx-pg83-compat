//! DBX external driver sidecar: legacy PostgreSQL (8.3+) compatibility.
//!
//! Speaks the DBX "jdbc" external-driver protocol over stdio JSON-RPC
//! (JSON-lines transport, protocol version 1) and the PostgreSQL 3.0 wire
//! protocol directly. See README.md for the SQL compatibility contract.

mod jdbc_url;
mod md5;
mod meta83;
mod pgproto;
mod rewrite83;

use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pgproto::{ConnectOptions, PgConnection, PgError, StatementOutcome};
use serde_json::{json, Value};

/// Number of rows a cursor session keeps server-side before the host asks
/// for the next page. DBX pages at its own grid page size; we simply cache
/// the whole statement result — metadata result sets are small and preview
/// SELECTs already carry a LIMIT.
struct CursorSession {
    columns: Vec<String>,
    column_types: Vec<String>,
    column_sortables: Vec<bool>,
    rows: Vec<Vec<Value>>,
    next_row: usize,
}

struct SharedState {
    connection: Option<PgConnection>,
    /// host:port of the live connection, needed for out-of-band cancel.
    endpoint: Option<(String, u16)>,
    /// Active statement being executed, for cancelStatement.
    active_cancel: Option<ActiveStatement>,
    /// Manual-transaction state.
    manual_txn: bool,
    /// Query cursors by session id.
    cursors: std::collections::HashMap<String, CursorSession>,
}

#[derive(Clone)]
struct ActiveStatement {
    token: String,
    host: String,
    port: u16,
    pid: u32,
    secret: u32,
}

type Shared = Arc<Mutex<SharedState>>;

fn main() {
    let state: Shared = Arc::new(Mutex::new(SharedState {
        connection: None,
        endpoint: None,
        active_cancel: None,
        manual_txn: false,
        cursors: std::collections::HashMap::new(),
    }));

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();

    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(error) => {
                let _ = writeln!(writer, "{{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{{\"code\":-32700,\"message\":\"parse error: {error}\"}}}}");
                let _ = writer.flush();
                continue;
            }
        };
        let id = request.get("id").cloned();
        let method = request.get("method").and_then(Value::as_str).unwrap_or_default().to_string();
        let params = request.get("params").cloned().unwrap_or(Value::Null);

        let response = match handle_request(&state, &method, &params) {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": error.0, "message": error.1 } }),
        };
        let mut encoded = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
        encoded.push('\n');
        if writer.write_all(encoded.as_bytes()).is_err() || writer.flush().is_err() {
            break;
        }
    }
}

type RpcError = (i32, String);

/// Locks the shared state, mapping poisoning to an RPC error.
fn lock_state(state: &Shared) -> Result<std::sync::MutexGuard<'_, SharedState>, RpcError> {
    state.lock().map_err(|_| (-32000, "state poisoned".to_string()))
}

/// Unwraps the live connection or fails with the protocol's not-connected error.
fn require_connection_mut<'a>(
    guard: &'a mut std::sync::MutexGuard<'_, SharedState>,
) -> Result<&'a mut PgConnection, RpcError> {
    guard
        .connection
        .as_mut()
        .ok_or_else(|| (-32002, "not connected".to_string()))
}


fn rpc_error(message: impl Into<String>) -> Result<Value, RpcError> {
    Err((-32000, message.into()))
}

fn handle_request(state: &Shared, method: &str, params: &Value) -> Result<Value, RpcError> {
    match method {
        "plugin/initialize" => Ok(json!({
            "protocolVersion": 1,
            "capabilities": ["query", "metadata", "cursorPaging", "manualTransaction", "cancel"],
            "plugin": { "id": "dbx-pg83-compat", "version": env!("CARGO_PKG_VERSION") }
        })),
        "testConnection" => test_connection(params),
        "connect" => connect(params, state),
        "connectionInfo" => connection_info(state),
        "disconnect" | "close" => {
            let mut guard = lock_state(state)?;
            guard.connection = None;
            guard.cursors.clear();
            guard.manual_txn = false;
            Ok(json!({ "ok": true }))
        }
        "cancelStatement" => cancel_statement(state, params),
        "executeQuery" => {
            let sql = required_str(params, "sql")?;
            execute_query(state, params, &sql, ExecutionMode::AutoCommit)
        }
        "executeInManualTransaction" => {
            let sql = required_str(params, "sql")?;
            execute_query(state, params, &sql, ExecutionMode::InTransaction)
        }
        "beginManualTransaction" => begin_manual_transaction(state, params),
        "commitManualTransaction" => commit_manual_transaction(state),
        "rollbackManualTransaction" => rollback_manual_transaction(state),
        "executeQueryPage" => {
            let sql = required_str(params, "sql")?;
            execute_query(state, params, &sql, ExecutionMode::AutoCommit)
        }
        "fetchQueryPage" => fetch_query_page(state, params),
        "closeQuerySession" => {
            let session_id = required_str(params, "sessionId")?;
            let mut guard = lock_state(state)?;
            Ok(json!({ "ok": guard.cursors.remove(session_id).is_some() }))
        }
        "listDatabases" => list_databases(state, params),
        "listSchemas" => list_schemas(state, params),
        "listTables" => list_tables(state, params),
        "listObjects" => list_objects(state, params),
        "listDataTypes" => list_data_types(state, params),
        "listIndexes" => list_indexes(state, params),
        "getColumns" => get_columns(state, params),
        "getObjectSource" => get_object_source(state, params),
        "getExplainInfo" => get_explain_info(state, params),
        "identity" => Ok(json!({ "driver": "dbx-pg83-compat", "version": env!("CARGO_PKG_VERSION") })),
        _ => rpc_error(format!("Unsupported JDBC plugin method: {method}")),
    }
}

fn required_str<'a>(params: &'a Value, key: &str) -> Result<&'a str, RpcError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| (-32602, format!("missing required parameter '{key}'")))
}

fn optional_str<'a>(params: &'a Value, key: &str) -> Option<&'a str> {
    params.get(key).and_then(Value::as_str).map(str::trim).filter(|value| !value.is_empty())
}

fn connect_options_from(params: &Value) -> Result<(jdbc_url::PgEndpoint, Duration), RpcError> {
    let connection = params
        .get("connection")
        .ok_or_else(|| (-32602, "missing connection".to_string()))?;
    let endpoint = jdbc_url::endpoint_from_connection(connection)
        .ok_or_else(|| (-32602, "connection_string must be a jdbc:postgresql:// URL".to_string()))?;
    let timeout = connection
        .get("connect_timeout_secs")
        .and_then(Value::as_u64)
        .or_else(|| connection.get("connectTimeoutSecs").and_then(Value::as_u64))
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(30));
    Ok((endpoint, timeout))
}

fn open_connection(state: &Shared, params: &Value) -> Result<(), RpcError> {
    let (endpoint, timeout) = connect_options_from(params)?;
    let options = ConnectOptions {
        host: &endpoint.host,
        port: endpoint.port,
        database: {
            // postgres is the conventional maintenance database.
            let database = if endpoint.database.is_empty() { "postgres" } else { &endpoint.database };
            database
        },
        username: {
            let username = if endpoint.username.is_empty() { "postgres" } else { &endpoint.username };
            username
        },
        password: &endpoint.password,
        connect_timeout: timeout,
    };
    let connection = PgConnection::connect(&options).map_err(|error| (-32001, error.to_display()))?;
    let mut guard = lock_state(state)?;
    guard.connection = Some(connection);
    guard.endpoint = Some((endpoint.host.clone(), endpoint.port));
    guard.manual_txn = false;
    guard.cursors.clear();
    Ok(())
}

fn test_connection(params: &Value) -> Result<Value, RpcError> {
    let (endpoint, timeout) = connect_options_from(params)?;
    let options = ConnectOptions {
        host: &endpoint.host,
        port: endpoint.port,
        database: if endpoint.database.is_empty() { "postgres" } else { &endpoint.database },
        username: if endpoint.username.is_empty() { "postgres" } else { &endpoint.username },
        password: &endpoint.password,
        connect_timeout: timeout,
    };
    let mut connection = PgConnection::connect(&options).map_err(|error| (-32001, error.to_display()))?;
    let info = connection.simple_query(meta83::connection_info_sql());
    let (product, version) = match info {
        Ok(outcome) => {
            let version = outcome
                .rows
                .first()
                .and_then(|row| row.first())
                .and_then(|cell| cell.clone())
                .unwrap_or_default();
            ("PostgreSQL", version)
        }
        Err(_) => ("PostgreSQL", String::new()),
    };
    Ok(json!({
        "ok": true,
        "databaseInfo": {
            "productName": product,
            "productVersion": version,
        }
    }))
}

fn connect(params: &Value, state: &Shared) -> Result<Value, RpcError> {
    open_connection(state, params)?;
    let mut guard = lock_state(state)?;
    if let Some(connection) = guard.connection.as_mut() {
        if let Ok(outcome) = connection.simple_query(meta83::connection_info_sql()) {
            if let Some(row) = outcome.rows.first() {
                if let Some(Some(version)) = row.first() {
                    return Ok(json!({
                        "ok": true,
                        "serverVersion": version,
                        "productName": "PostgreSQL"
                    }));
                }
            }
        }
    }
    Ok(json!({ "ok": true, "productName": "PostgreSQL" }))
}

fn connection_info(state: &Shared) -> Result<Value, RpcError> {
    let mut guard = lock_state(state)?;
    let connection = require_connection_mut(&mut guard)?;
    let outcome = connection.simple_query(meta83::connection_info_sql()).map_err(pg_error_to_rpc)?;
    let mut info = serde_json::Map::new();
    if let Some(row) = outcome.rows.first() {
        if let Some(Some(version)) = row.first() {
            info.insert("productName".into(), json!("PostgreSQL"));
            info.insert("productVersion".into(), json!(version));
        }
        if let Some(Some(user)) = row.get(1) {
            info.insert("userName".into(), json!(user));
        }
        if let Some(Some(database)) = row.get(2) {
            info.insert("databaseName".into(), json!(database));
        }
    }
    Ok(Value::Object(info))
}

// ---------------------------------------------------------------------------
// Query execution
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum ExecutionMode {
    AutoCommit,
    InTransaction,
}

fn pg_error_to_rpc(error: PgError) -> RpcError {
    (-32003, error.to_display())
}

fn execute_query(state: &Shared, params: &Value, sql: &str, mode: ExecutionMode) -> Result<Value, RpcError> {
    // Rewrite 9.1+ functions, then run the statement on the live connection.
    let rewritten = rewrite83::rewrite_for_pg83(sql);

    let cancel_token = optional_str(params, "cancelToken").map(str::to_string);
    let schema = optional_str(params, "schema");
    let max_rows = params
        .get("maxRows")
        .and_then(Value::as_u64)
        .map(|rows| rows as usize)
        .unwrap_or(1000);

    let (host, port, backend) = {
        let mut guard = lock_state(state)?;
        let connection = require_connection_mut(&mut guard)?;
        if mode == ExecutionMode::InTransaction && !connection.in_transaction {
            return rpc_error("no manual transaction");
        }
        let backend = (connection.backend_pid, connection.secret_key);
        let (host, port) = guard.endpoint.clone().unwrap_or_else(|| ("127.0.0.1".into(), 5432));
        (host, port, backend)
    };

    // Register the statement for cancellation before running it.
    if let Some(token) = &cancel_token {
        let mut guard = lock_state(state)?;
        guard.active_cancel = Some(ActiveStatement {
            token: token.clone(),
            host,
            port,
            pid: backend.0,
            secret: backend.1,
        });
    }

    let result = {
        let mut guard = lock_state(state)?;
        let connection = require_connection_mut(&mut guard)?;
        if let Some(schema) = schema {
            connection.set_search_path(schema).map_err(pg_error_to_rpc)?;
        }
        let started = Instant::now();
        let outcome = connection.simple_query(&rewritten);
        let elapsed = started.elapsed().as_millis() as u64;
        build_query_result(outcome.map_err(pg_error_to_rpc)?, elapsed, max_rows, None)?
    };

    if cancel_token.is_some() {
        let mut guard = lock_state(state)?;
        guard.active_cancel = None;
    }
    Ok(result)
}

/// Maps a wire outcome to the DBX QueryResult JSON shape.
fn build_query_result(
    outcome: StatementOutcome,
    elapsed_ms: u64,
    max_rows: usize,
    session_id: Option<String>,
) -> Result<Value, RpcError> {
    let mut columns = Vec::new();
    let mut column_types = Vec::new();
    let mut sortables = Vec::new();
    for field in &outcome.fields {
        columns.push(field.name.clone());
        column_types.push(oid_to_type_name(field.type_oid));
        sortables.push(is_sortable_oid(field.type_oid));
    }
    let truncated = outcome.rows.len() > max_rows;
    let rows: Vec<Vec<Value>> = outcome
        .rows
        .into_iter()
        .take(max_rows)
        .map(|row| row.into_iter().map(|cell| cell.map(Value::String).unwrap_or(Value::Null)).collect())
        .collect();
    let messages: Vec<Value> = outcome
        .notices
        .iter()
        .map(|notice| {
            json!({
                "severity": notice.severity,
                "message": notice.message,
                "code": notice.code,
                "detail": notice.detail,
                "hint": notice.hint,
            })
        })
        .collect();
    let mut result = json!({
        "columns": columns,
        "column_types": column_types,
        "column_sortables": sortables,
        "rows": rows,
        "affected_rows": outcome.affected_rows,
        "execution_time_ms": elapsed_ms,
        "truncated": truncated,
        "session_id": session_id,
        "has_more": false,
    });
    if !messages.is_empty() {
        result["messages"] = Value::Array(messages);
    }
    Ok(result)
}

/// Built-in type OIDs that existed in 8.3; unknown OIDs fall back to "text"
/// because every value we transfer is already text-encoded.
fn oid_to_type_name(oid: u32) -> String {
    let name = match oid {
        16 => "boolean",
        17 => "bytea",
        18 => "char",
        19 => "name",
        20 => "int8",
        21 => "int2",
        23 => "int4",
        24 => "regproc",
        25 => "text",
        26 => "oid",
        114 => "json",
        700 => "float4",
        701 => "float8",
        790 => "money",
        1042 => "bpchar",
        1043 => "varchar",
        1082 => "date",
        1083 => "time",
        1114 => "timestamp",
        1184 => "timestamptz",
        1186 => "interval",
        1231 => "numeric[]",
        1700 => "numeric",
        2278 => "void",
        _ => return "text".to_string(),
    };
    name.to_string()
}

fn is_sortable_oid(oid: u32) -> bool {
    !matches!(oid, 17 /* bytea */)
}

fn begin_manual_transaction(state: &Shared, _params: &Value) -> Result<Value, RpcError> {
    let mut guard = lock_state(state)?;
    let connection = require_connection_mut(&mut guard)?;
    connection.simple_query("BEGIN").map_err(pg_error_to_rpc)?;
    guard.manual_txn = true;
    Ok(json!({ "ok": true }))
}

fn commit_manual_transaction(state: &Shared) -> Result<Value, RpcError> {
    let mut guard = lock_state(state)?;
    let connection = require_connection_mut(&mut guard)?;
    if !connection.in_transaction {
        return rpc_error("no manual transaction");
    }
    connection.simple_query("COMMIT").map_err(pg_error_to_rpc)?;
    guard.manual_txn = false;
    Ok(json!({ "ok": true }))
}

fn rollback_manual_transaction(state: &Shared) -> Result<Value, RpcError> {
    let mut guard = lock_state(state)?;
    let connection = require_connection_mut(&mut guard)?;
    if !connection.in_transaction {
        return rpc_error("no manual transaction");
    }
    connection.simple_query("ROLLBACK").map_err(pg_error_to_rpc)?;
    guard.manual_txn = false;
    Ok(json!({ "ok": true }))
}

fn cancel_statement(state: &Shared, params: &Value) -> Result<Value, RpcError> {
    let token = required_str(params, "cancelToken")?;
    let guard = lock_state(state)?;
    let Some(active) = guard.active_cancel.as_ref() else {
        // The statement already finished; the host ignores late cancels.
        return Ok(json!({ "ok": false, "reason": "no active statement" }));
    };
    if active.token != token {
        return Ok(json!({ "ok": false, "reason": "token mismatch" }));
    }
    let (host, port, pid, secret) = (active.host.clone(), active.port, active.pid, active.secret);
    drop(guard);
    let _ = pgproto::send_cancel(&host, port, pid, secret);
    let mut guard = lock_state(state)?;
    guard.active_cancel = None;
    Ok(json!({ "ok": true }))
}

// ---------------------------------------------------------------------------
// Cursor paging
// ---------------------------------------------------------------------------

fn fetch_query_page(state: &Shared, params: &Value) -> Result<Value, RpcError> {
    let session_id = required_str(params, "sessionId")?;
    let page_size = params.get("pageSize").and_then(Value::as_u64).unwrap_or(1000) as usize;
    let mut guard = lock_state(state)?;
    let Some(cursor) = guard.cursors.get_mut(session_id) else {
        return rpc_error(format!("unknown query session '{session_id}'"));
    };
    let start = cursor.next_row;
    let end = (start + page_size).min(cursor.rows.len());
    let rows: Vec<Vec<Value>> = cursor.rows[start..end].to_vec();
    cursor.next_row = end;
    let has_more = end < cursor.rows.len();
    Ok(json!({
        "columns": cursor.columns,
        "column_types": cursor.column_types,
        "column_sortables": cursor.column_sortables,
        "rows": rows,
        "affected_rows": 0,
        "execution_time_ms": 0,
        "truncated": false,
        "session_id": session_id,
        "has_more": has_more,
    }))
}

// ---------------------------------------------------------------------------
// Metadata
// ---------------------------------------------------------------------------

fn run_metadata_query(state: &Shared, sql: &str, schema: Option<&str>) -> Result<StatementOutcome, RpcError> {
    let rewritten = rewrite83::rewrite_for_pg83(sql);
    let mut guard = lock_state(state)?;
    let connection = require_connection_mut(&mut guard)?;
    if let Some(schema) = schema {
        connection.set_search_path(schema).map_err(pg_error_to_rpc)?;
    }
    connection.simple_query(&rewritten).map_err(pg_error_to_rpc)
}

fn cell_string(row: &[Option<String>], index: usize) -> String {
    row.get(index).and_then(|cell| cell.clone()).unwrap_or_default()
}

fn cell_bool(row: &[Option<String>], index: usize) -> bool {
    matches!(row.get(index).and_then(|cell| cell.as_deref()), Some("t") | Some("true") | Some("1"))
}

fn cell_i64(row: &[Option<String>], index: usize) -> Option<i64> {
    row.get(index).and_then(|cell| cell.as_deref()).and_then(|value| value.parse().ok())
}

fn list_databases(state: &Shared, _params: &Value) -> Result<Value, RpcError> {
    let outcome = run_metadata_query(state, meta83::list_databases_sql(), None)?;
    let databases: Vec<Value> = outcome
        .rows
        .iter()
        .filter_map(|row| row.first().and_then(|cell| cell.clone()))
        .map(|name| json!({ "name": name }))
        .collect();
    Ok(Value::Array(databases))
}

fn list_schemas(state: &Shared, params: &Value) -> Result<Value, RpcError> {
    let database = optional_str(params, "database");
    let outcome = run_metadata_query(state, meta83::list_schemas_sql(), None)?;
    let _ = database; // each DBX pool is one physical connection per database
    let schemas: Vec<Value> = outcome
        .rows
        .iter()
        .filter_map(|row| row.first().and_then(|cell| cell.clone()))
        .map(Value::String)
        .collect();
    Ok(Value::Array(schemas))
}

fn list_tables(state: &Shared, params: &Value) -> Result<Value, RpcError> {
    let schema = optional_str(params, "schema").unwrap_or("public");
    let filter = optional_str(params, "filter");
    let object_types = params.get("object_types").and_then(Value::as_array);
    let object_types = object_types
        .map(|types| types.iter().filter_map(Value::as_str).map(str::to_string).collect::<Vec<_>>());
    let relkinds = meta83::TableTypeFlag::relkinds(object_types.as_ref());
    let sql = meta83::list_tables_sql(schema, filter, relkinds);
    let outcome = run_metadata_query(state, &sql, None)?;
    let mut tables = Vec::new();
    for row in &outcome.rows {
        tables.push(json!({
            "name": cell_string(row, 0),
            "table_type": cell_string(row, 1),
            "comment": row.get(2).and_then(|cell| cell.clone()),
            "parent_schema": schema,
            "parent_name": Value::Null,
        }));
    }
    Ok(Value::Array(tables))
}

/// Relations plus routines and types, for the schema object browser.
fn list_objects(state: &Shared, params: &Value) -> Result<Value, RpcError> {
    let schema = optional_str(params, "schema").unwrap_or("public");
    let filter = optional_str(params, "filter");
    // 8.3-compatible object listing: relations (r/v/S), functions, triggers.
    let filter_clause = filter
        .map(|pattern| format!(" AND c.relname LIKE {}", jdbc_url::sql_quote_string(pattern)))
        .unwrap_or_default();
    let relations_sql = format!("{}{filter_clause}", meta83::list_relations_sql(schema));
    let functions_sql = meta83::list_functions_sql(schema);
    let triggers_sql = meta83::list_triggers_sql(schema);

    let mut objects = Vec::new();
    for sql in [relations_sql, functions_sql, triggers_sql] {
        let outcome = run_metadata_query(state, &sql, None)?;
        for row in &outcome.rows {
            let object_type = cell_string(row, 1);
            // Relations carry a comment in slot 2; routines carry a signature.
            let is_relation = matches!(object_type.as_str(), "TABLE" | "VIEW" | "SEQUENCE");
            let third = row.get(2).and_then(|cell| cell.clone());
            let third_json = third.map(Value::String).unwrap_or(Value::Null);
            objects.push(json!({
                "name": cell_string(row, 0),
                "object_type": object_type,
                "schema": schema,
                "signature": if is_relation { Value::Null } else { third_json.clone() },
                "comment": if is_relation { third_json } else { Value::Null },
            }));
        }
    }
    Ok(Value::Array(objects))
}

fn list_data_types(state: &Shared, _params: &Value) -> Result<Value, RpcError> {
    let outcome = run_metadata_query(state, meta83::list_data_types_sql(), None)?;
    let types: Vec<Value> = outcome
        .rows
        .iter()
        .filter_map(|row| row.first().and_then(|cell| cell.clone()))
        .map(Value::String)
        .collect();
    Ok(Value::Array(types))
}

fn list_indexes(state: &Shared, params: &Value) -> Result<Value, RpcError> {
    let schema = optional_str(params, "schema").unwrap_or("public");
    let table = required_str(params, "table")?;
    let sql = meta83::list_indexes_sql(schema, table);
    let outcome = run_metadata_query(state, &sql, None)?;
    let mut indexes = Vec::new();
    for row in &outcome.rows {
        // Column names come from the definition via pg_index indkey mapping;
        // the caller also accepts the raw definition, so ship both.
        let definition = cell_string(row, 1);
        let columns = index_columns_from_definition(&definition);
        indexes.push(json!({
            "name": cell_string(row, 0),
            "columns": columns,
            "is_unique": cell_bool(row, 2),
            "is_primary": cell_bool(row, 3),
            "index_type": index_type_from_definition(&definition),
            "comment": Value::Null,
        }));
    }
    Ok(Value::Array(indexes))
}

fn index_columns_from_definition(definition: &str) -> Vec<String> {
    // CREATE [UNIQUE] INDEX name ON table USING method (col1, col2 ...)
    // The column list starts after the access-method name; fall back to the
    // first '(' for definitions without a USING clause.
    let open = match definition.to_ascii_lowercase().find(" using ") {
        Some(using) => definition[using..].find('(').map(|offset| using + offset),
        None => definition.find('('),
    };
    let Some(open) = open else { return Vec::new() };
    let body = &definition[open + 1..];
    let close = body.rfind(')').unwrap_or(body.len());
    split_top_level_commas(&body[..close])
        .into_iter()
        .map(|part| {
            part.split_whitespace()
                .next()
                .unwrap_or(&part)
                .trim_matches('"')
                .to_string()
        })
        .collect()
}

/// Splits on commas that are not nested inside parentheses.
fn split_top_level_commas(input: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (index, character) in input.char_indices() {
        match character {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(input[start..index].trim().to_string());
                start = index + 1;
            }
            _ => {}
        }
    }
    let tail = input[start..].trim();
    if !tail.is_empty() {
        parts.push(tail.to_string());
    }
    parts
}

fn index_type_from_definition(definition: &str) -> String {
    let lower = definition.to_ascii_lowercase();
    if lower.contains("using btree") {
        "btree".to_string()
    } else if lower.contains("using hash") {
        "hash".to_string()
    } else if lower.contains("using gist") {
        "gist".to_string()
    } else if lower.contains("using gin") {
        "gin".to_string()
    } else {
        "btree".to_string()
    }
}

fn get_columns(state: &Shared, params: &Value) -> Result<Value, RpcError> {
    let schema = optional_str(params, "schema").unwrap_or("public");
    let table = required_str(params, "table")?;

    // Primary keys come from pg_get_constraintdef text (the psql 8.3 way),
    // so the column query never has to dereference pg_constraint.conkey.
    let mut primary_keys = std::collections::HashSet::new();
    let pk_outcome = run_metadata_query(state, &meta83::primary_keys_sql(schema, table), None)?;
    for row in &pk_outcome.rows {
        if let Some(Some(definition)) = row.first() {
            for column in parse_primary_key_columns(definition) {
                primary_keys.insert(column.to_ascii_lowercase());
            }
        }
    }

    let outcome = run_metadata_query(state, &meta83::get_columns_sql(schema, table), None)?;
    let mut columns = Vec::new();
    for row in &outcome.rows {
        // attname, data_type, typtype, attnotnull, default, attnum, char_len, comment
        let typtype = cell_string(row, 2);
        let raw_type = cell_string(row, 1);
        let is_array = typtype == "b" && raw_type.starts_with('_');
        let data_type = if is_array {
            format!("{}[]", raw_type.trim_start_matches('_'))
        } else {
            raw_type
        };
        let name = cell_string(row, 0);
        columns.push(json!({
            "name": name,
            "data_type": data_type,
            "is_nullable": !cell_bool(row, 3),
            "column_default": row.get(4).and_then(|cell| cell.clone()),
            "is_primary_key": primary_keys.contains(&name.to_ascii_lowercase()),
            "is_unique": false,
            "comment": row.get(7).and_then(|cell| cell.clone()),
            "character_maximum_length": cell_i64(row, 6).map(|len| len as i32),
            "numeric_precision": Value::Null,
            "numeric_scale": Value::Null,
            "extra": Value::Null,
        }));
    }
    Ok(Value::Array(columns))
}

/// Extracts column names from a `pg_get_constraintdef` PRIMARY KEY rendering
/// such as `PRIMARY KEY (id, "name col")`, unquoting each identifier.
fn parse_primary_key_columns(definition: &str) -> Vec<String> {
    let (Some(open), Some(close)) = (definition.find('('), definition.rfind(')')) else {
        return Vec::new();
    };
    if close <= open {
        return Vec::new();
    }
    split_top_level_commas(&definition[open + 1..close])
        .into_iter()
        .map(|part| unquote_sql_ident(&part))
        .collect()
}

/// Strips SQL double-quoting from a single identifier (`"x""y"` → `x"y`).
fn unquote_sql_ident(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        trimmed[1..trimmed.len() - 1].replace("\"\"", "\"")
    } else {
        trimmed.to_string()
    }
}

fn get_object_source(state: &Shared, params: &Value) -> Result<Value, RpcError> {
    let schema = optional_str(params, "schema").unwrap_or("public");
    let name = required_str(params, "name")?;
    let object_type = optional_str(params, "object_type").unwrap_or("VIEW");

    // Sequence DDL is assembled driver-side from the sequence relation's own
    // parameters; there is no pg_sequence catalog and no seq_*() functions on
    // 8.3, and the true START value is not recoverable before PostgreSQL 10.
    if object_type.eq_ignore_ascii_case("SEQUENCE") {
        let outcome = run_metadata_query(state, &meta83::sequence_state_sql(schema, name), None)?;
        let source = outcome.rows.first().map(|row| {
            let text = |index: usize| cell_string(row, index);
            format!(
                "CREATE SEQUENCE {}.{} INCREMENT BY {} MINVALUE {} MAXVALUE {} CACHE {}{}; -- last_value: {} (true START not recorded before PostgreSQL 10)",
                jdbc_url::sql_quote_ident(schema),
                jdbc_url::sql_quote_ident(name),
                text(0),
                text(1),
                text(2),
                text(3),
                if cell_bool(row, 4) { " CYCLE" } else { " NO CYCLE" },
                text(5),
            )
        });
        return Ok(json!({
            "name": name,
            "object_type": "SEQUENCE",
            "schema": schema,
            "source": source.unwrap_or_default(),
        }));
    }

    let sql = match object_type.to_ascii_uppercase().as_str() {
        "VIEW" | "MATERIALIZED_VIEW" => meta83::get_view_source_sql(schema, name),
        "FUNCTION" | "PROCEDURE" => meta83::get_function_source_sql(schema, name),
        "TRIGGER" => meta83::get_trigger_source_sql(schema, name),
        other => return rpc_error(format!("object source for '{other}' is not supported")),
    };
    let outcome = run_metadata_query(state, &sql, None)?;
    let source = outcome
        .rows
        .first()
        .and_then(|row| row.first().and_then(|cell| cell.clone()))
        .unwrap_or_default();
    Ok(json!({
        "name": name,
        "object_type": object_type.to_ascii_uppercase(),
        "schema": schema,
        "source": source,
    }))
}

fn get_explain_info(state: &Shared, params: &Value) -> Result<Value, RpcError> {
    let sql = required_str(params, "sql")?;
    let mode = optional_str(params, "mode").unwrap_or("explain");
    let prefix = match mode.to_ascii_lowercase().as_str() {
        "analyze" | "execution" => "EXPLAIN ANALYZE",
        _ => "EXPLAIN",
    };
    let explain_sql = format!("{prefix} {}", rewrite83::rewrite_for_pg83(sql));
    let outcome = run_metadata_query(state, &explain_sql, optional_str(params, "schema"))?;
    // EXPLAIN output is one text column "QUERY PLAN", one row per plan line.
    let plan = outcome
        .rows
        .iter()
        .map(|row| cell_string(row, 0))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(json!({ "plan": plan }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_key_definition_parsing() {
        assert_eq!(parse_primary_key_columns("PRIMARY KEY (id)"), vec!["id".to_string()]);
        assert_eq!(
            parse_primary_key_columns("PRIMARY KEY (order_id, item_no)"),
            vec!["order_id".to_string(), "item_no".to_string()]
        );
        // Quoted identifiers with spaces and embedded commas/quotes.
        assert_eq!(
            parse_primary_key_columns(r#"PRIMARY KEY ("name col", "x""y", plain)"#),
            vec![r#""name col""#.replace('"', ""), "x\"y".to_string(), "plain".to_string()]
        );
        assert!(parse_primary_key_columns("PRIMARY KEY").is_empty());
        assert!(parse_primary_key_columns("").is_empty());
    }

    #[test]
    fn identifier_unquoting() {
        assert_eq!(unquote_sql_ident("id"), "id");
        assert_eq!(unquote_sql_ident("  id  "), "id");
        assert_eq!(unquote_sql_ident(r#""name col""#), "name col");
        assert_eq!(unquote_sql_ident(r#""x""y""#), "x\"y");
        assert_eq!(unquote_sql_ident(r#""a, b""#), "a, b");
    }

    #[test]
    fn index_definition_parsing() {
        let columns = index_columns_from_definition(
            "CREATE UNIQUE INDEX idx_user_email ON public.users USING btree (email COLLATE pg_catalog.\"default\" ASC)",
        );
        assert_eq!(columns, vec!["email".to_string()]);
        let multi = index_columns_from_definition("CREATE INDEX i ON t USING btree (a, b DESC, (lower(c)))");
        assert_eq!(multi, vec!["a".to_string(), "b".to_string(), "(lower(c))".to_string()]);
        assert_eq!(index_type_from_definition("CREATE INDEX i ON t USING hash (a)"), "hash");
    }

    #[test]
    fn type_names_cover_8_3_builtins() {
        assert_eq!(oid_to_type_name(23), "int4");
        assert_eq!(oid_to_type_name(25), "text");
        assert_eq!(oid_to_type_name(1700), "numeric");
        assert_eq!(oid_to_type_name(999999), "text");
    }

    #[test]
    fn query_result_shape_matches_protocol() {
        let outcome = StatementOutcome {
            fields: vec![
                pgproto::FieldDescription { name: "id".into(), type_oid: 23 },
                pgproto::FieldDescription { name: "name".into(), type_oid: 25 },
            ],
            rows: vec![vec![Some("1".into()), None]],
            affected_rows: 0,
            notices: Vec::new(),
            empty_query: false,
            in_transaction: false,
        };
        let result = build_query_result(outcome, 5, 100, Some("s1".into())).unwrap();
        assert_eq!(result["columns"], json!(["id", "name"]));
        assert_eq!(result["column_types"], json!(["int4", "text"]));
        assert_eq!(result["rows"], json!([["1", null]]));
        assert_eq!(result["session_id"], json!("s1"));
        assert_eq!(result["truncated"], json!(false));
    }
}
