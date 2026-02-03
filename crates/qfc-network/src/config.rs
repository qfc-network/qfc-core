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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = NetworkConfig::default();

        assert_eq!(config.max_inbound_peers, 50);
        assert_eq!(config.max_outbound_peers, 25);
        assert_eq!(config.idle_timeout, Duration::from_secs(60));
        assert!(config.enable_mdns);
        assert!(config.secret_key.is_none());
        assert!(config.bootnodes.is_empty());
        assert!(!config.listen_addresses.is_empty());
    }

    #[test]
    fn test_default_listen_address() {
        let config = NetworkConfig::default();

        let addr = &config.listen_addresses[0];
        let addr_str = addr.to_string();

        assert!(addr_str.contains("0.0.0.0"));
        assert!(addr_str.contains("30303"));
    }

    #[test]
    fn test_dev_config() {
        let config = NetworkConfig::dev();

        assert!(config.enable_mdns);

        let addr = &config.listen_addresses[0];
        let addr_str = addr.to_string();

        // Dev config uses localhost
        assert!(addr_str.contains("127.0.0.1"));
        // Port 0 means random port
        assert!(addr_str.contains("/tcp/0"));
    }

    #[test]
    fn test_config_clone() {
        let config = NetworkConfig {
            max_inbound_peers: 100,
            max_outbound_peers: 50,
            secret_key: Some([1u8; 32]),
            ..Default::default()
        };

        let cloned = config.clone();

        assert_eq!(cloned.max_inbound_peers, 100);
        assert_eq!(cloned.max_outbound_peers, 50);
        assert_eq!(cloned.secret_key, Some([1u8; 32]));
    }

    #[test]
    fn test_config_with_bootnodes() {
        let bootnode: Multiaddr = "/ip4/1.2.3.4/tcp/30303".parse().unwrap();
        let config = NetworkConfig {
            bootnodes: vec![bootnode.clone()],
            ..Default::default()
        };

        assert_eq!(config.bootnodes.len(), 1);
        assert_eq!(config.bootnodes[0].to_string(), "/ip4/1.2.3.4/tcp/30303");
    }
}
