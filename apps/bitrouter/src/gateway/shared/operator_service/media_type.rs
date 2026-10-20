// Validate a complete textual Content-Type rather than discarding parameters.
// This boundary accepts ASCII headers, consistent with HeaderValue::to_str.
// Parameters and quoted strings follow:
// https://www.rfc-editor.org/rfc/rfc9110.html#section-5.6.6
// https://www.rfc-editor.org/rfc/rfc9110.html#section-5.6.4
pub(super) fn matches(value: &str, expected: &str) -> bool {
    if !value.is_ascii() {
        return false;
    }
    let (essence, mut parameters) = value
        .find(';')
        .map_or((value, ""), |index| value.split_at(index));
    if !essence
        .trim_matches([' ', '\t'])
        .eq_ignore_ascii_case(expected)
    {
        return false;
    }
    loop {
        parameters = parameters.trim_start_matches([' ', '\t']);
        if parameters.is_empty() {
            return true;
        }
        let Some(rest) = parameters.strip_prefix(';') else {
            return false;
        };
        parameters = rest.trim_start_matches([' ', '\t']);
        // Empty parameters are permitted, including trailing semicolons.
        if parameters.is_empty() || parameters.starts_with(';') {
            continue;
        }
        if !token(&mut parameters) {
            return false;
        }
        let Some(rest) = parameters.strip_prefix('=') else {
            return false;
        };
        parameters = rest;
        if parameters.starts_with('"') {
            if !quoted(&mut parameters) {
                return false;
            }
        } else if !token(&mut parameters) {
            return false;
        }
    }
}

fn token(input: &mut &str) -> bool {
    let length = input
        .bytes()
        .take_while(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(byte))
        .count();
    *input = &input[length..];
    length > 0
}

fn quoted(input: &mut &str) -> bool {
    let mut bytes = input.bytes().enumerate().skip(1);
    while let Some((index, byte)) = bytes.next() {
        match byte {
            b'"' => {
                *input = &input[index + 1..];
                return true;
            }
            b'\\' => {
                if !bytes
                    .next()
                    .is_some_and(|(_, byte)| matches!(byte, b'\t' | b' '..=b'~'))
                {
                    return false;
                }
            }
            b'\t' | b' '..=b'~' => {}
            _ => return false,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_textual_media_type_grammar() {
        for value in [
            "application/json",
            " Application/JSON\t",
            "application/json ;\tcharset=utf-8",
            "application/json;; ;",
            "application/json; ; charset=\"utf-8\" ;",
            "application/json; extension=!#$%&'*+-.^_`|~09AZaz",
            r#"application/json; note="a,b;c\"d\\e"; empty="""#,
            "application/json; note=\"a\tb\"",
        ] {
            assert!(matches(value, "application/json"), "{value:?}");
        }
        for value in [
            "",
            "application /json",
            "application/json, text/plain",
            "application/json; charset=utf-8, text/plain",
            "application/json; a=\"ok\", text/plain",
            "application/json; a=\"ok\"trailing",
            "application/json; a=\"unterminated",
            "application/json; a=\"escaped-end\\\"",
            "application/json; a=\"trailing\\",
            "application/json; a=",
            "application/json; =a",
            "application/json; a",
            "application/json; a =x",
            "application/json; a= x",
            "application/json; a=x b=y",
            "application/json; a=x/y",
            "application/json; a=\"x\n\"",
            "application/json; a=\"x\\\n\"",
            "application/json; a=\"雪\"",
        ] {
            assert!(!matches(value, "application/json"), "{value:?}");
        }
    }
}
