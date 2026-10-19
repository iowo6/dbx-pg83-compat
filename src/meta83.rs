//! PostgreSQL 8.3-compatible metadata SQL.
//!
//! Every statement here restricts itself to catalog features that already
//! existed in 8.3, cross-checked against what psql 8.3 itself queried
//! (`describe.c` of that era): `oidvectortypes` for function signatures,
//! `pg_get_constraintdef` for constraints, `format_type`, and the core
//! catalogs `pg_class` / `pg_namespace` / `pg_attribute` / `pg_index`
//! / `pg_constraint` / `pg_proc` / `pg_type` / `pg_language` /
//! `pg_description` / `pg_attrdef`.
//!
//! Deliberately NOT used (server versions): `format()` (9.1), `left/right()`
//! (9.1), `row_to_json`/`to_json` (9.2), `LATERAL` (9.3), `unnest(..) WITH
//! ORDINALITY` (9.4), `pg_collation` (9.1), `conparentid` on pg_constraint
//! (11), `prokind` (11), `acldefault()` (9.6+), `pg_sequence` (10),
//! `pg_get_functiondef` / `pg_get_function_identity_arguments` /
//! `pg_get_function_arguments` (8.4 / 8.4 / 9.0 — the reason functions are
//! reconstructed from raw catalog columns here), the `information_schema`
//! private helpers `_pg_truetypid` / `_pg_truetypmod` /
//! `_pg_char_max_length`, and the pretty-printing boolean overloads of
//! `pg_get_viewdef` / `pg_get_indexdef` (8.2+; the 1-argument forms are
//! ancient and used instead).

use crate::jdbc_url::{sql_quote_ident, sql_quote_string};

/// `SELECT datname FROM pg_database WHERE datallowconn ORDER BY 1`
/// (`datallowconn` exists since 7.1).
pub fn list_databases_sql() -> &'static str {
    "SELECT datname FROM pg_catalog.pg_database WHERE datallowconn ORDER BY 1"
}

/// Non-system schemas of the current database.
/// `information_schema.schemata` has had catalog_name/schema_name since 7.x.
pub fn list_schemas_sql() -> &'static str {
    "SELECT schema_name FROM information_schema.schemata \
     WHERE schema_name NOT IN ('pg_catalog', 'information_schema') \
     AND schema_name NOT LIKE 'pg_toast%' AND schema_name NOT LIKE 'pg_temp%' \
     ORDER BY 1"
}

pub struct TableTypeFlag;

impl TableTypeFlag {
    /// relkind letters for `object_types` — 8.3 uses 'r', 'v', 'S' only
    /// ('f' foreign tables arrive in 9.1, 'm' materialized views in 9.3).
    /// The returned slice leaks a small string per distinct combination;
    /// call sites are bounded by a handful of combinations per session.
    pub fn relkinds(object_types: Option<&Vec<String>>) -> &'static str {
        let Some(types) = object_types else { return "'r','v','S'" };
        let mut want_tables = false;
        let mut want_views = false;
        let mut want_sequences = false;
        for object_type in types {
            match object_type.trim().to_ascii_uppercase().as_str() {
                "TABLE" => want_tables = true,
                "VIEW" => want_views = true,
                "SEQUENCE" => want_sequences = true,
                _ => {}
            }
        }
        if !want_tables && !want_views && !want_sequences {
            return "'r','v','S'";
        }
        let mut letters = Vec::new();
        if want_tables {
            letters.push("'r'");
        }
        if want_views {
            letters.push("'v'");
        }
        if want_sequences {
            letters.push("'S'");
        }
        Box::leak(letters.join(",").into_boxed_str())
    }
}

/// Relations of one schema. `relkind IN (...)` plus an optional name filter.
pub fn list_tables_sql(schema: &str, name_filter: Option<&str>, relkinds: &str) -> String {
    let filter = name_filter
        .map(str::trim)
        .filter(|pattern| !pattern.is_empty())
        .map(|pattern| format!(" AND c.relname LIKE {}", sql_quote_string(pattern)))
        .unwrap_or_default();
    format!(
        "SELECT c.relname, \
           CASE c.relkind WHEN 'r' THEN 'TABLE' WHEN 'v' THEN 'VIEW' WHEN 'S' THEN 'SEQUENCE' END AS table_type, \
           d.description AS comment \
         FROM pg_catalog.pg_class c \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         LEFT JOIN pg_catalog.pg_description d ON d.objoid = c.oid AND d.objsubid = 0 \
         WHERE n.nspname = {schema_literal} AND c.relkind IN ({relkinds}){filter} \
         ORDER BY c.relkind, c.relname",
        schema_literal = sql_quote_string(schema),
    )
}

/// Columns of one table. `char_len` comes straight from `atttypmod` (the
/// varchar/bpchar length is stored as length + VARHDRSZ), and primary keys
/// are resolved separately via [`primary_keys_sql`] so this query never
/// touches `conkey` (whose array semantics differ between 8.x releases).
pub fn get_columns_sql(schema: &str, table: &str) -> String {
    format!(
        "SELECT a.attname, \
           CASE WHEN t.typtype = 'd' THEN bt.typname ELSE t.typname END AS data_type, \
           t.typtype, \
           a.attnotnull, \
           pg_catalog.pg_get_expr(ad.adbin, ad.adrelid) AS column_default, \
           a.attnum, \
           CASE WHEN (CASE WHEN t.typtype = 'd' THEN bt.typname ELSE t.typname END) IN ('bpchar', 'varchar') \
                  AND a.atttypmod >= 4 \
                THEN a.atttypmod - 4 ELSE NULL END AS char_len, \
           d.description AS comment \
         FROM pg_catalog.pg_attribute a \
         JOIN pg_catalog.pg_class c ON c.oid = a.attrelid \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         JOIN pg_catalog.pg_type t ON t.oid = a.atttypid \
         LEFT JOIN pg_catalog.pg_type bt ON bt.oid = t.typbasetype \
         LEFT JOIN pg_catalog.pg_attrdef ad ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum \
         LEFT JOIN pg_catalog.pg_description d ON d.objoid = a.attrelid AND d.objsubid = a.attnum \
         WHERE n.nspname = {schema_literal} AND c.relname = {table_literal} AND a.attnum > 0 AND NOT a.attisdropped \
         ORDER BY a.attnum",
        schema_literal = sql_quote_string(schema),
        table_literal = sql_quote_string(table),
    )
}

/// PRIMARY KEY definition of one table as `pg_get_constraintdef` text, e.g.
/// `PRIMARY KEY (id, "name col")`. One-argument `pg_get_constraintdef` exists
/// since 7.4 and is what psql 8.3's `\d` used; the caller parses the column
/// list out of the rendered text instead of touching `conkey`.
pub fn primary_keys_sql(schema: &str, table: &str) -> String {
    format!(
        "SELECT pg_catalog.pg_get_constraintdef(con.oid) AS definition \
         FROM pg_catalog.pg_constraint con \
         JOIN pg_catalog.pg_class c ON c.oid = con.conrelid \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE con.contype = 'p' AND n.nspname = {schema_literal} AND c.relname = {table_literal}",
        schema_literal = sql_quote_string(schema),
        table_literal = sql_quote_string(table),
    )
}

/// Indexes of one table. The one-argument `pg_get_indexdef` is the ancient
/// form; the (oid, colno, pretty) overload is 8.2+ and deliberately avoided.
pub fn list_indexes_sql(schema: &str, table: &str) -> String {
    format!(
        "SELECT ic.relname AS index_name, \
           pg_catalog.pg_get_indexdef(ix.indexrelid) AS definition, \
           ix.indisunique, ix.indisprimary \
         FROM pg_catalog.pg_index ix \
         JOIN pg_catalog.pg_class tc ON tc.oid = ix.indrelid \
         JOIN pg_catalog.pg_class ic ON ic.oid = ix.indexrelid \
         JOIN pg_catalog.pg_namespace n ON n.oid = tc.relnamespace \
         WHERE n.nspname = {schema_literal} AND tc.relname = {table_literal} \
         ORDER BY ic.relname",
        schema_literal = sql_quote_string(schema),
        table_literal = sql_quote_string(table),
    )
}

/// View DDL. One-argument `pg_get_viewdef` exists since 7.x; the pretty
/// boolean overload is 8.2+.
pub fn get_view_source_sql(schema: &str, name: &str) -> String {
    format!(
        "SELECT 'CREATE OR REPLACE VIEW ' || quote_ident(n.nspname) || '.' || quote_ident(c.relname) || ' AS ' || pg_catalog.pg_get_viewdef(c.oid) \
         FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = {schema_literal} AND c.relname = {name_literal} AND c.relkind = 'v'",
        schema_literal = sql_quote_string(schema),
        name_literal = sql_quote_string(name),
    )
}

/// Function DDL reconstructed from `pg_proc` columns.
/// `pg_get_functiondef` only exists from 8.4, so on 8.3 the definition is
/// rebuilt the way pg_dump 8.3 did: name, `oidvectortypes` argument list,
/// `format_type` return type, the `prosrc` body and the `pg_language` name.
/// Defaults, volatility and strictness are not recoverable this way; the
/// source is for viewing, not round-tripping.
pub fn get_function_source_sql(schema: &str, name: &str) -> String {
    format!(
        "SELECT 'CREATE OR REPLACE FUNCTION ' || quote_ident(n.nspname) || '.' || quote_ident(p.proname) || \
           '(' || COALESCE(pg_catalog.oidvectortypes(p.proargtypes), '') || ')' || \
           ' RETURNS ' || CASE WHEN p.proretset THEN 'SETOF ' ELSE '' END || pg_catalog.format_type(p.prorettype, NULL) || \
           ' AS ' || quote_literal(COALESCE(p.prosrc, p.probin, '')) || \
           ' LANGUAGE ' || l.lanname || ';' AS source \
         FROM pg_catalog.pg_proc p \
         JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
         JOIN pg_catalog.pg_language l ON l.oid = p.prolang \
         WHERE n.nspname = {schema_literal} AND p.proname = {name_literal} \
         ORDER BY p.oid",
        schema_literal = sql_quote_string(schema),
        name_literal = sql_quote_string(name),
    )
}

/// Trigger DDL. `pg_get_triggerdef(oid)` exists since 8.0 and is what
/// pg_dump 8.3 used.
pub fn get_trigger_source_sql(schema: &str, name: &str) -> String {
    format!(
        "SELECT pg_catalog.pg_get_triggerdef(t.oid) AS source \
         FROM pg_catalog.pg_trigger t \
         JOIN pg_catalog.pg_class c ON c.oid = t.tgrelid \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = {schema_literal} AND t.tgname = {name_literal} AND NOT t.tgisconstraint",
        schema_literal = sql_quote_string(schema),
        name_literal = sql_quote_string(name),
    )
}

/// Sequence parameters, read from the sequence relation itself.
/// `pg_sequence` as a catalog table only exists from 10, and there are no
/// `seq_*` catalog functions; selecting the sequence is the 8.3-native way
/// (psql \d did the same) and a plain SELECT never advances the sequence.
pub fn sequence_state_sql(schema: &str, name: &str) -> String {
    format!(
        "SELECT increment_by, min_value, max_value, cache_value, is_cycled, last_value \
         FROM {}.{}",
        sql_quote_ident(schema),
        sql_quote_ident(name),
    )
}

/// Function list with signatures. `pg_get_function_identity_arguments` is
/// 8.4+ and `pg_get_function_arguments` is 9.0+; `oidvectortypes` is the
/// pre-8.4 equivalent psql itself used for `\df`.
pub fn list_functions_sql(schema: &str) -> String {
    format!(
        "SELECT p.proname, 'FUNCTION', \
           '(' || COALESCE(pg_catalog.oidvectortypes(p.proargtypes), '') || ')' AS signature \
         FROM pg_catalog.pg_proc p \
         JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
         WHERE n.nspname = {schema_literal}",
        schema_literal = sql_quote_string(schema),
    )
}

/// Trigger list of one schema.
pub fn list_triggers_sql(schema: &str) -> String {
    format!(
        "SELECT t.tgname, 'TRIGGER', NULL AS signature \
         FROM pg_catalog.pg_trigger t \
         JOIN pg_catalog.pg_class c ON c.oid = t.tgrelid \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = {schema_literal} AND NOT t.tgisconstraint",
        schema_literal = sql_quote_string(schema),
    )
}

/// Relations of one schema for the object browser (name, type, comment).
pub fn list_relations_sql(schema: &str) -> String {
    format!(
        "SELECT c.relname, \
           CASE c.relkind WHEN 'r' THEN 'TABLE' WHEN 'v' THEN 'VIEW' WHEN 'S' THEN 'SEQUENCE' END AS object_type, \
           d.description AS comment \
         FROM pg_catalog.pg_class c \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         LEFT JOIN pg_catalog.pg_description d ON d.objoid = c.oid AND d.objsubid = 0 \
         WHERE n.nspname = {schema_literal} AND c.relkind IN ('r','v','S')",
        schema_literal = sql_quote_string(schema),
    )
}

/// Distinct data type names visible in the current database.
pub fn list_data_types_sql() -> &'static str {
    "SELECT DISTINCT t.typname FROM pg_catalog.pg_type t \
     JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace \
     WHERE n.nspname = 'pg_catalog' OR n.nspname = 'public' \
     ORDER BY 1"
}

/// Server version banner + current user + current database, used for the
/// DBX connection info panel.
pub fn connection_info_sql() -> &'static str {
    "SELECT current_setting('server_version'), current_user, current_database()"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relkind_letters_follow_object_types() {
        let tables = vec!["TABLE".to_string()];
        assert_eq!(TableTypeFlag::relkinds(Some(&tables)), "'r'");
        let views = vec!["VIEW".to_string()];
        assert_eq!(TableTypeFlag::relkinds(Some(&views)), "'v'");
        let sequences = vec!["SEQUENCE".to_string()];
        assert_eq!(TableTypeFlag::relkinds(Some(&sequences)), "'S'");
        let unknown = vec!["FOREIGN TABLE".to_string()];
        assert_eq!(TableTypeFlag::relkinds(Some(&unknown)), "'r','v','S'");
        assert_eq!(TableTypeFlag::relkinds(None), "'r','v','S'");
    }

    #[test]
    fn metadata_sql_has_no_modern_functions() {
        let all = [
            list_databases_sql().to_string(),
            list_schemas_sql().to_string(),
            list_tables_sql("public", None, "'r','v','S'"),
            get_columns_sql("public", "t"),
            primary_keys_sql("public", "t"),
            list_indexes_sql("public", "t"),
            get_view_source_sql("public", "v"),
            get_function_source_sql("public", "f"),
            get_trigger_source_sql("public", "trg"),
            sequence_state_sql("public", "seq"),
            list_functions_sql("public"),
            list_triggers_sql("public"),
            list_relations_sql("public"),
            list_data_types_sql().to_string(),
            connection_info_sql().to_string(),
        ];
        for sql in &all {
            let lower = sql.to_lowercase();
            assert!(!lower.contains("format("), "format() is 9.1+: {sql}");
            assert!(!lower.contains("left("), "left( is 9.1+: {sql}");
            assert!(!lower.contains("right("), "right( is 9.1+: {sql}");
            assert!(!lower.contains("row_to_json"), "row_to_json is 9.2+: {sql}");
            assert!(!lower.contains("to_jsonb"), "jsonb is 9.4+: {sql}");
            assert!(!lower.contains("lateral"), "LATERAL is 9.3+: {sql}");
            assert!(!lower.contains("with ordinality"), "ORDINALITY is 9.4+: {sql}");
            assert!(!lower.contains("pg_collation"), "pg_collation is 9.1+: {sql}");
            assert!(!lower.contains("conparentid"), "conparentid is 11+: {sql}");
            assert!(!lower.contains("prokind"), "prokind is 11+: {sql}");
            assert!(!lower.contains("acldefault"), "acldefault is 9.6+: {sql}");
            assert!(!lower.contains("string_agg"), "string_agg is 9.0+: {sql}");
            assert!(!lower.contains("attidentity"), "attidentity is 10+: {sql}");
            assert!(!lower.contains("attgenerated"), "attgenerated is 12+: {sql}");
            assert!(!lower.contains("indnullsnotdistinct"), "indnullsnotdistinct is 15+: {sql}");
            // The two functions the user's 8.3 server rejected (both are 8.4+):
            assert!(!lower.contains("pg_get_functiondef"), "pg_get_functiondef is 8.4+: {sql}");
            assert!(
                !lower.contains("pg_get_function_identity_arguments"),
                "pg_get_function_identity_arguments is 8.4+: {sql}"
            );
            assert!(!lower.contains("pg_get_function_arguments"), "pg_get_function_arguments is 9.0+: {sql}");
            // The information_schema private helpers (and the doubled-prefix
            // bug that produced the cross-database-reference error):
            assert!(!lower.contains("_pg_truetypid"), "information_schema helpers are private: {sql}");
            assert!(!lower.contains("_pg_truetypmod"), "information_schema helpers are private: {sql}");
            assert!(!lower.contains("_pg_char_max_length"), "information_schema helpers are private: {sql}");
            // Sequence helpers that do not exist as catalog functions:
            assert!(!lower.contains("seq_increment"), "no such catalog function: {sql}");
            assert!(!lower.contains("pg_sequence"), "pg_sequence is 10+: {sql}");
            // Pretty-printing overloads are 8.2+; only 1-argument forms are used.
            assert!(!lower.contains("pg_get_viewdef(c.oid, true)"), "pretty overload is 8.2+: {sql}");
            assert!(!lower.contains("pg_get_indexdef(ix.indexrelid, 0"), "3-arg overload is 8.2+: {sql}");
        }
    }
}
