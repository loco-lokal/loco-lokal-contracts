//! `serialize :column, coder: JSON`: JSON stored in a `text` column.

use serde_json::Value;
use sqlx::{
    Decode, Encode, Postgres, Type,
    encode::IsNull,
    error::BoxDynError,
    postgres::{PgArgumentBuffer, PgTypeInfo, PgValueRef},
};

/// A JSON value read from or written to a `text` column, like Rails' JSON coder. A `NULL`
/// column is an `Option<JsonText>`; an empty string reads as `null`, as `JSON.load` does.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct JsonText(pub Value);

impl JsonText {
    #[must_use]
    pub fn into_inner(self) -> Value {
        self.0
    }
}

impl From<Value> for JsonText {
    fn from(value: Value) -> Self {
        Self(value)
    }
}

impl std::ops::Deref for JsonText {
    type Target = Value;

    fn deref(&self) -> &Value {
        &self.0
    }
}

impl std::ops::DerefMut for JsonText {
    fn deref_mut(&mut self) -> &mut Value {
        &mut self.0
    }
}

impl Type<Postgres> for JsonText {
    fn type_info() -> PgTypeInfo {
        <String as Type<Postgres>>::type_info()
    }

    fn compatible(ty: &PgTypeInfo) -> bool {
        <String as Type<Postgres>>::compatible(ty)
    }
}

impl<'r> Decode<'r, Postgres> for JsonText {
    fn decode(value: PgValueRef<'r>) -> Result<Self, BoxDynError> {
        let text = <&str as Decode<Postgres>>::decode(value)?;
        if text.trim().is_empty() {
            return Ok(Self(Value::Null));
        }
        Ok(Self(serde_json::from_str(text)?))
    }
}

impl Encode<'_, Postgres> for JsonText {
    fn encode_by_ref(&self, buf: &mut PgArgumentBuffer) -> Result<IsNull, BoxDynError> {
        <String as Encode<Postgres>>::encode(self.0.to_string(), buf)
    }
}
