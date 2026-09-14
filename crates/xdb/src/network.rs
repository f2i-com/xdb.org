//! XDB Network Module
//! Handles P2P networking via libp2p with GossipSub and (optional) mDNS discovery.
//!
//! Policy (September 2026 ecosystem review, XD-01/XD-02/XD-03):
//!
//! - Networking is a separate, explicit step from opening a database. Nothing in
//!   this module starts because a database was opened; the host decides through
//!   [`NetworkOptions`] and persists that choice (see `tauri::NetworkSettings`).
//! - The v1 protocol is a **trusted-LAN development feature**: one shared topic,
//!   one default namespace, mDNS discovery. Messages are signed by the transport
//!   key and the claimed sender is checked against the signer, but that is
//!   transport identity, not application authorization. Named-app data never
//!   enters this protocol (`tauri::supports_legacy_sync`).
//! - Convergence is explicit work, not a side effect of connectivity. On every
//!   new peer the node announces its collections and requests reconciliation for
//!   each of them; a bounded periodic repair pass repeats that while peers are
//!   connected; an announced collection this node has never seen is requested
//!   with an empty state vector so it is discovered, not skipped.
//! - Every message carries the collection's reset epoch. A peer that is behind
//!   an acknowledged reset cannot reintroduce pre-reset state; a peer that is
//!   ahead is told to adopt the reset before its updates apply.
//! - Status is honest: the node reports what it *did* (publishes that reached a
//!   peer versus none, updates applied versus rejected, last reconcile time),
//!   never a single "synced" boolean. Queueing a publish is not peer receipt,
//!   and peer receipt is not peer persistence.

use crate::db::{RemoteApplyOutcome, SharedDb};
use futures::StreamExt;
use libp2p::{
    gossipsub::{self, IdentTopic, MessageAuthenticity, ValidationMode},
    identify, mdns, noise,
    swarm::{behaviour::toggle::Toggle, NetworkBehaviour, SwarmEvent},
    tcp, yamux, PeerId, Swarm,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, Mutex};
use tracing::{debug, error, info, warn};

const SYNC_TOPIC: &str = "xdb-sync";
const PROTOCOL_VERSION: &str = "/xdb/1.0.0";

/// How often the bounded repair pass runs while at least one peer is connected.
pub const REPAIR_INTERVAL: Duration = Duration::from_secs(30);
/// Maximum collections announced / requested per announce or repair pass.
pub const MAX_COLLECTIONS_PER_PASS: usize = 200;
/// Collections requested per repair tick (round-robin over the local set).
pub const REPAIR_BATCH: usize = 25;

#[derive(NetworkBehaviour)]
pub struct XdbBehaviour {
    gossipsub: gossipsub::Behaviour,
    mdns: Toggle<mdns::tokio::Behaviour>,
    identify: identify::Behaviour,
}

/// What the host allows this node to do. All three default to OFF at the type
/// level so a forgotten setting can never mean "listen on every interface".
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetworkOptions {
    /// Advertise and look for peers on the local network (mDNS).
    pub discovery: bool,
    /// Accept inbound connections on all interfaces.
    pub listen: bool,
}

impl NetworkOptions {
    /// The trusted-LAN development configuration (discovery + listening).
    pub fn trusted_lan() -> Self {
        Self {
            discovery: true,
            listen: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NetworkMessage {
    SyncUpdate {
        collection: String,
        update: Vec<u8>,
        sender_id: String,
        #[serde(default)]
        epoch: u64,
    },
    SyncRequest {
        collection: String,
        state_vector: Vec<u8>,
        requester_id: String,
        #[serde(default)]
        epoch: u64,
    },
    SyncResponse {
        collection: String,
        update: Vec<u8>,
        requester_id: String,
        responder_id: String,
        #[serde(default)]
        epoch: u64,
    },
    /// Sent on connection and on every repair pass: the collections this peer
    /// holds, so the receiver can request the ones it does not know yet.
    PeerAnnounce {
        peer_id: String,
        collections: Vec<String>,
    },
    /// An administrative reset of a whole collection (audit XD-03). Receivers
    /// with an older epoch clear the collection and adopt the epoch; updates
    /// from peers still on the old epoch are rejected from then on.
    CollectionReset {
        collection: String,
        epoch: u64,
        origin_id: String,
    },
}

impl NetworkMessage {
    pub fn collection(&self) -> Option<&str> {
        match self {
            NetworkMessage::SyncUpdate { collection, .. }
            | NetworkMessage::SyncRequest { collection, .. }
            | NetworkMessage::SyncResponse { collection, .. }
            | NetworkMessage::CollectionReset { collection, .. } => Some(collection),
            NetworkMessage::PeerAnnounce { .. } => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PeerInfo {
    pub peer_id: String,
    pub addresses: Vec<String>,
}

/// What the node actually did, for an honest status display. Counters are
/// cumulative for the node's lifetime; timestamps are RFC 3339 UTC.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SyncStats {
    /// Publishes that gossipsub accepted with at least one subscribed peer.
    /// This is delivery to the mesh, NOT an acknowledgement that a peer
    /// applied or persisted the update.
    pub publishes_sent: u64,
    /// Publishes made while no peer was subscribed. The local write is safe;
    /// the update was not delivered and will be reconciled on the next
    /// announce/repair pass.
    pub publishes_without_peers: u64,
    pub publish_failures: u64,
    pub updates_applied: u64,
    pub updates_rejected_stale: u64,
    pub updates_skipped_paused: u64,
    pub resets_applied: u64,
    pub sync_requests_sent: u64,
    pub sync_responses_applied: u64,
    pub last_announce_at: Option<String>,
    pub last_repair_at: Option<String>,
    pub last_update_applied_at: Option<String>,
}

/// Pause switch shared between the host and the event loop (audit XD-03):
/// after a local-scope restore, synchronization is held until the operator
/// decides whether that restore stays local, forks or replaces shared state.
#[derive(Debug, Default)]
pub struct SyncGate {
    paused: AtomicBool,
}

impl SyncGate {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    pub fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
    }
    pub fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
    }
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }
}

#[derive(Clone)]
pub struct NetworkNode {
    local_peer_id: PeerId,
    command_tx: mpsc::Sender<NetworkCommand>,
    connected_peers: Arc<Mutex<HashSet<PeerId>>>,
    options: NetworkOptions,
    stats: Arc<StdMutex<SyncStats>>,
    gate: Arc<SyncGate>,
}

#[derive(Debug, Clone)]
pub enum NetworkCommand {
    Publish { message: NetworkMessage },
    /// Announce local collections and request reconciliation for every one of them.
    Reconcile,
    Shutdown,
}

#[derive(Debug, Clone)]
pub enum NetworkEvent {
    MessageReceived(NetworkMessage),
    PeerConnected(PeerInfo),
    PeerDisconnected(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishOutcome {
    /// Accepted by gossipsub with at least one subscribed peer.
    Sent,
    /// No subscribed peer: nothing was delivered.
    NoPeers,
    Failed,
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
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
        NetworkMessage::CollectionReset { origin_id, .. } => origin_id == &author,
    }
}

fn publish_message(
    behaviour: &mut gossipsub::Behaviour,
    topic: &IdentTopic,
    message: &NetworkMessage,
    stats: &Arc<StdMutex<SyncStats>>,
) -> PublishOutcome {
    let outcome = match serde_json::to_vec(message) {
        Ok(data) => match behaviour.publish(topic.clone(), data) {
            Ok(_) => PublishOutcome::Sent,
            Err(gossipsub::PublishError::NoPeersSubscribedToTopic) => {
                // Local writes have already been persisted. An offline node
                // cannot deliver this update; this is not a database failure,
                // and it is NOT delivery either — the counters say which.
                debug!("No subscribed peers available to receive XDB message");
                PublishOutcome::NoPeers
            }
            Err(e) => {
                error!("Failed to publish message: {}", e);
                PublishOutcome::Failed
            }
        },
        Err(e) => {
            error!("Failed to serialize network message: {}", e);
            PublishOutcome::Failed
        }
    };
    if let Ok(mut s) = stats.lock() {
        match outcome {
            PublishOutcome::Sent => s.publishes_sent += 1,
            PublishOutcome::NoPeers => s.publishes_without_peers += 1,
            PublishOutcome::Failed => s.publish_failures += 1,
        }
        if matches!(message, NetworkMessage::SyncRequest { .. }) && outcome == PublishOutcome::Sent {
            s.sync_requests_sent += 1;
        }
    }
    outcome
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

/// Which collections a repair tick should request next: a round-robin window
/// over the local set, so a large database is reconciled in bounded slices.
fn repair_window(collections: &[String], cursor: &mut usize, batch: usize) -> Vec<String> {
    if collections.is_empty() {
        *cursor = 0;
        return Vec::new();
    }
    let start = *cursor % collections.len();
    let out: Vec<String> = collections
        .iter()
        .cycle()
        .skip(start)
        .take(batch.min(collections.len()))
        .cloned()
        .collect();
    *cursor = (start + out.len()) % collections.len();
    out
}

/// Deterministic per-node jitter so a LAN full of peers does not repair in
/// lock-step: 0–4999 ms derived from the peer id bytes.
fn repair_jitter(peer_id: &PeerId) -> Duration {
    let sum: u64 = peer_id.to_bytes().iter().map(|b| *b as u64).sum();
    Duration::from_millis(sum % 5000)
}

impl NetworkNode {
    pub async fn new(
        db: SharedDb,
        event_tx: broadcast::Sender<NetworkEvent>,
        options: NetworkOptions,
        gate: Arc<SyncGate>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let id_keys = libp2p::identity::Keypair::generate_ed25519();
        let local_peer_id = id_keys.public().to_peer_id();

        info!(
            "Local peer ID: {} (discovery={}, listen={})",
            local_peer_id, options.discovery, options.listen
        );

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

        // mDNS only when discovery was explicitly enabled.
        let mdns = if options.discovery {
            Toggle::from(Some(mdns::tokio::Behaviour::new(
                mdns::Config::default(),
                local_peer_id,
            )?))
        } else {
            Toggle::from(None)
        };

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

        // Listen on all interfaces only when the host asked for it.
        if options.listen {
            swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()?)?;
        }

        // Create command channel
        let (command_tx, mut command_rx) = mpsc::channel::<NetworkCommand>(100);

        let connected_peers = Arc::new(Mutex::new(HashSet::new()));
        let peers_clone = connected_peers.clone();
        let stats = Arc::new(StdMutex::new(SyncStats::default()));
        let stats_clone = stats.clone();
        let gate_clone = gate.clone();

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
                stats_clone,
                gate_clone,
            )
            .await;
        });

        Ok(Self {
            local_peer_id,
            command_tx,
            connected_peers,
            options,
            stats,
            gate,
        })
    }

    /// Announce local collections and request reconciliation for each of them
    /// (bounded). Returns the number of requests queued.
    fn reconcile_locally(
        swarm: &mut Swarm<XdbBehaviour>,
        topic: &IdentTopic,
        db: &SharedDb,
        local_peer_id: &PeerId,
        stats: &Arc<StdMutex<SyncStats>>,
        collections: &[String],
        announce: bool,
    ) -> usize {
        let plan = match db.lock() {
            Ok(mut db_lock) => {
                let mut plan = Vec::with_capacity(collections.len());
                for collection in collections {
                    match (
                        db_lock.get_state_vector(collection),
                        db_lock.get_epoch(collection),
                    ) {
                        (Ok(sv), Ok(epoch)) => plan.push((collection.clone(), sv, epoch)),
                        (Err(e), _) | (_, Err(e)) => {
                            warn!("Cannot prepare reconciliation for {}: {}", collection, e)
                        }
                    }
                }
                plan
            }
            Err(e) => {
                error!("DB lock poisoned, skipping reconciliation: {}", e);
                return 0;
            }
        };
        if announce {
            publish_message(
                &mut swarm.behaviour_mut().gossipsub,
                topic,
                &NetworkMessage::PeerAnnounce {
                    peer_id: local_peer_id.to_string(),
                    collections: collections.to_vec(),
                },
                stats,
            );
        }
        let mut queued = 0;
        for (collection, state_vector, epoch) in plan {
            let request = NetworkMessage::SyncRequest {
                collection,
                state_vector,
                requester_id: local_peer_id.to_string(),
                epoch,
            };
            if publish_message(&mut swarm.behaviour_mut().gossipsub, topic, &request, stats)
                == PublishOutcome::Sent
            {
                queued += 1;
            }
        }
        queued
    }

    fn local_collections(db: &SharedDb) -> Vec<String> {
        match db.lock() {
            Ok(db_lock) => db_lock
                .get_collections()
                .unwrap_or_default()
                .into_iter()
                .take(MAX_COLLECTIONS_PER_PASS)
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Apply an inbound update honouring reset epochs. Returns true when applied.
    #[allow(clippy::too_many_arguments)]
    fn apply_inbound(
        db: &SharedDb,
        collection: &str,
        epoch: u64,
        update: &[u8],
        author: &str,
        stats: &Arc<StdMutex<SyncStats>>,
        gate: &Arc<SyncGate>,
        response: bool,
    ) -> bool {
        if gate.is_paused() {
            if let Ok(mut s) = stats.lock() {
                s.updates_skipped_paused += 1;
            }
            debug!("Synchronization paused; holding update for {}", collection);
            return false;
        }
        let mut db_lock = match db.lock() {
            Ok(l) => l,
            Err(e) => {
                error!("DB lock poisoned, dropping update for {}: {}", collection, e);
                return false;
            }
        };
        for _attempt in 0..2 {
            match db_lock.apply_remote_update_at_epoch(collection, epoch, update) {
                Ok(RemoteApplyOutcome::Applied(_)) => {
                    if let Ok(mut s) = stats.lock() {
                        s.updates_applied += 1;
                        if response {
                            s.sync_responses_applied += 1;
                        }
                        s.last_update_applied_at = Some(now_rfc3339());
                    }
                    return true;
                }
                Ok(RemoteApplyOutcome::StaleEpoch { local, remote }) => {
                    if let Ok(mut s) = stats.lock() {
                        s.updates_rejected_stale += 1;
                    }
                    warn!(
                        "Rejected update for {} from {}: sender epoch {} is behind local reset epoch {}",
                        collection, author, remote, local
                    );
                    return false;
                }
                Ok(RemoteApplyOutcome::MissingReset { local, remote }) => {
                    // The sender has acknowledged a reset this node never saw:
                    // adopt it (clearing pre-reset state) and apply the update.
                    info!(
                        "Adopting reset epoch {} for {} announced by {} (local was {})",
                        remote, collection, author, local
                    );
                    match db_lock.apply_remote_reset(collection, remote, author) {
                        Ok(true) => {
                            if let Ok(mut s) = stats.lock() {
                                s.resets_applied += 1;
                            }
                            continue;
                        }
                        Ok(false) => return false,
                        Err(e) => {
                            error!("Failed to adopt reset for {}: {}", collection, e);
                            return false;
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to apply update for {}: {}", collection, e);
                    return false;
                }
            }
        }
        false
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_event_loop(
        mut swarm: Swarm<XdbBehaviour>,
        command_rx: &mut mpsc::Receiver<NetworkCommand>,
        event_tx: broadcast::Sender<NetworkEvent>,
        db: SharedDb,
        connected_peers: Arc<Mutex<HashSet<PeerId>>>,
        local_peer_id: PeerId,
        stats: Arc<StdMutex<SyncStats>>,
        gate: Arc<SyncGate>,
    ) {
        let topic = IdentTopic::new(SYNC_TOPIC);
        let mut repair = tokio::time::interval(REPAIR_INTERVAL + repair_jitter(&local_peer_id));
        repair.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut repair_cursor = 0usize;

        loop {
            tokio::select! {
                // Bounded periodic repair (audit XD-02): while peers are connected,
                // re-announce and request a round-robin slice of local collections.
                _ = repair.tick() => {
                    if connected_peers.lock().await.is_empty() {
                        continue;
                    }
                    let all = Self::local_collections(&db);
                    let window = repair_window(&all, &mut repair_cursor, REPAIR_BATCH);
                    Self::reconcile_locally(&mut swarm, &topic, &db, &local_peer_id, &stats, &window, true);
                    if let Ok(mut s) = stats.lock() {
                        let now = now_rfc3339();
                        s.last_repair_at = Some(now.clone());
                        s.last_announce_at = Some(now);
                    }
                }

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
                                let still_known = swarm
                                    .behaviour()
                                    .mdns
                                    .as_ref()
                                    .map(|m| m.discovered_nodes().any(|id| id == &peer_id))
                                    .unwrap_or(false);
                                if !still_known {
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
                                    let author_id = author.to_string();
                                    // Only notify subscribers after an update was applied.
                                    match &msg {
                                        NetworkMessage::SyncUpdate { collection, update, epoch, .. } => {
                                            info!("Received sync update for collection: {}", collection);
                                            if !Self::apply_inbound(&db, collection, *epoch, update, &author_id, &stats, &gate, false) {
                                                continue;
                                            }
                                        }
                                        NetworkMessage::SyncRequest { collection, state_vector, requester_id, epoch } => {
                                            info!("Received sync request for collection: {}", collection);
                                            if gate.is_paused() {
                                                debug!("Synchronization paused; not answering request for {}", collection);
                                                continue;
                                            }
                                            let reply = match db.lock() {
                                                Ok(mut db_lock) => {
                                                    let local_epoch = db_lock.get_epoch(collection).unwrap_or(0);
                                                    if *epoch < local_epoch {
                                                        // The requester is behind a reset: tell it, then
                                                        // give it the whole post-reset state.
                                                        publish_message(&mut swarm.behaviour_mut().gossipsub, &topic, &NetworkMessage::CollectionReset {
                                                            collection: collection.clone(),
                                                            epoch: local_epoch,
                                                            origin_id: local_peer_id.to_string(),
                                                        }, &stats);
                                                        db_lock.get_full_state(collection).map(|update| (update, local_epoch))
                                                    } else if *epoch > local_epoch {
                                                        // We are behind: ask so the peer sends its reset + state.
                                                        match db_lock.get_state_vector(collection) {
                                                            Ok(sv) => {
                                                                publish_message(&mut swarm.behaviour_mut().gossipsub, &topic, &NetworkMessage::SyncRequest {
                                                                    collection: collection.clone(),
                                                                    state_vector: sv,
                                                                    requester_id: local_peer_id.to_string(),
                                                                    epoch: local_epoch,
                                                                }, &stats);
                                                            }
                                                            Err(e) => warn!("Cannot request newer epoch for {}: {}", collection, e),
                                                        }
                                                        continue;
                                                    } else {
                                                        db_lock.get_updates_since(collection, state_vector).map(|update| (update, local_epoch))
                                                    }
                                                }
                                                Err(e) => {
                                                    error!("DB lock poisoned, dropping sync request for {}: {}", collection, e);
                                                    continue;
                                                }
                                            };
                                            match reply {
                                                Ok((update, local_epoch)) => {
                                                    let response = NetworkMessage::SyncResponse {
                                                        collection: collection.clone(),
                                                        update,
                                                        requester_id: requester_id.clone(),
                                                        responder_id: local_peer_id.to_string(),
                                                        epoch: local_epoch,
                                                    };
                                                    publish_message(&mut swarm.behaviour_mut().gossipsub, &topic, &response, &stats);
                                                }
                                                Err(e) => {
                                                    warn!("Failed to prepare sync response for {}: {}", collection, e);
                                                    continue;
                                                }
                                            }
                                        }
                                        NetworkMessage::SyncResponse { collection, update, requester_id, epoch, .. } => {
                                            if requester_id != &local_peer_id.to_string() {
                                                continue;
                                            }
                                            info!("Received sync response for collection: {}", collection);
                                            if !Self::apply_inbound(&db, collection, *epoch, update, &author_id, &stats, &gate, true) {
                                                continue;
                                            }
                                        }
                                        NetworkMessage::PeerAnnounce { collections, .. } => {
                                            // Discover collections this node has never seen: an
                                            // unknown collection gets an empty state vector, which
                                            // asks the peer for everything (audit XD-02).
                                            if gate.is_paused() {
                                                continue;
                                            }
                                            let wanted: Vec<String> = collections.iter().take(MAX_COLLECTIONS_PER_PASS).cloned().collect();
                                            Self::reconcile_locally(&mut swarm, &topic, &db, &local_peer_id, &stats, &wanted, false);
                                            continue;
                                        }
                                        NetworkMessage::CollectionReset { collection, epoch, .. } => {
                                            let applied = match db.lock() {
                                                Ok(mut db_lock) => match db_lock.apply_remote_reset(collection, *epoch, &author_id) {
                                                    Ok(applied) => applied,
                                                    Err(e) => {
                                                        error!("Failed to apply reset for {}: {}", collection, e);
                                                        false
                                                    }
                                                },
                                                Err(e) => {
                                                    error!("DB lock poisoned, dropping reset for {}: {}", collection, e);
                                                    false
                                                }
                                            };
                                            if !applied {
                                                continue;
                                            }
                                            if let Ok(mut s) = stats.lock() {
                                                s.resets_applied += 1;
                                            }
                                            // Fetch the post-reset state from the origin.
                                            Self::reconcile_locally(&mut swarm, &topic, &db, &local_peer_id, &stats, std::slice::from_ref(collection), false);
                                        }
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
                                // Join reconciliation (audit XD-02): announce and request every
                                // local collection as soon as a NEW peer is connected.
                                let all = Self::local_collections(&db);
                                Self::reconcile_locally(&mut swarm, &topic, &db, &local_peer_id, &stats, &all, true);
                                if let Ok(mut s) = stats.lock() {
                                    s.last_announce_at = Some(now_rfc3339());
                                }
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
                            publish_message(&mut swarm.behaviour_mut().gossipsub, &topic, &message, &stats);
                        }
                        Some(NetworkCommand::Reconcile) => {
                            let all = Self::local_collections(&db);
                            Self::reconcile_locally(&mut swarm, &topic, &db, &local_peer_id, &stats, &all, true);
                            if let Ok(mut s) = stats.lock() {
                                s.last_announce_at = Some(now_rfc3339());
                            }
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

    pub fn options(&self) -> NetworkOptions {
        self.options
    }

    pub fn gate(&self) -> Arc<SyncGate> {
        self.gate.clone()
    }

    pub fn stats(&self) -> SyncStats {
        self.stats
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default()
    }

    pub async fn publish(&self, message: NetworkMessage) -> Result<(), String> {
        self.command_tx
            .send(NetworkCommand::Publish { message })
            .await
            .map_err(|e| e.to_string())
    }

    /// Queue an update for delivery. Success means the command was queued in
    /// this process — see `stats()` for whether any peer was subscribed.
    pub async fn broadcast_update(
        &self,
        collection: &str,
        epoch: u64,
        update: Vec<u8>,
    ) -> Result<(), String> {
        self.publish(NetworkMessage::SyncUpdate {
            collection: collection.to_string(),
            update,
            sender_id: self.local_peer_id.to_string(),
            epoch,
        })
        .await
    }

    pub async fn broadcast_reset(&self, collection: &str, epoch: u64) -> Result<(), String> {
        self.publish(NetworkMessage::CollectionReset {
            collection: collection.to_string(),
            epoch,
            origin_id: self.local_peer_id.to_string(),
        })
        .await
    }

    pub async fn request_sync(
        &self,
        collection: &str,
        epoch: u64,
        state_vector: Vec<u8>,
    ) -> Result<(), String> {
        self.publish(NetworkMessage::SyncRequest {
            collection: collection.to_string(),
            state_vector,
            requester_id: self.local_peer_id.to_string(),
            epoch,
        })
        .await
    }

    /// Announce and request reconciliation for every local collection now.
    pub async fn reconcile(&self) -> Result<(), String> {
        self.command_tx
            .send(NetworkCommand::Reconcile)
            .await
            .map_err(|e| e.to_string())
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
                epoch: 0,
            },
            NetworkMessage::SyncRequest {
                collection: "notes".into(),
                state_vector: vec![],
                requester_id: author.to_string(),
                epoch: 0,
            },
            NetworkMessage::SyncResponse {
                collection: "notes".into(),
                update: vec![],
                requester_id: relay.to_string(),
                responder_id: author.to_string(),
                epoch: 0,
            },
            NetworkMessage::PeerAnnounce {
                peer_id: author.to_string(),
                collections: vec![],
            },
            NetworkMessage::CollectionReset {
                collection: "notes".into(),
                epoch: 1,
                origin_id: author.to_string(),
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
            options: NetworkOptions::default(),
            stats: Arc::new(StdMutex::new(SyncStats::default())),
            gate: SyncGate::new(),
        };
        assert!(node.is_running());
        drop(command_rx);
        assert!(!node.is_running());
        assert!(node.broadcast_update("notes", 0, vec![]).await.is_err());
        assert!(node.reconcile().await.is_err());
    }

    #[test]
    fn network_options_default_to_local_only() {
        let options = NetworkOptions::default();
        assert!(!options.discovery);
        assert!(!options.listen);
        assert_eq!(
            NetworkOptions::trusted_lan(),
            NetworkOptions {
                discovery: true,
                listen: true
            }
        );
    }

    #[test]
    fn messages_from_peers_without_epochs_still_deserialize_as_epoch_zero() {
        // Wire compatibility with nodes built before reset epochs existed.
        let legacy = r#"{"SyncUpdate":{"collection":"notes","update":[1,2],"sender_id":"peer"}}"#;
        let message: NetworkMessage = serde_json::from_str(legacy).unwrap();
        match message {
            NetworkMessage::SyncUpdate { epoch, collection, .. } => {
                assert_eq!(epoch, 0);
                assert_eq!(collection, "notes");
            }
            other => panic!("unexpected message: {other:?}"),
        }
        let request = r#"{"SyncRequest":{"collection":"notes","state_vector":[],"requester_id":"peer"}}"#;
        assert!(matches!(
            serde_json::from_str::<NetworkMessage>(request).unwrap(),
            NetworkMessage::SyncRequest { epoch: 0, .. }
        ));
    }

    #[test]
    fn repair_window_walks_the_collection_set_in_bounded_slices() {
        let collections: Vec<String> = (0..7).map(|i| format!("c{i}")).collect();
        let mut cursor = 0;
        assert_eq!(
            repair_window(&collections, &mut cursor, 3),
            vec!["c0", "c1", "c2"]
        );
        assert_eq!(
            repair_window(&collections, &mut cursor, 3),
            vec!["c3", "c4", "c5"]
        );
        assert_eq!(
            repair_window(&collections, &mut cursor, 3),
            vec!["c6", "c0", "c1"]
        );
        assert!(repair_window(&[], &mut cursor, 3).is_empty());
        assert_eq!(cursor, 0);
        let mut cursor = 0;
        assert_eq!(repair_window(&collections, &mut cursor, 50).len(), 7);
    }

    #[test]
    fn sync_gate_pauses_and_resumes() {
        let gate = SyncGate::new();
        assert!(!gate.is_paused());
        gate.pause();
        assert!(gate.is_paused());
        gate.resume();
        assert!(!gate.is_paused());
    }

    #[test]
    fn repair_jitter_is_bounded_and_deterministic() {
        let peer = PeerId::random();
        assert_eq!(repair_jitter(&peer), repair_jitter(&peer));
        assert!(repair_jitter(&peer) < Duration::from_secs(5));
    }
}

#[cfg(test)]
#[path = "network_tests.rs"]
mod loopback_tests;
