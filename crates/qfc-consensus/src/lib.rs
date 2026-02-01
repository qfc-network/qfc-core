//! QFC Proof of Contribution Consensus
//!
//! Implements the PoC consensus mechanism for block production and finality.

mod engine;
mod scoring;
mod error;

pub use engine::*;
pub use scoring::*;
pub use error::*;
