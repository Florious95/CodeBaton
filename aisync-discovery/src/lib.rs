use std::collections::HashMap;
use std::fs;
use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aisync_core::{
    AisyncError, DeviceId, DeviceInfo, Discoverer, OsType, PeerChange, PeerChangeCallback,
    PeerChangeKind, Result,
};
use ed25519_dalek::SigningKey;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use rand_core::OsRng;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const AISYNC_SERVICE_TYPE: &str = "_aisync._tcp.local.";
pub const AISYNC_KEYRING_SERVICE: &str = "AISync";
pub const PROTOCOL_VERSION: u32 = 1;
pub const DEFAULT_OFFLINE_AFTER: Duration = Duration::from_secs(90);
const ED25519_PRIVATE_KEY_LEN: usize = 32;

#[derive(Debug, Clone)]
pub struct DiscoveryConfig {
    pub local_device: DeviceInfo,
    pub port: u16,
    pub receiver_cert_der: Option<Vec<u8>>,
    pub server_name: String,
    pub pairing_store_path: PathBuf,
    pub offline_after: Duration,
    pub poll_interval: Duration,
    pub tailscale_port: u16,
    pub probe_timeout: Duration,
}

impl DiscoveryConfig {
    pub fn new(device_name: impl Into<String>, port: u16) -> Self {
        Self {
            local_device: DeviceInfo {
                id: DeviceId::new(),
                name: device_name.into(),
                os: current_os(),
                addresses: Vec::new(),
                protocol_version: PROTOCOL_VERSION,
            },
            port,
            receiver_cert_der: None,
            server_name: "aisync-receiver".to_string(),
            pairing_store_path: default_pairing_store_path(),
            offline_after: DEFAULT_OFFLINE_AFTER,
            poll_interval: Duration::from_millis(250),
            tailscale_port: port,
            probe_timeout: Duration::from_millis(300),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerSource {
    Mdns,
    Tailscale,
    Manual,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairedPeer {
    pub device: DeviceInfo,
    pub public_key: String,
    pub paired_at_unix_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairRequest {
    pub peer: DeviceInfo,
    pub request_id: String,
    pub pairing_code: String,
    pub expires_at_unix_secs: u64,
    pub local_public_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairResult {
    pub peer: DeviceInfo,
    pub pairing_code: String,
    pub local_public_key: String,
    pub peer_public_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerConnectionInfo {
    pub endpoint: Option<SocketAddr>,
    pub receiver_cert_der: Option<Vec<u8>>,
    pub server_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalEd25519Identity {
    pub public_key: String,
}

pub trait SecretStore {
    fn get_secret(&self, key: &str) -> Result<Option<Vec<u8>>>;
    fn set_secret(&self, key: &str, secret: &[u8]) -> Result<()>;
    fn delete_secret(&self, key: &str) -> Result<()>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct KeyringSecretStore;

impl SecretStore for KeyringSecretStore {
    fn get_secret(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let entry = keyring_entry(key)?;
        match entry.get_secret() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(AisyncError::Discovery(format!("keyring get: {error}"))),
        }
    }

    fn set_secret(&self, key: &str, secret: &[u8]) -> Result<()> {
        keyring_entry(key)?
            .set_secret(secret)
            .map_err(|error| AisyncError::Discovery(format!("keyring set: {error}")))
    }

    fn delete_secret(&self, key: &str) -> Result<()> {
        let entry = keyring_entry(key)?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(AisyncError::Discovery(format!("keyring delete: {error}"))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailscaleCandidate {
    pub name: String,
    pub dns_name: Option<String>,
    pub addresses: Vec<IpAddr>,
}

#[derive(Debug, Clone)]
struct PeerRecord {
    device: DeviceInfo,
    source: PeerSource,
    service_fullname: Option<String>,
    connection: PeerConnectionInfo,
    last_seen: Instant,
    removed: bool,
}

#[derive(Default, Serialize, Deserialize)]
struct PairingFile {
    peers: Vec<PairedPeer>,
}

#[derive(Default)]
struct SharedState {
    local_device_id: DeviceId,
    peers: Mutex<HashMap<DeviceId, PeerRecord>>,
    service_names: Mutex<HashMap<String, DeviceId>>,
    callbacks: Mutex<Vec<PeerChangeCallback>>,
    paired_peers: Mutex<HashMap<DeviceId, PairedPeer>>,
}

pub struct MdnsDiscoverer {
    config: DiscoveryConfig,
    shared: Arc<SharedState>,
    mdns: Option<ServiceDaemon>,
    registered_fullname: Option<String>,
    stop_browser: Arc<AtomicBool>,
    browser_thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for MdnsDiscoverer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MdnsDiscoverer")
            .field("config", &self.config)
            .field("registered_fullname", &self.registered_fullname)
            .finish_non_exhaustive()
    }
}

impl MdnsDiscoverer {
    pub fn new(config: DiscoveryConfig) -> Result<Self> {
        let paired_peers = load_pairings(&config.pairing_store_path)?;
        let shared = Arc::new(SharedState {
            local_device_id: config.local_device.id,
            paired_peers: Mutex::new(paired_peers),
            ..SharedState::default()
        });

        Ok(Self {
            config,
            shared,
            mdns: None,
            registered_fullname: None,
            stop_browser: Arc::new(AtomicBool::new(false)),
            browser_thread: None,
        })
    }

    pub fn local_device(&self) -> &DeviceInfo {
        &self.config.local_device
    }

    pub fn set_local_device_name(&mut self, name: impl Into<String>) -> Result<()> {
        let name = name.into();
        if self.config.local_device.name == name {
            return Ok(());
        }

        let was_running = self.mdns.is_some();
        self.config.local_device.name = name;
        if was_running {
            self.stop();
            self.start()?;
        }
        Ok(())
    }

    pub fn stop(&mut self) {
        self.stop_browser.store(true, Ordering::Relaxed);

        if let Some(mdns) = self.mdns.take() {
            if let Some(fullname) = &self.registered_fullname {
                let _ = mdns.unregister(fullname);
            }
            let _ = mdns.shutdown();
        }

        if let Some(handle) = self.browser_thread.take() {
            let _ = handle.join();
        }
    }

    pub fn begin_pairing(
        &self,
        peer_id: &DeviceId,
        local_public_key: impl Into<String>,
    ) -> Result<PairRequest> {
        let peer = self
            .peer(peer_id)?
            .ok_or_else(|| AisyncError::Discovery("peer is not online".to_string()))?;

        log_discovery(
            "pair_request_opened",
            format!(
                "local_device_id={} peer_device_id={} peer_name={}",
                self.config.local_device.id.0, peer.id.0, peer.name
            ),
        );

        let request_id = new_pairing_request_id();
        Ok(PairRequest {
            pairing_code: derive_pairing_code_with_nonce(
                &self.config.local_device,
                &peer,
                &request_id,
            ),
            request_id,
            expires_at_unix_secs: unix_secs() + 120,
            peer,
            local_public_key: local_public_key.into(),
        })
    }

    pub fn begin_pairing_with_keyring(&self, peer_id: &DeviceId) -> Result<PairRequest> {
        self.begin_pairing_with_secret_store(peer_id, &KeyringSecretStore)
    }

    pub fn begin_pairing_with_secret_store(
        &self,
        peer_id: &DeviceId,
        store: &impl SecretStore,
    ) -> Result<PairRequest> {
        let identity = ensure_local_ed25519_identity_in_store(&self.config.local_device.id, store)?;
        self.begin_pairing(peer_id, identity.public_key)
    }

    pub fn confirm_pairing(
        &mut self,
        peer_id: &DeviceId,
        local_public_key: impl Into<String>,
        peer_public_key: impl Into<String>,
    ) -> Result<PairResult> {
        let peer = self
            .peer(peer_id)?
            .ok_or_else(|| AisyncError::Discovery("peer is not online".to_string()))?;
        let local_public_key = local_public_key.into();
        let peer_public_key = peer_public_key.into();
        let pairing_code = derive_pairing_code(&self.config.local_device, &peer);

        let paired = PairedPeer {
            device: peer.clone(),
            public_key: peer_public_key.clone(),
            paired_at_unix_secs: unix_secs(),
        };

        self.shared
            .paired_peers
            .lock()
            .expect("paired peer lock poisoned")
            .insert(*peer_id, paired);
        self.persist_pairings()?;
        emit(
            &self.shared,
            PeerChange {
                peer: peer.clone(),
                kind: PeerChangeKind::Paired,
            },
        );

        Ok(PairResult {
            peer,
            pairing_code,
            local_public_key,
            peer_public_key,
        })
    }

    pub fn confirm_pairing_with_keyring(
        &mut self,
        peer_id: &DeviceId,
        peer_public_key: impl Into<String>,
    ) -> Result<PairResult> {
        self.confirm_pairing_with_secret_store(peer_id, peer_public_key, &KeyringSecretStore)
    }

    pub fn confirm_pairing_with_secret_store(
        &mut self,
        peer_id: &DeviceId,
        peer_public_key: impl Into<String>,
        store: &impl SecretStore,
    ) -> Result<PairResult> {
        let identity = ensure_local_ed25519_identity_in_store(&self.config.local_device.id, store)?;
        self.confirm_pairing(peer_id, identity.public_key, peer_public_key)
    }

    pub fn unpair(&mut self, peer_id: &DeviceId) -> Result<()> {
        let removed = self
            .shared
            .paired_peers
            .lock()
            .expect("paired peer lock poisoned")
            .remove(peer_id);
        self.persist_pairings()?;

        if let Some(paired) = removed {
            emit(
                &self.shared,
                PeerChange {
                    peer: paired.device,
                    kind: PeerChangeKind::Unpaired,
                },
            );
        }

        Ok(())
    }

    pub fn paired_peers(&self) -> Vec<PairedPeer> {
        let mut peers: Vec<_> = self
            .shared
            .paired_peers
            .lock()
            .expect("paired peer lock poisoned")
            .values()
            .cloned()
            .collect();
        peers.sort_by(|left, right| left.device.name.cmp(&right.device.name));
        peers
    }

    pub fn peer_sources(&self) -> HashMap<DeviceId, PeerSource> {
        self.shared
            .peers
            .lock()
            .expect("peer lock poisoned")
            .iter()
            .map(|(peer_id, record)| (*peer_id, record.source.clone()))
            .collect()
    }

    pub fn discover_tailscale_peers(&self) -> Result<Vec<DeviceInfo>> {
        let output = match Command::new("tailscale")
            .arg("status")
            .arg("--json")
            .output()
        {
            Ok(output) => output,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(AisyncError::Discovery(error.to_string())),
        };

        if !output.status.success() {
            return Err(AisyncError::Discovery(format!(
                "tailscale status --json failed with status {}",
                output.status
            )));
        }

        let status = std::str::from_utf8(&output.stdout)
            .map_err(|error| AisyncError::Discovery(error.to_string()))?;
        let devices = devices_from_tailscale_status_json(
            status,
            self.config.tailscale_port,
            self.config.probe_timeout,
        )?;

        for device in &devices {
            let endpoint = device
                .addresses
                .first()
                .map(|address| SocketAddr::new(*address, self.config.tailscale_port));
            upsert_peer(
                &self.shared,
                device.clone(),
                PeerSource::Tailscale,
                None,
                PeerConnectionInfo {
                    endpoint,
                    receiver_cert_der: None,
                    server_name: None,
                },
                Instant::now(),
            );
        }

        Ok(devices)
    }

    pub fn discover_manual_peer(&self, address: SocketAddr) -> Result<DeviceInfo> {
        manual_device_from_socket_addr(address, self.config.probe_timeout).map(|device| {
            upsert_peer(
                &self.shared,
                device.clone(),
                PeerSource::Manual,
                None,
                PeerConnectionInfo {
                    endpoint: Some(address),
                    receiver_cert_der: None,
                    server_name: None,
                },
                Instant::now(),
            );
            device
        })
    }

    pub fn peer(&self, peer_id: &DeviceId) -> Result<Option<DeviceInfo>> {
        Ok(self
            .shared
            .peers
            .lock()
            .expect("peer lock poisoned")
            .get(peer_id)
            .map(|record| record.device.clone()))
    }

    pub fn peer_connection_info(&self, peer_id: &DeviceId) -> Result<Option<PeerConnectionInfo>> {
        Ok(self
            .shared
            .peers
            .lock()
            .expect("peer lock poisoned")
            .get(peer_id)
            .map(|record| record.connection.clone()))
    }

    fn persist_pairings(&self) -> Result<()> {
        let peers: Vec<_> = self
            .shared
            .paired_peers
            .lock()
            .expect("paired peer lock poisoned")
            .values()
            .cloned()
            .collect();
        save_pairings(&self.config.pairing_store_path, peers)
    }
}

impl Drop for MdnsDiscoverer {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Discoverer for MdnsDiscoverer {
    fn start(&mut self) -> Result<()> {
        if self.mdns.is_some() {
            return Ok(());
        }

        log_discovery(
            "discovery_start",
            format!(
                "device_id={} name={} port={}",
                self.config.local_device.id.0, self.config.local_device.name, self.config.port
            ),
        );

        self.stop_browser.store(false, Ordering::Relaxed);

        let mdns = ServiceDaemon::new()
            .map_err(|error| AisyncError::Discovery(format!("mDNS daemon: {error}")))?;
        let service_info = service_info_for(&self.config)?;
        let registered_fullname = service_info.get_fullname().to_string();

        mdns.register(service_info)
            .map_err(|error| AisyncError::Discovery(format!("mDNS register: {error}")))?;

        let receiver = mdns
            .browse(AISYNC_SERVICE_TYPE)
            .map_err(|error| AisyncError::Discovery(format!("mDNS browse: {error}")))?;

        let shared = Arc::clone(&self.shared);
        let stop = Arc::clone(&self.stop_browser);
        let offline_after = self.config.offline_after;
        let poll_interval = self.config.poll_interval;

        self.browser_thread = Some(thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match receiver.recv_timeout(poll_interval) {
                    Ok(ServiceEvent::ServiceFound(_, fullname)) => {
                        refresh_peer_last_seen_by_fullname(&shared, &fullname, Instant::now());
                    }
                    Ok(ServiceEvent::ServiceResolved(service)) => {
                        if let Some(resolved) = resolved_peer_from_service(&service) {
                            shared
                                .service_names
                                .lock()
                                .expect("service name lock poisoned")
                                .insert(service.get_fullname().to_string(), resolved.device.id);
                            upsert_peer(
                                &shared,
                                resolved.device,
                                PeerSource::Mdns,
                                Some(service.get_fullname().to_string()),
                                resolved.connection,
                                Instant::now(),
                            );
                        }
                    }
                    Ok(ServiceEvent::ServiceRemoved(_, fullname)) => {
                        mark_peer_removed_by_fullname(
                            &shared,
                            &fullname,
                            Instant::now(),
                            offline_after,
                        );
                    }
                    Ok(_) => {}
                    Err(_) => {}
                }

                prune_stale_peers(&shared, offline_after, Instant::now());
            }
        }));

        self.registered_fullname = Some(registered_fullname);
        self.mdns = Some(mdns);
        Ok(())
    }

    fn peers(&self) -> Result<Vec<DeviceInfo>> {
        let mut peers = refresh_peers_for_query(&self.shared, Instant::now());
        peers.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(peers)
    }

    fn on_peer_change(&mut self, callback: PeerChangeCallback) -> Result<()> {
        self.shared
            .callbacks
            .lock()
            .expect("callback lock poisoned")
            .push(callback);
        Ok(())
    }
}

pub fn ensure_local_ed25519_identity(device_id: &DeviceId) -> Result<LocalEd25519Identity> {
    ensure_local_ed25519_identity_in_store(device_id, &KeyringSecretStore)
}

pub fn rotate_local_ed25519_identity(device_id: &DeviceId) -> Result<LocalEd25519Identity> {
    rotate_local_ed25519_identity_in_store(device_id, &KeyringSecretStore)
}

pub fn ensure_local_ed25519_identity_in_store(
    device_id: &DeviceId,
    store: &impl SecretStore,
) -> Result<LocalEd25519Identity> {
    let key = ed25519_keyring_key(device_id);
    match store.get_secret(&key)? {
        Some(secret) => identity_from_private_key(&secret),
        None => {
            let mut rng = OsRng;
            let signing_key = SigningKey::generate(&mut rng);
            let private_key = signing_key.to_bytes();
            store.set_secret(&key, &private_key)?;
            Ok(identity_from_signing_key(&signing_key))
        }
    }
}

pub fn rotate_local_ed25519_identity_in_store(
    device_id: &DeviceId,
    store: &impl SecretStore,
) -> Result<LocalEd25519Identity> {
    let key = ed25519_keyring_key(device_id);
    store.delete_secret(&key)?;
    ensure_local_ed25519_identity_in_store(device_id, store)
}

#[derive(Debug, Default)]
pub struct NoopDiscoverer {
    peers: Vec<DeviceInfo>,
}

impl NoopDiscoverer {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Discoverer for NoopDiscoverer {
    fn start(&mut self) -> Result<()> {
        Ok(())
    }

    fn peers(&self) -> Result<Vec<DeviceInfo>> {
        Ok(self.peers.clone())
    }

    fn on_peer_change(&mut self, _callback: PeerChangeCallback) -> Result<()> {
        Ok(())
    }
}

pub fn derive_pairing_code(left: &DeviceInfo, right: &DeviceInfo) -> String {
    derive_pairing_code_from_parts(left, right, None)
}

pub fn derive_pairing_code_with_nonce(
    left: &DeviceInfo,
    right: &DeviceInfo,
    nonce: &str,
) -> String {
    derive_pairing_code_from_parts(left, right, Some(nonce))
}

pub fn new_pairing_request_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{nanos:x}")
}

fn derive_pairing_code_from_parts(
    left: &DeviceInfo,
    right: &DeviceInfo,
    nonce: Option<&str>,
) -> String {
    let mut ids = [left.id.0.to_string(), right.id.0.to_string()];
    ids.sort();
    let input = format!(
        "aisync-pairing-v{PROTOCOL_VERSION}:{}:{}:{}",
        ids[0],
        ids[1],
        nonce.unwrap_or("")
    );
    let hash = blake3::hash(input.as_bytes());
    let mut bytes = [0_u8; 4];
    bytes.copy_from_slice(&hash.as_bytes()[..4]);
    let code = u32::from_be_bytes(bytes) % 1_000_000;
    format!("{code:06}")
}

fn keyring_entry(key: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(AISYNC_KEYRING_SERVICE, key)
        .map_err(|error| AisyncError::Discovery(format!("keyring entry: {error}")))
}

fn ed25519_keyring_key(device_id: &DeviceId) -> String {
    format!("device:{}:ed25519", device_id.0)
}

fn identity_from_private_key(secret: &[u8]) -> Result<LocalEd25519Identity> {
    let private_key: [u8; ED25519_PRIVATE_KEY_LEN] = secret.try_into().map_err(|_| {
        AisyncError::Discovery(format!(
            "stored Ed25519 key has invalid length: expected {ED25519_PRIVATE_KEY_LEN}, got {}",
            secret.len()
        ))
    })?;
    Ok(identity_from_signing_key(&SigningKey::from_bytes(
        &private_key,
    )))
}

fn identity_from_signing_key(signing_key: &SigningKey) -> LocalEd25519Identity {
    LocalEd25519Identity {
        public_key: hex_encode(&signing_key.verifying_key().to_bytes()),
    }
}

pub fn tailscale_candidates_from_status_json(status: &str) -> Result<Vec<TailscaleCandidate>> {
    let value: serde_json::Value = serde_json::from_str(status)
        .map_err(|error| AisyncError::Discovery(format!("tailscale JSON: {error}")))?;
    let peers = value
        .get("Peer")
        .and_then(|peer| peer.as_object())
        .ok_or_else(|| AisyncError::Discovery("tailscale JSON missing Peer".to_string()))?;

    let mut candidates = Vec::new();
    for peer in peers.values() {
        if peer
            .get("Online")
            .and_then(|online| online.as_bool())
            .is_some_and(|online| !online)
        {
            continue;
        }

        let addresses: Vec<IpAddr> = peer
            .get("TailscaleIPs")
            .and_then(|ips| ips.as_array())
            .into_iter()
            .flatten()
            .filter_map(|ip| ip.as_str())
            .filter_map(|ip| ip.parse().ok())
            .collect();

        if addresses.is_empty() {
            continue;
        }

        let dns_name = peer
            .get("DNSName")
            .and_then(|name| name.as_str())
            .map(trim_trailing_dot)
            .filter(|name| !name.is_empty());
        let host_name = peer
            .get("HostName")
            .and_then(|name| name.as_str())
            .filter(|name| !name.is_empty())
            .map(ToOwned::to_owned);

        let name = host_name
            .or_else(|| dns_name.clone())
            .unwrap_or_else(|| addresses[0].to_string());

        candidates.push(TailscaleCandidate {
            name,
            dns_name,
            addresses,
        });
    }

    Ok(candidates)
}

pub fn devices_from_tailscale_status_json(
    status: &str,
    aisync_port: u16,
    timeout: Duration,
) -> Result<Vec<DeviceInfo>> {
    let candidates = tailscale_candidates_from_status_json(status)?;
    let mut devices = Vec::new();

    for candidate in candidates {
        let reachable: Vec<IpAddr> = candidate
            .addresses
            .iter()
            .copied()
            .filter(|address| probe_aisync_port(*address, aisync_port, timeout))
            .collect();

        if reachable.is_empty() {
            continue;
        }

        let id_seed = format!(
            "tailscale:{}:{}",
            candidate
                .dns_name
                .as_deref()
                .unwrap_or(candidate.name.as_str()),
            reachable[0]
        );
        devices.push(DeviceInfo {
            id: deterministic_device_id(&id_seed),
            name: candidate.name,
            os: OsType::Other("tailscale".to_string()),
            addresses: reachable,
            protocol_version: PROTOCOL_VERSION,
        });
    }

    Ok(devices)
}

pub fn manual_device_from_socket_addr(
    address: SocketAddr,
    timeout: Duration,
) -> Result<DeviceInfo> {
    if !probe_aisync_port(address.ip(), address.port(), timeout) {
        return Err(AisyncError::Discovery(format!(
            "AISync peer is not reachable at {address}"
        )));
    }

    Ok(DeviceInfo {
        id: deterministic_device_id(&format!("manual:{address}")),
        name: format!("manual-{address}"),
        os: OsType::Other("manual".to_string()),
        addresses: vec![address.ip()],
        protocol_version: PROTOCOL_VERSION,
    })
}

fn service_info_for(config: &DiscoveryConfig) -> Result<ServiceInfo> {
    let instance_name = format!(
        "{}-{}",
        sanitize_instance_name(&config.local_device.name),
        short_device_id(&config.local_device.id)
    );
    let host_name = format!("{}.local.", dns_label(&instance_name));
    let mut properties = HashMap::from([
        ("device_name".to_string(), config.local_device.name.clone()),
        (
            "device_id".to_string(),
            config.local_device.id.0.to_string(),
        ),
        (
            "os".to_string(),
            os_to_str(&config.local_device.os).to_string(),
        ),
        (
            "version".to_string(),
            config.local_device.protocol_version.to_string(),
        ),
        ("server_name".to_string(), config.server_name.clone()),
    ]);
    if let Some(cert) = &config.receiver_cert_der {
        insert_receiver_cert_properties(&mut properties, cert);
    }
    let tailscale_fallback = if config
        .local_device
        .addresses
        .iter()
        .any(|address| matches!(address, IpAddr::V4(_)))
    {
        None
    } else {
        local_tailscale_ip()
    };
    let endpoint_ip = preferred_endpoint_ip(
        config.local_device.addresses.iter().copied(),
        tailscale_fallback,
    );
    if let Some(endpoint_ip) = endpoint_ip {
        properties.insert("endpoint_ip".to_string(), endpoint_ip.to_string());
    }

    let service_ip = endpoint_ip.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    ServiceInfo::new(
        AISYNC_SERVICE_TYPE,
        &instance_name,
        &host_name,
        service_ip,
        config.port,
        properties,
    )
    .map(|info| info.enable_addr_auto())
    .map_err(|error| AisyncError::Discovery(format!("mDNS service info: {error}")))
}

struct ResolvedPeer {
    device: DeviceInfo,
    connection: PeerConnectionInfo,
}

fn resolved_peer_from_service(service: &ServiceInfo) -> Option<ResolvedPeer> {
    let device_id = service.get_property_val_str("device_id")?;
    let device_id = Uuid::parse_str(device_id).ok()?;
    let name = service
        .get_property_val_str("device_name")
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| service.get_fullname().to_string());
    let os = service
        .get_property_val_str("os")
        .map(str_to_os)
        .unwrap_or_else(|| OsType::Other("unknown".to_string()));
    let protocol_version = service
        .get_property_val_str("version")
        .and_then(|version| version.parse().ok())
        .unwrap_or(PROTOCOL_VERSION);
    let endpoint_property = service
        .get_property_val_str("endpoint_ip")
        .and_then(|value| value.parse::<IpAddr>().ok());
    let addresses = service.get_addresses().iter().copied().collect::<Vec<_>>();
    let endpoint = preferred_endpoint_ip(
        addresses
            .iter()
            .copied()
            .chain(endpoint_property.into_iter()),
        None,
    )
    .map(|address| SocketAddr::new(address, service.get_port()));

    Some(ResolvedPeer {
        device: DeviceInfo {
            id: DeviceId(device_id),
            name,
            os,
            addresses,
            protocol_version,
        },
        connection: PeerConnectionInfo {
            endpoint,
            receiver_cert_der: receiver_cert_from_service(service),
            server_name: service
                .get_property_val_str("server_name")
                .map(ToOwned::to_owned),
        },
    })
}

fn preferred_endpoint_ip(
    addresses: impl IntoIterator<Item = IpAddr>,
    tailscale_fallback: Option<IpAddr>,
) -> Option<IpAddr> {
    let mut addresses: Vec<IpAddr> = addresses.into_iter().collect();
    if let Some(tailscale) = tailscale_fallback {
        if !addresses.contains(&tailscale) {
            addresses.push(tailscale);
        }
    }

    addresses
        .iter()
        .copied()
        .find(|address| matches!(address, IpAddr::V4(ip) if is_primary_ipv4(*ip)))
        .or_else(|| {
            addresses
                .iter()
                .copied()
                .find(|address| matches!(address, IpAddr::V4(ip) if is_tailscale_ipv4(*ip)))
        })
        .or_else(|| {
            addresses
                .iter()
                .copied()
                .find(|address| matches!(address, IpAddr::V4(_)))
        })
        .or_else(|| {
            addresses
                .iter()
                .copied()
                .find(|address| matches!(address, IpAddr::V6(ip) if !ip.is_unicast_link_local()))
        })
        .or_else(|| {
            addresses
                .into_iter()
                .find(|address| matches!(address, IpAddr::V6(_)))
        })
}

fn is_primary_ipv4(ip: Ipv4Addr) -> bool {
    !ip.is_unspecified() && !ip.is_loopback() && !ip.is_link_local() && !is_tailscale_ipv4(ip)
}

fn is_tailscale_ipv4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    octets[0] == 100 && (64..=127).contains(&octets[1])
}

fn local_tailscale_ip() -> Option<IpAddr> {
    let output = Command::new("tailscale")
        .arg("ip")
        .arg("-4")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = std::str::from_utf8(&output.stdout).ok()?;
    stdout
        .lines()
        .find_map(|line| line.trim().parse::<Ipv4Addr>().ok())
        .map(IpAddr::V4)
}

fn insert_receiver_cert_properties(properties: &mut HashMap<String, String>, cert: &[u8]) {
    const CHUNK_HEX_LEN: usize = 180;
    let cert_hex = hex_encode(cert);
    for (index, chunk) in cert_hex.as_bytes().chunks(CHUNK_HEX_LEN).enumerate() {
        let value = String::from_utf8_lossy(chunk).into_owned();
        properties.insert(format!("receiver_cert_{index}"), value);
    }
}

fn receiver_cert_from_service(service: &ServiceInfo) -> Option<Vec<u8>> {
    let mut chunks = Vec::new();
    for prop in service.get_properties().iter() {
        let Some(index) = prop.key().strip_prefix("receiver_cert_") else {
            continue;
        };
        let index = index.parse::<usize>().ok()?;
        chunks.push((index, prop.val_str().to_string()));
    }
    if chunks.is_empty() {
        return None;
    }

    chunks.sort_by_key(|(index, _)| *index);
    let mut cert_hex = String::new();
    for (_, chunk) in chunks {
        cert_hex.push_str(&chunk);
    }
    hex_decode(&cert_hex)
}

fn upsert_peer(
    shared: &SharedState,
    device: DeviceInfo,
    source: PeerSource,
    service_fullname: Option<String>,
    connection: PeerConnectionInfo,
    seen_at: Instant,
) {
    log_discovery(
        "peer_seen",
        format!(
            "device_id={} name={} source={:?} endpoint={}",
            device.id.0,
            device.name,
            source,
            endpoint_for_log(&connection)
        ),
    );

    if device.id == shared.local_device_id {
        log_discovery(
            "peer_filtered_self",
            format!(
                "device_id={} name={} source={:?} endpoint={}",
                device.id.0,
                device.name,
                source,
                endpoint_for_log(&connection)
            ),
        );
        return;
    }

    let event = {
        let mut peers = shared.peers.lock().expect("peer lock poisoned");
        let existing_by_endpoint = connection.endpoint.and_then(|endpoint| {
            peers
                .iter()
                .find(|(_, record)| record.connection.endpoint == Some(endpoint))
                .map(|(peer_id, _)| *peer_id)
        });
        let merge_id = existing_by_endpoint.unwrap_or(device.id);
        let old = peers.remove(&merge_id);
        let kind = match &old {
            Some(existing) if existing.device == device && existing.connection == connection => {
                None
            }
            Some(_) => Some(PeerChangeKind::Updated),
            None => Some(PeerChangeKind::Discovered),
        };
        if let Some(existing) = &old {
            log_discovery(
                "peer_merged",
                format!(
                    "old_device_id={} new_device_id={} name={} source={:?} endpoint={}",
                    existing.device.id.0,
                    device.id.0,
                    device.name,
                    source,
                    endpoint_for_log(&connection)
                ),
            );
        }

        peers.insert(
            device.id,
            PeerRecord {
                device: device.clone(),
                source,
                service_fullname,
                connection,
                last_seen: seen_at,
                removed: false,
            },
        );

        kind.map(|kind| PeerChange { peer: device, kind })
    };

    if let Some(event) = event {
        emit(shared, event);
    }
}

fn refresh_peer_last_seen_by_fullname(
    shared: &SharedState,
    fullname: &str,
    seen_at: Instant,
) -> bool {
    let mapped_peer_id = shared
        .service_names
        .lock()
        .expect("service name lock poisoned")
        .get(fullname)
        .copied();

    let refreshed = {
        let mut peers = shared.peers.lock().expect("peer lock poisoned");
        let mut refreshed = None;
        if let Some(peer_id) = mapped_peer_id {
            if let Some(record) = peers.get_mut(&peer_id) {
                record.last_seen = seen_at;
                record.removed = false;
                refreshed = Some((
                    peer_id,
                    record.device.name.clone(),
                    endpoint_for_log(&record.connection),
                ));
            }
        }
        if refreshed.is_none() {
            for (peer_id, record) in peers.iter_mut() {
                if record.service_fullname.as_deref() == Some(fullname) {
                    record.last_seen = seen_at;
                    record.removed = false;
                    refreshed = Some((
                        *peer_id,
                        record.device.name.clone(),
                        endpoint_for_log(&record.connection),
                    ));
                    break;
                }
            }
        }
        refreshed
    };

    if let Some((peer_id, name, endpoint)) = refreshed {
        shared
            .service_names
            .lock()
            .expect("service name lock poisoned")
            .insert(fullname.to_string(), peer_id);
        log_discovery(
            "peer_seen_refreshed",
            format!(
                "device_id={} name={} fullname={} endpoint={}",
                peer_id.0, name, fullname, endpoint
            ),
        );
        true
    } else {
        log_discovery("peer_seen_unmapped", format!("fullname={fullname}"));
        false
    }
}

fn refresh_peers_for_query(shared: &SharedState, seen_at: Instant) -> Vec<DeviceInfo> {
    let mut peers = shared.peers.lock().expect("peer lock poisoned");
    let mut refreshed = 0usize;
    let mut recovered = 0usize;
    let devices = peers
        .values_mut()
        .map(|record| {
            if record.removed {
                recovered += 1;
                record.removed = false;
            }
            record.last_seen = seen_at;
            refreshed += 1;
            record.device.clone()
        })
        .collect::<Vec<_>>();
    if refreshed > 0 {
        log_discovery(
            "peer_query_refreshed",
            format!(
                "count={refreshed} recovered_removed={recovered} total={}",
                devices.len()
            ),
        );
    }
    devices
}

fn mark_peer_removed_by_fullname(
    shared: &SharedState,
    fullname: &str,
    seen_at: Instant,
    offline_after: Duration,
) -> bool {
    let mapped_peer_id = shared
        .service_names
        .lock()
        .expect("service name lock poisoned")
        .get(fullname)
        .copied();
    let marked = {
        let mut peers = shared.peers.lock().expect("peer lock poisoned");
        let mut marked = None;
        if let Some(peer_id) = mapped_peer_id {
            if let Some(record) = peers.get_mut(&peer_id) {
                record.last_seen = seen_at;
                record.removed = true;
                marked = Some((
                    peer_id,
                    record.device.name.clone(),
                    endpoint_for_log(&record.connection),
                ));
            }
        }
        if marked.is_none() {
            for (peer_id, record) in peers.iter_mut() {
                if record.service_fullname.as_deref() == Some(fullname) {
                    record.last_seen = seen_at;
                    record.removed = true;
                    marked = Some((
                        *peer_id,
                        record.device.name.clone(),
                        endpoint_for_log(&record.connection),
                    ));
                    break;
                }
            }
        }
        marked
    };

    if let Some((peer_id, name, endpoint)) = marked {
        log_discovery(
            "peer_remove_deferred",
            format!(
                "device_id={} name={} fullname={} endpoint={} offline_after_secs={}",
                peer_id.0,
                name,
                fullname,
                endpoint,
                offline_after.as_secs()
            ),
        );
        true
    } else {
        log_discovery(
            "peer_remove_unmapped",
            format!(
                "fullname={} offline_after_secs={}",
                fullname,
                offline_after.as_secs()
            ),
        );
        false
    }
}

fn endpoint_for_log(connection: &PeerConnectionInfo) -> String {
    connection
        .endpoint
        .map(|endpoint| endpoint.to_string())
        .unwrap_or_else(|| "-".to_string())
}

fn log_discovery(event: &str, detail: impl std::fmt::Display) {
    let line = format!("[aisync-discovery] event={event} {detail}");
    eprintln!("{line}");
    if let Some(path) = discovery_log_file_path() {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
            use std::io::Write;
            let _ = writeln!(file, "{} {}", log_timestamp(), line);
        }
    }
}

fn discovery_log_file_path() -> Option<PathBuf> {
    std::env::var_os("AISYNC_LOG_FILE")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".aisync").join("logs").join("aisync.log"))
        })
}

fn log_timestamp() -> String {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => format!("[t={}]", duration.as_secs()),
        Err(_) => "[t=?]".to_string(),
    }
}

fn remove_peer(shared: &SharedState, peer_id: &DeviceId) {
    let removed = shared
        .peers
        .lock()
        .expect("peer lock poisoned")
        .remove(peer_id);

    if let Some(record) = removed {
        if let Some(fullname) = record.service_fullname {
            shared
                .service_names
                .lock()
                .expect("service name lock poisoned")
                .remove(&fullname);
        }

        emit(
            shared,
            PeerChange {
                peer: record.device,
                kind: PeerChangeKind::Lost,
            },
        );
    }
}

fn prune_stale_peers(shared: &SharedState, offline_after: Duration, now: Instant) {
    let stale_ids = {
        let peers = shared.peers.lock().expect("peer lock poisoned");
        peers
            .iter()
            .filter(|(_, record)| now.duration_since(record.last_seen) > offline_after)
            .map(|(id, _)| *id)
            .collect::<Vec<_>>()
    };

    for peer_id in stale_ids {
        remove_peer(shared, &peer_id);
    }
}

fn emit(shared: &SharedState, change: PeerChange) {
    let callbacks = shared.callbacks.lock().expect("callback lock poisoned");
    for callback in callbacks.iter() {
        callback(change.clone());
    }
}

fn probe_aisync_port(address: IpAddr, port: u16, timeout: Duration) -> bool {
    TcpStream::connect_timeout(&SocketAddr::new(address, port), timeout).is_ok()
}

fn load_pairings(path: &Path) -> Result<HashMap<DeviceId, PairedPeer>> {
    match fs::read(path) {
        Ok(bytes) => {
            let file: PairingFile = serde_json::from_slice(&bytes)
                .map_err(|error| AisyncError::Discovery(format!("pairing store: {error}")))?;
            Ok(file
                .peers
                .into_iter()
                .map(|peer| (peer.device.id, peer))
                .collect())
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(HashMap::new()),
        Err(error) => Err(error.into()),
    }
}

fn save_pairings(path: &Path, peers: Vec<PairedPeer>) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let tmp_path = path.with_extension("tmp");
    let bytes = serde_json::to_vec_pretty(&PairingFile { peers })
        .map_err(|error| AisyncError::Discovery(format!("pairing store: {error}")))?;
    fs::write(&tmp_path, bytes)?;

    if cfg!(windows) && path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(tmp_path, path)?;
    Ok(())
}

fn deterministic_device_id(seed: &str) -> DeviceId {
    let hash = blake3::hash(seed.as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&hash.as_bytes()[..16]);
    DeviceId(Uuid::from_bytes(bytes))
}

fn current_os() -> OsType {
    if cfg!(target_os = "macos") {
        OsType::Darwin
    } else if cfg!(target_os = "windows") {
        OsType::Windows
    } else if cfg!(target_os = "linux") {
        OsType::Linux
    } else {
        OsType::Other(std::env::consts::OS.to_string())
    }
}

fn default_pairing_store_path() -> PathBuf {
    if cfg!(windows) {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("AISync")
            .join("paired_peers.json")
    } else {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".aisync")
            .join("paired_peers.json")
    }
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn short_device_id(id: &DeviceId) -> String {
    id.0.to_string().chars().take(8).collect()
}

fn sanitize_instance_name(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect();
    sanitized.trim_matches('-').chars().take(40).collect()
}

fn dns_label(value: &str) -> String {
    let label: String = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let label = label.trim_matches('-');
    if label.is_empty() {
        "aisync".to_string()
    } else {
        label.chars().take(63).collect()
    }
}

fn os_to_str(os: &OsType) -> &str {
    match os {
        OsType::Darwin => "darwin",
        OsType::Windows => "windows",
        OsType::Linux => "linux",
        OsType::Other(value) => value.as_str(),
    }
}

fn str_to_os(value: &str) -> OsType {
    match value {
        "darwin" => OsType::Darwin,
        "windows" => OsType::Windows,
        "linux" => OsType::Linux,
        other => OsType::Other(other.to_string()),
    }
}

fn trim_trailing_dot(value: &str) -> String {
    value.trim_end_matches('.').to_string()
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn hex_decode(value: &str) -> Option<Vec<u8>> {
    if value.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(value.len() / 2);
    for chunk in value.as_bytes().chunks_exact(2) {
        let high = hex_nibble(chunk[0])?;
        let low = hex_nibble(chunk[1])?;
        out.push((high << 4) | low);
    }
    Some(out)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    use std::sync::mpsc;

    use super::*;

    fn device(name: &str) -> DeviceInfo {
        DeviceInfo {
            id: DeviceId::new(),
            name: name.to_string(),
            os: OsType::Darwin,
            addresses: vec!["127.0.0.1".parse().unwrap()],
            protocol_version: PROTOCOL_VERSION,
        }
    }

    fn test_connection_info() -> PeerConnectionInfo {
        PeerConnectionInfo {
            endpoint: None,
            receiver_cert_der: None,
            server_name: None,
        }
    }

    #[derive(Default)]
    struct MemorySecretStore {
        secrets: Mutex<HashMap<String, Vec<u8>>>,
    }

    impl SecretStore for MemorySecretStore {
        fn get_secret(&self, key: &str) -> Result<Option<Vec<u8>>> {
            Ok(self.secrets.lock().unwrap().get(key).map(ToOwned::to_owned))
        }

        fn set_secret(&self, key: &str, secret: &[u8]) -> Result<()> {
            self.secrets
                .lock()
                .unwrap()
                .insert(key.to_string(), secret.to_vec());
            Ok(())
        }

        fn delete_secret(&self, key: &str) -> Result<()> {
            self.secrets.lock().unwrap().remove(key);
            Ok(())
        }
    }

    #[test]
    fn pairing_code_is_six_digits_and_order_independent() {
        let left = device("left");
        let right = device("right");

        let first = derive_pairing_code(&left, &right);
        let second = derive_pairing_code(&right, &left);

        assert_eq!(first, second);
        assert_eq!(first.len(), 6);
        assert!(first.chars().all(|ch| ch.is_ascii_digit()));
    }

    #[test]
    fn pairing_code_with_nonce_changes_per_request_id() {
        let left = device("left");
        let right = device("right");

        let first = derive_pairing_code_with_nonce(&left, &right, "request-a");
        let second = derive_pairing_code_with_nonce(&right, &left, "request-b");

        assert_ne!(first, second);
        assert_eq!(first.len(), 6);
        assert_eq!(second.len(), 6);
        assert!(first.chars().all(|ch| ch.is_ascii_digit()));
        assert!(second.chars().all(|ch| ch.is_ascii_digit()));
    }

    #[test]
    fn mdns_service_round_trips_receiver_connection_info() {
        let peer = device("peer");
        let cert: Vec<u8> = (0..140).map(|value| value as u8).collect();
        let mut properties = HashMap::from([
            ("device_name".to_string(), peer.name.clone()),
            ("device_id".to_string(), peer.id.0.to_string()),
            ("os".to_string(), "darwin".to_string()),
            ("version".to_string(), "1".to_string()),
            ("server_name".to_string(), "aisync-receiver".to_string()),
        ]);
        insert_receiver_cert_properties(&mut properties, &cert);
        let service = ServiceInfo::new(
            AISYNC_SERVICE_TYPE,
            "peer-test",
            "peer-test.local.",
            "127.0.0.1",
            52017,
            properties,
        )
        .unwrap();

        let resolved = resolved_peer_from_service(&service).unwrap();

        assert_eq!(resolved.device.id, peer.id);
        assert_eq!(
            resolved.connection.endpoint,
            Some("127.0.0.1:52017".parse().unwrap())
        );
        assert_eq!(resolved.connection.receiver_cert_der, Some(cert));
        assert_eq!(
            resolved.connection.server_name,
            Some("aisync-receiver".to_string())
        );
    }

    #[test]
    fn mdns_endpoint_prefers_ipv4_over_ipv6_link_local() {
        let peer = device("peer");
        let service = ServiceInfo::new(
            AISYNC_SERVICE_TYPE,
            "peer-test",
            "peer-test.local.",
            "fe80::4e64:593:ae58:52f9,192.168.50.23",
            52000,
            HashMap::from([
                ("device_name".to_string(), peer.name.clone()),
                ("device_id".to_string(), peer.id.0.to_string()),
                ("os".to_string(), "darwin".to_string()),
                ("version".to_string(), "1".to_string()),
            ]),
        )
        .unwrap();

        let resolved = resolved_peer_from_service(&service).unwrap();

        assert_eq!(
            resolved.connection.endpoint,
            Some("192.168.50.23:52000".parse().unwrap())
        );
    }

    #[test]
    fn endpoint_priority_uses_tailscale_before_ipv6_link_local() {
        let selected = preferred_endpoint_ip(
            [
                "fe80::4e64:593:ae58:52f9".parse::<IpAddr>().unwrap(),
                "100.75.207.88".parse::<IpAddr>().unwrap(),
            ],
            None,
        );

        assert_eq!(selected, Some("100.75.207.88".parse().unwrap()));
    }

    #[test]
    fn ed25519_identity_is_generated_and_reused_from_secret_store() {
        let device_id = DeviceId::new();
        let store = MemorySecretStore::default();

        let first = ensure_local_ed25519_identity_in_store(&device_id, &store).unwrap();
        let second = ensure_local_ed25519_identity_in_store(&device_id, &store).unwrap();

        assert_eq!(first, second);
        assert_eq!(first.public_key.len(), 64);
        let stored = store
            .secrets
            .lock()
            .unwrap()
            .get(&ed25519_keyring_key(&device_id))
            .cloned()
            .unwrap();
        assert_eq!(stored.len(), ED25519_PRIVATE_KEY_LEN);
    }

    #[test]
    fn keyring_pairing_flow_uses_generated_public_key() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = DiscoveryConfig::new("local", 47000);
        config.pairing_store_path = dir.path().join("paired.json");
        let store = MemorySecretStore::default();
        let peer = device("peer");
        let peer_id = peer.id;
        let mut discoverer = MdnsDiscoverer::new(config).unwrap();
        upsert_peer(
            &discoverer.shared,
            peer,
            PeerSource::Mdns,
            Some("peer._aisync._tcp.local.".to_string()),
            test_connection_info(),
            Instant::now(),
        );

        let request = discoverer
            .begin_pairing_with_secret_store(&peer_id, &store)
            .unwrap();
        let result = discoverer
            .confirm_pairing_with_secret_store(&peer_id, "peer-public", &store)
            .unwrap();

        assert_eq!(request.local_public_key, result.local_public_key);
        assert_eq!(result.peer_public_key, "peer-public");
        assert_eq!(discoverer.paired_peers()[0].public_key, "peer-public");
    }

    #[test]
    fn pairing_is_persisted_and_reloaded() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = DiscoveryConfig::new("local", 47000);
        config.pairing_store_path = dir.path().join("paired.json");

        let peer = device("peer");
        let peer_id = peer.id;
        let mut discoverer = MdnsDiscoverer::new(config.clone()).unwrap();
        upsert_peer(
            &discoverer.shared,
            peer,
            PeerSource::Mdns,
            Some("peer._aisync._tcp.local.".to_string()),
            test_connection_info(),
            Instant::now(),
        );

        let result = discoverer
            .confirm_pairing(&peer_id, "local-public", "peer-public")
            .unwrap();
        assert_eq!(result.peer_public_key, "peer-public");

        let reloaded = MdnsDiscoverer::new(config).unwrap();
        let paired = reloaded.paired_peers();
        assert_eq!(paired.len(), 1);
        assert_eq!(paired[0].public_key, "peer-public");
    }

    #[test]
    fn stale_peer_is_removed_and_emits_lost_event() {
        let mut config = DiscoveryConfig::new("local", 47000);
        config.offline_after = Duration::from_millis(10);
        let mut discoverer = MdnsDiscoverer::new(config).unwrap();
        let (tx, rx) = mpsc::channel();
        discoverer
            .on_peer_change(Box::new(move |change| {
                tx.send(change.kind).unwrap();
            }))
            .unwrap();

        let peer = device("peer");
        let peer_id = peer.id;
        upsert_peer(
            &discoverer.shared,
            peer,
            PeerSource::Mdns,
            None,
            test_connection_info(),
            Instant::now() - Duration::from_secs(1),
        );
        prune_stale_peers(
            &discoverer.shared,
            Duration::from_millis(10),
            Instant::now(),
        );

        assert!(discoverer.peer(&peer_id).unwrap().is_none());
        assert_eq!(rx.recv().unwrap(), PeerChangeKind::Discovered);
        assert_eq!(rx.recv().unwrap(), PeerChangeKind::Lost);
    }

    #[test]
    fn peer_survives_until_default_offline_ttl_expires() {
        assert_eq!(DEFAULT_OFFLINE_AFTER, Duration::from_secs(90));
        let config = DiscoveryConfig::new("local", 47000);
        let discoverer = MdnsDiscoverer::new(config).unwrap();
        let peer = device("peer");
        let peer_id = peer.id;
        let seen_at = Instant::now();

        upsert_peer(
            &discoverer.shared,
            peer,
            PeerSource::Mdns,
            Some("peer._aisync._tcp.local.".to_string()),
            test_connection_info(),
            seen_at,
        );
        prune_stale_peers(
            &discoverer.shared,
            DEFAULT_OFFLINE_AFTER,
            seen_at + Duration::from_secs(60),
        );
        assert!(discoverer.peer(&peer_id).unwrap().is_some());

        prune_stale_peers(
            &discoverer.shared,
            DEFAULT_OFFLINE_AFTER,
            seen_at + DEFAULT_OFFLINE_AFTER + Duration::from_secs(1),
        );
        assert!(discoverer.peer(&peer_id).unwrap().is_none());
    }

    #[test]
    fn service_found_refreshes_last_seen_for_known_peer() {
        let config = DiscoveryConfig::new("local", 47000);
        let discoverer = MdnsDiscoverer::new(config).unwrap();
        let peer = device("peer");
        let peer_id = peer.id;
        let fullname = "peer._aisync._tcp.local.";
        let seen_at = Instant::now();

        discoverer
            .shared
            .service_names
            .lock()
            .unwrap()
            .insert(fullname.to_string(), peer_id);
        upsert_peer(
            &discoverer.shared,
            peer,
            PeerSource::Mdns,
            Some(fullname.to_string()),
            test_connection_info(),
            seen_at,
        );
        assert!(refresh_peer_last_seen_by_fullname(
            &discoverer.shared,
            fullname,
            seen_at + Duration::from_secs(80),
        ));

        prune_stale_peers(
            &discoverer.shared,
            DEFAULT_OFFLINE_AFTER,
            seen_at + Duration::from_secs(120),
        );
        assert!(discoverer.peer(&peer_id).unwrap().is_some());

        prune_stale_peers(
            &discoverer.shared,
            DEFAULT_OFFLINE_AFTER,
            seen_at + Duration::from_secs(171),
        );
        assert!(discoverer.peer(&peer_id).unwrap().is_none());
    }

    #[test]
    fn get_peers_query_refreshes_last_seen_for_active_peers() {
        let config = DiscoveryConfig::new("local", 47000);
        let discoverer = MdnsDiscoverer::new(config).unwrap();
        let peer = device("peer");
        let peer_id = peer.id;
        let seen_at = Instant::now();

        upsert_peer(
            &discoverer.shared,
            peer,
            PeerSource::Mdns,
            Some("peer._aisync._tcp.local.".to_string()),
            test_connection_info(),
            seen_at,
        );
        let devices =
            refresh_peers_for_query(&discoverer.shared, seen_at + Duration::from_secs(80));
        assert_eq!(devices.len(), 1);

        prune_stale_peers(
            &discoverer.shared,
            DEFAULT_OFFLINE_AFTER,
            seen_at + Duration::from_secs(120),
        );
        assert!(discoverer.peer(&peer_id).unwrap().is_some());

        prune_stale_peers(
            &discoverer.shared,
            DEFAULT_OFFLINE_AFTER,
            seen_at + Duration::from_secs(171),
        );
        assert!(discoverer.peer(&peer_id).unwrap().is_none());
    }

    #[test]
    fn get_peers_query_refreshes_removed_peer_to_avoid_transient_offline() {
        let config = DiscoveryConfig::new("local", 47000);
        let discoverer = MdnsDiscoverer::new(config).unwrap();
        let peer = device("peer");
        let peer_id = peer.id;
        let fullname = "peer._aisync._tcp.local.";
        let seen_at = Instant::now();

        discoverer
            .shared
            .service_names
            .lock()
            .unwrap()
            .insert(fullname.to_string(), peer_id);
        upsert_peer(
            &discoverer.shared,
            peer,
            PeerSource::Mdns,
            Some(fullname.to_string()),
            test_connection_info(),
            seen_at,
        );
        assert!(mark_peer_removed_by_fullname(
            &discoverer.shared,
            fullname,
            seen_at + Duration::from_secs(20),
            DEFAULT_OFFLINE_AFTER,
        ));

        let devices =
            refresh_peers_for_query(&discoverer.shared, seen_at + Duration::from_secs(80));
        assert_eq!(devices.len(), 1);
        prune_stale_peers(
            &discoverer.shared,
            DEFAULT_OFFLINE_AFTER,
            seen_at + Duration::from_secs(111),
        );
        assert!(discoverer.peer(&peer_id).unwrap().is_some());

        prune_stale_peers(
            &discoverer.shared,
            DEFAULT_OFFLINE_AFTER,
            seen_at + Duration::from_secs(171),
        );
        assert!(discoverer.peer(&peer_id).unwrap().is_none());
    }

    #[test]
    fn tailscale_json_discovers_only_online_reachable_peers() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let status = r#"
        {
          "Peer": {
            "nodekey:online": {
              "HostName": "mac-mini",
              "DNSName": "mac-mini.tailnet.ts.net.",
              "TailscaleIPs": ["127.0.0.1"],
              "Online": true
            },
            "nodekey:offline": {
              "HostName": "offline",
              "TailscaleIPs": ["127.0.0.2"],
              "Online": false
            }
          }
        }
        "#;

        let peers =
            devices_from_tailscale_status_json(status, port, Duration::from_millis(100)).unwrap();

        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].name, "mac-mini");
        assert_eq!(
            peers[0].addresses,
            vec!["127.0.0.1".parse::<IpAddr>().unwrap()]
        );
    }

    #[test]
    fn manual_ip_fallback_adds_reachable_peer() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let mut config = DiscoveryConfig::new("local", 47000);
        config.probe_timeout = Duration::from_millis(100);
        let mut discoverer = MdnsDiscoverer::new(config).unwrap();
        let (tx, rx) = mpsc::channel();
        discoverer
            .on_peer_change(Box::new(move |change| {
                tx.send(change).unwrap();
            }))
            .unwrap();

        let peer = discoverer.discover_manual_peer(address).unwrap();

        assert_eq!(peer.addresses, vec![address.ip()]);
        assert_eq!(
            discoverer.peer_sources().get(&peer.id),
            Some(&PeerSource::Manual)
        );
        let event = rx.recv().unwrap();
        assert_eq!(event.peer.id, peer.id);
        assert_eq!(event.kind, PeerChangeKind::Discovered);
        assert_eq!(discoverer.peers().unwrap(), vec![peer]);
    }

    #[test]
    fn manual_ip_fallback_rejects_unreachable_peer() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let mut config = DiscoveryConfig::new("local", 47000);
        config.probe_timeout = Duration::from_millis(30);
        let discoverer = MdnsDiscoverer::new(config).unwrap();

        let error = discoverer.discover_manual_peer(address).unwrap_err();

        assert!(error.to_string().contains("AISync peer is not reachable"));
        assert!(discoverer.peers().unwrap().is_empty());
    }
}
