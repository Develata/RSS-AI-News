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
            Err(ScoreOutOfRange {
                value: i64::from(value),
            })
        }
    }

    /// Constructs a score from a database integer column. Accepts any value in
    /// `0..=100`; any other integer (negative or > 100) returns
    /// [`ScoreOutOfRange`] carrying the original DB value, regardless of
    /// whether it fits in `u8`.
    fn try_from_db_int(raw: i64) -> Result<Self, ScoreOutOfRange> {
        if (0..=100).contains(&raw) {
            // SAFETY-equivalent: range check above guarantees the cast is in
            // u8 range.
            Ok(Self(raw as u8))
        } else {
            Err(ScoreOutOfRange { value: raw })
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
        Self::try_from_db_int(raw).map_err(Into::into)
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
        Self::try_from_db_int(i64::from(raw)).map_err(Into::into)
    }
}

impl<'q> Encode<'q, Postgres> for Score0To100 {
    fn encode_by_ref(&self, buf: &mut PgArgumentBuffer) -> Result<IsNull, BoxDynError> {
        <i32 as Encode<Postgres>>::encode(i32::from(self.0), buf)
    }
}

/// Error returned when constructing `Score0To100` from an out-of-range value.
///
/// `value` is `i64` so it can carry **any** rejected integer — both API-level
/// `try_new(u8)` calls (where the rejected value is always in `0..=255`) and
/// database `Decode` paths (where the underlying column may yield negative or
/// `> u8::MAX` values). This guarantees a single error surface for the
/// classification documented in `internal-dto-contracts.md` §6.4
/// (Score0To100): every out-of-range integer maps to `ScoreOutOfRange`,
/// never to `TryFromIntError` or other internal cast failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScoreOutOfRange {
    pub value: i64,
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

    #[test]
    fn try_from_db_int_accepts_in_range() {
        for raw in [0_i64, 1, 50, 99, 100] {
            assert_eq!(
                Score0To100::try_from_db_int(raw).map(Score0To100::get),
                Ok(raw as u8),
                "raw = {raw}"
            );
        }
    }

    #[test]
    fn try_from_db_int_rejects_negative_with_score_out_of_range() {
        for raw in [-1_i64, -100, i64::MIN] {
            let err =
                Score0To100::try_from_db_int(raw).expect_err("negative DB value must be rejected");
            assert_eq!(err.value, raw, "raw = {raw}");
        }
    }

    #[test]
    fn try_from_db_int_rejects_above_one_hundred_with_score_out_of_range() {
        for raw in [101_i64, 255, 256, i64::from(i32::MAX), i64::MAX] {
            let err =
                Score0To100::try_from_db_int(raw).expect_err("DB value above 100 must be rejected");
            assert_eq!(err.value, raw, "raw = {raw}");
        }
    }

    #[test]
    fn score_out_of_range_carries_original_value_from_try_new() {
        let err = Score0To100::try_new(200).expect_err("200 must be rejected");
        assert_eq!(err.value, 200_i64);
    }
}
