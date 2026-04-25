//! Domain layer: core types, enums, DTOs, and traits.
//!
//! This crate has zero I/O dependencies. It defines the ontology objects,
//! state enums, DTOs for inter-crate communication, and shared traits.

pub mod dto;
pub mod error;
pub mod model;
pub mod score;
pub mod state;

pub use score::{Score0To100, ScoreOutOfRange};
