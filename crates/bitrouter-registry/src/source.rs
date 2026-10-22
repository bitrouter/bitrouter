//! JSON-compatible YAML with exact numeric lexemes and bounded alias expansion.

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::Arc;

use granit_parser::{Event, Parser, ScalarStyle, ScanError, Span};
use serde_json::Value;

use crate::document::{MAX_DOCUMENT_BYTES, MAX_DOCUMENT_DEPTH, MAX_DOCUMENT_NODES, RegistryError};

struct Node {
    value: NodeValue,
    height: usize,
}
enum NodeValue {
    Scalar(Value),
    Array(Vec<Arc<Node>>),
    Object(BTreeMap<String, Arc<Node>>),
}

#[derive(Default)]
struct Builder {
    anchors: BTreeMap<usize, Arc<Node>>,
    nodes: usize,
}

type ParsedEvent<'a> = Result<(Event<'a>, Span), ScanError>;

fn next_event<'a>(
    events: &mut impl Iterator<Item = ParsedEvent<'a>>,
) -> Result<Event<'a>, RegistryError> {
    loop {
        match events.next() {
            Some(Ok((Event::Comment(..), _))) => continue,
            Some(Ok((event, _))) => return Ok(event),
            _ => return Err(invalid("Invalid or truncated YAML stream")),
        }
    }
}

impl Builder {
    fn node<'a>(
        &mut self,
        event: Event<'a>,
        events: &mut impl Iterator<Item = ParsedEvent<'a>>,
        depth: usize,
    ) -> Result<Arc<Node>, RegistryError> {
        if depth > MAX_DOCUMENT_DEPTH || self.nodes >= MAX_DOCUMENT_NODES {
            return Err(invalid("Source nesting or node budget exceeded"));
        }
        self.nodes += 1;
        let (node, anchor) = match event {
            Event::Alias(id) => {
                return self
                    .anchors
                    .get(&id)
                    .filter(|node| node.height + depth <= MAX_DOCUMENT_DEPTH)
                    .cloned()
                    .ok_or_else(|| invalid("Unknown, forward, or cyclic YAML alias"));
            }
            Event::Scalar(text, style, anchor, tag) => {
                if tag.is_some() {
                    return Err(invalid("Explicit YAML tags are not supported"));
                }
                let value = if style == ScalarStyle::Plain {
                    scalar(&text)?
                } else {
                    Value::String(text.into_owned())
                };
                (NodeValue::Scalar(value), anchor)
            }
            Event::SequenceStart(_, anchor, tag) => {
                if tag.is_some() {
                    return Err(invalid("Explicit YAML tags are not supported"));
                }
                let mut values = Vec::new();
                loop {
                    let event = next_event(events)?;
                    if event == Event::SequenceEnd {
                        break;
                    }
                    values.push(self.node(event, events, depth + 1)?);
                }
                (NodeValue::Array(values), anchor)
            }
            Event::MappingStart(_, anchor, tag) => {
                if tag.is_some() {
                    return Err(invalid("Explicit YAML tags are not supported"));
                }
                let mut fields = BTreeMap::new();
                loop {
                    let event = next_event(events)?;
                    if event == Event::MappingEnd {
                        break;
                    }
                    let key = self.node(event, events, depth + 1)?;
                    let NodeValue::Scalar(Value::String(key)) = &key.value else {
                        return Err(invalid("YAML mapping keys must be strings"));
                    };
                    if fields.contains_key(key) {
                        return Err(invalid("Duplicate YAML mapping key"));
                    }
                    let event = next_event(events)?;
                    let value = self.node(event, events, depth + 1)?;
                    fields.insert(key.clone(), value);
                }
                (NodeValue::Object(fields), anchor)
            }
            _ => return Err(invalid("Unexpected YAML event")),
        };
        let height = match &node {
            NodeValue::Scalar(_) => 0,
            NodeValue::Array(v) => v.iter().map(|n| n.height + 1).max().unwrap_or(0),
            NodeValue::Object(v) => v.values().map(|n| n.height + 1).max().unwrap_or(0),
        };
        if height + depth > MAX_DOCUMENT_DEPTH {
            return Err(invalid("Expanded alias height exceeds depth budget"));
        }
        let node = Arc::new(Node {
            value: node,
            height,
        });
        if anchor != 0 {
            self.anchors.insert(anchor, Arc::clone(&node));
        }
        Ok(node)
    }
}

fn scalar(text: &str) -> Result<Value, RegistryError> {
    match text {
        "" | "null" => Ok(Value::Null),
        "true" => Ok(Value::Bool(true)),
        "false" => Ok(Value::Bool(false)),
        _ => {
            if matches!(
                text.to_ascii_lowercase().as_str(),
                ".nan" | ".inf" | "+.inf" | "-.inf"
            ) {
                return Err(invalid("Non-finite YAML numbers are not supported"));
            }
            // Parse the original decimal lexeme directly, never through f64.
            Ok(text
                .parse::<serde_json::Number>()
                .map_or_else(|_| Value::String(text.into()), Value::Number))
        }
    }
}

struct Budget {
    bytes: usize,
    nodes: usize,
}

impl Write for Budget {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_sub(bytes.len())
            .ok_or_else(|| std::io::Error::other("expanded source byte budget"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn expand(node: &Node, budget: &mut Budget, depth: usize) -> Result<Value, RegistryError> {
    if depth > MAX_DOCUMENT_DEPTH || budget.nodes == 0 {
        return Err(invalid("Expanded alias nesting or node budget exceeded"));
    }
    budget.nodes -= 1;
    match &node.value {
        NodeValue::Scalar(value) => {
            serde_json::to_writer(&mut *budget, value)
                .map_err(|_| invalid("Expanded source byte budget exceeded"))?;
            Ok(value.clone())
        }
        NodeValue::Array(values) => {
            budget
                .write_all(b"[]")
                .map_err(|_| invalid("Expanded source byte budget exceeded"))?;
            let mut output = Vec::new();
            for (i, node) in values.iter().enumerate() {
                if i > 0 {
                    budget
                        .write_all(b",")
                        .map_err(|_| invalid("Expanded source byte budget exceeded"))?;
                }
                output.push(expand(node, budget, depth + 1)?);
            }
            Ok(Value::Array(output))
        }
        NodeValue::Object(fields) => {
            budget
                .write_all(b"{}")
                .map_err(|_| invalid("Expanded source byte budget exceeded"))?;
            let mut output = serde_json::Map::new();
            for (i, (key, node)) in fields.iter().enumerate() {
                if i > 0 {
                    budget
                        .write_all(b",")
                        .map_err(|_| invalid("Expanded source byte budget exceeded"))?;
                }
                serde_json::to_writer(&mut *budget, key)
                    .map_err(|_| invalid("Expanded source byte budget exceeded"))?;
                budget
                    .write_all(b":")
                    .map_err(|_| invalid("Expanded source byte budget exceeded"))?;
                output.insert(key.clone(), expand(node, budget, depth + 1)?);
            }
            Ok(Value::Object(output))
        }
    }
}

/// Parse one JSON-compatible YAML document, preserving unknown values exactly.
/// JSON scalar resolution is used; quoted values and unrecognized plain scalars
/// remain strings. Aliases share immutable nodes until bounded expansion.
///
/// # Errors
/// Rejects duplicate/non-string keys, tags, non-finite numbers, cycles, multiple
/// documents and source/expanded resource excess without executing includes.
pub fn parse(input: &str) -> Result<Value, RegistryError> {
    if input.len() > MAX_DOCUMENT_BYTES {
        return Err(invalid("Source byte budget exceeded"));
    }
    // Use the same low-level parser as serde-saphyr, retaining numeric lexemes.
    // https://docs.rs/granit-parser/0.0.7/granit_parser/struct.Parser.html
    let mut events = Parser::new_from_str(input);
    if next_event(&mut events)? != Event::StreamStart {
        return Err(invalid("Missing YAML stream start"));
    }
    if !matches!(next_event(&mut events)?, Event::DocumentStart(..)) {
        return Err(invalid("Missing YAML document"));
    }
    let first = next_event(&mut events)?;
    let node = Builder::default().node(first, &mut events, 0)?;
    if next_event(&mut events)? != Event::DocumentEnd
        || next_event(&mut events)? != Event::StreamEnd
    {
        return Err(invalid("Expected exactly one YAML document"));
    }
    let value = expand(
        &node,
        &mut Budget {
            bytes: MAX_DOCUMENT_BYTES,
            nodes: MAX_DOCUMENT_NODES,
        },
        0,
    )?;
    crate::document::validate_resources(&value)?;
    Ok(value)
}

/// Write exact JSON, which is valid source YAML, without serializer-specific
/// numeric structs or lossy floating-point conversions.
///
/// # Errors
/// Returns a normal error if rendering would exceed the source byte budget.
pub fn render(value: &Value) -> Result<String, RegistryError> {
    crate::document::validate_resources(value)?;
    struct Output(Vec<u8>);
    impl Write for Output {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.0.len().saturating_add(bytes.len()) > MAX_DOCUMENT_BYTES {
                return Err(std::io::Error::other("source byte budget"));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut output = Output(Vec::new());
    serde_json::to_writer_pretty(&mut output, value)
        .map_err(|_| invalid("Could not render bounded JSON source"))?;
    output
        .write_all(b"\n")
        .map_err(|_| invalid("Source byte budget exceeded"))?;
    String::from_utf8(output.0).map_err(|_| invalid("Rendered source is not UTF-8"))
}

fn invalid(message: &str) -> RegistryError {
    RegistryError::invalid("", message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_alias_values_numeric_lexemes_and_private_json_keys() -> Result<(), RegistryError> {
        let value = parse(
            "price: &price 0.100000000000000000000000001\nalias: *price\nfuture: {'$serde_json::private::Number': '123', sibling: true}\nname: 2026-10-23\n",
        )?;
        assert_eq!(value["price"].to_string(), "0.100000000000000000000000001");
        assert_eq!(value["price"], value["alias"]);
        assert!(value["future"].is_object());
        assert_eq!(parse(&render(&value)?)?, value);
        Ok(())
    }

    #[test]
    fn rejects_duplicate_keys_custom_tags_cycles_and_multiple_documents() {
        for source in [
            "a: 1\na: 2\n",
            "a: !!str text\n",
            "a: &a [*a]\n",
            "---\na: 1\n---\nb: 2\n",
            "a: .nan\n",
            "1: value\n",
        ] {
            assert!(parse(source).is_err(), "{source}");
        }
    }
}
