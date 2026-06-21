//! TLS receiver identity + device-name leaf helpers.
//! Extracted from backend.rs (refactor phase 1, step 3).
//!
//! `endpoint_online` / `update_device_name_locked` are NOT here (they belong to
//! mod.rs — the former is a transport probe, the latter mutates Inner).

use std::fs;
use std::path::{Path, PathBuf};

use codebaton_core::{DeviceId, Result};
use codebaton_transport::{generate_tls_identity, TlsIdentity};

pub(crate) const PLACEHOLDER_DEVICE_NAME: &str = "CodeBaton Device";

/// Order: explicit override env → platform hostname → generic fallback. On
/// macOS we prefer the friendly `ComputerName` ("Alice's MacBook Pro") over the
/// DNS-style `hostname` ("alices-macbook-pro.local"). Never fabricates a name
/// like "CodeBaton Device" unless every real source fails.
pub(crate) fn default_device_name() -> String {
    if let Ok(name) = std::env::var("AISYNC_DEVICE_NAME") {
        if !name.trim().is_empty() {
            return name;
        }
    }
    if let Some(name) = system_hostname() {
        return name;
    }
    PLACEHOLDER_DEVICE_NAME.to_string()
}

/// A device name that should be re-derived from the host: empty, whitespace, or
/// the legacy placeholder a sandboxed older build wrote.
pub(crate) fn is_placeholder_device_name(name: &str) -> bool {
    let n = name.trim();
    n.is_empty() || n == PLACEHOLDER_DEVICE_NAME || n == "aisync-device"
}

/// Best-effort real hostname.
///
/// Uses the `gethostname(2)` syscall directly on Unix (no subprocess — a
/// sandboxed/hardened-runtime app cannot spawn `scutil`/`hostname`, which is why
/// the earlier subprocess approach silently fell back to the placeholder in the
/// release build). On Windows it reads `%COMPUTERNAME%`.
pub(crate) fn system_hostname() -> Option<String> {
    #[cfg(windows)]
    {
        let name = std::env::var("COMPUTERNAME").ok()?;
        let name = name.trim().to_string();
        return if name.is_empty() { None } else { Some(name) };
    }

    #[cfg(unix)]
    {
        // gethostname into a fixed buffer; truncate at the NUL terminator.
        let mut buf = [0u8; 256];
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
        if rc != 0 {
            return None;
        }
        let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        let raw = String::from_utf8_lossy(&buf[..len]).to_string();
        // Strip the trailing `.local` / DNS domain so the UI shows a short name
        // (e.g. "MacBook-Air" instead of "macbook-air.local").
        let short = raw.split('.').next().unwrap_or(&raw).trim().to_string();
        if short.is_empty() {
            None
        } else {
            Some(short)
        }
    }

    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

pub(crate) fn peer_receiver_cert_path(config_path: &Path, peer_id: &DeviceId) -> PathBuf {
    config_path
        .with_file_name("peers")
        .join(format!("{}-receiver.der", peer_id.0))
}

pub(crate) fn receiver_cert_path(config_path: &Path) -> PathBuf {
    config_path.with_file_name("receiver.der")
}

pub(crate) fn receiver_key_path(config_path: &Path) -> PathBuf {
    config_path.with_file_name("receiver.key.der")
}

pub(crate) fn load_or_create_receiver_identity(config_path: &Path) -> Result<TlsIdentity> {
    let cert_path = receiver_cert_path(config_path);
    let key_path = receiver_key_path(config_path);
    if let (Ok(cert_der), Ok(private_key_der)) = (fs::read(&cert_path), fs::read(&key_path)) {
        return Ok(TlsIdentity {
            cert_der,
            private_key_der,
        });
    }

    let identity = generate_tls_identity("aisync-receiver")?;
    if let Some(parent) = cert_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&cert_path, &identity.cert_der)?;
    fs::write(&key_path, &identity.private_key_der)?;
    Ok(identity)
}
