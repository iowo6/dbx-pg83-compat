# DBX Legacy PostgreSQL Driver

中文 | [English](README.md)

A pure-Rust external driver plugin for [DBX](https://github.com/t8y2/dbx) that makes **PostgreSQL 8.3 – 9.0** servers usable again — no Java, no JDBC driver JARs, no runtime dependencies.

## The problem

DBX's built-in PostgreSQL integration assumes a modern server and fails on 8.3:

| DBX behavior | Result on 8.3 |
| --- | --- |
| Table previews generate `left(col, 8193)` | `left()`/`right()` were added in **9.1** — `function left(text, integer) does not exist` (SQLSTATE 42883) |
| Metadata queries use `format()` (9.1), `row_to_json` (9.2), `LATERAL` (9.3), `unnest WITH ORDINALITY` (9.4) | Assorted "does not exist" errors; the database/schema/table tree cannot browse |

This plugin registers itself as DBX's **JDBC external driver**, speaks the PostgreSQL 3.0 wire protocol directly (the protocol 8.3 natively speaks), and stays compatible at two layers:

1. **SQL rewriter** (`src/rewrite83.rs`): every statement is rewritten before it is sent —
   - `left(e, n)`  → `(CASE WHEN (n) < 1 THEN substring(e from 1 for 0) ELSE substring(e from 1 for (n)) END)`
   - `right(e, n)` → an equivalent `substring` CASE expansion
   - String literals, quoted identifiers, comments, qualified `pg_catalog.left(...)` calls, JOIN keywords, and columns actually named `left` are all left untouched
2. **8.3-era metadata SQL** (`src/meta83.rs`): every query uses only catalogs and functions that existed in 8.3, cross-checked against what psql 8.3 itself queried (`oidvectortypes` signatures, `pg_get_constraintdef` primary keys, function sources rebuilt from `pg_proc`). Unit tests assert no 8.4+/9.x+ feature appears.

## Install

### Option A: download a release

Grab the archive for your platform from [Releases](../../releases) and unpack `dbx-pg83-compat/` into DBX's plugin directory:

| DBX flavor | Plugin directory |
| --- | --- |
| **Windows installed** (`uninstall.exe` beside dbx.exe, most common) | `%APPDATA%\com.dbx.app\plugins\` |
| Windows portable (`portable.dbx` beside dbx.exe) | `<DBX folder>\data\plugins\` |
| Custom plugin store dir in settings | wherever settings say |
| CLI / MCP | `~/.dbx/plugins/` |

### Option B: build from source

```bash
git clone https://github.com/cyh2002-408/dbx-pg83-compat.git
cd dbx-pg83-compat
cargo build --release
cargo test              # 32 unit tests
python smoke/smoke.py   # end-to-end smoke (Python 3 required)
```

Place `target/release/dbx-pg83-driver` (`.exe` on Windows) under the layout below, or run `install.ps1` (Windows).

### Layout

```
<plugin dir>/dbx-pg83-compat/
├── manifest.json
└── bin/
    ├── dbx-pg83-driver.bat      ← loaded on Windows
    └── dbx-pg83-driver.exe
```

On Linux/macOS, `bin/dbx-pg83-driver` is the extensionless native binary.

## Usage

1. **Fully restart DBX** (watch the tray — the close button minimizes there by default).
2. Create a connection of type **JDBC** with URL `jdbc:postgresql://host:5432/dbname`.
3. Fill in the server credentials (or put them in the URL as `?user=..&password=..`).

Database/schema/table browsing, table-data previews (large-value truncation included), SQL execution, manual transactions, statement cancel, and EXPLAIN all run through this plugin.

**Uninstall**: delete the plugin directory; nothing else in DBX is affected.

## Implemented surface & limitations

**DBX external-driver protocol** (same channel as the official JDBC plugin):
`testConnection` / `connect` / `connectionInfo` / `disconnect` / `executeQuery` / `executeQueryPage` / `fetchQueryPage` / `closeQuerySession` / `beginManualTransaction` / `executeInManualTransaction` / `commitManualTransaction` / `rollbackManualTransaction` / `cancelStatement` / `listDatabases` / `listSchemas` / `listTables` / `listObjects` / `listDataTypes` / `listIndexes` / `getColumns` / `getObjectSource` / `getExplainInfo`

**Wire protocol**: PostgreSQL 3.0 — trust / password / md5 auth, simple query, text-format values, UTF-8 client encoding.

**Known limitations**:
- No SSL (allow plaintext on `sslmode=require` servers first; 8.3-era deployments were typically plaintext intranets).
- No SCRAM (PG 10+ only; 8.3 only ever offers md5/password, which is exactly what's covered).
- `getObjectSource` covers VIEW / FUNCTION / TRIGGER / SEQUENCE; function sources are approximations rebuilt from `pg_proc` (nothing better exists before 8.4) — for viewing, not round-tripping; sequence `START WITH` is not recorded in the 8.3 catalog, so the current value is attached as a comment.
- One sidecar process per database connection (DBX's JDBC pool model).

## How it works

```
DBX host
   │  db_type = JDBC → external_driver_pool("jdbc")
   ▼
manifest.json (v0 drivers declaration, driver id "jdbc")
   │  stdio JSON-Lines (JSON-RPC 2.0)
   ▼
dbx-pg83-driver (sidecar)
   ├─ jdbc_url.rs   parse jdbc:postgresql:// URLs (url_params, credentials, IPv6, multi-host)
   ├─ rewrite83.rs  left()/right() → substring() CASE rewriting (lexical, literal/comment-safe)
   ├─ meta83.rs     8.3-compatible metadata SQL generation
   └─ pgproto.rs    PG 3.0 wire protocol (startup/md5/simple query/cancel)
```

The smoke suite (`smoke/smoke.py`) spins up a fake PostgreSQL 8.3 server implementing the 3.0 wire protocol and verifies at the byte level: MD5 auth, that `left()`/`right()` rewriting actually happens (the fake server rejects raw `left()` with 42883 exactly like a real 8.3), primary keys/columns, function signatures, sequence state, manual transactions, and error propagation — plus a global assertion that no 8.4+ function ever reaches the server.

## License

Apache-2.0 (same as DBX itself).
