//! In-process end-to-end decentralized-training pilot (ROADMAP-AI-V3
//! milestone A6; design in `docs/adr/0010-training-pilot.md`).
//!
//! This crate is the first time Feature A executes end to end: N simulated
//! miners run in ONE process through the REAL A1–A5 APIs — a real
//! `MinerRegistry`/`TrainingPool` assignment, a real `ShardService` doing
//! trimmed-mean aggregation, the real `TrainingVerifier`, and the real
//! `settle_epoch`. The only thing simulated is the network/process boundary
//! between miners (ADR-0010 Decision 1).
//!
//! What it proves (the integration-test gates, ADR-0010 D1):
//! 1. With ≥3 honest miners the model's loss decreases and converges.
//! 2. A planted cheater's update fails sampled re-execution (exact-match,
//!    ADR-0005 stage 1) → a `TrainingPenalty`.
//! 3. `settle_epoch` slashes the cheater, voids its rewards, pays honest
//!    miners.
//! 4. Two operators fed the same accepted-update set in different order
//!    produce a byte-identical `commit_hash` (cross-operator determinism).
//! 5. The commitment persists and reloads via `TrainingChainStore`.

pub mod data;
pub mod executor;
pub mod harness;
pub mod model;

pub use data::{build_manifest, synth_datastore, true_weights, DataStore};
pub use executor::{PilotExecutor, VersionMap};
pub use harness::{apply_settlement, run_pilot, PilotConfig, PilotReport};
pub use model::{compute_delta, mse_loss, Dataset, PilotModel};
