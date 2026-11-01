//! Allocation preflight before the existing canonical object decoders.
use super::*;
use serde::de::{DeserializeSeed, Error, MapAccess, SeqAccess, Visitor};
use std::fmt;

struct Nodes<'a> {
    count: &'a mut usize,
    depth: usize,
}
impl<'de> DeserializeSeed<'de> for Nodes<'_> {
    type Value = ();

    fn deserialize<D: serde::Deserializer<'de>>(self, decoder: D) -> Result<(), D::Error> {
        if self.depth > 64 {
            return Err(D::Error::custom("configuration JSON exceeds depth bound"));
        }
        *self.count = self
            .count
            .checked_add(1)
            .ok_or_else(|| D::Error::custom("JSON node overflow"))?;
        decoder.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for Nodes<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("bounded configuration JSON")
    }
    fn visit_bool<E: Error>(self, _: bool) -> Result<(), E> {
        Ok(())
    }
    fn visit_i64<E: Error>(self, _: i64) -> Result<(), E> {
        Ok(())
    }
    fn visit_u64<E: Error>(self, _: u64) -> Result<(), E> {
        Ok(())
    }
    fn visit_f64<E: Error>(self, _: f64) -> Result<(), E> {
        Ok(())
    }
    fn visit_str<E: Error>(self, _: &str) -> Result<(), E> {
        Ok(())
    }
    fn visit_unit<E: Error>(self) -> Result<(), E> {
        Ok(())
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<(), A::Error> {
        while sequence
            .next_element_seed(Nodes {
                count: self.count,
                depth: self.depth + 1,
            })?
            .is_some()
        {}
        Ok(())
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while map
            .next_key_seed(Nodes {
                count: self.count,
                depth: self.depth,
            })?
            .is_some()
        {
            map.next_value_seed(Nodes {
                count: self.count,
                depth: self.depth + 1,
            })?;
        }
        Ok(())
    }
}

fn required(row: &QueryResult, fields: &[&str]) -> Result<usize> {
    let encoded = usize::try_from(row.try_get::<i64>("", "directory_bytes")?)?;
    let mut nodes = 0_usize;
    for field in fields {
        let text: String = row.try_get("", field)?;
        let mut decoder = serde_json::Deserializer::from_str(&text);
        Nodes {
            count: &mut nodes,
            depth: 0,
        }
        .deserialize(&mut decoder)?;
        decoder.end()?;
    }
    // The unchanged canonical decoder can overlap the SQL row, decoded strings,
    // serialization buffers (including capacity slack), and reparsed JSON.
    // Six encoded copies plus 512 bytes per node conservatively cover those
    // buffers and container/tree growth. String-heavy encrypted envelopes use
    // the full-object allowance; dense JSON pays for its actual node structure.
    encoded
        .checked_mul(6)
        .and_then(|bytes| {
            nodes
                .checked_mul(512)
                .and_then(|tree| bytes.checked_add(tree))
        })
        .ok_or_else(|| CapacityExceeded.into())
}

pub(super) fn resource(row: &QueryResult) -> Result<usize> {
    required(row, &["payload_json", "dependency_json"])
}

pub(super) fn configuration(row: &QueryResult) -> Result<usize> {
    required(
        row,
        &["document_json", "compiled_json", "dependencies_json"],
    )
}

pub(in crate::tenancy::store::startup) fn operation(row: &QueryResult) -> Result<usize> {
    required(row, &["read_context_json", "result_json", "response_json"])
}
