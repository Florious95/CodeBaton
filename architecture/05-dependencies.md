# 05 - External Dependency Topology

Every external resource the running app touches -- the filesystem, the network, the operating system, and external processes -- with the exact path/port/resource, the assumption made about its availability, and the precise behavior when it is unavailable (panic / Err propagated / silent fallback / swallowed).

Entity inventory and persistent state shapes are in 02-entities.md and 01-state-model.md §1; the per-operation failure-cleanup matrix is in 06-operations.md §D. This document is the *dependency* angle: for each external thing, what happens when it is missing or broken.

Config dir is `~/.aisync/` (legacy name; product is CodeBaton). On macOS/Linux `home_dir()` = `$HOME`, on Windows `%USERPROFILE%`; the pairing store alone uses `%APPDATA%\CodeBaton\` on Windows (`codebaton-discovery/src/lib.rs:1596-1609`). When `$HOME`/`$USERPROFILE` are both unset, `default_config_path()` returns `None` and `Backend::new` falls back to a relative `PathBuf::from(".aisync/config.toml")` (`backend/mod.rs:240`).

---

## Filesystem

All persistent state is plain files under `~/.aisync/` (and session sources under `~/.claude` / `~/.codex`). There is no database. Directories are created lazily via `create_dir_all` immediately before writing. The dominant pattern is `?`-propagation of IO errors; a handful of write paths deliberately swallow failures (history, logs, trash purge).

### `~/.aisync/config.toml` -- primary config (TOML)
- **What**: `SyncConfig` -- device identity, peers, projects, workspaces, exclude rules, receive port, sync snapshots (01-state-model.md §1 L1).
- **Resource**: `default_config_path()` = `home/.aisync/config.toml` (`codebaton-sync/src/config.rs:369-371`).
- **Read**: `load_config` does `fs::read_to_string` (IO err -> `AisyncError::Config`) then `toml::from_str` (parse err -> `AisyncError::Config`) then `validate_config` (`config.rs:345-356`). `Backend::new` only calls `load_config` when `config_path.exists()`; otherwise it builds an in-memory `SyncConfig::new(default_device_name())` (`backend/mod.rs:240`).
- **Write**: `save_config` = `validate_config` -> `create_dir_all(parent)?` -> `toml::to_string_pretty` -> `fs::write` (`config.rs:358-367`). Called whenever config changes (device-name heal / state_path fill-in at `backend/mod.rs:240`, and every mutating command -- see 06-operations.md §A).
- **Availability assumption**: `~/.aisync` is readable and writable.
- **When unavailable**: missing file -> **silent in-memory default** (not an error). Present-but-corrupt/unparseable -> **Err propagated**, `Backend::new` fails, app cannot init (`backend/mod.rs:240`). Write failure (`create_dir_all`/`fs::write`) -> **Err propagated** to the caller. No file-level lock exists; concurrent `load -> modify -> save` from watcher threads, manual sync and UI can clobber each other (01-state-model.md BUG-2).

### `~/.aisync/state.toml` -- sync version state (TOML)
- **What**: `SyncState` -- per-project version counters + fingerprints for the local-copy sync coordinator (01-state-model.md §1 L2).
- **Resource**: `default_state_path()` = `home/.aisync/state.toml` (`config.rs:373-374`); resolved via `config.state_path()`.
- **Read**: `SyncState::load` returns an empty `projects` map if `!path.exists()`, else `fs::read_to_string` + `toml::from_str` (parse err -> `AisyncError::Config`) (`codebaton-sync/src/lib.rs:417-425`).
- **Write**: `SyncState::save` = `create_dir_all(parent)?` + `toml::to_string_pretty` + `fs::write` (`lib.rs:427-435`).
- **Availability assumption**: `~/.aisync` writable.
- **When unavailable**: missing -> **silent empty state**. Corrupt -> **Err propagated**. Save failure -> **Err propagated** (sync reported failed). NOTE: `SyncState` belongs to the `SyncCoordinator` in `codebaton-sync`, which is the *local filesystem* copy path, distinct from the TCP/TLS network sync invoked from `backend.rs` (see §Network single-points note).

### `~/.aisync/paired_peers.json` -- pairing store (JSON, atomic write)
- **What**: persisted paired-peer identities + pinned certs (01-state-model.md §1 L4).
- **Resource**: `default_pairing_store_path()` = `~/.aisync/paired_peers.json`, or `%APPDATA%\CodeBaton\paired_peers.json` on Windows (`codebaton-discovery/src/lib.rs:1596-1609`).
- **Read**: `load_pairings` = `fs::read`; `ErrorKind::NotFound` -> `Ok(empty)`; bad JSON -> `AisyncError::Discovery`; other IO -> `Err` (`codebaton-discovery/src/lib.rs:1544-1558`). Called from `MdnsDiscoverer::new`, hence from `Backend::new`.
- **Write**: `save_pairings` = `create_dir_all(parent)?` -> write `<path>.tmp` -> (Windows: remove existing) -> `fs::rename` (atomic temp+rename) (`codebaton-discovery/src/lib.rs:1560-1575`). Called from `persist_pairings` on confirm/unpair.
- **Availability assumption**: dir writable.
- **When unavailable**: missing -> **silent empty map**. Corrupt JSON / IO error on load -> **Err propagated**, blocks `Backend::new`. Save failure -> **Err propagated** (but at the call sites in confirm/unpair the discovery write is treated as non-fatal; see 06-operations.md §A1/§A2).

### `~/.aisync/history.jsonl`, `chat_history.jsonl`, `file_transfer_history.jsonl` -- append-only audit logs (JSONL)
- **What**: one JSON object per line; sync events, inbound/outbound text messages, file-transfer events (01-state-model.md §1 L5).
- **Resource**: `config_path.with_file_name("history.jsonl" | "chat_history.jsonl" | "file_transfer_history.jsonl")` -- i.e. siblings of `config.toml` under `~/.aisync/` (`backend/history.rs:157`, history writer paths).
- **Write**: `append_json_line` = `create_dir_all(parent)?` + `OpenOptions{create,append}` + `writeln!` (`backend/history.rs:157`). The `Result` is **matched, not `?`-propagated**: on `Err` the record functions only emit `app_log("file_transfer_history_failed" / "chat_store_append_failed" / ...)` (`backend/file_transfer.rs:594`, `backend/messaging.rs:47`).
- **Read**: `read_jsonl` = `fs::read_to_string`; on any error `let Ok(text) = ... else return Vec::new()` (`backend/history.rs:170`); per-line `serde_json::from_str(...).ok()` silently drops bad lines.
- **Availability assumption**: `~/.aisync` writable, but the system tolerates it not being.
- **When unavailable**: write failure -> **swallowed** (history silently lost, only logged). Read failure or missing file -> **silent empty `Vec`**. History is write-only evidence and never feeds sync decisions (01-state-model.md L5), so loss is non-functional.

### `~/.aisync/receiver.der` + `receiver.key.der` -- local TLS identity
- **What**: this node's self-signed receiver cert + PKCS8 key, reused across restarts so peers can keep their pin.
- **Resource**: `receiver_cert_path` / `receiver_key_path` = `config_path.with_file_name("receiver.der" | "receiver.key.der")` (`backend/identity.rs:84`).
- **Read/Create**: `load_or_create_receiver_identity` -- if both `fs::read(cert)` and `fs::read(key)` succeed, reuse; else `generate_tls_identity("aisync-receiver")` + `create_dir_all(parent)?` + `fs::write(cert)?` + `fs::write(key)?` (`backend/identity.rs:92`). `start_serve_daemon` additionally re-writes the cert file with `fs::write(&cert_out, ...)` (`backend/identity.rs:92`, daemon write at `backend/serve.rs:60`).
- **Availability assumption**: `~/.aisync` writable.
- **When unavailable**: one-of-two files readable -> **regenerates** a fresh identity (peers' old pins break). Write failure in `load_or_create_receiver_identity` -> **Err propagated**. Write/identity failure inside `start_serve_daemon` -> **`return None`**, disabling the receive daemon (see below) rather than failing the whole app (`backend/serve.rs:60`).

### `~/.aisync/peers/<peer_id>-receiver.der` -- pinned peer certs
- **What**: each paired peer's pinned receiver cert, used to verify the peer's TLS cert by exact DER bytes on push.
- **Resource**: `peer_receiver_cert_path` = `config_path.with_file_name("peers").join("{peer_id}-receiver.der")` (around `backend/identity.rs:78`, verify region).
- **Write**: `persist_peer_connection` -- if `Some(cert)`, `create_dir_all(parent)?` + `fs::write(&path, cert)?`, then `config.server_cert = Some(path)`.
- **Read**: read back via `fs::read(server_cert_path)` on push, with a fallback to `config_path.with_file_name("receiver.der")` and a preference for a live mDNS-supplied cert (`backend/transport.rs:286`, verify).
- **Availability assumption**: `~/.aisync/peers` writable.
- **When unavailable**: write failure -> **Err propagated**. Read-miss at push time with no live discovery cert -> **Err** "server certificate not found".

### `~/.aisync/logs/aisync.log` -- log sink
- **What**: every `log_line` is teed here (stderr is `/dev/null` when the DMG is `open -a`-launched).
- **Resource**: `$AISYNC_LOG_FILE`, else `home/.aisync/logs/aisync.log` (`backend/events.rs:155`).
- **Write**: `log_line` = `eprintln!` then best-effort `create_dir_all(parent)` via `let _ =`, `OpenOptions{create,append}` inside `if let Ok(...)`, `writeln!` via `let _ =` (`backend/events.rs:138`). Identical swallow-everything pattern in discovery's `log_discovery` (`codebaton-discovery/src/lib.rs:1461-1483`, verify region).
- **Availability assumption**: none -- entirely optional.
- **When unavailable**: **every failure swallowed**; logging silently degrades, no error.

### Per-sync staging + commit directories (sibling-of-target, atomic rename)
- **Local-copy coordinator** (`codebaton-sync`): `.aisync-stage-code` / `.aisync-stage-session` staging siblings; `commit_two_dirs` does `unique_sibling(.aisync-backup)`, `rename(target->backup)`, `rename(stage->target)` with rollback, then `remove_dir_all(backup)` (`codebaton-sync/src/lib.rs:277-309,623-686`).
- **Transport receive staging**: `prepare_staging` = `create_dir_all(parent)?`; `staging = parent/.aisync-staging-<nanos>`; if exists `remove_dir_all?`; `create_dir_all(&staging)?`; if target exists `copy_dir_contents` (`codebaton-transport/src/lib.rs:3245-3266`).
- **Session staging** (`~/.aisync/.aisync-session-stage-<nanos>/...`): `create_dir_all(&staged_projects_dir)?` then per-session `write_session` = `create_dir_all(&dir)?` + `fs::write(file)?` (`codebaton-session/src/claude_code.rs:275-282`, verify); cleanup `let _ = fs::remove_dir_all(staging_root)` (06-operations.md §A8).
- **Codex staging** (`~/.aisync/.aisync-codex-session-stage-<nanos>/...`): `create_dir_all` + `fs::copy` per file (around `backend/session_stage.rs:190`, verify).
- **Availability assumption**: target's parent dir writable (staging is a sibling).
- **When unavailable**: any `create_dir_all`/`rename`/`copy` failure -> **Err propagated**, aborting the sync. Rollback in `commit_two_dirs` is best-effort (`let _ =`). Cleanup `remove_dir_all` failures are **swallowed** (staging dir leaked, but harmless).

### Data-loss guards on the receive/commit path (transport)
- **Recycle bin** `<target_dir>/.aisync-trash/<unix_secs>/<relative>`: before deleting target files/dirs during merge, `trash_file`/`trash_dir` = `create_dir_all(grave parent)?` + `rename` (fallback copy+remove). `purge_expired_trash` removes batches older than `RETENTION_SECS` = 7d via `let _ =`. `.aisync-trash` is excluded from manifests/counts (`list_relative_files` skips it, `codebaton-transport/src/lib.rs:3313-3315`; trash logic `3462-3533`, verify).
  - **When unavailable**: trash create/rename failure -> **Err propagated** (blocks the destructive op, protecting data). Purge failures -> **swallowed**.
- **Permanent backup** `<name>.bak-<unix_secs>` (full-overwrite commit only): `backup_target_dir` skips if target missing or empty, else `create_dir_all(&backup).map_err("backup create failed")?` + `copy_dir_contents.map_err("backup write failed")?` (`codebaton-transport/src/lib.rs:3403-3433`). Marked P0 data-loss guard: the `?` aborts the commit *before* any target write.
  - **When unavailable**: **Err propagated, overwrite aborted, target left intact** -- the strongest fail-safe in the codebase.
- **Atomic file write** `path.with_extension("aisync-tmp")`: `write_file_atomic` = clear type conflict + `create_dir_all(parent)?` + `fs::write(tmp)?` + (Windows remove existing) + `rename(tmp,path)?` (around `codebaton-transport/src/lib.rs:3582-3594`, verify).
  - **When unavailable**: any step -> **Err propagated**.
- **Session blob** `.aisync-sessions/<project_id>.bin`: `session_data_path` rejects `project_id` that is empty / contains `/` or `\` / equals `.` or `..`, then `checked_relative_path` rejects absolute and `ParentDir`/`RootDir` components (around `codebaton-transport/src/lib.rs:3611-3648`, verify).
  - **When unavailable**: unsafe id -> **`transport_err` (Err)** (path-traversal defense).
- **Workspace remote dir** (receive server): missing `target_dir` (= `expand_remote_dir(remote_dir)`, `~` -> `$HOME`) -> `fs::create_dir_all`; `Ok` traces `workspace_remote_dir_created`, `Err` traces `workspace_remote_dir_create_failed` then `return Err(error.into())` (`codebaton-transport/src/lib.rs:1932-1953`).
  - **When unavailable**: **Err returned to peer**, receive fails.

### File-receive directory `~/Downloads/CodeBaton`
- **What**: where inbound user-initiated file transfers (not sync) land.
- **Resource**: `config.default_file_receive_dir`, else `~/Downloads/CodeBaton`, else `config_path.with_file_name("files")` (`backend/mod.rs:486`, verify).
- **Write**: `ensure_file_receive_target` = `create_dir_all(receive_dir)?` + `canonicalize?` + `create_dir_all(parent)?` + `canonicalize?` + `starts_with(root)` escape check (`backend/file_transfer.rs:468`, verify). `set_default_file_receive_dir` = `create_dir_all(&path)?` (`backend/mod.rs:491`).
- **Availability assumption**: target dir writable.
- **When unavailable**: `create_dir_all`/`canonicalize` failure -> **Err propagated**. Path escaping the receive root -> **Config Err** (rejected).

### Receive root `~/.aisync/received` (serve daemon target)
- **What**: where incoming *sync push* files are committed.
- **Resource**: `receive_root` = `config.receive_dir_override`, else `$AISYNC_RECEIVE_DIR`, else `config_path.with_file_name("received")` (= `~/.aisync/received`) (`backend/mod.rs:169`).
- **Create**: `start_serve_daemon` runs `fs::create_dir_all(&receive_dir)`; on `Err` it does `eprintln!` and **`return None`** (`backend/serve.rs:60`).
- **Availability assumption**: `~/.aisync` writable.
- **When unavailable**: **receive disabled silently** (daemon `None`); `Backend::new` tolerates `serve == None` (`backend/mod.rs:240`), so the UI still runs but this node cannot receive.

### `~/.claude/projects` -- Claude Code session source (read/write-back)
- **What**: Claude session `.jsonl` files, one JSON record per line, the ground truth for conversation data (01-state-model.md §1 L7; 02-entities.md).
- **Resource (read)**: `config.claude_config.local`, else `home/.claude`; the `projects/` subdir must be a directory or `local_claude_projects_dir` -> `None` (`codebaton-session/src/claude_code.rs:230-234`; `backend/claude_paths.rs:47`, verify).
- **Read**: `parse_sessions_filtered` = `fs::read_dir(projects_dir)?` then per-matching-dir `parse_session_file` = `fs::read_to_string` + per-line `serde_json::from_str` (`codebaton-session/src/claude_code.rs:226-262,323-342`). A single malformed line (e.g. a half-flushed final line during a live Claude write) -> `AisyncError::Session("...: invalid json: ...")` (`claude_code.rs:334-340`), which aborts the **entire** session file -- there is NO file-lock, retry, atomicity, or partial-line tolerance on the read path.
- **Write-back**: rewritten sessions are staged under `~/.aisync/.aisync-session-stage-<nanos>/projects/<encoded_dir>/` then `write_session` = `create_dir_all(&dir)?` + `fs::write(file)?` (`codebaton-session/src/claude_code.rs:275-282`, verify).
- **Encoded-dir reconstruction**: the target encoded directory name is recomputed by `claude_project_dir_name()` (keep `[A-Za-z0-9-_.]`, collapse the rest to `-`) rather than read from disk; it MUST stay byte-for-byte identical to Claude Code's own encoder or sessions are silently missed (`claude_code.rs:224-225` doc; recompute around `backend/session_stage.rs:623`, verify).
- **Availability assumption**: `~/.claude` readable. The app reads it via plain `std::fs`; there is **no explicit macOS full-disk-access (TCC) check** and no entitlements requesting it (tauri.conf.json macOS block has only `signingIdentity "-"`, ad-hoc).
- **When unavailable**: dir missing -> handled as `None`/skip upstream (`installed=false`, count 0). A parse error mid-sync -> **Err propagated** through `?`, failing that project's session-sync step (06-operations.md §A8), not just the bad file.

### `~/.codex/sessions` -- Codex session source (read, verbatim copy)
- **What**: Codex `.jsonl` sessions.
- **Resource**: `$AISYNC_CODEX_SESSIONS_DIR` (must `is_dir`), else `home/.codex/sessions` (must `is_dir`) (`backend/mod.rs:2179`).
- **Read/stage**: `collect_jsonl_files` recursive `fs::read_dir` with `?` (`backend/mod.rs:2187`); matched by raw substring `line.contains(local_code_dir)` with NO schema/version validation and NO path rewriting; staged via `fs::copy` byte-for-byte (around `backend/session_stage.rs:190`, verify). Remote target is the literal `~/.codex/sessions`, expanded on the peer.
- **Availability assumption**: optional.
- **When unavailable**: missing dir -> **`Ok(None)`, silent skip**. Read/copy errors -> **Err propagated**.

### Filesystem watcher (`notify` v8 / FSEvents on macOS) -- live change source
- **What**: recursive watch of each enabled project/workspace local root to trigger auto-sync (02-entities.md; 06-operations.md §B1/§B2).
- **Resource**: `notify::recommended_watcher` (`RecommendedWatcher` = FSEvents on macOS), watching `path` `RecursiveMode::Recursive` (`codebaton-sync/src/watcher.rs:8,67-83`). Debounce default 2s (`watcher.rs:10`).
- **Availability assumption**: each local root exists and the OS change-notification facility (FSEvents/inotify) is available.
- **When unavailable**: non-existent watch path -> `FsWatcher::start` returns `AisyncError::Config "watch path does not exist"` (`watcher.rs:57-63`); watcher-create / `watch()` failure -> `AisyncError::Io` (`watcher.rs:78,81-83`). The backend's `start_project_watcher`/`start_workspace_watcher` call `.map_err(app_log("project_watch_failed")).ok()?`, so a failing watcher becomes **`None`** -- auto-sync for that one root is disabled, the app continues (`backend/mod.rs:1220`). Watch *event* errors inside the callback are **dropped**: `let Ok(event) = event else { return; }` (`watcher.rs:68-70`).

### Exclude rules (config data, not a file)
- **What**: layered glob rules (built-in defaults + config-level + per-project/workspace) gating both watcher events and manifest scanning. Includes `.git/**`, `node_modules/**`, `target/**`, `.aisync-trash/**`, and sensitive patterns `.env*`, `*.key`, `*.pem`. Git is handled purely as exclusion -- CodeBaton never shells out to `git` and links no git library.
- **Resource**: static `default_exclude_patterns` + `sensitive_file_patterns` (`codebaton-transport/src/lib.rs:3146-3188`), merged in `exclude_rules_for_project`/`workspace_exclude_rules` and expanded; `validate_config` rejects any empty entry (`config.rs:429-437`).
- **When unavailable**: an empty exclude entry -> **config validation Err** (blocks load/save).

---

## Network

There is exactly **one** port and **one** TLS channel. Control frames (Hello, pairing, mappings, text, file-transfer) and sync frames (manifest, signatures, deltas, batches, deletes, complete) are multiplexed over the same TCP+TLS connection, dispatched by a 1-byte `MessageType` tag (`codebaton-transport/src/lib.rs:120-139`). Discovery is a three-tier fallback (mDNS / Tailscale / manual).

### TCP serve port `0.0.0.0:52000`
- **What**: the single sync+control listener.
- **Resource**: `default_receive_port()` = `52000` (`codebaton-sync/src/config.rs:456-458`); bound on `SocketAddr::from(([0,0,0,0], port))` (all interfaces) (`backend/serve.rs:60`). The bind is synchronous before `start_serve_daemon` returns, so a live daemon is confirmed; it runs inside a dedicated 2-worker tokio runtime (`backend/serve.rs:60`).
- **Availability assumption**: port 52000 is free and bindable.
- **When unavailable**: bind failure (port in use / permission) -> `eprintln!("receive daemon bind failed on {listen}: {e}")` then **`return None`** (`backend/serve.rs:60`). There is NO automatic port fallback in production -- the read-back `service.local_addr()` supports ephemeral port 0 only in test usage. The app runs without a receiver.

### mDNS service `_aisync._tcp.local.`
- **What**: LAN discovery + self-advertisement, single `ServiceDaemon` that both registers and browses.
- **Resource**: `AISYNC_SERVICE_TYPE` = `"_aisync._tcp.local."`, discovery `PROTOCOL_VERSION` = `1` (`codebaton-discovery/src/lib.rs:24,26`). `ServiceDaemon::new()` -> `register(service_info)` -> `browse(AISYNC_SERVICE_TYPE)`; a background thread polls `recv_timeout(poll_interval)`, default 250ms (`codebaton-discovery/src/lib.rs:513-532`, poll `:58`). Advertised `ServiceInfo` embeds device name/id/os/version, an `endpoint_ip` property, and the receiver's pinned cert chunked into TXT properties. Peers go offline after `DEFAULT_OFFLINE_AFTER` = 90s of silence (`codebaton-discovery/src/lib.rs:27`).
- **Backend uses the pure-Rust `mdns-sd` crate** (`codebaton-discovery/Cargo.toml:11`), not the system `dns-sd`/`avahi`/`mDNSResponder` binaries -- so multicast UDP must work but no external daemon is required.
- **Availability assumption**: multicast/mDNS works on the LAN.
- **When unavailable**: `MdnsDiscoverer::start` returns `AisyncError::Discovery("mDNS daemon/register/browse: ...")` (`codebaton-discovery/src/lib.rs:513-523`), but `Backend::new` calls it best-effort: `let _ = discoverer.start();` with the comment "failing to start mDNS (no network) must not break the UI" (`backend/mod.rs:240`). **Discovery silently degrades**; peers must then be reached via Tailscale or manual IP.

### TLS 1.3 (rustls + rcgen, pinned, no CA)
- **What**: both ends generate an ephemeral self-signed ECDSA cert via `rcgen`; rustls is pinned to TLS 1.3 only, server uses `with_no_client_auth` and a single cert (`codebaton-transport/src/lib.rs:3042-3053`). The client uses a custom `PinnedPeerCertVerifier` that compares the peer's end-entity cert by **exact DER byte-equality** against the pinned cert -- NOT CA/hostname validation; `server_name` is set but ignored for trust (`codebaton-transport/src/lib.rs:3055-3109`). Crypto provider is aws-lc-rs installed as default (`codebaton-transport/src/lib.rs:3074-3085`). This is entirely userspace -- it does **not** touch the macOS keychain or system trust store.
- **Availability assumption**: the peer presents exactly the pinned cert (from mDNS TXT or config).
- **When unavailable**: cert mismatch -> rustls handshake fails, surfaced as `transport_err("TLS connect: ...")` to the caller, no retry (`codebaton-transport/src/lib.rs:507`). Client with no pin -> immediate `transport_err("TLS peer certificate is not pinned")` before connecting (`codebaton-transport/src/lib.rs:3059`). Bad `server_name` -> `transport_err("invalid TLS server name: ...")` (`codebaton-transport/src/lib.rs:491-492`).

### Connection timeouts and framing limits
- **What**: hard upper bounds on each network phase.
- **Resource** (`codebaton-transport/src/lib.rs:86-90`):
  - TCP connect: `CONNECT_TIMEOUT` = 10s -> "TCP connect timed out after 10000ms to {addr}" (`:477-484`).
  - TLS handshake: `TLS_HANDSHAKE_TIMEOUT` = 10s, both connect and accept -> "TLS connect/accept timed out after 10000ms" (`:498-507`; accept at `:1569-1576`, verify).
  - Frame header read: `FRAME_HEADER_TIMEOUT` = 10s.
  - Frame body read: `FRAME_BODY_TIMEOUT` = 60s.
  - Max frame size: `MAX_FRAME_SIZE` = 512 MiB; `len < FRAME_TYPE_SIZE || len > MAX_FRAME_SIZE` -> "invalid frame length" (`:2621-2647`, verify).
- **When unavailable** (slow/blocked/firewalled peer): each phase yields a timeout **Err** to the caller; a firewall-blocked port manifests as the 10s TCP connect timeout or a failed 300ms probe (peer shown unreachable). There is **no firewall-specific detection or messaging**.
- After each session the client sends TLS `close_notify` via `shutdown()` (10s timeout, best-effort) because rustls does not auto-send on drop; skipping it makes the peer read `UnexpectedEof` (`codebaton-transport/src/lib.rs:525-530`).

### Per-peer connect fallback + no application retry
- **What**: `connect_to_peer` iterates ALL of `peer.addresses` in order, `connect_addr` (TCP+TLS) on each at the given port; first success returns, else the **last** error is returned -- a single sequential sweep, no backoff, no second pass (`codebaton-transport/src/lib.rs:574-595`). Empty addresses -> `transport_err("peer '{name}' has no addresses")`.
- **When unavailable**: there is **no retry/backoff loop** around the network connect in push/probe paths; a connect failure propagates as `Err` for that operation and is reported, not auto-retried (`backend/sync_push.rs:20`, verify). Each call regenerates a fresh client TLS identity and a fresh connection.

### Discovery tiers (mDNS / Tailscale / manual) and reachability probe
- **What**: `PeerSource::{Mdns, Tailscale, Manual}` upserted into one shared peer map (`codebaton-discovery/src/lib.rs:65-70`). All three tiers verify reachability with a blocking `TcpStream::connect_timeout` to the peer's port, default `probe_timeout` 300ms (`probe_aisync_port`, `codebaton-discovery/src/lib.rs:1540-1542`; default `:60`).
  - **Tailscale**: shells out to `tailscale status --json` (peer discovery) and `tailscale ip -4` (local IP fallback) -- see §External Processes.
  - **Manual**: `manual_device_from_socket_addr` probes the explicit `SocketAddr`; if unreachable within `probe_timeout` -> `AisyncError::Discovery("CodeBaton peer is not reachable at {addr}")` (`codebaton-discovery/src/lib.rs:812-829`, verify).
- Local IPs are enumerated from interfaces (`getifaddrs`, Unix only); if empty, a UDP route-probe to TEST-NET addresses (192.0.2.1:9 / 198.51.100.1:9) reads the OS-selected source IP; Tailscale IP (100.64.0.0/10) appended if available (`codebaton-discovery/src/lib.rs:837-848,979-995`, verify).

### Serve daemon shutdown poke
- **What**: to break the blocking std `accept()` loop, `ServeShutdownHandle::shutdown` sets the stop flag then opens a throwaway `TcpStream::connect_timeout` to `127.0.0.1:<port>` (200ms) to wake `accept` (`backend/serve.rs:39`).
- The per-connection serve loop catches and logs each connection's errors (`trace_stage("receive_error", ...)`) then continues -- a single bad peer never kills the daemon (`codebaton-transport/src/lib.rs:2178-2199`, verify).

### Transport vs discovery protocol versions
- Transport wire protocol `PROTOCOL_VERSION` = **2** (`codebaton-transport/src/lib.rs:81`); discovery `PROTOCOL_VERSION` = **1** (`codebaton-discovery/src/lib.rs:26`). Distinct counters; Hello frames carry the transport version for compat.

---

## Operating System

### Keyring / secret store (`keyring` v3) -- device signing key
- **What**: a 32-byte ed25519 private signing key per device, generated via `OsRng` on first access and reused.
- **Resource**: `keyring = { version = "3", features = ["apple-native", "windows-native", "sync-secret-service"] }` -- only `codebaton-discovery` pulls it in (`codebaton-discovery/Cargo.toml:10`). Service name `AISYNC_KEYRING_SERVICE` = `"CodeBaton"` (`codebaton-discovery/src/lib.rs:25`), account key `device:<device_id>:ed25519` (`codebaton-discovery/src/lib.rs:694-696`). Generated in `ensure_local_ed25519_identity_in_store` via `SigningKey::generate(&mut OsRng)` + `store.set_secret` (`codebaton-discovery/src/lib.rs:598-613`); entry built by `keyring::Entry::new(...)` (`codebaton-discovery/src/lib.rs:689-692`).
- **When unavailable**: get/set/delete errors map to `AisyncError::Discovery` strings and bubble as `Result` errors; `keyring::Error::NoEntry` is treated as success (`None` for get, `Ok` for delete) (`codebaton-discovery/src/lib.rs:117-139`). There is **no silent fallback to file storage** -- a denied Keychain prompt would fail the call.
- **IMPORTANT caveat**: this keyring-backed identity path (`ensure_local_ed25519_identity_in_store` / `KeyringSecretStore` / `begin_pairing_with_keyring`) is **not invoked by the running GUI/transport/CLI** -- those symbols appear only in `codebaton-discovery`'s own unit tests. The live GUI pairing passes hardcoded placeholder key strings: `begin_pairing(peer_id, "gui-local-key")` (`backend/peers.rs:174`, verify) and `confirm_pairing(peer_id, "gui-local-key", "gui-peer-key")` (`backend/peers.rs:300`, verify). So in the shipped app the keyring is a **latent dependency, currently unexercised**.

### System notifications (`tauri-plugin-notification` =2.3.3)
- **What**: sync-result toasts.
- **Resource**: `tauri-plugin-notification = "=2.3.3"` (`codebaton-app/Cargo.toml:31`), registered `.plugin(tauri_plugin_notification::init())` (`codebaton-app/src/lib.rs:25`), granted `"notification:default"` (`codebaton-app/capabilities/default.json:12`). Frontend hook requests permission once and only sends when granted AND the window is unfocused (`codebaton-app/ui/notifications.ts:8-37`, verify).
- **When unavailable**: if permission is denied, `sendNotification` is simply never called -- **no error, no retry**; sync-result events just produce no toast.

### System tray (`tauri` `tray-icon` feature)
- **What**: tray icon + menu.
- **Resource**: `tauri = { version = "=2.11.3", features = ["tray-icon", "image-png"] }` (`codebaton-app/Cargo.toml:30`). The tray icon is always `app.default_window_icon().unwrap().clone()` (`codebaton-app/src/tray.rs:19-20`) -- there are NO per-state icon images; state is reflected only via tooltip text ("CodeBaton — 空闲", `tray.rs:21`) and menu labels (peer name + online/offline, pause toggle).
- **When unavailable**: `.unwrap()` on the default window icon would **panic** at tray build if no icon were bundled; in practice the icon is always present from `tauri.conf.json`. Tray construction errors propagate as `tauri::Result` from `build` (`tray.rs:15-37`).

### Window control + native dialogs (Tauri core + `tauri-plugin-dialog` v2)
- **Window**: granted `core:window:allow-show/-hide/-set-focus/-close` (`codebaton-app/capabilities/default.json:8-11`). On startup the main window is force `show()`/`center()`/`unminimize()`/`set_focus()` to fix a WKWebView black-surface bug; `CloseRequested` for `main` hides + `prevent_close()` when `minimize_to_tray` (`codebaton-app/src/lib.rs:29-62`, verify region).
- **Dialogs**: `tauri-plugin-dialog = "2"` (`codebaton-app/Cargo.toml:32`), granted `dialog:default` + `dialog:allow-open` (`capabilities/default.json:13-14`). `pick_files`/`pick_folder` via `app.dialog().file()`; cancellation returns empty/`None` gracefully (`codebaton-app/src/commands.rs:902-917,1987-1994`, verify).

### `libc` (Unix-only) -- interface enumeration + hostname
- **Resource**: `[target.'cfg(unix)'.dependencies] libc = "0.2"` in both `codebaton-app/Cargo.toml:34-35` and `codebaton-discovery/Cargo.toml:17-18`. Used for `getifaddrs`/`freeifaddrs` (local interface addresses, discovery) and `gethostname` (device name, backend) (`codebaton-discovery/src/lib.rs:925-953`, verify; `backend/identity.rs:44`, verify).
- **When unavailable**: `#[cfg(not(unix))]` paths return empty `Vec`/`None`. A code comment documents that a sandboxed/hardened-runtime app cannot spawn `scutil`/`hostname`, which is why in-process `gethostname` is used instead of a subprocess (`backend/identity.rs:44`).
- macOS product version is read by parsing `/System/Library/CoreServices/SystemVersion.plist` directly (not `sw_vers`), specifically so it works in the sandboxed/hardened release build (`codebaton-app/src/commands.rs:463-484`, verify).

### macOS RSS sampling via `mach_task_self` (NOT a TCC operation)
- `current_rss_bytes()` calls `libc::task_info(libc::mach_task_self(), MACH_TASK_BASIC_INFO, ...)` purely for memory sampling in test assertions; needs no permission; non-macOS returns 0 (`backend/events.rs:68`). No TCC/full-disk-access prompt is ever issued by the app.

---

## External Processes / Tools

CodeBaton spawns **no** AI-tool binary. Claude Code / Codex / Gemini are read/written as files only -- "installed" is a directory `.exists()` check, never a process probe (`backend/mod.rs:579`; `codebaton-session/src/claude_code.rs:203-211`). It also does **not** shell out to `git`. The only subprocesses are `tailscale` (discovery), the platform file-opener, and macOS `osascript`.

### `tailscale` CLI -- discovery fallback
- **Invocations**: `tailscale status --json` (peer discovery, `codebaton-discovery/src/lib.rs:392-396`) and `tailscale ip -4` (local IP fallback, around `:1159-1178`, verify).
- **Availability assumption**: `tailscale` binary installed + node up.
- **When unavailable**: `status` -- `ErrorKind::NotFound` -> **`Ok(Vec::new())`** (graceful empty); other spawn error or non-zero exit -> `AisyncError::Discovery` (`codebaton-discovery/src/lib.rs:399-408`). `ip -4` -- spawn failure or non-success -> **`None`** via `.ok()?`. Candidate Tailscale IPs are kept only if `probe_aisync_port` succeeds.

### Platform file-opener (`open` / `explorer` / `xdg-open`)
- **What**: the `open_path` Tauri command reveals a file/dir in the OS file manager.
- **Resource**: macOS `open`, Windows `explorer`, other-Unix `xdg-open` (`codebaton-app/src/commands.rs:714-723`).
- **When unavailable**: spawn failure -> `Err("failed to open '{path}': {e}")`; non-zero exit -> `Err("file manager exited with {s}")` (`codebaton-app/src/commands.rs:725-729`). There is **no fallback** if the binary is missing (notably `xdg-open` on minimal Linux). Returned as an Err string to the UI.

### macOS `osascript` (JXA) -- clipboard file paths
- **What**: reading file paths from the clipboard (NSPasteboard file URLs) on macOS.
- **Resource**: `Command::new("osascript").arg("-l").arg("JavaScript")...` (`codebaton-app/src/commands.rs:1000-1037`, verify).
- **When unavailable**: non-zero exit -> `Err("read clipboard files failed: ...")`. On non-macOS, `clipboard_file_paths` returns **`Ok(Vec::new())`** unconditionally (`codebaton-app/src/commands.rs:1040-1043`, verify).

### Gemini CLI -- declared but unimplemented
- `GeminiParser` is a skeleton; `ClaudeToGeminiConverter::convert` always returns `AisyncError::Session("gemini converter not yet implemented ...")` (`codebaton-session/src/converter.rs:187-191`, verify); `ai_tools()` hardcodes Gemini `installed: false`, empty config dir, 0 sessions (`backend/mod.rs:579`, verify). Not a live dependency.

---

## Dependency Availability Matrix

| Dependency | Resource | Assumed available | Failure mode | Graceful? |
|---|---|---|---|---|
| `~/.aisync/config.toml` (read) | `config.rs:369-371` | yes (or default) | missing -> in-memory default; corrupt -> Err, init fails | partial (missing OK, corrupt fatal) |
| `~/.aisync/config.toml` (write) | `config.rs:358-367` | yes | Err propagated; no lock (BUG-2) | no |
| `~/.aisync/state.toml` | `config.rs:373-374`, `lib.rs:417-435` | yes | missing -> empty; corrupt/save -> Err | partial |
| `paired_peers.json` | `discovery/lib.rs:1544-1575` | yes | missing -> empty; corrupt -> Err blocks init; save Err | partial |
| `*.jsonl` history | `backend/history.rs:157` | optional | write swallowed; read -> empty | yes |
| `receiver.der`/`.key.der` | `backend/identity.rs:84` | yes | regenerate on partial; write Err / serve `None` | partial |
| `peers/<id>-receiver.der` | `backend/identity.rs:78` | yes | write Err; read-miss -> push Err | no |
| `~/.aisync/logs/aisync.log` | `backend/events.rs:138` | optional | all swallowed | yes |
| staging/commit dirs | `transport/lib.rs:3245-3266`, `sync/lib.rs:580-686` | parent writable | Err propagated; cleanup swallowed | partial |
| permanent backup `.bak-<ts>` | `transport/lib.rs:3403-3433` | parent writable | Err -> overwrite aborted (P0 guard) | yes (fail-safe) |
| recycle bin `.aisync-trash` | `transport/lib.rs:3462-3533` | target writable | create/rename Err blocks delete; purge swallowed | yes (fail-safe) |
| `~/Downloads/CodeBaton` | `backend/file_transfer.rs:468` | target writable | Err propagated; escape -> Config Err | no |
| `~/.aisync/received` (serve) | `backend/serve.rs:60` | yes | create fail -> serve `None` | yes (receive disabled) |
| `~/.claude/projects` | `claude_code.rs:226-262` | yes | missing -> skip; bad line -> Err aborts session-sync | partial |
| `~/.codex/sessions` | `backend/mod.rs:2179` | optional | missing -> Ok(None); read/copy Err | partial |
| `notify` watcher (FSEvents) | `watcher.rs:54-94` | local root exists | missing path / create -> watcher `None`; event err dropped | yes (per-root) |
| TCP port 52000 | `config.rs:456-458`, `backend/serve.rs:60` | free | bind fail -> serve `None`, no fallback | yes (no receiver) |
| mDNS `_aisync._tcp.local.` | `discovery/lib.rs:24,513-523` | LAN multicast | start Err -> `let _ =`, discovery degrades | yes |
| TLS 1.3 pinned cert | `transport/lib.rs:3042-3109` | exact pin matches | mismatch/no-pin -> Err, no retry | no |
| TCP/TLS/frame timeouts | `transport/lib.rs:86-90` | peer responsive | timeout Err per phase | no |
| peer reachability | `transport/lib.rs:574-595` | one address reachable | last-error Err, no backoff | no |
| keyring (`CodeBaton`) | `discovery/lib.rs:117-139` | OS secret store | Err (no file fallback) -- but UNEXERCISED in shipped app | n/a (latent) |
| notification plugin | `Cargo.toml:31`, `notifications.ts:8-37` | permission granted | denied -> no toast | yes |
| tray icon | `tray.rs:19-20` | default icon bundled | `.unwrap()` panic if absent | no |
| window/dialog plugins | `capabilities/default.json:8-14` | Tauri runtime | dialog cancel -> empty/None | yes |
| `libc` getifaddrs/gethostname | `discovery/lib.rs:925-953`, `backend/identity.rs:44` | Unix | non-Unix -> empty/None | yes |
| `tailscale` CLI | `discovery/lib.rs:392-408` | installed + up | NotFound -> empty; else Discovery Err | partial |
| `open`/`explorer`/`xdg-open` | `commands.rs:714-729` | platform tool present | Err string to UI, no fallback | no |
| `osascript` (macOS) | `commands.rs:1000-1043` | macOS | non-macOS -> Ok(empty); exit -> Err | partial |

---

## Single Points of Failure

These dependencies, when broken, take down a whole capability or the entire app (not just one operation):

1. **`~/.aisync/config.toml` corruption -> whole app cannot start.** `load_config` is `?`-propagated through `Backend::new` (`backend/mod.rs:240`), which is `.expect("failed to initialize CodeBaton backend")` in the Tauri entrypoint (`codebaton-app/src/lib.rs:22`). A parse error or a validation failure (e.g. an empty exclude rule, `config.rs:429-437`) makes the GUI **panic on launch**. Contrast: a *missing* config falls back to defaults, and *write* failures only break the current operation. This is the single most fragile dependency.

2. **`paired_peers.json` corruption -> app cannot start.** Loaded inside `MdnsDiscoverer::new` -> `Backend::new`; bad JSON or a non-NotFound IO error is `Err`-propagated (`codebaton-discovery/src/lib.rs:1544-1556`) and reaches the same `.expect(...)`. A missing file is fine; a corrupt one is fatal.

3. **Port 52000 -> all receiving disabled (no app crash).** If bind fails, `start_serve_daemon` returns `None` (`backend/serve.rs:60`) with no port fallback. The node can still push and browse but can never *receive* a sync or pairing/file/text frame -- pairing from the peer side, incoming pushes, and file transfers all silently stop working. The UI gives no port-conflict-specific signal.

4. **No network retry layer.** Neither the transport nor the discovery layer has a reconnect/backoff scheduler for the TCP/TLS sync path; `connect_to_peer` is a single sequential address sweep (`codebaton-transport/src/lib.rs:574-595`) and push/probe paths propagate the first failure as `Err` (`backend/sync_push.rs:20`). A transient network blip fails the operation outright; recovery depends entirely on the user retrying or the next watcher-triggered auto-sync cycle.

5. **TLS pin is the only trust anchor.** There is no CA, no hostname check; trust is exact-DER equality against the pinned cert (`codebaton-transport/src/lib.rs:3102-3107`). If the peer regenerates its receiver identity (e.g. `receiver.der` was unreadable and got regenerated, `backend/identity.rs:92`) the old pin no longer matches and **every connection to that peer fails** with "server certificate does not match pinned peer certificate" until re-pairing refreshes the pin -- a silent, total connectivity break between two specific peers.

6. **Claude session parse is all-or-nothing per file.** A single malformed JSONL line -- including a half-flushed final line written *live* by Claude Code at scan time -- raises `AisyncError::Session("invalid json")` that aborts the whole file and, via `?`, the whole project's session-sync step (`codebaton-session/src/claude_code.rs:334-340`; 06-operations.md §A8). There is no lock-detection, retry, or skip-bad-file path, so an actively-written session can intermittently block session sync for its entire project.

7. **`claude_project_dir_name()` re-implementation drift.** The encoded project directory name is *recomputed* to match Claude Code's encoder rather than discovered from disk (`codebaton-session/src/claude_code.rs:224-225`). If Claude Code changes its path-encoding scheme, the predicate stops matching and sessions are **silently skipped** -- no error, just missing data. This is a hidden coupling to an external tool's internal behavior, with no cross-check test guarding it.

### Notes / lower-confidence items
- Exact line numbers tagged "verify" above were inferred from scout leads but their surrounding region was not line-read in this pass; the *behavior* (Err-propagation via `?`, or the specific swallow pattern) is consistent with the verified neighboring code. Treat tagged numbers as approximate.
- The local-filesystem `SyncCoordinator` (`codebaton-sync`, with `state.toml`) and the TCP/TLS network sync (`codebaton-transport`, invoked from `backend.rs`) are two distinct sync engines; `state.toml`'s version counters belong only to the former. No higher-level network retry/backoff scheduler was located in the transport/discovery/serve code reviewed.
