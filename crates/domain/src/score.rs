//! Bounded score newtypes.

use std::{fmt, ops::Deref};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Integer score constrained to the inclusive range `0..=100`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Score0To100(u8);

impl Score0To100 {
    /// Constructs a score if `value <= 100`.
    pub fn try_new(value: u8) -> Result<Self, ScoreOutOfRange> {
        Self::try_from(i64::from(value))
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

impl TryFrom<i64> for Score0To100 {
    type Error = ScoreOutOfRange;

    /// Validates an integer without truncation, preserving rejected values.
    fn try_from(value: i64) -> Result<Self, Self::Error> {
        if (0..=100).contains(&value) {
            // The range check guarantees this value fits in u8.
            Ok(Self(value as u8))
        } else {
            Err(ScoreOutOfRange { value })
        }
    }
}

/// Error returned when constructing `Score0To100` from an out-of-range value.
///
/// `value` preserves every rejected integer, including negatives and values
/// wider than `u8`, so callers retain the original value in diagnostics.
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
    fn try_from_accepts_in_range() {
        for raw in [0_i64, 1, 50, 99, 100] {
            assert_eq!(
                Score0To100::try_from(raw).map(Score0To100::get),
                Ok(raw as u8),
                "raw = {raw}"
            );
        }
    }

    #[test]
    fn try_from_rejects_negative_with_score_out_of_range() {
        for raw in [-1_i64, -100, i64::MIN] {
            let err = Score0To100::try_from(raw).expect_err("negative value must be rejected");
            assert_eq!(err.value, raw, "raw = {raw}");
        }
    }

    #[test]
    fn try_from_rejects_above_one_hundred_with_score_out_of_range() {
        for raw in [101_i64, 255, 256, i64::from(i32::MAX), i64::MAX] {
            let err = Score0To100::try_from(raw).expect_err("value above 100 must be rejected");
            assert_eq!(err.value, raw, "raw = {raw}");
        }
    }

    #[test]
    fn score_out_of_range_carries_original_value_from_try_new() {
        let err = Score0To100::try_new(200).expect_err("200 must be rejected");
        assert_eq!(err.value, 200_i64);
    }
}
