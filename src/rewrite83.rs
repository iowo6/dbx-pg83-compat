//! SQL rewriter that makes PostgreSQL 9.1+ string functions executable on
//! PostgreSQL 8.3-9.0 servers.
//!
//! `left(text, n)` and `right(text, n)` were introduced in PostgreSQL 9.1.
//! DBX generates `left(col, N)` for large-value table previews on PostgreSQL
//! connections, and users write them in their own SQL. Both map losslessly
//! onto `substring(x from a for n)`, which exists since 7.x and behaves
//! identically for text and bytea:
//!
//! * `left(e, n)`  → `(CASE WHEN (n) < 1 THEN substring(e from 1 for 0) ELSE substring(e from 1 for (n)) END)`
//! * `right(e, n)` → `(CASE WHEN (n) < 1 THEN substring(e from 1 for 0)
//!                           WHEN length(e) <= (n) THEN substring(e from 1 for (n))
//!                           ELSE substring(e from length(e) - (n) + 1 for (n)) END)`
//!
//! Rewriting is purely lexical: string literals, quoted identifiers and
//! comments are skipped, and only whole-word `left`/`right` tokens directly
//! followed by `(` are treated as calls, so JOINs and column names named
//! `left` survive untouched.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Token<'a> {
    /// `'...'` string literal, `''` escapes included verbatim.
    StringLiteral(&'a str),
    /// `"..."` quoted identifier, `""` escapes included verbatim.
    QuotedIdent(&'a str),
    /// `-- line` or `/* block */` comment, verbatim.
    Comment(&'a str),
    /// Any other run of characters (words, operators, whitespace).
    Other(&'a str),
}

fn is_ident_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$' || byte >= 0x80
}

fn lex<'a>(sql: &'a str) -> Vec<Token<'a>> {
    let bytes = sql.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    let mut other_start = 0;

    fn push_other<'a>(tokens: &mut Vec<Token<'a>>, sql: &'a str, start: usize, end: usize) {
        if start < end {
            tokens.push(Token::Other(&sql[start..end]));
        }
    }

    while index < bytes.len() {
        match bytes[index] {
            b'\'' => {
                push_other(&mut tokens, sql, other_start, index);
                let start = index;
                index += 1;
                while index < bytes.len() {
                    if bytes[index] == b'\'' {
                        // '' is an escaped quote inside the literal.
                        if index + 1 < bytes.len() && bytes[index + 1] == b'\'' {
                            index += 2;
                        } else {
                            index += 1;
                            break;
                        }
                    } else {
                        index += 1;
                    }
                }
                tokens.push(Token::StringLiteral(&sql[start..index]));
                other_start = index;
            }
            b'"' => {
                push_other(&mut tokens, sql, other_start, index);
                let start = index;
                index += 1;
                while index < bytes.len() {
                    if bytes[index] == b'"' {
                        if index + 1 < bytes.len() && bytes[index + 1] == b'"' {
                            index += 2;
                        } else {
                            index += 1;
                            break;
                        }
                    } else {
                        index += 1;
                    }
                }
                tokens.push(Token::QuotedIdent(&sql[start..index]));
                other_start = index;
            }
            b'-' if index + 1 < bytes.len() && bytes[index + 1] == b'-' => {
                push_other(&mut tokens, sql, other_start, index);
                let start = index;
                index += 2;
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
                tokens.push(Token::Comment(&sql[start..index]));
                other_start = index;
            }
            b'/' if index + 1 < bytes.len() && bytes[index + 1] == b'*' => {
                push_other(&mut tokens, sql, other_start, index);
                let start = index;
                index += 2;
                // Unlike PostgreSQL itself, unterminated block comments just
                // consume the rest of the input; the server reports the error.
                while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/') {
                    index += 1;
                }
                index = (index + 2).min(bytes.len());
                tokens.push(Token::Comment(&sql[start..index]));
                other_start = index;
            }
            _ => {
                index += 1;
            }
        }
    }
    push_other(&mut tokens, sql, other_start, bytes.len());
    tokens
}

/// Character content of an `Other` word token (no whitespace).
fn word_at(bytes: &[u8], position: usize) -> Option<(usize, usize)> {
    let mut end = position;
    while end < bytes.len() && is_ident_char(bytes[end]) {
        end += 1;
    }
    (end > position).then_some((position, end))
}

struct CallSite {
    /// Byte position where the function name starts in the original SQL.
    name_start: usize,
    /// Byte range covering `(` ... matching `)` in the original SQL.
    open_paren: usize,
    close_paren: usize,
    /// Byte position of the first top-level comma inside the parentheses.
    top_comma: usize,
}

fn find_call_sites(sql: &str, function: &str) -> Vec<CallSite> {
    let bytes = sql.as_bytes();
    let mut sites = Vec::new();
    for token in lex(sql) {
        let Token::Other(chunk) = token else { continue };
        let offset = chunk.as_ptr() as usize - bytes.as_ptr() as usize;
        let mut index = 0;
        let chunk_bytes = chunk.as_bytes();
        while index < chunk_bytes.len() {
            if !is_ident_char(chunk_bytes[index]) {
                index += 1;
                continue;
            }
            let Some((start, end)) = word_at(chunk_bytes, index) else {
                index += 1;
                continue;
            };
            let word = &chunk[start..end];
            index = end;
            if !word.eq_ignore_ascii_case(function) {
                continue;
            }
            // Reject `x.left(` / `someleft(`: the name must stand alone.
            // A preceding `.` also disqualifies: `pg_catalog.left(x, 2)` names
            // a specific (possibly user-defined) function, not the 9.1 builtin.
            if start > 0 {
                let previous = chunk_bytes[start - 1];
                if is_ident_char(previous) || previous == b'.' {
                    continue;
                }
            }
            // Skip whitespace and comments between the name and `(`.
            let mut cursor = offset + end;
            let mut paren: Option<usize> = None;
            'find_paren: while cursor < bytes.len() {
                match bytes[cursor] {
                    byte if byte.is_ascii_whitespace() => cursor += 1,
                    b'(' => {
                        paren = Some(cursor);
                        break 'find_paren;
                    }
                    b'-' | b'/' => {
                        // Only a comment can legally appear here.
                        let rest = &sql[cursor..];
                        if rest.starts_with("--") {
                            match rest.find('\n') {
                                Some(newline) => cursor += newline + 1,
                                None => break 'find_paren,
                            }
                        } else if rest.starts_with("/*") {
                            match rest.find("*/") {
                                Some(close) => cursor += close + 2,
                                None => break 'find_paren,
                            }
                        } else {
                            break 'find_paren;
                        }
                    }
                    _ => break 'find_paren,
                }
            }
            let Some(open_paren) = paren else { continue };

            // Scan for the matching close paren, counting depth; string
            // literals, quoted identifiers and comments inside the arguments
            // are opaque. Also record the first depth-1 comma.
            let mut depth = 1usize;
            let mut position = open_paren + 1;
            let mut top_comma = None;
            let mut closed = false;
            while position < bytes.len() {
                match bytes[position] {
                    b'\'' => {
                        position += 1;
                        while position < bytes.len() {
                            if bytes[position] == b'\'' {
                                if position + 1 < bytes.len() && bytes[position + 1] == b'\'' {
                                    position += 2;
                                } else {
                                    position += 1;
                                    break;
                                }
                            } else {
                                position += 1;
                            }
                        }
                    }
                    b'"' => {
                        position += 1;
                        while position < bytes.len() {
                            if bytes[position] == b'"' {
                                if position + 1 < bytes.len() && bytes[position + 1] == b'"' {
                                    position += 2;
                                } else {
                                    position += 1;
                                    break;
                                }
                            } else {
                                position += 1;
                            }
                        }
                    }
                    b'-' if position + 1 < bytes.len() && bytes[position + 1] == b'-' => {
                        match sql[position..].find('\n') {
                            Some(newline) => position += newline + 1,
                            None => position = bytes.len(),
                        }
                    }
                    b'/' if position + 1 < bytes.len() && bytes[position + 1] == b'*' => {
                        match sql[position..].find("*/") {
                            Some(close) => position += close + 2,
                            None => position = bytes.len(),
                        }
                    }
                    b'(' => {
                        depth += 1;
                        position += 1;
                    }
                    b')' => {
                        depth -= 1;
                        if depth == 0 {
                            closed = true;
                            break;
                        }
                        position += 1;
                    }
                    b',' if depth == 1 && top_comma.is_none() => {
                        top_comma = Some(position);
                        position += 1;
                    }
                    _ => position += 1,
                }
            }
            if !closed {
                continue;
            }
            let close_paren = position;
            let top_comma = match top_comma {
                Some(comma) => comma,
                // `left(x)` without a count is not the 9.1 function; leave it.
                None => continue,
            };
            sites.push(CallSite {
                name_start: offset + start,
                open_paren,
                close_paren,
                top_comma,
            });
        }
    }
    sites.sort_by_key(|site| site.open_paren);
    sites
}

/// Rewrites `left`/`right` calls to their 8.3-compatible equivalents.
/// Returns the SQL unchanged (same allocation) when no call sites exist.
pub fn rewrite_for_pg83(sql: &str) -> String {
    let mut sites = find_call_sites(sql, "left");
    sites.extend(find_call_sites(sql, "right"));
    if sites.is_empty() {
        return sql.to_string();
    }
    sites.sort_by_key(|site| site.open_paren);

    // Nested calls (`left(left(x, 2), 1)`) are rewritten innermost-first by
    // processing sites in ascending order and rebuilding offsets afterwards.
    let mut current = sql.to_string();
    for _ in 0..sites.len() {
        // Re-scan after every replacement: byte offsets shift each time.
        let mut left_sites = find_call_sites(&current, "left");
        let mut right_sites = find_call_sites(&current, "right");
        if left_sites.is_empty() && right_sites.is_empty() {
            break;
        }
        // Rewrite the outermost-innermost pair first: pick the site whose
        // open paren is the largest (innermost) so outer sites re-scan later.
        left_sites.sort_by_key(|site| site.open_paren);
        right_sites.sort_by_key(|site| site.open_paren);
        let (site, is_right) = match (right_sites.last(), left_sites.last()) {
            (Some(right_site), Some(left_site)) => {
                if right_site.open_paren > left_site.open_paren {
                    (right_site, true)
                } else {
                    (left_site, false)
                }
            }
            (Some(right_site), None) => (right_site, true),
            (None, Some(left_site)) => (left_site, false),
            (None, None) => break,
        };
        let arguments = current[site.open_paren + 1..site.top_comma].trim();
        let count = current[site.top_comma + 1..site.close_paren].trim();
        let replacement = if is_right {
            format!(
                "(CASE WHEN ({count}) < 1 THEN substring({arguments} from 1 for 0) \
                 WHEN length({arguments}) <= ({count}) THEN substring({arguments} from 1 for ({count})) \
                 ELSE substring({arguments} from length({arguments}) - ({count}) + 1 for ({count})) END)"
            )
        } else {
            format!(
                "(CASE WHEN ({count}) < 1 THEN substring({arguments} from 1 for 0) \
                 ELSE substring({arguments} from 1 for ({count})) END)"
            )
        };
        current = format!(
            "{}{}{}",
            &current[..site.name_start],
            replacement,
            &current[site.close_paren + 1..]
        );
    }
    current
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_simple_left() {
        assert_eq!(
            rewrite_for_pg83(r#"SELECT left("payload", 8193) AS "payload" FROM t"#),
            r#"SELECT (CASE WHEN (8193) < 1 THEN substring("payload" from 1 for 0) ELSE substring("payload" from 1 for (8193)) END) AS "payload" FROM t"#
        );
    }

    #[test]
    fn rewrites_left_text_cast() {
        let rewritten = rewrite_for_pg83("SELECT left(metadata::text, 9) AS metadata FROM t");
        assert!(rewritten.contains("substring(metadata::text from 1 for (9))"), "{rewritten}");
        assert!(!rewritten.contains("left("), "{rewritten}");
    }

    #[test]
    fn rewrites_right() {
        let rewritten = rewrite_for_pg83("SELECT right(name, 3) FROM t");
        assert!(
            rewritten.contains("WHEN length(name) <= (3) THEN substring(name from 1 for (3))"),
            "{rewritten}"
        );
        assert!(
            rewritten.contains("ELSE substring(name from length(name) - (3) + 1 for (3)) END)"),
            "{rewritten}"
        );
    }

    #[test]
    fn nested_calls_rewrite_innermost_first() {
        // The outer rewrite references its argument twice (length check plus
        // the substring), so a nested inner CASE appears once per reference.
        // That doubles the substring count per nesting level but keeps the
        // semantics exact; DBX-generated SQL never nests left() calls.
        let rewritten = rewrite_for_pg83("SELECT left(left(name, 2), 1) FROM t");
        assert!(!rewritten.contains("left("), "{rewritten}");
        assert_eq!(rewritten.matches("substring(").count(), 6);
        // The outer count drives the outer CASE.
        assert!(rewritten.contains("(1) < 1 THEN substring((CASE WHEN (2)"), "{rewritten}");
    }

    #[test]
    fn literals_and_comments_are_untouched() {
        let sql = r#"SELECT 'left(x, 2)' AS a, -- left(y, 2)
        /* left(z, 3) */ "left" FROM t"#;
        let rewritten = rewrite_for_pg83(sql);
        assert_eq!(rewritten, sql);
    }

    #[test]
    fn join_keywords_and_columns_survive() {
        let sql = "SELECT t.left FROM t LEFT OUTER JOIN u ON true WHERE left IS NOT NULL";
        assert_eq!(rewrite_for_pg83(sql), sql);
        // A column named left used in a function-style position is still a call.
        let rewritten = rewrite_for_pg83("SELECT left(left, 2) FROM t");
        assert!(rewritten.contains("substring(left from 1 for (2))"), "{rewritten}");
    }

    #[test]
    fn schema_qualified_and_cased_calls() {
        // Schema-qualified calls are ambiguous (a user function), skip them.
        let sql = "SELECT pg_catalog.left(x, 2)";
        assert_eq!(rewrite_for_pg83(sql), sql);
        let rewritten = rewrite_for_pg83("SELECT LEFT ( x , 2 ) FROM t");
        assert!(rewritten.to_lowercase().contains("substring(x from 1 for (2))"), "{rewritten}");
    }

    #[test]
    fn argument_expressions_with_commas_and_strings() {
        let rewritten = rewrite_for_pg83("SELECT left(concat(a, ',', b), greatest(1, n)) FROM t");
        assert!(
            rewritten.contains("substring(concat(a, ',', b) from 1 for (greatest(1, n)))"),
            "{rewritten}"
        );
    }

    #[test]
    fn left_without_count_is_ignored() {
        let sql = "SELECT left(x) FROM t";
        assert_eq!(rewrite_for_pg83(sql), sql);
    }

    #[test]
    fn string_argument_with_escapes_and_commas() {
        let rewritten = rewrite_for_pg83("SELECT left('a''b, c', 4)");
        assert!(rewritten.contains("substring('a''b, c' from 1 for (4))"), "{rewritten}");
    }

    #[test]
    fn no_change_returns_same_string() {
        let sql = "SELECT 1";
        assert_eq!(rewrite_for_pg83(sql), "SELECT 1");
    }
}

