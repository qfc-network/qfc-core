//! Network service

use crate::behaviour::{topics, NetworkMessage, QfcBehaviour, QfcBehaviourEvent};
use crate::config::NetworkConfig;
use crate::error::{NetworkError, Result};
use futures::StreamExt;
use libp2p::{
    gossipsub::{self, IdentTopic, MessageAuthenticity},
    identify, kad,
    noise, ping,
    swarm::SwarmEvent,
    tcp, yamux, PeerId, Swarm, Transport,
};
use parking_lot::RwLock;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Network service handle
pub struct NetworkService {
    /// Local peer ID
    local_peer_id: PeerId,
    /// Connected peers
    peers: Arc<RwLock<HashSet<PeerId>>>,
    /// Message sender
    message_tx: mpsc::Sender<NetworkMessage>,
    /// Configuration
    config: NetworkConfig,
}

impl NetworkService {
    /// Create and start the network service
    pub async fn start(
        config: NetworkConfig,
    ) -> Result<(Self, mpsc::Receiver<NetworkMessage>)> {
        // Generate or load keypair
        let local_key = if let Some(secret) = &config.secret_key {
            let mut secret_bytes = secret.clone();
            libp2p::identity::Keypair::ed25519_from_bytes(&mut secret_bytes)
                .map_err(|e| NetworkError::Transport(e.to_string()))?
        } else {
            libp2p::identity::Keypair::generate_ed25519()
        };

        let local_peer_id = PeerId::from(local_key.public());
        info!("Local peer ID: {}", local_peer_id);

        // Create transport
        let transport = tcp::tokio::Transport::default()
            .upgrade(libp2p::core::upgrade::Version::V1Lazy)
            .authenticate(noise::Config::new(&local_key).unwrap())
            .multiplex(yamux::Config::default())
            .boxed();

        // Create gossipsub
        let gossipsub_config = gossipsub::ConfigBuilder::default()
            .heartbeat_interval(Duration::from_secs(1))
            .validation_mode(gossipsub::ValidationMode::Strict)
            .build()
            .map_err(|e| NetworkError::Protocol(e.to_string()))?;

        let gossipsub = gossipsub::Behaviour::new(
            MessageAuthenticity::Signed(local_key.clone()),
            gossipsub_config,
        )
        .map_err(|e| NetworkError::Protocol(e.to_string()))?;

        // Create Kademlia
        let store = kad::store::MemoryStore::new(local_peer_id);
        let kademlia = kad::Behaviour::new(local_peer_id, store);

        // Create identify
        let identify = identify::Behaviour::new(identify::Config::new(
            "/qfc/1.0.0".to_string(),
            local_key.public(),
        ));

        // Create ping
        let ping = ping::Behaviour::new(ping::Config::new());

        // Create behaviour
        let behaviour = QfcBehaviour {
            gossipsub,
            kademlia,
            identify,
            ping,
        };

        // Create swarm
        let mut swarm = Swarm::new(
            transport,
            behaviour,
            local_peer_id,
            libp2p::swarm::Config::with_tokio_executor()
                .with_idle_connection_timeout(config.idle_timeout),
        );

        // Subscribe to topics
        let block_topic = IdentTopic::new(topics::NEW_BLOCKS);
        let tx_topic = IdentTopic::new(topics::NEW_TRANSACTIONS);
        let vote_topic = IdentTopic::new(topics::CONSENSUS_VOTES);
        let validator_topic = IdentTopic::new(topics::VALIDATOR_MESSAGES);

        swarm.behaviour_mut().gossipsub.subscribe(&block_topic)
            .map_err(|e| NetworkError::Protocol(e.to_string()))?;
        swarm.behaviour_mut().gossipsub.subscribe(&tx_topic)
            .map_err(|e| NetworkError::Protocol(e.to_string()))?;
        swarm.behaviour_mut().gossipsub.subscribe(&vote_topic)
            .map_err(|e| NetworkError::Protocol(e.to_string()))?;
        swarm.behaviour_mut().gossipsub.subscribe(&validator_topic)
            .map_err(|e| NetworkError::Protocol(e.to_string()))?;

        // Listen on addresses
        for addr in &config.listen_addresses {
            swarm.listen_on(addr.clone())
                .map_err(|e| NetworkError::Listen(e.to_string()))?;
        }

        // Connect to bootnodes
        for bootnode in &config.bootnodes {
            if let Err(e) = swarm.dial(bootnode.clone()) {
                warn!("Failed to dial bootnode {}: {}", bootnode, e);
            }
        }

        let peers = Arc::new(RwLock::new(HashSet::new()));
        let (message_tx, message_rx) = mpsc::channel(1000);
        let (internal_tx, mut internal_rx) = mpsc::channel::<NetworkMessage>(100);

        let peers_clone = Arc::clone(&peers);
        let message_tx_clone = message_tx.clone();

        // Spawn the swarm event loop
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    event = swarm.select_next_some() => {
                        match event {
                            SwarmEvent::Behaviour(QfcBehaviourEvent::Gossipsub(gossipsub::Event::Message {
                                propagation_source: _,
                                message_id: _,
                                message,
                            })) => {
                                // Handle received gossipsub message
                                let topic = message.topic.as_str();
                                let data = message.data;

                                let network_msg = match topic {
                                    t if t == topics::NEW_BLOCKS => NetworkMessage::NewBlock(data),
                                    t if t == topics::NEW_TRANSACTIONS => NetworkMessage::NewTransaction(data),
                                    t if t == topics::CONSENSUS_VOTES => NetworkMessage::Vote(data),
                                    t if t == topics::VALIDATOR_MESSAGES => NetworkMessage::ValidatorMsg(data),
                                    _ => {
                                        debug!("Unknown topic: {}", topic);
                                        continue;
                                    }
                                };

                                if let Err(e) = message_tx_clone.send(network_msg).await {
                                    warn!("Failed to forward message: {}", e);
                                }
                            }
                            SwarmEvent::Behaviour(QfcBehaviourEvent::Identify(identify::Event::Received { peer_id, info, .. })) => {
                                debug!("Identified peer {}: {:?}", peer_id, info.protocol_version);
                                // Add peer to Kademlia DHT
                                for addr in info.listen_addrs {
                                    swarm.behaviour_mut().kademlia.add_address(&peer_id, addr);
                                }
                            }
                            SwarmEvent::Behaviour(_) => {
                                // Other behaviour events
                            }
                            SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                                info!("Connected to peer: {}", peer_id);
                                peers_clone.write().insert(peer_id);
                            }
                            SwarmEvent::ConnectionClosed { peer_id, .. } => {
                                info!("Disconnected from peer: {}", peer_id);
                                peers_clone.write().remove(&peer_id);
                            }
                            SwarmEvent::NewListenAddr { address, .. } => {
                                info!("Listening on: {}", address);
                            }
                            _ => {}
                        }
                    }
                    Some(msg) = internal_rx.recv() => {
                        // Publish message to appropriate topic
                        let (topic, data) = match msg {
                            NetworkMessage::NewBlock(data) => (topics::NEW_BLOCKS, data),
                            NetworkMessage::NewTransaction(data) => (topics::NEW_TRANSACTIONS, data),
                            NetworkMessage::Vote(data) => (topics::CONSENSUS_VOTES, data),
                            NetworkMessage::ValidatorMsg(data) => (topics::VALIDATOR_MESSAGES, data),
                        };

                        let topic = IdentTopic::new(topic);
                        if let Err(e) = swarm.behaviour_mut().gossipsub.publish(topic, data) {
                            warn!("Failed to publish message: {}", e);
                        }
                    }
                }
            }
        });

        let service = NetworkService {
            local_peer_id,
            peers,
            message_tx: internal_tx,
            config,
        };

        Ok((service, message_rx))
    }

    /// Get local peer ID
    pub fn local_peer_id(&self) -> PeerId {
        self.local_peer_id
    }

    /// Get connected peers
    pub fn peers(&self) -> Vec<PeerId> {
        self.peers.read().iter().copied().collect()
    }

    /// Get peer count
    pub fn peer_count(&self) -> usize {
        self.peers.read().len()
    }

    /// Broadcast a new block
    pub async fn broadcast_block(&self, block_data: Vec<u8>) -> Result<()> {
        self.message_tx
            .send(NetworkMessage::NewBlock(block_data))
            .await
            .map_err(|e| NetworkError::Protocol(e.to_string()))
    }

    /// Broadcast a new transaction
    pub async fn broadcast_transaction(&self, tx_data: Vec<u8>) -> Result<()> {
        self.message_tx
            .send(NetworkMessage::NewTransaction(tx_data))
            .await
            .map_err(|e| NetworkError::Protocol(e.to_string()))
    }

    /// Broadcast a vote
    pub async fn broadcast_vote(&self, vote_data: Vec<u8>) -> Result<()> {
        self.message_tx
            .send(NetworkMessage::Vote(vote_data))
            .await
            .map_err(|e| NetworkError::Protocol(e.to_string()))
    }
}
