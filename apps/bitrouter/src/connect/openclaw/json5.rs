//! Small span-preserving JSON5 reader: edits never reserialize unrelated settings.
use std::ops::Range;

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value};

pub(super) struct Node {
    pub span: Range<usize>,
    pub value: Value,
    pub members: Vec<Member>,
}

pub(super) struct Member {
    pub key: String,
    pub start: usize,
    pub node: Node,
    pub comma: Option<usize>,
}

impl Node {
    pub fn member(&self, key: &str) -> Option<&Member> {
        self.members.iter().find(|member| member.key == key)
    }
}

struct Reader<'a> {
    text: &'a str,
    cursor: usize,
}

impl Reader<'_> {
    fn space(&mut self) -> Result<()> {
        let bytes = self.text.as_bytes();
        loop {
            while let Some(character) = self.text[self.cursor..].chars().next() {
                if !character.is_whitespace() {
                    break;
                }
                self.cursor += character.len_utf8();
            }
            if bytes.get(self.cursor..self.cursor + 2) == Some(b"//") {
                while bytes.get(self.cursor).is_some_and(|byte| *byte != b'\n') {
                    self.cursor += 1;
                }
            } else if bytes.get(self.cursor..self.cursor + 2) == Some(b"/*") {
                self.cursor += 2;
                while bytes.get(self.cursor..self.cursor + 2) != Some(b"*/") {
                    ensure!(self.cursor < bytes.len(), "unterminated JSON5 comment");
                    self.cursor += 1;
                }
                self.cursor += 2;
            } else {
                return Ok(());
            }
        }
    }

    fn byte(&self) -> Option<u8> {
        self.text.as_bytes().get(self.cursor).copied()
    }

    fn string(&mut self) -> Result<String> {
        let start = self.cursor;
        let quote = self.byte().context("missing JSON5 string")?;
        ensure!(matches!(quote, b'"' | b'\''), "expected JSON5 string");
        self.cursor += 1;
        loop {
            match self.byte() {
                Some(byte) if byte == quote => {
                    self.cursor += 1;
                    return json5::from_str(&self.text[start..self.cursor])
                        .context("invalid JSON5 string");
                }
                Some(b'\\') => {
                    self.cursor += 2;
                    ensure!(self.cursor <= self.text.len(), "invalid JSON5 escape");
                }
                Some(_) => self.cursor += 1,
                None => bail!("unterminated JSON5 string"),
            }
        }
    }

    fn node(&mut self, depth: usize) -> Result<Node> {
        ensure!(depth < 128, "JSON5 configuration is too deeply nested");
        self.space()?;
        let start = self.cursor;
        let mut members = Vec::new();
        let value = match self.byte() {
            Some(b'{') => {
                self.cursor += 1;
                let mut object = Map::new();
                self.space()?;
                while self.byte() != Some(b'}') {
                    let member_start = self.cursor;
                    let key = if matches!(self.byte(), Some(b'"' | b'\'')) {
                        self.string()?
                    } else {
                        let start = self.cursor;
                        while self.byte().is_some_and(|byte| {
                            !byte.is_ascii_whitespace() && !b":/".contains(&byte)
                        }) {
                            self.cursor += 1;
                        }
                        let key: Value = json5::from_str(&format!(
                            "{{{}:null}}",
                            &self.text[start..self.cursor]
                        ))?;
                        key.as_object()
                            .and_then(|object| object.keys().next())
                            .context("invalid JSON5 identifier")?
                            .to_owned()
                    };
                    ensure!(!object.contains_key(&key), "duplicate JSON5 object key");
                    self.space()?;
                    ensure!(self.byte() == Some(b':'), "expected JSON5 colon");
                    self.cursor += 1;
                    let node = self.node(depth + 1)?;
                    object.insert(key.clone(), node.value.clone());
                    self.space()?;
                    let comma = if self.byte() == Some(b',') {
                        let comma = self.cursor;
                        self.cursor += 1;
                        self.space()?;
                        Some(comma)
                    } else {
                        ensure!(self.byte() == Some(b'}'), "expected JSON5 object separator");
                        None
                    };
                    members.push(Member {
                        key,
                        start: member_start,
                        node,
                        comma,
                    });
                }
                self.cursor += 1;
                Value::Object(object)
            }
            Some(b'[') => {
                self.cursor += 1;
                let mut array = Vec::new();
                self.space()?;
                while self.byte() != Some(b']') {
                    array.push(self.node(depth + 1)?.value);
                    self.space()?;
                    if self.byte() == Some(b',') {
                        self.cursor += 1;
                        self.space()?;
                    } else {
                        ensure!(self.byte() == Some(b']'), "expected JSON5 array separator");
                    }
                }
                self.cursor += 1;
                Value::Array(array)
            }
            Some(b'"' | b'\'') => Value::String(self.string()?),
            Some(_) => {
                while self
                    .byte()
                    .is_some_and(|byte| !byte.is_ascii_whitespace() && !b",]}:/".contains(&byte))
                {
                    self.cursor += 1;
                }
                ensure!(self.cursor > start, "invalid JSON5 value");
                json5::from_str(&self.text[start..self.cursor]).context("invalid JSON5 value")?
            }
            None => bail!("missing JSON5 value"),
        };
        Ok(Node {
            span: start..self.cursor,
            value,
            members,
        })
    }
}

pub(super) fn parse(text: &str) -> Result<Node> {
    let mut reader = Reader {
        text,
        cursor: if text.starts_with('\u{feff}') { 3 } else { 0 },
    };
    let node = reader.node(0)?;
    reader.space()?;
    ensure!(
        reader.cursor == text.len(),
        "unexpected trailing JSON5 content"
    );
    ensure!(
        node.value.is_object(),
        "expected a JSON5 configuration object"
    );
    Ok(node)
}

pub(super) fn insert(text: &str, object: &Node, key: &str, value: &Value) -> Result<String> {
    ensure!(object.value.is_object(), "expected a JSON5 object");
    ensure!(object.member(key).is_none(), "JSON5 key already exists");
    let mut output = text.to_owned();
    let entry = format!(
        "\n  {}: {}\n",
        serde_json::to_string(key)?,
        serde_json::to_string_pretty(value)?
    );
    output.insert_str(object.span.end - 1, &entry);
    if let Some(last) = object.members.last()
        && last.comma.is_none()
    {
        output.insert(last.node.span.end, ',');
    }
    Ok(output)
}

pub(super) fn remove(text: &str, object: &Node, key: &str) -> Result<String> {
    let index = object
        .members
        .iter()
        .position(|member| member.key == key)
        .context("missing JSON5 key")?;
    let member = &object.members[index];
    let mut output = text.to_owned();
    let end = member.comma.map_or(member.node.span.end, |comma| comma + 1);
    output.replace_range(member.start..end, "");
    if member.comma.is_none()
        && let Some(previous) = index
            .checked_sub(1)
            .and_then(|index| object.members.get(index))
        && let Some(comma) = previous.comma
    {
        output.remove(comma);
    }
    Ok(output)
}
