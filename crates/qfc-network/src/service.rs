//! Network service

use crate::behaviour::{topics, NetworkMessage, QfcBehaviour, QfcBehaviourEvent};
use crate::config::NetworkConfig;
use crate::error::{NetworkError, Result};
use crate::sync_protocol::{SyncRequest, SyncResponse, SYNC_PROTOCOL};
use futures::StreamExt;
use libp2p::{
    gossipsub::{self, IdentTopic, MessageAuthenticity},
    identify, kad, noise, ping,
    request_response::{self, OutboundRequestId, ProtocolSupport},
    swarm::SwarmEvent,
    tcp, yamux, PeerId, Swarm, Transport,
};
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

/// Commands for the network service
#[derive(Debug)]
pub enum NetworkCommand {
    /// Broadcast a gossipsub message
    Broadcast(NetworkMessage),
    /// Send a sync request to a peer
    SyncRequest {
        peer_id: PeerId,
        request: SyncRequest,
        response_tx: oneshot::Sender<Result<SyncResponse>>,
    },
}

/// Sync event for the node to handle
#[derive(Debug)]
pub enum SyncEvent {
    /// Received a sync request from a peer
    Request {
        peer_id: PeerId,
        request: SyncRequest,
        response_tx: oneshot::Sender<SyncResponse>,
    },
}

/// Network service handle
pub struct NetworkService {
    /// Local peer ID
    local_peer_id: PeerId,
    /// Connected peers
    peers: Arc<RwLock<HashSet<PeerId>>>,
    /// Command sender
    command_tx: mpsc::Sender<NetworkCommand>,
    /// Pending sync requests (for future async request handling)
    #[allow(dead_code)]
    pending_requests:
        Arc<RwLock<HashMap<OutboundRequestId, oneshot::Sender<Result<SyncResponse>>>>>,
    /// Configuration
    #[allow(dead_code)]
    config: NetworkConfig,
}

impl NetworkService {
    /// Create and start the network service
    pub async fn start(
        config: NetworkConfig,
    ) -> Result<(
        Self,
        mpsc::Receiver<NetworkMessage>,
        mpsc::Receiver<SyncEvent>,
    )> {
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

        // Create sync request-response
        let sync = request_response::Behaviour::new(
            [(SYNC_PROTOCOL, ProtocolSupport::Full)],
            request_response::Config::default().with_request_timeout(Duration::from_secs(30)),
        );

        // Create behaviour
        let behaviour = QfcBehaviour {
            gossipsub,
            kademlia,
            identify,
            ping,
            sync,
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

        swarm
            .behaviour_mut()
            .gossipsub
            .subscribe(&block_topic)
            .map_err(|e| NetworkError::Protocol(e.to_string()))?;
        swarm
            .behaviour_mut()
            .gossipsub
            .subscribe(&tx_topic)
            .map_err(|e| NetworkError::Protocol(e.to_string()))?;
        swarm
            .behaviour_mut()
            .gossipsub
            .subscribe(&vote_topic)
            .map_err(|e| NetworkError::Protocol(e.to_string()))?;
        swarm
            .behaviour_mut()
            .gossipsub
            .subscribe(&validator_topic)
            .map_err(|e| NetworkError::Protocol(e.to_string()))?;

        // Listen on addresses
        for addr in &config.listen_addresses {
            swarm
                .listen_on(addr.clone())
                .map_err(|e| NetworkError::Listen(e.to_string()))?;
        }

        // Connect to bootnodes
        for bootnode in &config.bootnodes {
            if let Err(e) = swarm.dial(bootnode.clone()) {
                warn!("Failed to dial bootnode {}: {}", bootnode, e);
            }
        }

        let peers = Arc::new(RwLock::new(HashSet::new()));
        let pending_requests: Arc<
            RwLock<HashMap<OutboundRequestId, oneshot::Sender<Result<SyncResponse>>>>,
        > = Arc::new(RwLock::new(HashMap::new()));

        let (message_tx, message_rx) = mpsc::channel(1000);
        let (sync_event_tx, sync_event_rx) = mpsc::channel(100);
        let (command_tx, mut command_rx) = mpsc::channel::<NetworkCommand>(100);
        let (swarm_response_tx, mut swarm_response_rx) = mpsc::channel::<(
            request_response::ResponseChannel<SyncResponse>,
            SyncResponse,
        )>(100);

        let peers_clone = Arc::clone(&peers);
        let pending_clone = Arc::clone(&pending_requests);
        let swarm_response_tx_clone = swarm_response_tx.clone();

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

                                if let Err(e) = message_tx.send(network_msg).await {
                                    warn!("Failed to forward message: {}", e);
                                }
                            }
                            SwarmEvent::Behaviour(QfcBehaviourEvent::Sync(request_response::Event::Message { peer, message, .. })) => {
                                match message {
                                    request_response::Message::Request { request, channel, .. } => {
                                        info!("Received sync request from {}: {:?}", peer, request);
                                        // Create a channel for the response
                                        let (tx, rx) = oneshot::channel();
                                        let event = SyncEvent::Request {
                                            peer_id: peer,
                                            request,
                                            response_tx: tx,
                                        };
                                        let swarm_response_tx = swarm_response_tx_clone.clone();
                                        if let Err(e) = sync_event_tx.send(event).await {
                                            warn!("Failed to forward sync request: {}", e);
                                        } else {
                                            // Wait for response and send it back
                                            tokio::spawn(async move {
                                                if let Ok(response) = rx.await {
                                                    if swarm_response_tx.send((channel, response)).await.is_err() {
                                                        warn!("Failed to queue sync response");
                                                    }
                                                }
                                            });
                                        }
                                    }
                                    request_response::Message::Response { request_id, response } => {
                                        info!("Received sync response from {}: {:?}", peer, response);
                                        // Complete the pending request
                                        if let Some(tx) = pending_clone.write().remove(&request_id) {
                                            let _ = tx.send(Ok(response));
                                        }
                                    }
                                }
                            }
                            SwarmEvent::Behaviour(QfcBehaviourEvent::Sync(request_response::Event::OutboundFailure { peer, request_id, error, .. })) => {
                                warn!("Sync request to {} failed: {:?}", peer, error);
                                if let Some(tx) = pending_clone.write().remove(&request_id) {
                                    let _ = tx.send(Err(NetworkError::Protocol(format!("{:?}", error))));
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
                    Some(cmd) = command_rx.recv() => {
                        match cmd {
                            NetworkCommand::Broadcast(msg) => {
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
                            NetworkCommand::SyncRequest { peer_id, request, response_tx } => {
                                let request_id = swarm.behaviour_mut().sync.send_request(&peer_id, request);
                                pending_clone.write().insert(request_id, response_tx);
                            }
                        }
                    }
                    Some((channel, response)) = swarm_response_rx.recv() => {
                        // Send sync response back to peer
                        info!("Sending sync response: {:?}", response);
                        if swarm.behaviour_mut().sync.send_response(channel, response).is_err() {
                            warn!("Failed to send sync response");
                        }
                    }
                }
            }
        });

        let service = NetworkService {
            local_peer_id,
            peers,
            command_tx,
            pending_requests,
            config,
        };

        Ok((service, message_rx, sync_event_rx))
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
        self.command_tx
            .send(NetworkCommand::Broadcast(NetworkMessage::NewBlock(
                block_data,
            )))
            .await
            .map_err(|e| NetworkError::Protocol(e.to_string()))
    }

    /// Broadcast a new transaction
    pub async fn broadcast_transaction(&self, tx_data: Vec<u8>) -> Result<()> {
        self.command_tx
            .send(NetworkCommand::Broadcast(NetworkMessage::NewTransaction(
                tx_data,
            )))
            .await
            .map_err(|e| NetworkError::Protocol(e.to_string()))
    }

    /// Broadcast a vote
    pub async fn broadcast_vote(&self, vote_data: Vec<u8>) -> Result<()> {
        self.command_tx
            .send(NetworkCommand::Broadcast(NetworkMessage::Vote(vote_data)))
            .await
            .map_err(|e| NetworkError::Protocol(e.to_string()))
    }

    /// Broadcast a validator message (heartbeat, epoch announcement, slashing evidence)
    pub async fn broadcast_validator_msg(&self, msg_data: Vec<u8>) -> Result<()> {
        self.command_tx
            .send(NetworkCommand::Broadcast(NetworkMessage::ValidatorMsg(
                msg_data,
            )))
            .await
            .map_err(|e| NetworkError::Protocol(e.to_string()))
    }

    /// Request a block by hash from a peer
    pub async fn request_block_by_hash(
        &self,
        peer_id: PeerId,
        hash: qfc_types::Hash,
    ) -> Result<SyncResponse> {
        let (tx, rx) = oneshot::channel();
        self.command_tx
            .send(NetworkCommand::SyncRequest {
                peer_id,
                request: SyncRequest::GetBlockByHash(hash),
                response_tx: tx,
            })
            .await
            .map_err(|e| NetworkError::Protocol(e.to_string()))?;

        rx.await
            .map_err(|_| NetworkError::Protocol("Request cancelled".to_string()))?
    }

    /// Request a block by number from a peer
    pub async fn request_block_by_number(
        &self,
        peer_id: PeerId,
        number: u64,
    ) -> Result<SyncResponse> {
        let (tx, rx) = oneshot::channel();
        self.command_tx
            .send(NetworkCommand::SyncRequest {
                peer_id,
                request: SyncRequest::GetBlockByNumber(number),
                response_tx: tx,
            })
            .await
            .map_err(|e| NetworkError::Protocol(e.to_string()))?;

        rx.await
            .map_err(|_| NetworkError::Protocol("Request cancelled".to_string()))?
    }

    /// Request a range of blocks from a peer
    pub async fn request_block_range(
        &self,
        peer_id: PeerId,
        start: u64,
        end: u64,
    ) -> Result<SyncResponse> {
        let (tx, rx) = oneshot::channel();
        self.command_tx
            .send(NetworkCommand::SyncRequest {
                peer_id,
                request: SyncRequest::GetBlockRange { start, end },
                response_tx: tx,
            })
            .await
            .map_err(|e| NetworkError::Protocol(e.to_string()))?;

        rx.await
            .map_err(|_| NetworkError::Protocol("Request cancelled".to_string()))?
    }

    /// Request status from a peer
    pub async fn request_status(&self, peer_id: PeerId) -> Result<SyncResponse> {
        let (tx, rx) = oneshot::channel();
        self.command_tx
            .send(NetworkCommand::SyncRequest {
                peer_id,
                request: SyncRequest::GetStatus,
                response_tx: tx,
            })
            .await
            .map_err(|e| NetworkError::Protocol(e.to_string()))?;

        rx.await
            .map_err(|_| NetworkError::Protocol("Request cancelled".to_string()))?
    }
}
