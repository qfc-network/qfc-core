//! Network configuration

use libp2p::Multiaddr;
use std::time::Duration;

/// Network configuration
#[derive(Clone, Debug)]
pub struct NetworkConfig {
    /// Listen addresses
    pub listen_addresses: Vec<Multiaddr>,

    /// Bootstrap nodes
    pub bootnodes: Vec<Multiaddr>,

    /// Maximum inbound peers
    pub max_inbound_peers: u32,

    /// Maximum outbound peers
    pub max_outbound_peers: u32,

    /// Connection idle timeout
    pub idle_timeout: Duration,

    /// Enable mDNS discovery
    pub enable_mdns: bool,

    /// Node secret key (None for random)
    pub secret_key: Option<[u8; 32]>,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            listen_addresses: vec![
                "/ip4/0.0.0.0/tcp/30303".parse().unwrap(),
            ],
            bootnodes: Vec::new(),
            max_inbound_peers: 50,
            max_outbound_peers: 25,
            idle_timeout: Duration::from_secs(60),
            enable_mdns: true,
            secret_key: None,
        }
    }
}

impl NetworkConfig {
    /// Create config for development
    pub fn dev() -> Self {
        Self {
            listen_addresses: vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()],
            enable_mdns: true,
            ..Default::default()
        }
    }
}
