use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use codebaton_core::{AisyncError, DeviceId, DeviceInfo, Discoverer, OsType, Result};
use codebaton_sync::{save_config, PeerConfig, SyncConfig};
use codebaton_transport::PairingRequestPayload;

use super::identity::peer_receiver_cert_path;
use super::transport::{pairing_tls_config, send_pairing_request_async};
use super::{
    active_pairing_session, endpoint_online, live_peers_with_endpoints, log_line, unix_secs_now,
    with_endpoint_first, Backend,
};

pub struct PairingInfo {
    pub peer: DeviceInfo,
    pub code: String,
    pub request_id: String,
    pub expires_at_unix_secs: u64,
}

#[derive(Clone)]
pub(crate) struct PairingSession {
    pub(crate) peer: DeviceInfo,
    pub(crate) request_id: String,
    pub(crate) code: String,
    pub(crate) expires_at_unix_secs: u64,
    pub(crate) connection: Option<PairingConnection>,
    pub(crate) inbound: bool,
}

#[derive(Clone)]
pub(crate) struct PairingConnection {
    pub(crate) endpoint: Option<SocketAddr>,
    pub(crate) receiver_cert_der: Option<Vec<u8>>,
    pub(crate) server_name: Option<String>,
}

impl Backend {
    pub fn take_pending_pairing_request(&self) -> Option<(DeviceInfo, String, String, u64)> {
        let request = self.pending_pairing_requests.lock().unwrap().pop_front()?;
        let mut g = self.inner.lock().unwrap();
        let connection = PairingConnection {
            endpoint: request.endpoint,
            receiver_cert_der: request.receiver_cert_der.clone(),
            server_name: request.server_name.clone(),
        };
        let peer = with_endpoint_first(request.device.clone(), connection.endpoint);
        g.pairing_sessions.insert(
            peer.id,
            PairingSession {
                peer: peer.clone(),
                request_id: request.request_id.clone(),
                code: request.code.clone(),
                expires_at_unix_secs: request.expires_at_unix_secs,
                connection: Some(connection),
                inbound: true,
            },
        );
        log_line(&format!(
            "[pair] pairing_request_ready peer_id={} peer_name={} request_id={} expires_at={}",
            peer.id.0, peer.name, request.request_id, request.expires_at_unix_secs
        ));
        Some((
            peer,
            request.code,
            request.request_id,
            request.expires_at_unix_secs,
        ))
    }

    /// Register (or update) a peer's push endpoint + pinned certificate so the
    /// GUI can push to it. Persists to config.
    pub fn add_peer_endpoint(
        &self,
        name: String,
        id: DeviceId,
        endpoint: SocketAddr,
        server_cert: Option<PathBuf>,
        server_name: Option<String>,
    ) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        g.config.peers.insert(
            name.clone(),
            PeerConfig {
                id,
                name,
                endpoint: Some(endpoint),
                server_cert,
                server_name,
                last_seen: None,
            },
        );
        let path = g.config_path.clone();
        let cfg = g.config.clone();
        save_config(&path, &cfg)
    }

    /// Paired peers (from the discoverer's persisted pairing store) merged with
    /// peers declared in config. Returns `(DeviceInfo, online)` pairs.
    pub fn paired_peers(&self) -> Vec<(DeviceInfo, bool)> {
        let g = self.inner.lock().unwrap();
        let live = live_peers_with_endpoints(&g.discoverer);
        let now = unix_secs_now();
        let mut out = Vec::new();
        for p in g.discoverer.paired_peers() {
            if p.device.id == g.config.device.id {
                continue;
            }
            let live_match = live.iter().find(|(device, _)| device.id == p.device.id);
            let session = active_pairing_session(&g, &p.device.id, now);
            let configured = g
                .config
                .peers
                .values()
                .find(|peer| peer.id == p.device.id || peer.name == p.device.name);
            let endpoint = live_match
                .and_then(|(_, endpoint)| *endpoint)
                .or_else(|| session.and_then(|session| session.connection.as_ref()?.endpoint))
                .or_else(|| configured.and_then(|peer| peer.endpoint));
            let online =
                live_match.is_some() || session.is_some() || endpoint.is_some_and(endpoint_online);
            out.push((with_endpoint_first(p.device, endpoint), online));
        }
        // Also surface config-declared peers not present in the pairing store.
        for (name, peer) in &g.config.peers {
            if peer.id == g.config.device.id {
                continue;
            }
            if out.iter().any(|(d, _)| d.id == peer.id) {
                continue;
            }
            let live_match = live.iter().find(|(device, endpoint)| {
                device.id == peer.id || (peer.endpoint.is_some() && *endpoint == peer.endpoint)
            });
            let session = active_pairing_session(&g, &peer.id, now);
            let endpoint = live_match
                .and_then(|(_, endpoint)| *endpoint)
                .or(peer.endpoint)
                .or_else(|| session.and_then(|session| session.connection.as_ref()?.endpoint));
            let online =
                live_match.is_some() || session.is_some() || endpoint.is_some_and(endpoint_online);
            out.push((
                with_endpoint_first(
                    DeviceInfo {
                        id: peer.id,
                        name: name.clone(),
                        os: live_match
                            .map(|(device, _)| device.os.clone())
                            .unwrap_or_else(|| OsType::Other("configured".to_string())),
                        addresses: live_match
                            .map(|(device, _)| device.addresses.clone())
                            .unwrap_or_default(),
                        protocol_version: live_match
                            .map(|(device, _)| device.protocol_version)
                            .unwrap_or(1),
                    },
                    endpoint,
                ),
                online,
            ));
        }
        out
    }

    /// Pairing code for a peer (D4). Derives from both `DeviceInfo`s.
    ///
    /// Primary path: the discoverer's live `begin_pairing` (requires the peer to
    /// be currently online). When the peer flickered out of the live mDNS list
    /// between the user seeing it and clicking 配对 — which the field report
    /// observed on Tailscale — fall back to a config-declared peer so the dialog
    /// still gets a stable, order-independent code instead of failing silently.
    pub fn pairing_code(&self, peer_id: &DeviceId) -> Result<PairingInfo> {
        let (peer, request_id, code, expires_at_unix_secs, connection, local_payload, tls_config) = {
            let mut g = self.inner.lock().unwrap();
            let now = unix_secs_now();
            if let Some(session) = g.pairing_sessions.get(peer_id).cloned() {
                if session.expires_at_unix_secs > now {
                    log_line(&format!(
                        "[pair] pairing_session_reused peer_id={} request_id={} inbound={}",
                        peer_id.0, session.request_id, session.inbound
                    ));
                    return Ok(PairingInfo {
                        peer: with_endpoint_first(
                            session.peer,
                            session.connection.as_ref().and_then(|c| c.endpoint),
                        ),
                        code: session.code,
                        request_id: session.request_id,
                        expires_at_unix_secs: session.expires_at_unix_secs,
                    });
                }
                g.pairing_sessions.remove(peer_id);
            }
            let live_connection = g.discoverer.peer_connection_info(peer_id).ok().flatten();
            let fallback_peer = peer_from_config(&g.config, peer_id);
            let (peer, request_id, code, expires_at_unix_secs) =
                match g.discoverer.begin_pairing(peer_id, "gui-local-key") {
                    Ok(req) => (
                        with_endpoint_first(
                            req.peer,
                            live_connection.as_ref().and_then(|c| c.endpoint),
                        ),
                        req.request_id,
                        req.pairing_code,
                        req.expires_at_unix_secs,
                    ),
                    Err(e) => {
                        log_line(&format!(
                            "[pair] live begin_pairing failed ({e}); trying config peer fallback"
                        ));
                        let peer = fallback_peer.clone().ok_or(e)?;
                        let request_id = codebaton_discovery::new_pairing_request_id();
                        let code = codebaton_discovery::derive_pairing_code_with_nonce(
                            g.discoverer.local_device(),
                            &peer,
                            &request_id,
                        );
                        log_line(&format!(
                            "[pair] pair_fallback_config peer_id={} peer_name={}",
                            peer_id.0, peer.name
                        ));
                        (peer, request_id, code, unix_secs_now() + 120)
                    }
                };
            let connection = live_connection
                .as_ref()
                .map(pairing_connection_from_discovery)
                .or_else(|| connection_from_config(&g.config, peer_id));
            let peer = with_endpoint_first(peer, connection.as_ref().and_then(|c| c.endpoint));
            g.pairing_sessions.insert(
                *peer_id,
                PairingSession {
                    peer: peer.clone(),
                    request_id: request_id.clone(),
                    code: code.clone(),
                    expires_at_unix_secs,
                    connection: connection.clone(),
                    inbound: false,
                },
            );
            let local = g.discoverer.local_device().clone();
            let local_endpoint = g.serve.as_ref().and_then(|serve| {
                local
                    .addresses
                    .first()
                    .map(|ip| SocketAddr::new(*ip, serve.port))
            });
            let local_payload = PairingRequestPayload {
                request_id: request_id.clone(),
                code: code.clone(),
                expires_at_unix_secs,
                device: with_endpoint_first(local, local_endpoint),
                endpoint: local_endpoint,
                receiver_cert_der: g
                    .serve
                    .as_ref()
                    .and_then(|serve| fs::read(&serve.cert_path).ok()),
                server_name: Some("aisync-receiver".to_string()),
            };
            let tls_config = pairing_tls_config(connection.as_ref());
            (
                peer,
                request_id,
                code,
                expires_at_unix_secs,
                connection,
                local_payload,
                tls_config,
            )
        };

        let endpoint = connection.as_ref().and_then(|c| c.endpoint);
        if let (Some(tls_config), Some(endpoint)) = (tls_config, endpoint) {
            send_pairing_request_async(endpoint, tls_config, local_payload);
        } else {
            log_line(&format!(
                "[pair] pairing_request_not_sent peer_id={} reason=missing_endpoint_or_cert",
                peer_id.0
            ));
        }

        Ok(PairingInfo {
            peer,
            code,
            request_id,
            expires_at_unix_secs,
        })
    }

    /// Confirm pairing and persist the peer to `config.peers`.
    ///
    /// The on-disk `config.peers` entry is what moves a device from 发现 to
    /// 已配对 and survives restarts, so we ALWAYS write it here — even when the
    /// live discoverer confirm fails because the peer flickered off the mDNS
    /// list (the Tailscale case from the field). The discoverer confirm and the
    /// live connection info (endpoint + pinned cert) are best-effort enrichments
    /// layered on top; their absence must not block local pairing.
    pub fn confirm_pairing(&self, peer_id: &DeviceId) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        let session = g.pairing_sessions.get(peer_id).cloned();

        // Best-effort: record the pairing in the discoverer's store + grab the
        // peer DeviceInfo it returns. If the peer isn't live, fall back to the
        // discovered/config peer so we still have a name+id to persist.
        let confirmed_peer =
            match g
                .discoverer
                .confirm_pairing(peer_id, "gui-local-key", "gui-peer-key")
            {
                Ok(result) => Some(result.peer),
                Err(e) => {
                    log_line(&format!(
                        "[pair] discoverer confirm best-effort failed ({e}); persisting anyway"
                    ));
                    None
                }
            };
        let peer = session
            .as_ref()
            .map(|session| session.peer.clone())
            .or(confirmed_peer)
            .or_else(|| {
                g.discoverer
                    .peers()
                    .unwrap_or_default()
                    .into_iter()
                    .find(|d| d.id == *peer_id)
            })
            .or_else(|| {
                g.config
                    .peers
                    .iter()
                    .find(|(_, p)| p.id == *peer_id)
                    .map(|(name, p)| DeviceInfo {
                        id: p.id,
                        name: name.clone(),
                        os: OsType::Other("configured".to_string()),
                        addresses: p.endpoint.map(|ep| vec![ep.ip()]).unwrap_or_default(),
                        protocol_version: 1,
                    })
            })
            .ok_or_else(|| {
                AisyncError::Discovery(format!("peer {} not found to confirm", peer_id.0))
            })?;

        // Live connection info (endpoint + pinned cert) if the peer is reachable;
        // None is fine — we still persist the peer entry.
        let connection = g.discoverer.peer_connection_info(peer_id).ok().flatten();
        let session_connection = session.and_then(|session| session.connection);
        let config_path = g.config_path.clone();
        persist_peer_connection(
            &mut g.config,
            &config_path,
            peer,
            connection
                .as_ref()
                .and_then(|c| c.endpoint)
                .or_else(|| session_connection.as_ref().and_then(|c| c.endpoint)),
            connection
                .as_ref()
                .and_then(|c| c.receiver_cert_der.as_deref())
                .or_else(|| {
                    session_connection
                        .as_ref()
                        .and_then(|c| c.receiver_cert_der.as_deref())
                }),
            connection
                .as_ref()
                .and_then(|c| c.server_name.clone())
                .or_else(|| {
                    session_connection
                        .as_ref()
                        .and_then(|c| c.server_name.clone())
                }),
        )?;
        let cfg = g.config.clone();
        save_config(&config_path, &cfg)?;
        log_line(&format!(
            "[pair] config.peers now has {} entr{}",
            g.config.peers.len(),
            if g.config.peers.len() == 1 {
                "y"
            } else {
                "ies"
            }
        ));
        Ok(())
    }

    pub fn unpair(&self, peer_id: &DeviceId) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        g.discoverer.unpair(peer_id)?;
        // Drop config peer + its project mappings for this peer.
        let name = g
            .config
            .peers
            .iter()
            .find(|(_, p)| p.id == *peer_id)
            .map(|(n, _)| n.clone());
        if let Some(name) = name {
            g.config.peers.remove(&name);
            g.config.claude_config.peers.remove(&name);
            for project in &mut g.config.projects {
                project.peers.remove(&name);
            }
            for workspace in &mut g.config.workspaces {
                workspace.peers.remove(&name);
            }
        }
        let path = g.config_path.clone();
        let cfg = g.config.clone();
        let _ = save_config(&path, &cfg);
        Ok(())
    }
}

pub(crate) fn peer_from_config(config: &SyncConfig, peer_id: &DeviceId) -> Option<DeviceInfo> {
    config
        .peers
        .iter()
        .find(|(_, peer)| peer.id == *peer_id)
        .map(|(name, peer)| {
            with_endpoint_first(
                DeviceInfo {
                    id: peer.id,
                    name: name.clone(),
                    os: OsType::Other("configured".to_string()),
                    addresses: peer
                        .endpoint
                        .map(|endpoint| vec![endpoint.ip()])
                        .unwrap_or_default(),
                    protocol_version: 1,
                },
                peer.endpoint,
            )
        })
}

pub(crate) fn connection_from_config(
    config: &SyncConfig,
    peer_id: &DeviceId,
) -> Option<PairingConnection> {
    let peer = config.peers.values().find(|peer| peer.id == *peer_id)?;
    Some(PairingConnection {
        endpoint: peer.endpoint,
        receiver_cert_der: peer
            .server_cert
            .as_ref()
            .and_then(|path| fs::read(path).ok()),
        server_name: peer.server_name.clone(),
    })
}

pub(crate) fn pairing_connection_from_discovery(
    connection: &codebaton_discovery::PeerConnectionInfo,
) -> PairingConnection {
    PairingConnection {
        endpoint: connection.endpoint,
        receiver_cert_der: connection.receiver_cert_der.clone(),
        server_name: connection.server_name.clone(),
    }
}

pub(crate) fn persist_peer_connection(
    config: &mut SyncConfig,
    config_path: &Path,
    peer: DeviceInfo,
    endpoint: Option<SocketAddr>,
    receiver_cert_der: Option<&[u8]>,
    server_name: Option<String>,
) -> Result<()> {
    let server_cert = if let Some(cert) = receiver_cert_der {
        let path = peer_receiver_cert_path(config_path, &peer.id);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, cert)?;
        Some(path)
    } else {
        None
    };

    let entry = config
        .peers
        .entry(peer.name.clone())
        .or_insert_with(|| PeerConfig {
            id: peer.id,
            name: peer.name.clone(),
            endpoint: None,
            server_cert: None,
            server_name: None,
            last_seen: None,
        });
    entry.id = peer.id;
    entry.name = peer.name;
    if let Some(endpoint) = endpoint {
        entry.endpoint = Some(endpoint);
    }
    if let Some(server_cert) = server_cert {
        entry.server_cert = Some(server_cert);
    }
    entry.server_name = Some(server_name.unwrap_or_else(|| "aisync-receiver".to_string()));
    Ok(())
}
