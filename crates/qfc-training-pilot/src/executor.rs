//! The real [`TrainingExecutor`] for the pilot (ADR-0010).
//!
//! `replay_step` re-runs an honest worker's training step EXACTLY as the
//! worker did: it resolves `ctx.data_shard_indices` to a concatenated
//! [`Dataset`] from the local [`DataStore`], pulls the parameters the worker
//! trained against (the epoch-start version, keyed by `ctx.params_version_hash`
//! in a version map the harness populates), and calls the SAME deterministic
//! [`compute_delta`] the worker called. Because the math is bit-identical,
//! ADR-0005 exact-match holds: an honest update's `update_hash` equals the
//! verifier's recomputed hash, and any deviation (a cheater) is caught.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use qfc_ai_coordinator::{ReplayContext, TrainingExecutor, TrainingVerificationError};
use qfc_types::Hash;

use crate::data::DataStore;
use crate::model::compute_delta;

/// A version map `params_version_hash → params` the verifier reads to fetch
/// the exact bytes a worker started from (ADR-0007: PS snapshot retention).
/// Shared with the harness, which populates it with the epoch-start params
/// BEFORE that epoch's steps run.
pub type VersionMap = Arc<Mutex<HashMap<Hash, Vec<f32>>>>;

/// The pilot's deterministic CPU re-execution backend.
#[derive(Clone)]
pub struct PilotExecutor {
    data: Arc<DataStore>,
    versions: VersionMap,
    lr: f64,
}

impl PilotExecutor {
    pub fn new(data: Arc<DataStore>, versions: VersionMap, lr: f64) -> Self {
        Self { data, versions, lr }
    }

    /// Register the parameter vector for a version hash (the harness calls this
    /// at each epoch start, mirroring PS snapshot retention).
    pub fn record_version(&self, version: Hash, params: Vec<f32>) {
        self.versions
            .lock()
            .expect("version map poisoned")
            .insert(version, params);
    }
}

#[async_trait]
impl TrainingExecutor for PilotExecutor {
    async fn replay_step(
        &self,
        ctx: &ReplayContext,
    ) -> Result<Vec<f32>, TrainingVerificationError> {
        // Fetch the epoch-start params the worker pulled. Missing version = a
        // verifier-side infrastructure failure (not a worker mismatch), so it
        // is an Err, not a false-negative verdict.
        let params = {
            let map = self.versions.lock().expect("version map poisoned");
            map.get(&ctx.params_version_hash).cloned().ok_or_else(|| {
                TrainingVerificationError::ReplayFailed(format!(
                    "no params recorded for version {} (snapshot retention miss)",
                    ctx.params_version_hash
                ))
            })?
        };

        // Resolve assigned shards into the worker's local training set, in
        // index order — must match how the honest worker assembled its data.
        let data = self.data.assemble(&ctx.data_shard_indices);

        // IDENTICAL math to the honest worker's push: same params, same seed,
        // same clock, same range, same lr → bit-identical delta (ADR-0005
        // exact-match).
        let delta = compute_delta(&params, &data, ctx.seed, ctx.clock, ctx.range, self.lr);
        Ok(delta)
    }
}
