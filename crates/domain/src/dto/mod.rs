//! Data Transfer Objects for inter-crate communication.
//!
//! DTOs are short-lived, immutable, owned structs passed between layers.
//! See docs/design/internal-dto-contracts.md for the full contract.

pub mod ai;
pub mod extract;
pub mod feed;
pub mod publish;
pub mod replay;
