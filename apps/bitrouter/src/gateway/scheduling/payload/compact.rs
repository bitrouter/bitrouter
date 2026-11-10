//! Streaming heap bound for compact host DTOs, without allocating JSON copies.

use serde::ser::{self, Serialize};

type Result<T> = std::result::Result<T, serde_json::Error>;
struct Counter {
    bytes: u64,
    visits: u32,
    depth: u32,
}
impl Counter {
    fn add(&mut self, bytes: usize) -> Result<()> {
        self.visits += 1;
        self.bytes = self
            .bytes
            .checked_add(bytes as u64)
            .ok_or_else(|| <serde_json::Error as ser::Error>::custom("retained byte overflow"))?;
        if self.visits > 1_000_000 {
            return Err(<serde_json::Error as ser::Error>::custom(
                "retained work limit",
            ));
        }
        Ok(())
    }
    fn enter(&mut self) -> Result<()> {
        self.depth += 1;
        if self.depth > 64 {
            return Err(<serde_json::Error as ser::Error>::custom(
                "retained depth limit",
            ));
        }
        self.add(128)
    }
    fn leave(&mut self) -> Result<()> {
        self.depth -= 1;
        Ok(())
    }
}

pub(super) fn measure<T: Serialize>(value: &T) -> crate::gateway::error::Result<u64> {
    let mut counter = Counter {
        bytes: std::mem::size_of::<T>() as u64 + 128,
        visits: 0,
        depth: 0,
    };
    value
        .serialize(&mut counter)
        .map_err(|_| super::super::saturated())?;
    Ok(counter.bytes)
}

macro_rules! scalar {
    ($($name:ident($ty:ty)),* $(,)?) => { $(fn $name(self, _value: $ty) -> Result<()> { self.add(32) })* };
}
impl ser::Serializer for &mut Counter {
    type Ok = ();
    type Error = serde_json::Error;
    type SerializeSeq = Self;
    type SerializeTuple = Self;
    type SerializeTupleStruct = Self;
    type SerializeTupleVariant = Self;
    type SerializeMap = Self;
    type SerializeStruct = Self;
    type SerializeStructVariant = Self;
    scalar!(
        serialize_bool(bool),
        serialize_i8(i8),
        serialize_i16(i16),
        serialize_i32(i32),
        serialize_i64(i64),
        serialize_i128(i128),
        serialize_u8(u8),
        serialize_u16(u16),
        serialize_u32(u32),
        serialize_u64(u64),
        serialize_u128(u128),
        serialize_f32(f32),
        serialize_f64(f64),
        serialize_char(char)
    );
    fn serialize_str(self, value: &str) -> Result<()> {
        self.add(
            value
                .len()
                .checked_add(64)
                .ok_or_else(|| <serde_json::Error as ser::Error>::custom("retained overflow"))?,
        )
    }
    fn serialize_bytes(self, value: &[u8]) -> Result<()> {
        self.add(
            value
                .len()
                .checked_add(64)
                .ok_or_else(|| <serde_json::Error as ser::Error>::custom("retained overflow"))?,
        )
    }
    fn serialize_none(self) -> Result<()> {
        self.add(32)
    }
    fn serialize_unit(self) -> Result<()> {
        self.add(32)
    }
    fn serialize_unit_struct(self, _name: &'static str) -> Result<()> {
        self.add(64)
    }
    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
    ) -> Result<()> {
        self.add(64)
    }
    fn serialize_some<T: ?Sized + Serialize>(self, value: &T) -> Result<()> {
        self.enter()?;
        value.serialize(&mut *self)?;
        self.leave()
    }
    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<()> {
        self.enter()?;
        value.serialize(&mut *self)?;
        self.leave()
    }
    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        value: &T,
    ) -> Result<()> {
        self.enter()?;
        value.serialize(&mut *self)?;
        self.leave()
    }
    fn serialize_seq(self, _len: Option<usize>) -> Result<Self> {
        self.enter()?;
        Ok(self)
    }
    fn serialize_tuple(self, _len: usize) -> Result<Self> {
        self.enter()?;
        Ok(self)
    }
    fn serialize_tuple_struct(self, _name: &'static str, _len: usize) -> Result<Self> {
        self.enter()?;
        Ok(self)
    }
    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self> {
        self.enter()?;
        Ok(self)
    }
    fn serialize_map(self, _len: Option<usize>) -> Result<Self> {
        self.enter()?;
        Ok(self)
    }
    fn serialize_struct(self, _name: &'static str, _len: usize) -> Result<Self> {
        self.enter()?;
        Ok(self)
    }
    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self> {
        self.enter()?;
        Ok(self)
    }
}
macro_rules! sequence {
    ($trait:ident, $method:ident) => {
        impl ser::$trait for &mut Counter {
            type Ok = ();
            type Error = serde_json::Error;
            fn $method<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<()> {
                self.add(64)?;
                value.serialize(&mut **self)
            }
            fn end(self) -> Result<()> {
                self.leave()
            }
        }
    };
}
sequence!(SerializeSeq, serialize_element);
sequence!(SerializeTuple, serialize_element);
sequence!(SerializeTupleStruct, serialize_field);
sequence!(SerializeTupleVariant, serialize_field);
impl ser::SerializeMap for &mut Counter {
    type Ok = ();
    type Error = serde_json::Error;
    fn serialize_key<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<()> {
        self.add(256)?;
        value.serialize(&mut **self)
    }
    fn serialize_value<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<()> {
        value.serialize(&mut **self)
    }
    fn end(self) -> Result<()> {
        self.leave()
    }
}
macro_rules! structure {
    ($trait:ident) => {
        impl ser::$trait for &mut Counter {
            type Ok = ();
            type Error = serde_json::Error;
            fn serialize_field<T: ?Sized + Serialize>(
                &mut self,
                _key: &'static str,
                value: &T,
            ) -> Result<()> {
                self.add(64)?;
                value.serialize(&mut **self)
            }
            fn end(self) -> Result<()> {
                self.leave()
            }
        }
    };
}
structure!(SerializeStruct);
structure!(SerializeStructVariant);
