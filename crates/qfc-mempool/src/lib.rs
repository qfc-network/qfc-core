//! QFC Transaction Pool (Mempool)
//!
//! Manages pending transactions waiting to be included in blocks.

mod error;
mod pool;

pub use error::*;
pub use pool::*;

// Implement NonceLookup for StateDB
impl NonceLookup for qfc_state::StateDB {
    fn get_nonce(&self, address: &qfc_types::Address) -> std::result::Result<u64, String> {
        self.get_nonce(address).map_err(|e| e.to_string())
    }
}
