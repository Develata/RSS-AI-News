//! Offline revalidation (ADR 0011 §8): rerun every Phase A response check on a
//! stored 2xx body with the frozen execution parameters and policy snapshot,
//! without the network or the current configuration.

use crate::{
    DecisionError, DecisionResponse, Provider,
    policy::questions_from_canonical,
    provider::{self, WireParams},
    response::WireResponse,
};

pub fn revalidate(
    provider: Provider,
    requested_model: &str,
    laya_max_len: Option<u32>,
    canonical_policy_json: &str,
    raw_body: &[u8],
) -> Result<DecisionResponse, DecisionError> {
    let (questions, _max_state_chars) = questions_from_canonical(canonical_policy_json)
        .map_err(|reason| DecisionError::InvalidConfig(format!("policy snapshot: {reason}")))?;
    let params = WireParams {
        model: requested_model,
        laya_max_len: laya_max_len.unwrap_or(0),
    };
    let wire: WireResponse =
        serde_json::from_slice(raw_body).map_err(|error| DecisionError::InvalidResponse {
            reason: format!(
                "response does not match the decision schema ({:?})",
                error.classify()
            ),
        })?;
    provider::decode(provider, &params, &questions, wire)
        .map_err(|reason| DecisionError::InvalidResponse { reason })
}
