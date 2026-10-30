//! Finite source-expression profile checked before integrity/VACUUM. Ordinary
//! BitRouter CHECKs are comparisons/membership, never arbitrary SQL programs.
use anyhow::{Result, bail, ensure};

#[derive(Debug, PartialEq, Eq)]
enum Token {
    Word(String),
    Name(String),
    Text(String),
    Number(String),
    Symbol(u8),
}

fn tokens(sql: &str) -> Result<Vec<Token>> {
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
    }
    Ok(out)
}

fn word(token: Option<&Token>, value: &str) -> bool {
    matches!(token, Some(Token::Word(word)) if word == value)
}
fn name(token: Option<&Token>) -> bool {
    matches!(token, Some(Token::Word(_) | Token::Name(_)))
}

/// Accept only a column comparison with bounded literals or IN membership.
/// This includes all four historical provider_continuations constraints.
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
    let tokens = tokens(std::str::from_utf8(sql)?)?;
    let mut at = 0;
    while at < tokens.len() {
        if word(tokens.get(at), "check") {
            ensure!(kind == b"table", "unsupported index constraint");
            at += 1;
            check(&tokens, &mut at)?;
        } else if word(tokens.get(at), "default") {
            at += 1;
            literal(&tokens, &mut at)?;
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
