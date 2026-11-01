//! A required nullable field is distinct from an optional serde field.

use schemars::{JsonSchema, Schema, SchemaGenerator};
use std::{borrow::Cow, marker::PhantomData};

pub(super) struct RequiredNullable<T>(PhantomData<T>);

impl<T: JsonSchema> JsonSchema for RequiredNullable<T> {
    fn schema_name() -> Cow<'static, str> {
        format!("RequiredNullable_{}", T::schema_name()).into()
    }
    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        <Option<T>>::json_schema(generator)
    }
}
