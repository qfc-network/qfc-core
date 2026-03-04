//! QFC Proof of Contribution Consensus
//!
//! Implements the PoC consensus mechanism for block production and finality.

mod engine;
mod error;
mod scoring;

pub use engine::*;
pub use error::*;
pub use scoring::*;
