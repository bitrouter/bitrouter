//! Finite source-expression profile checked before integrity/VACUUM. Ordinary
//! BitRouter CHECKs are comparisons/membership, never arbitrary SQL programs.
use anyhow::{Result, bail, ensure};
use std::ops::Range;

#[derive(Debug, PartialEq, Eq)]
enum Token {
    Word(String),
    Name(String),
    Text(String),
    Number(String),
    Symbol(u8),
}

fn tokens(sql: &str, mut spans: Option<&mut Vec<Range<usize>>>) -> Result<Vec<Token>> {
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        ensure!(out.len() < 16_384, "schema token limit exceeded");
        let start = at;
        let byte = bytes[at];
        at += 1;
        if byte.is_ascii_whitespace() {
            continue;
        }
        if byte == b'-' && bytes.get(at) == Some(&b'-') {
            while at < bytes.len() && bytes[at] != b'\n' {
                at += 1;
            }
            continue;
        }
        if byte == b'/' && bytes.get(at) == Some(&b'*') {
            at += 1;
            while at + 1 < bytes.len() && &bytes[at..at + 2] != b"*/" {
                at += 1;
            }
            ensure!(at + 1 < bytes.len(), "unterminated schema comment");
            at += 2;
            continue;
        }
        if matches!(byte, b'\'' | b'"' | b'`' | b'[') {
            let end = if byte == b'[' { b']' } else { byte };
            let mut value = Vec::new();
            loop {
                ensure!(at < bytes.len(), "unterminated schema quote");
                let current = bytes[at];
                at += 1;
                if current == end {
                    if byte != b'[' && bytes.get(at) == Some(&end) {
                        value.push(end);
                        at += 1;
                    } else {
                        break;
                    }
                } else {
                    value.push(current);
                }
            }
            let value = String::from_utf8(value)?;
            out.push(if byte == b'\'' {
                Token::Text(value)
            } else {
                Token::Name(value)
            });
        } else if byte.is_ascii_alphabetic() || byte == b'_' {
            while at < bytes.len() && (bytes[at].is_ascii_alphanumeric() || bytes[at] == b'_') {
                at += 1;
            }
            out.push(Token::Word(sql[start..at].to_ascii_lowercase()));
        } else if byte.is_ascii_digit() {
            while at < bytes.len() && (bytes[at].is_ascii_digit() || bytes[at] == b'.') {
                at += 1;
            }
            out.push(Token::Number(sql[start..at].into()));
        } else {
            ensure!(byte.is_ascii(), "quote non-ASCII schema identifiers");
            out.push(Token::Symbol(byte));
        }
        if let Some(spans) = spans.as_mut() {
            spans.push(start..at);
        }
    }
    Ok(out)
}

fn word(token: Option<&Token>, value: &str) -> bool {
    matches!(token, Some(Token::Word(word)) if word == value)
}
fn name(token: Option<&Token>) -> bool {
    matches!(token, Some(Token::Word(_) | Token::Name(_)))
}

/// Accept only a column comparison, BETWEEN range or IN membership with literals.
/// This includes continuation constraints and the indexed obligation layout.
fn check(tokens: &[Token], at: &mut usize) -> Result<()> {
    ensure!(
        tokens.get(*at) == Some(&Token::Symbol(b'(')),
        "unsupported CHECK"
    );
    *at += 1;
    ensure!(name(tokens.get(*at)), "unsupported CHECK operand");
    *at += 1;
    if word(tokens.get(*at), "in") {
        *at += 1;
        ensure!(
            tokens.get(*at) == Some(&Token::Symbol(b'(')),
            "unsupported IN"
        );
        *at += 1;
        let mut literals = 0;
        loop {
            literal(tokens, at)?;
            literals += 1;
            ensure!(literals <= 32, "CHECK membership limit");
            if tokens.get(*at) != Some(&Token::Symbol(b',')) {
                break;
            }
            *at += 1;
        }
        ensure!(
            tokens.get(*at) == Some(&Token::Symbol(b')')),
            "unsupported IN expression"
        );
        *at += 1;
    } else if word(tokens.get(*at), "between") {
        *at += 1;
        literal(tokens, at)?;
        ensure!(word(tokens.get(*at), "and"), "unsupported BETWEEN range");
        *at += 1;
        literal(tokens, at)?;
    } else {
        match tokens.get(*at..*at + 2) {
            Some(
                [Token::Symbol(b'<'), Token::Symbol(b'>')]
                | [Token::Symbol(b'!'), Token::Symbol(b'=')],
            ) => *at += 2,
            _ if tokens.get(*at) == Some(&Token::Symbol(b'=')) => *at += 1,
            _ => bail!("unsupported CHECK operator"),
        }
        literal(tokens, at)?;
    }
    ensure!(
        tokens.get(*at) == Some(&Token::Symbol(b')')),
        "unsupported CHECK expression"
    );
    *at += 1;
    Ok(())
}

fn literal(tokens: &[Token], at: &mut usize) -> Result<()> {
    if matches!(tokens.get(*at), Some(Token::Symbol(b'+' | b'-'))) {
        *at += 1;
        ensure!(
            matches!(tokens.get(*at), Some(Token::Number(_))),
            "unsupported signed literal"
        );
    }
    match tokens.get(*at) {
        Some(Token::Text(value) | Token::Number(value)) if value.len() <= 1024 => {}
        Some(Token::Word(value)) if matches!(value.as_str(), "null" | "true" | "false") => {}
        _ => bail!("unsupported schema literal"),
    }
    *at += 1;
    Ok(())
}

pub(super) fn validate(kind: &[u8], sql: Option<&[u8]>) -> Result<()> {
    ensure!(
        kind == b"table" || kind == b"index",
        "views/triggers outside offline profile"
    );
    let Some(sql) = sql else {
        ensure!(kind == b"index", "table definition missing");
        return Ok(());
    };
    validate_tokens(kind, &tokens(std::str::from_utf8(sql)?, None)?)
}

fn validate_tokens(kind: &[u8], tokens: &[Token]) -> Result<()> {
    let mut at = 0;
    while at < tokens.len() {
        if word(tokens.get(at), "check") {
            ensure!(kind == b"table", "unsupported index constraint");
            at += 1;
            check(tokens, &mut at)?;
        } else if word(tokens.get(at), "default") {
            at += 1;
            literal(tokens, &mut at)?;
            ensure!(
                !matches!(
                    tokens.get(at),
                    Some(Token::Symbol(b'(' | b'|' | b'+' | b'-' | b'*' | b'/'))
                ),
                "unsupported DEFAULT expression"
            );
        } else if word(tokens.get(at), "collate") {
            at += 1;
            let value = match tokens.get(at) {
                Some(Token::Name(v) | Token::Word(v)) => v.to_ascii_lowercase(),
                _ => bail!("unsupported collation"),
            };
            ensure!(
                matches!(value.as_str(), "binary" | "nocase" | "rtrim"),
                "unsupported collation"
            );
            at += 1;
        } else {
            ensure!(
                !["virtual", "generated", "select", "where"]
                    .iter()
                    .any(|value| word(tokens.get(at), value)),
                "unsupported executable schema"
            );
            at += 1;
        }
    }
    Ok(())
}

/// Original, fully qualified CHECK spans; SQLite retains name and literal semantics.
pub(super) struct CheckCursor<'a> {
    sql: &'a str,
    tokens: Vec<Token>,
    spans: Vec<Range<usize>>,
    at: usize,
}
impl<'a> CheckCursor<'a> {
    pub(super) fn new(sql: &'a str) -> Result<Self> {
        let mut spans = Vec::new();
        let tokens = tokens(sql, Some(&mut spans))?;
        validate_tokens(b"table", &tokens)?;
        Ok(Self {
            sql,
            tokens,
            spans,
            at: 0,
        })
    }

    pub(super) fn next_predicate(&mut self) -> Result<Option<&'a str>> {
        while self.at < self.tokens.len() {
            let is_check = word(self.tokens.get(self.at), "check");
            self.at += 1;
            if is_check {
                let start = self.at;
                check(&self.tokens, &mut self.at)?;
                return Ok(Some(
                    &self.sql[self.spans[start].start..self.spans[self.at - 1].end],
                ));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_spans_preserve_original_sql_and_qualify_the_whole_definition() -> Result<()> {
        let predicate = "(\"not a column\" /* CHECK(abs(9)) */ = 'not a column')";
        let sql = format!(
            "CREATE TABLE t(value NUMERIC DEFAULT 'check', CHECK {predicate}, CHECK(value IN(TRUE, FALSE, NULL, +01, -2.0)))"
        );
        let mut checks = CheckCursor::new(&sql)?;
        ensure!(checks.next_predicate()? == Some(predicate));
        ensure!(checks.next_predicate()? == Some("(value IN(TRUE, FALSE, NULL, +01, -2.0))"));
        ensure!(checks.next_predicate()?.is_none());
        // Even an earlier harmless CHECK must not escape an unqualified schema.
        ensure!(CheckCursor::new("CREATE TABLE t(v CHECK(v=1), CHECK(v=abs(1)))").is_err());
        Ok(())
    }

    #[test]
    fn obligation_ranges_allow_literals_but_never_executable_bounds() -> Result<()> {
        for sql in [
            "CREATE TABLE t (source_mask INTEGER CHECK (source_mask BETWEEN 1 AND 7))",
            "CREATE TABLE t (due_nanos INTEGER CHECK (due_nanos BETWEEN 0 AND 999999999))",
        ] {
            validate(b"table", Some(sql.as_bytes()))?;
        }
        for sql in [
            "CREATE TABLE t (n INTEGER CHECK (n BETWEEN 0 AND abs(7)))",
            "CREATE TABLE t (n INTEGER CHECK (n BETWEEN 0 AND 7 OR n=9))",
            "CREATE TABLE t (n INTEGER CHECK (n BETWEEN 0 AND (SELECT 7)))",
            "CREATE TABLE t (n INTEGER CHECK (n BETWEEN 0 AND 7+1))",
        ] {
            ensure!(validate(b"table", Some(sql.as_bytes())).is_err());
        }
        Ok(())
    }
}
