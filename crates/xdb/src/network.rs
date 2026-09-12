//! XDB Network Module
//! Handles P2P networking via libp2p with GossipSub and mDNS discovery

use crate::db::SharedDb;
use futures::StreamExt;
use libp2p::{
    gossipsub::{self, IdentTopic, MessageAuthenticity, ValidationMode},
    identify, mdns, noise,
    swarm::{NetworkBehaviour, SwarmEvent},
    tcp, yamux, PeerId, Swarm,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, Mutex};
use tracing::{debug, error, info, warn};

const SYNC_TOPIC: &str = "xdb-sync";
const PROTOCOL_VERSION: &str = "/xdb/1.0.0";

#[derive(NetworkBehaviour)]
pub struct XdbBehaviour {
    gossipsub: gossipsub::Behaviour,
    mdns: mdns::tokio::Behaviour,
    identify: identify::Behaviour,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NetworkMessage {
    SyncUpdate {
        collection: String,
        update: Vec<u8>,
        sender_id: String,
    },
    SyncRequest {
        collection: String,
        state_vector: Vec<u8>,
        requester_id: String,
    },
    SyncResponse {
        collection: String,
        update: Vec<u8>,
        requester_id: String,
        responder_id: String,
    },
    PeerAnnounce {
        peer_id: String,
        collections: Vec<String>,
    },
}

#[derive(Debug, Clone)]
pub struct PeerInfo {
    pub peer_id: String,
    pub addresses: Vec<String>,
}

#[derive(Clone)]
pub struct NetworkNode {
    local_peer_id: PeerId,
    command_tx: mpsc::Sender<NetworkCommand>,
    connected_peers: Arc<Mutex<HashSet<PeerId>>>,
}

#[derive(Debug, Clone)]
pub enum NetworkCommand {
    Publish { message: NetworkMessage },
    Shutdown,
}

#[derive(Debug, Clone)]
pub enum NetworkEvent {
    MessageReceived(NetworkMessage),
    PeerConnected(PeerInfo),
    PeerDisconnected(String),
}

// GossipSub validates the signature against `message.source`. The propagation
// source is only the last relay, which can differ from the original author.
fn message_matches_author(message: &NetworkMessage, author: &PeerId) -> bool {
    let author = author.to_string();
    match message {
        NetworkMessage::SyncUpdate { sender_id, .. } => sender_id == &author,
        NetworkMessage::SyncRequest { requester_id, .. } => requester_id == &author,
        NetworkMessage::SyncResponse { responder_id, .. } => responder_id == &author,
        NetworkMessage::PeerAnnounce { peer_id, .. } => peer_id == &author,
    }
}

fn publish_message(
    behaviour: &mut gossipsub::Behaviour,
    topic: &IdentTopic,
    message: &NetworkMessage,
) {
    match serde_json::to_vec(message) {
        Ok(data) => match behaviour.publish(topic.clone(), data) {
            Ok(_) => {}
            Err(gossipsub::PublishError::InsufficientPeers) => {
                // Local writes have already been persisted. An offline node
                // cannot deliver this update; this is not a database failure.
                debug!("No subscribed peers available to receive XDB update");
            }
            Err(e) => error!("Failed to publish message: {}", e),
        },
        Err(e) => error!("Failed to serialize network message: {}", e),
    }
}

fn connection_event(
    peers: &mut HashSet<PeerId>,
    peer_id: PeerId,
    connection_count: u32,
    address: Option<String>,
) -> Option<NetworkEvent> {
    if connection_count == 0 {
        peers
            .remove(&peer_id)
            .then(|| NetworkEvent::PeerDisconnected(peer_id.to_string()))
    } else if peers.insert(peer_id) {
        Some(NetworkEvent::PeerConnected(PeerInfo {
            peer_id: peer_id.to_string(),
            addresses: address.into_iter().collect(),
        }))
    } else {
        None
    }
}

impl NetworkNode {
    pub async fn new(
        db: SharedDb,
        event_tx: broadcast::Sender<NetworkEvent>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let id_keys = libp2p::identity::Keypair::generate_ed25519();
        let local_peer_id = id_keys.public().to_peer_id();

        info!("Local peer ID: {}", local_peer_id);

        // Create gossipsub config
        let gossipsub_config = gossipsub::ConfigBuilder::default()
            .heartbeat_interval(Duration::from_secs(1))
            .validation_mode(ValidationMode::Strict)
            .message_id_fn(|message: &gossipsub::Message| {
                gossipsub::MessageId::from(
                    format!("{:?}{:?}", message.source, message.sequence_number).into_bytes(),
                )
            })
            .build()
            .map_err(|e| format!("Gossipsub config error: {}", e))?;

        // Create gossipsub behaviour
        let gossipsub = gossipsub::Behaviour::new(
            MessageAuthenticity::Signed(id_keys.clone()),
            gossipsub_config,
        )
        .map_err(|e| format!("Gossipsub behaviour error: {}", e))?;

        // Create mDNS behaviour
        let mdns = mdns::tokio::Behaviour::new(mdns::Config::default(), local_peer_id)?;

        // Create identify behaviour
        let identify = identify::Behaviour::new(identify::Config::new(
            PROTOCOL_VERSION.to_string(),
            id_keys.public(),
        ));

        // Build the swarm
        let mut swarm = libp2p::SwarmBuilder::with_existing_identity(id_keys)
            .with_tokio()
            .with_tcp(
                tcp::Config::default(),
                noise::Config::new,
                yamux::Config::default,
            )?
            .with_behaviour(|_key| XdbBehaviour {
                gossipsub,
                mdns,
                identify,
            })?
            .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
            .build();

        // Subscribe to sync topic
        let topic = IdentTopic::new(SYNC_TOPIC);
        swarm.behaviour_mut().gossipsub.subscribe(&topic)?;

        // Listen on all interfaces
        swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()?)?;

        // Create command channel
        let (command_tx, mut command_rx) = mpsc::channel::<NetworkCommand>(100);

        let connected_peers = Arc::new(Mutex::new(HashSet::new()));
        let peers_clone = connected_peers.clone();

        // Spawn the network loop
        let db_clone = db.clone();
        let peer_id_clone = local_peer_id;
        tokio::spawn(async move {
            Self::run_event_loop(
                swarm,
                &mut command_rx,
                event_tx,
                db_clone,
                peers_clone,
                peer_id_clone,
            )
            .await;
        });

        Ok(Self {
            local_peer_id,
            command_tx,
            connected_peers,
        })
    }

    async fn run_event_loop(
        mut swarm: Swarm<XdbBehaviour>,
        command_rx: &mut mpsc::Receiver<NetworkCommand>,
        event_tx: broadcast::Sender<NetworkEvent>,
        db: SharedDb,
        connected_peers: Arc<Mutex<HashSet<PeerId>>>,
        local_peer_id: PeerId,
    ) {
        let topic = IdentTopic::new(SYNC_TOPIC);

        loop {
            tokio::select! {
                // Handle swarm events
                event = swarm.select_next_some() => {
                    match event {
                        SwarmEvent::Behaviour(XdbBehaviourEvent::Mdns(mdns::Event::Discovered(peers))) => {
                            for (peer_id, addr) in peers {
                                info!("Discovered peer via mDNS: {} at {}", peer_id, addr);
                                swarm.behaviour_mut().gossipsub.add_explicit_peer(&peer_id);
                            }
                        }
                        SwarmEvent::Behaviour(XdbBehaviourEvent::Mdns(mdns::Event::Expired(peers))) => {
                            for (peer_id, _addr) in peers {
                                info!("Peer expired: {}", peer_id);
                                // One address expiring does not mean the peer
                                // disappeared, nor does it close live connections.
                                if !swarm.behaviour().mdns.discovered_nodes().any(|id| id == &peer_id) {
                                    swarm.behaviour_mut().gossipsub.remove_explicit_peer(&peer_id);
                                }
                            }
                        }
                        SwarmEvent::Behaviour(XdbBehaviourEvent::Gossipsub(gossipsub::Event::Message {
                            propagation_source: _,
                            message_id: _,
                            message,
                        })) => {
                            match serde_json::from_slice::<NetworkMessage>(&message.data) {
                                Ok(msg) => {
                                    let Some(author) = message.source else {
                                        warn!("Dropping XDB message without a signed author");
                                        continue;
                                    };
                                    if author == local_peer_id {
                                        continue;
                                    }
                                    if !message_matches_author(&msg, &author) {
                                        warn!("Dropping XDB message whose claimed sender differs from its signed author");
                                        continue;
                                    }
                                    // Only notify subscribers after an update was applied.
                                    match &msg {
                                        NetworkMessage::SyncUpdate { collection, update, .. } => {
                                            info!("Received sync update for collection: {}", collection);
                                            match db.lock() {
                                                Ok(mut db_lock) => {
                                                    if let Err(e) = db_lock.apply_remote_update(collection, update) {
                                                        error!("Failed to apply remote update: {}", e);
                                                        continue;
                                                    }
                                                }
                                                Err(e) => {
                                                    error!("DB lock poisoned, dropping sync update for {}: {}", collection, e);
                                                    continue;
                                                }
                                            }
                                        }
                                        NetworkMessage::SyncRequest { collection, state_vector, requester_id } => {
                                            info!("Received sync request for collection: {}", collection);
                                            match db.lock() {
                                                Ok(mut db_lock) => match db_lock.get_updates_since(collection, state_vector) {
                                                    Ok(update) => {
                                                        let response = NetworkMessage::SyncResponse {
                                                            collection: collection.clone(),
                                                            update,
                                                            requester_id: requester_id.clone(),
                                                            responder_id: local_peer_id.to_string(),
                                                        };
                                                        publish_message(&mut swarm.behaviour_mut().gossipsub, &topic, &response);
                                                    }
                                                    Err(e) => {
                                                        warn!("Failed to prepare sync response for {}: {}", collection, e);
                                                        continue;
                                                    }
                                                },
                                                Err(e) => {
                                                    error!("DB lock poisoned, dropping sync request for {}: {}", collection, e);
                                                    continue;
                                                }
                                            }
                                        }
                                        NetworkMessage::SyncResponse { collection, update, requester_id, .. } => {
                                            if requester_id != &local_peer_id.to_string() {
                                                continue;
                                            }
                                            info!("Received sync response for collection: {}", collection);
                                            match db.lock() {
                                                Ok(mut db_lock) => {
                                                    if let Err(e) = db_lock.apply_remote_update(collection, update) {
                                                        error!("Failed to apply sync response: {}", e);
                                                        continue;
                                                    }
                                                }
                                                Err(e) => {
                                                    error!("DB lock poisoned, dropping sync response for {}: {}", collection, e);
                                                    continue;
                                                }
                                            }
                                        }
                                        _ => {}
                                    }
                                    let _ = event_tx.send(NetworkEvent::MessageReceived(msg));
                                }
                                Err(e) => {
                                    warn!("Failed to deserialize network message: {}", e);
                                }
                            }
                        }
                        SwarmEvent::NewListenAddr { address, .. } => {
                            info!("Listening on: {}", address);
                        }
                        SwarmEvent::ConnectionEstablished { peer_id, endpoint, num_established, .. } => {
                            info!("Connection established with: {}", peer_id);
                            let event = connection_event(
                                &mut *connected_peers.lock().await,
                                peer_id,
                                num_established.get(),
                                Some(endpoint.get_remote_address().to_string()),
                            );
                            if let Some(event) = event {
                                let _ = event_tx.send(event);
                            }
                        }
                        SwarmEvent::ConnectionClosed { peer_id, num_established, .. } => {
                            info!("Connection closed with: {}", peer_id);
                            let event = connection_event(
                                &mut *connected_peers.lock().await,
                                peer_id,
                                num_established,
                                None,
                            );
                            if let Some(event) = event {
                                let _ = event_tx.send(event);
                            }
                        }
                        _ => {}
                    }
                }

                // Handle commands
                command = command_rx.recv() => {
                    match command {
                        Some(NetworkCommand::Publish { message }) => {
                            publish_message(&mut swarm.behaviour_mut().gossipsub, &topic, &message);
                        }
                        Some(NetworkCommand::Shutdown) | None => {
                            info!("Network node shutting down");
                            break;
                        }
                    }
                }
            }
        }
        connected_peers.lock().await.clear();
    }

    pub fn local_peer_id(&self) -> String {
        self.local_peer_id.to_string()
    }

    pub fn is_running(&self) -> bool {
        !self.command_tx.is_closed()
    }

    pub async fn publish(&self, message: NetworkMessage) -> Result<(), String> {
        self.command_tx
            .send(NetworkCommand::Publish { message })
            .await
            .map_err(|e| e.to_string())
    }

    pub async fn broadcast_update(&self, collection: &str, update: Vec<u8>) -> Result<(), String> {
        self.publish(NetworkMessage::SyncUpdate {
            collection: collection.to_string(),
            update,
            sender_id: self.local_peer_id.to_string(),
        })
        .await
    }

    pub async fn request_sync(
        &self,
        collection: &str,
        state_vector: Vec<u8>,
    ) -> Result<(), String> {
        self.publish(NetworkMessage::SyncRequest {
            collection: collection.to_string(),
            state_vector,
            requester_id: self.local_peer_id.to_string(),
        })
        .await
    }

    pub async fn get_connected_peers(&self) -> Vec<String> {
        self.connected_peers
            .lock()
            .await
            .iter()
            .map(|p| p.to_string())
            .collect()
    }

    pub async fn shutdown(&self) -> Result<(), String> {
        self.command_tx
            .send(NetworkCommand::Shutdown)
            .await
            .map_err(|e| e.to_string())
    }
}

pub type SharedNetwork = Arc<Mutex<Option<NetworkNode>>>;

pub fn create_shared_network() -> SharedNetwork {
    Arc::new(Mutex::new(None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_remains_connected_until_its_last_connection_closes() {
        let peer = PeerId::random();
        let mut peers = HashSet::new();
        let event = connection_event(&mut peers, peer, 1, Some("/memory/1".into()));
        assert!(matches!(event, Some(NetworkEvent::PeerConnected(_))));
        assert!(connection_event(&mut peers, peer, 2, Some("/memory/2".into())).is_none());
        assert!(connection_event(&mut peers, peer, 1, None).is_none());
        assert!(peers.contains(&peer));
        assert!(matches!(
            connection_event(&mut peers, peer, 0, None),
            Some(NetworkEvent::PeerDisconnected(_))
        ));
        assert!(peers.is_empty());
        assert!(connection_event(&mut peers, peer, 0, None).is_none());
    }

    #[test]
    fn sync_messages_match_the_original_signed_author() {
        let author = PeerId::random();
        let relay = PeerId::random();
        let messages = [
            NetworkMessage::SyncUpdate {
                collection: "notes".into(),
                update: vec![],
                sender_id: author.to_string(),
            },
            NetworkMessage::SyncRequest {
                collection: "notes".into(),
                state_vector: vec![],
                requester_id: author.to_string(),
            },
            NetworkMessage::SyncResponse {
                collection: "notes".into(),
                update: vec![],
                requester_id: relay.to_string(),
                responder_id: author.to_string(),
            },
            NetworkMessage::PeerAnnounce {
                peer_id: author.to_string(),
                collections: vec![],
            },
        ];
        for message in messages {
            assert!(message_matches_author(&message, &author));
            assert!(!message_matches_author(&message, &relay));
        }
    }

    #[tokio::test]
    async fn closed_network_task_reports_stopped_and_rejects_new_work() {
        let (command_tx, command_rx) = mpsc::channel(1);
        let node = NetworkNode {
            local_peer_id: PeerId::random(),
            command_tx,
            connected_peers: Arc::new(Mutex::new(HashSet::new())),
        };
        assert!(node.is_running());
        drop(command_rx);
        assert!(!node.is_running());
        assert!(node.broadcast_update("notes", vec![]).await.is_err());
    }
}
