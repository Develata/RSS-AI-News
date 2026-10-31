//! Normalised results and the shared response checks (ADR 0010 §2, §4).
//!
//! Provider modules decode the wire format into these types; nothing raw
//! crosses this boundary. Every answer has passed range, distribution and
//! consistency checks, so a value here is a real judgement, never a default.

use std::{collections::HashSet, fmt, marker::PhantomData};

use serde::{
    Deserialize, Deserializer,
    de::{MapAccess, Visitor},
};

use crate::provider::Provider;

#[derive(Debug, Clone, PartialEq)]
pub struct DecisionResponse {
    pub execution: Execution,
    /// In question order.
    pub answers: Vec<QuestionAnswer>,
    pub usage: Usage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Execution {
    pub provider: Provider,
    pub requested_model: String,
    pub reported_model: String,
    /// Laya's `routing.model`: the checkpoint name (not a weights revision).
    /// `None` only for Jev, which has no routing.
    pub routing_model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub state: StateTruncation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateTruncation {
    /// Jev rejects over-long input (HTTP 400) instead of truncating.
    NotApplicable,
    Laya {
        truncated: bool,
        state_tokens_dropped: u64,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct QuestionAnswer {
    pub id: String,
    pub answer: DecisionAnswer,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DecisionAnswer {
    Noul {
        p_yes: f64,
        top_probability: Option<f64>,
    },
    Choice {
        label: String,
        /// In option order.
        probabilities: Vec<f64>,
        spread_confidence: SpreadConfidence,
        top_probability: Option<f64>,
    },
    Score {
        expected: f64,
        /// Level 0 first.
        probabilities: Vec<f64>,
        spread_confidence: SpreadConfidence,
        top_probability: Option<f64>,
    },
}

/// The provider's own concentration statistic. The kinds use different
/// formulas, so a threshold fitted on one never applies to another.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpreadConfidence {
    pub kind: SpreadKind,
    pub value: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpreadKind {
    /// `(n·p_max − 1)/(n − 1)`.
    JevChoice,
    /// `max(0, 1 − Σpᵢ·|i − m| / D_n)`.
    JevScore,
    /// `1 − H(p)/ln n`.
    LayaEntropy,
}

// ── Wire format shared by both providers; presence rules live per provider ──

#[derive(Debug, Deserialize)]
pub(crate) struct WireResponse {
    pub model: String,
    pub answers: UniqueMap<WireAnswer>,
    pub usage: WireUsage,
    #[serde(default)]
    pub routing: Option<WireRouting>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WireUsage {
    pub input_tokens: u64,
    #[serde(default)]
    pub truncated: Option<bool>,
    #[serde(default)]
    pub state_tokens_dropped: Option<u64>,
    /// Laya: questions whose options no longer have distinct token spans.
    #[serde(default)]
    pub options: Option<UniqueMap<WireCollapse>>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WireCollapse {
    pub total: u64,
    pub distinct: u64,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WireRouting {
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub(crate) enum WireAnswer {
    Noul {
        noul: f64,
        #[serde(default)]
        confidence: Option<f64>,
        #[serde(default)]
        answer_confidence: Option<f64>,
    },
    Choice {
        choice: String,
        probabilities: UniqueMap<f64>,
        #[serde(default)]
        confidence: Option<f64>,
        #[serde(default)]
        answer_confidence: Option<f64>,
    },
    Score {
        score: f64,
        probabilities: UniqueMap<f64>,
        #[serde(default)]
        confidence: Option<f64>,
        #[serde(default)]
        answer_confidence: Option<f64>,
    },
}

/// A JSON object that rejects duplicate keys instead of keeping the last one,
/// in document order.
#[derive(Debug)]
pub(crate) struct UniqueMap<V>(pub Vec<(String, V)>);

impl<'de, V: Deserialize<'de>> Deserialize<'de> for UniqueMap<V> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct UniqueVisitor<V>(PhantomData<V>);

        impl<'de, V: Deserialize<'de>> Visitor<'de> for UniqueVisitor<V> {
            type Value = UniqueMap<V>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON object without duplicate keys")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut seen = HashSet::new();
                let mut entries = Vec::new();
                while let Some((key, value)) = map.next_entry::<String, V>()? {
                    if !seen.insert(key.clone()) {
                        return Err(serde::de::Error::custom("duplicate key"));
                    }
                    entries.push((key, value));
                }
                Ok(UniqueMap(entries))
            }
        }

        deserializer.deserialize_map(UniqueVisitor(PhantomData))
    }
}

impl<V> UniqueMap<V> {
    pub(crate) fn take(&mut self, key: &str) -> Option<V> {
        let index = self.0.iter().position(|(candidate, _)| candidate == key)?;
        Some(self.0.swap_remove(index).1)
    }
}

// ── Shared checks (ADR 0010 §4) ──

pub(crate) type Check<T> = Result<T, String>;

pub(crate) fn unit(value: f64, what: &str) -> Check<f64> {
    if value.is_finite() && (0.0..=1.0).contains(&value) {
        Ok(value)
    } else {
        Err(format!("{what} must be a finite number in [0, 1]"))
    }
}

pub(crate) fn required(value: Option<f64>, what: &str) -> Check<f64> {
    unit(value.ok_or_else(|| format!("{what} is missing"))?, what)
}

/// Distribution keyed exactly by `keys`, each value in [0, 1], summing to 1
/// within `n·eps + 1e-6` (`eps` = half a unit in the provider's last digit).
pub(crate) fn distribution(
    mut probabilities: UniqueMap<f64>,
    keys: &[String],
    eps: f64,
) -> Check<Vec<f64>> {
    if probabilities.0.len() != keys.len() {
        return Err("probability keys do not match the question".to_string());
    }
    let mut ordered = Vec::with_capacity(keys.len());
    for key in keys {
        let value = probabilities
            .take(key)
            .ok_or_else(|| "probability keys do not match the question".to_string())?;
        ordered.push(unit(value, "probability")?);
    }
    let sum: f64 = ordered.iter().sum();
    if (sum - 1.0).abs() > keys.len() as f64 * eps + 1e-6 {
        return Err("probabilities do not sum to 1".to_string());
    }
    Ok(ordered)
}

/// Rounding never reorders values, so the true argmax stays a maximum of the
/// rounded distribution whatever the provider's precision.
pub(crate) fn is_argmax(probabilities: &[f64], index: usize) -> bool {
    probabilities
        .iter()
        .all(|other| probabilities[index] >= *other)
}

/// `expected` within [0, n−1] and within `eps·(n(n−1)/2 + 1) + 1e-6` of
/// `Σ i·pᵢ` (worst-case per-level rounding plus the score's own rounding).
pub(crate) fn expectation(expected: f64, probabilities: &[f64], eps: f64) -> Check<f64> {
    let top = (probabilities.len() - 1) as f64;
    if !expected.is_finite() || !(0.0..=top).contains(&expected) {
        return Err("score must be a finite number in [0, n-1]".to_string());
    }
    let weighted: f64 = probabilities
        .iter()
        .enumerate()
        .map(|(level, probability)| level as f64 * probability)
        .sum();
    let n = probabilities.len() as f64;
    if (expected - weighted).abs() > eps * (n * (n - 1.0) / 2.0 + 1.0) + 1e-6 {
        return Err("score does not match its level probabilities".to_string());
    }
    Ok(expected)
}

pub(crate) fn max_of(probabilities: &[f64]) -> f64 {
    probabilities.iter().copied().fold(0.0, f64::max)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(entries: &[(&str, f64)]) -> UniqueMap<f64> {
        UniqueMap(
            entries
                .iter()
                .map(|(key, value)| ((*key).to_string(), *value))
                .collect(),
        )
    }

    fn keys(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    #[test]
    fn unique_map_rejects_duplicate_keys() {
        let ok: UniqueMap<f64> = serde_json::from_str(r#"{"a":0.5,"b":0.5}"#).unwrap();
        assert_eq!(ok.0.len(), 2);
        let dup = serde_json::from_str::<UniqueMap<f64>>(r#"{"a":0.5,"a":0.5}"#);
        assert!(dup.unwrap_err().to_string().contains("duplicate key"));
    }

    #[test]
    fn unique_map_rejects_duplicates_inside_tagged_answers() {
        // Internally tagged enums buffer content first; duplicates must survive.
        let raw = r#"{"type":"choice","choice":"a","probabilities":{"a":1,"a":0}}"#;
        assert!(serde_json::from_str::<WireAnswer>(raw).is_err());
    }

    #[test]
    fn distribution_checks_keys_range_and_sum() {
        let names = keys(&["a", "b"]);
        assert_eq!(
            distribution(map(&[("b", 0.25), ("a", 0.75)]), &names, 5e-5).unwrap(),
            [0.75, 0.25]
        );
        assert!(distribution(map(&[("a", 1.0)]), &names, 5e-5).is_err());
        assert!(distribution(map(&[("a", 0.9), ("c", 0.1)]), &names, 5e-5).is_err());
        assert!(distribution(map(&[("a", 1.2), ("b", -0.2)]), &names, 5e-5).is_err());
        assert!(distribution(map(&[("a", 0.6), ("b", 0.6)]), &names, 5e-5).is_err());
    }

    #[test]
    fn sum_tolerance_covers_four_digit_rounding_of_twenty_options() {
        // 19 values of 0.0499949 round to 0.05; the remainder 0.0500969 to 0.0501.
        let names: Vec<String> = (0..20).map(|index| format!("o{index}")).collect();
        let mut entries: Vec<(String, f64)> =
            names.iter().map(|name| (name.clone(), 0.05)).collect();
        entries[19].1 = 0.0501;
        assert!(distribution(UniqueMap(entries), &names, 5e-5).is_ok());
    }

    #[test]
    fn argmax_accepts_ties_and_rejects_a_dominated_label() {
        assert!(is_argmax(&[0.5, 0.5], 1));
        assert!(!is_argmax(&[0.0, 1.0], 0));
    }

    #[test]
    fn expectation_rejects_inconsistent_or_out_of_range_scores() {
        assert_eq!(expectation(1.43, &[0.0, 0.57, 0.43], 5e-3).unwrap(), 1.43);
        // All mass on the top level reported as the lowest score.
        assert!(expectation(0.0, &[0.0, 0.0, 1.0], 5e-5).is_err());
        assert!(expectation(2.1, &[0.0, 0.0, 1.0], 5e-3).is_err());
        assert!(expectation(f64::NAN, &[0.5, 0.5], 5e-3).is_err());
    }
}
