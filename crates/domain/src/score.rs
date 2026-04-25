//! Bounded score newtypes.

use std::{fmt, ops::Deref};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sqlx::{
    Type,
    database::Database,
    decode::Decode,
    encode::{Encode, IsNull},
    error::BoxDynError,
    postgres::{PgArgumentBuffer, PgTypeInfo, PgValueRef, Postgres},
    sqlite::{Sqlite, SqliteTypeInfo, SqliteValueRef},
};

/// Integer score constrained to the inclusive range `0..=100`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Score0To100(u8);

impl Score0To100 {
    /// Constructs a score if `value <= 100`.
    pub fn try_new(value: u8) -> Result<Self, ScoreOutOfRange> {
        if value <= 100 {
            Ok(Self(value))
        } else {
            Err(ScoreOutOfRange { value })
        }
    }

    /// Returns the inner integer.
    pub fn get(self) -> u8 {
        self.0
    }
}

impl Deref for Score0To100 {
    type Target = u8;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Serialize for Score0To100 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u8(self.0)
    }
}

impl<'de> Deserialize<'de> for Score0To100 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = u8::deserialize(deserializer)?;
        Self::try_new(value).map_err(serde::de::Error::custom)
    }
}

impl Type<Sqlite> for Score0To100 {
    fn type_info() -> SqliteTypeInfo {
        <i64 as Type<Sqlite>>::type_info()
    }

    fn compatible(ty: &SqliteTypeInfo) -> bool {
        <i64 as Type<Sqlite>>::compatible(ty)
    }
}

impl<'r> Decode<'r, Sqlite> for Score0To100 {
    fn decode(value: SqliteValueRef<'r>) -> Result<Self, BoxDynError> {
        let raw = <i64 as Decode<Sqlite>>::decode(value)?;
        let value = u8::try_from(raw)?;
        Self::try_new(value).map_err(Into::into)
    }
}

impl<'q> Encode<'q, Sqlite> for Score0To100 {
    fn encode_by_ref(
        &self,
        buf: &mut <Sqlite as Database>::ArgumentBuffer<'q>,
    ) -> Result<IsNull, BoxDynError> {
        <i64 as Encode<Sqlite>>::encode(i64::from(self.0), buf)
    }
}

impl Type<Postgres> for Score0To100 {
    fn type_info() -> PgTypeInfo {
        <i32 as Type<Postgres>>::type_info()
    }

    fn compatible(ty: &PgTypeInfo) -> bool {
        <i32 as Type<Postgres>>::compatible(ty)
    }
}

impl<'r> Decode<'r, Postgres> for Score0To100 {
    fn decode(value: PgValueRef<'r>) -> Result<Self, BoxDynError> {
        let raw = <i32 as Decode<Postgres>>::decode(value)?;
        let value = u8::try_from(raw)?;
        Self::try_new(value).map_err(Into::into)
    }
}

impl<'q> Encode<'q, Postgres> for Score0To100 {
    fn encode_by_ref(&self, buf: &mut PgArgumentBuffer) -> Result<IsNull, BoxDynError> {
        <i32 as Encode<Postgres>>::encode(i32::from(self.0), buf)
    }
}

/// Error returned when constructing `Score0To100` from an out-of-range value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScoreOutOfRange {
    pub value: u8,
}

impl fmt::Display for ScoreOutOfRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "score must be in 0..=100, got {}", self.value)
    }
}

impl std::error::Error for ScoreOutOfRange {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_inclusive_boundaries() {
        assert_eq!(Score0To100::try_new(0).map(Score0To100::get), Ok(0));
        assert_eq!(Score0To100::try_new(100).map(Score0To100::get), Ok(100));
    }

    #[test]
    fn rejects_values_above_one_hundred() {
        assert!(Score0To100::try_new(101).is_err());
        assert!(Score0To100::try_new(255).is_err());
    }

    #[test]
    fn serde_round_trip_is_transparent_integer() {
        let score = Score0To100::try_new(42).expect("valid score");
        let json = serde_json::to_string(&score).expect("score serialization should succeed");
        assert_eq!(json, "42");

        let decoded: Score0To100 =
            serde_json::from_str(&json).expect("score deserialization should succeed");
        assert_eq!(decoded, score);
    }

    #[test]
    fn serde_rejects_out_of_range_integer() {
        let result = serde_json::from_str::<Score0To100>("101");
        assert!(result.is_err());
    }
}
