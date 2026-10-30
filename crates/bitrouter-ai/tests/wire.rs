use bitrouter_ai::wire::{self, Frame};

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[test]
fn json_bound_includes_escape_expansion() -> TestResult {
    let value = "\n";
    assert_eq!(wire::json(&value, 4)?, b"\"\\n\"");
    assert!(matches!(wire::json(&value, 3), Err(wire::Error::Limit)));
    Ok(())
}

#[test]
fn sse_multiline_and_field_boundaries() -> TestResult {
    let frame = Frame::Event {
        event: Some("delta"),
        id: Some(""),
        data: "a\r\nb\rc\n",
    };
    let expected = "event: delta\nid: \ndata: a\ndata: b\ndata: c\ndata: \n\n";
    assert_eq!(frame.encode(expected.len())?, expected);
    assert!(matches!(
        frame.encode(expected.len() - 1),
        Err(wire::Error::Limit)
    ));
    assert_eq!(Frame::Comment("a\nb").encode(100)?, ":a\n:b\n\n");
    for invalid in ["a\nb", "a\rb", "a\0b"] {
        assert!(matches!(
            Frame::Event {
                event: Some(invalid),
                id: None,
                data: "{}"
            }
            .encode(100),
            Err(wire::Error::InvalidField)
        ));
        assert!(matches!(
            Frame::Event {
                event: None,
                id: Some(invalid),
                data: "{}"
            }
            .encode(100),
            Err(wire::Error::InvalidField)
        ));
    }
    Ok(())
}
