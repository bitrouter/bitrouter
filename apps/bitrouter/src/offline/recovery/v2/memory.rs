//! Count JSON allocation shape before invoking the original owned decoders.
use anyhow::{Context, Result, ensure};
use serde::de::{DeserializeSeed, Error, MapAccess, SeqAccess, Visitor};

struct Nodes<'a> {
    count: &'a mut usize,
    depth: usize,
}
impl<'de> DeserializeSeed<'de> for Nodes<'_> {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        decoder: D,
    ) -> std::result::Result<(), D::Error> {
        if self.depth > 64 {
            return Err(D::Error::custom("offline JSON depth"));
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
    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("bounded JSON")
    }
    fn visit_bool<E: Error>(self, _: bool) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_i64<E: Error>(self, _: i64) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_u64<E: Error>(self, _: u64) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_f64<E: Error>(self, _: f64) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_str<E: Error>(self, _: &str) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_unit<E: Error>(self) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> std::result::Result<(), A::Error> {
        while sequence
            .next_element_seed(Nodes {
                count: self.count,
                depth: self.depth + 1,
            })?
            .is_some()
        {}
        Ok(())
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<(), A::Error> {
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

pub(crate) fn json_required(bytes: &[u8]) -> Result<usize> {
    let mut nodes = 0;
    let mut decoder = serde_json::Deserializer::from_slice(bytes);
    Nodes {
        count: &mut nodes,
        depth: 0,
    }
    .deserialize(&mut decoder)?;
    decoder.end()?;
    bytes
        .len()
        .checked_mul(6)
        .and_then(|size| {
            nodes
                .checked_mul(512)
                .and_then(|tree| size.checked_add(tree))
        })
        .context("JSON working size overflow")
}

/// Only fixed, source-owned column/key names reach this helper. The length pass
/// transfers scalars; the second pass transfers strings, never parsed Values.
pub(crate) async fn preflight(
    db: &impl sea_orm::ConnectionTrait,
    table: &str,
    keys: &[(&str, &str)],
    fields: &[&str],
    bytes: usize,
) -> Result<()> {
    use sea_orm::{Statement, Value};
    let predicate = keys
        .iter()
        .map(|(key, _)| format!("{key}=?"))
        .collect::<Vec<_>>()
        .join(" AND ");
    let values: Vec<Value> = keys.iter().map(|(_, value)| (*value).into()).collect();
    let lengths = fields
        .iter()
        .map(|name| format!("length(CAST({name} AS BLOB))"))
        .collect::<Vec<_>>()
        .join("+");
    let row = db
        .query_one(Statement::from_sql_and_values(
            db.get_database_backend(),
            format!("SELECT {lengths} AS bytes FROM {table} WHERE {predicate} LIMIT 1"),
            values.clone(),
        ))
        .await?
        .context("offline object missing")?;
    let encoded = usize::try_from(row.try_get::<i64>("", "bytes")?)?;
    let initial = encoded
        .checked_mul(6)
        .and_then(|n| n.checked_add(8192))
        .context("object size overflow")?;
    ensure!(initial <= bytes, super::streams::WorkingCapacity);
    let row = db
        .query_one(Statement::from_sql_and_values(
            db.get_database_backend(),
            format!(
                "SELECT {} FROM {table} WHERE {predicate} LIMIT 1",
                fields.join(",")
            ),
            values,
        ))
        .await?
        .context("offline object disappeared")?;
    let mut required = 8192usize;
    for name in fields {
        let text: String = row.try_get("", name)?;
        required = required
            .checked_add(json_required(text.as_bytes())?)
            .context("object working size overflow")?;
    }
    ensure!(required <= bytes, super::streams::WorkingCapacity);
    Ok(())
}
