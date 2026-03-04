//! Network behaviour definition

use crate::sync_protocol::SyncCodec;
use libp2p::{gossipsub, identify, kad, ping, request_response, swarm::NetworkBehaviour};

/// Type alias for the sync request-response behaviour
pub type SyncBehaviour = request_response::Behaviour<SyncCodec>;

/// QFC network behaviour
#[derive(NetworkBehaviour)]
pub struct QfcBehaviour {
    /// Gossipsub for pub/sub messaging
    pub gossipsub: gossipsub::Behaviour,

    /// Kademlia DHT for peer discovery
    pub kademlia: kad::Behaviour<kad::store::MemoryStore>,

    /// Identify protocol
    pub identify: identify::Behaviour,

    /// Ping for connection health
    pub ping: ping::Behaviour,

    /// Request-response for block sync
    pub sync: SyncBehaviour,
}

/// GossipSub topic names
pub mod topics {
    pub const NEW_BLOCKS: &str = "/qfc/blocks/1";
    pub const NEW_TRANSACTIONS: &str = "/qfc/txs/1";
    pub const CONSENSUS_VOTES: &str = "/qfc/votes/1";
    pub const VALIDATOR_MESSAGES: &str = "/qfc/validator/1";
}

/// Message types for the network
#[derive(Clone, Debug)]
pub enum NetworkMessage {
    /// New block announcement
    NewBlock(Vec<u8>),
    /// New transaction announcement
    NewTransaction(Vec<u8>),
    /// Consensus vote
    Vote(Vec<u8>),
    /// Validator message
    ValidatorMsg(Vec<u8>),
}
