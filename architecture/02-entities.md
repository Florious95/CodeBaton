# 02 - Runtime Entity Inventory

## 1. Processes

| Entity | Description | Created | Destroyed | Location |
|--------|-------------|---------|-----------|----------|
| Tauri main process | Single GUI process hosting Backend + webview | OS launch / `tauri::Builder::run()` | User quit / window close | N/A (in-memory) |

There is exactly **one OS process**. All concurrency is via OS threads + a tokio runtime embedded inside the serve daemon thread.

---

## 2. Core Singleton Structs (process-lifetime)

### 2.1 Backend

```
codebaton-app/src/backend/mod.rs:178
```

```rust
pub struct Backend {
    inner: Mutex<Inner>,
    pending_pairing_requests: Arc<Mutex<VecDeque<PairingRequestPayload>>>,
    pending_project_mapping_requests: ...,
    pending_project_mapping_acks: ...,
    pending_workspace_mapping_requests: ...,
    pending_workspace_mapping_acks: ...,
    pending_text_messages: ...,
    pending_file_transfer_requests: ...,
    pending_file_transfer_acks: ...,
    file_receive_states: Arc<Mutex<HashMap<String, FileReceiveState>>>,
}
```

- **Created**: `Backend::new()` during Tauri setup (before window opens). `backend/mod.rs:240`
- **Destroyed**: Tauri process exit; `Drop for Backend` calls `serve_shutdown.shutdown()`. `backend/mod.rs:192`
- **Persisted**: Config at `~/.aisync/config.toml`, state at `~/.aisync/state.toml`

### 2.2 Inner (guarded by Backend.inner Mutex)

```
codebaton-app/src/backend/mod.rs:202
```

```rust
struct Inner {
    config: SyncConfig,
    config_path: PathBuf,
    discoverer: MdnsDiscoverer,
    auto_sync_paused: bool,
    serve: Option<ServeInfo>,
    serve_shutdown: Option<ServeShutdownHandle>,
    pairing_sessions: HashMap<DeviceId, PairingSession>,
    project_mapping_requests: HashMap<String, ProjectMappingRequestPayload>,
    outbound_project_mappings: HashMap<String, OutboundProjectMapping>,
    workspace_mapping_requests: HashMap<String, WorkspaceMappingRequestPayload>,
    outbound_workspace_mappings: HashMap<String, OutboundWorkspaceMapping>,
    file_transfer_requests: HashMap<String, FileTransferRequestPayload>,
    outbound_file_transfers: HashMap<String, OutboundFileTransfer>,
    project_watchers: HashMap<String, FsWatcher>,
    workspace_watchers: HashMap<String, FsWatcher>,
}
```

---

## 3. Threads

### 3.1 Serve Daemon Thread (receive loop)

```
codebaton-app/src/backend/serve.rs:60 (start_serve_daemon)
```

- **What**: `std::thread::spawn` running `runtime.block_on(async { loop { service.receive_once_with_control_handlers(...) } })`
- **Created**: Inside `start_serve_daemon()`, called from `Backend::new()`. `backend/serve.rs:60`
- **Destroyed**: When `ServeShutdownHandle::shutdown()` sets the `stop` AtomicBool and pokes the port. `backend/serve.rs:46`
- **Contains**: A dedicated `tokio::runtime::Builder::new_multi_thread().worker_threads(2)` runtime, built inside `start_serve_daemon`. `backend/serve.rs:60`
- **Role**: Accepts inbound TLS TCP connections (pairing, project mapping, workspace mapping, text messages, file transfers, sync pushes).

### 3.2 Project Watcher Threads (one per enabled project)

```
codebaton-app/src/backend/mod.rs:1220 (start_project_watcher)
```

- **What**: `std::thread::spawn` running `while let Ok(batch) = rx.recv() { ... run_project_auto_sync ... }`
- **Created**: `start_project_watcher()` per enabled project during `Backend::new()`. The singular `start_project_watcher` (the gate-consumer loop) lives in `backend/mod.rs:1220`; the thin `start_project_watchers` spawn wrapper is in `backend/watchers.rs:8`.
- **Destroyed**: When the `FsWatcher` is dropped (sends on `stop_tx`, joins the debounce worker). `codebaton-sync/src/watcher.rs:106-111`
- **Cleanup owner**: `Backend::Drop` -> inner drop -> `project_watchers` HashMap drop -> `FsWatcher::drop`

### 3.3 Workspace Watcher Threads (one per enabled workspace)

```
codebaton-app/src/backend/mod.rs:1439 (start_workspace_watcher)
```

- **What**: Same pattern as project watchers but for workspaces.
- **Created**: `start_workspace_watcher()` per enabled workspace. The singular gate-consumer loop is in `backend/mod.rs:1439`; the thin `start_workspace_watchers` spawn wrapper is in `backend/watchers.rs:18`.
- **Destroyed**: Same as project watchers via `FsWatcher::drop`.

### 3.4 FsWatcher Internal Debounce Worker Thread

```
codebaton-sync/src/watcher.rs:87
```

- **What**: `thread::spawn(move || debounce_loop(raw_rx, stop_rx, output, config.debounce))`
- **Created**: Inside `FsWatcher::start()`. `watcher.rs:87`
- **Destroyed**: `FsWatcher::stop()` sends on `stop_tx` then `.join()`s the worker. `watcher.rs:96-103`
- **Role**: Coalesces raw FS events with a 2s debounce window (`DEFAULT_DEBOUNCE`). `watcher.rs:10`

### 3.5 Session Mtime Scanner Thread

```
codebaton-app/src/backend/session_scanner.rs:120 (start_session_mtime_scanner)
```

- **What**: `std::thread::spawn` running an infinite loop that sleeps `refresh_interval_secs` then scans session directories for mtime changes, triggering auto-sync.
- **Created**: `start_session_mtime_scanner()` from `Backend::new()`. `backend/session_scanner.rs:120` (invoked from `backend/mod.rs:240`)
- **Destroyed**: **NEVER explicitly stopped** -- lives until process exit.
- **Known bug**: No shutdown handle; if Backend is dropped in a test, this thread becomes orphaned until it hits a channel error or the process exits.

### 3.6 Sync Worker Threads (ephemeral, per manual sync)

```
codebaton-app/src/commands.rs:1520
```

- **What**: `thread::spawn` that runs `backend.run_sync(...)` or `backend.run_workspace_sync(...)` and emits Tauri events.
- **Created**: When user triggers a sync from UI (`start_sync` command). `commands.rs:1520`
- **Destroyed**: When the sync completes (success or error) -- thread exits.
- **Owner**: Detached (no join handle stored). The thread references the `AppHandle` which keeps the Tauri runtime alive.

### 3.7 File Transfer Auto-Accept Threads (ephemeral)

```
codebaton-app/src/backend/serve.rs:60 (start_serve_daemon, file_transfer_request_handler closure within)
```

- **What**: `std::thread::spawn` that sends a `FileTransferAck` to the peer over a new TCP connection.
- **Created**: Inside the serve daemon's file_transfer_request_handler closure. `backend/serve.rs:60` `start_serve_daemon`
- **Destroyed**: After `send_file_transfer_ack()` returns.

### 3.8 mDNS Browser Thread

```
codebaton-discovery/src/lib.rs:530
```

- **What**: `thread::spawn` running `while !stop { receiver.recv_timeout(poll_interval) ... prune_stale_peers ... }`
- **Created**: `MdnsDiscoverer::start()` -> `Discoverer::start()`. `discovery/src/lib.rs:530`
- **Destroyed**: `MdnsDiscoverer::stop()` sets `stop_browser` AtomicBool, then `.join()`s. `discovery/src/lib.rs:229-242`
- **Cleanup owner**: `MdnsDiscoverer::Drop` -> `self.stop()`. `discovery/src/lib.rs:491-495`

---

## 4. Tokio Runtime

```
codebaton-app/src/backend/serve.rs:60 (start_serve_daemon)
```

- **What**: `tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()`
- **Created**: Inside `start_serve_daemon()`. Bound to the serve daemon thread. `backend/serve.rs:60`
- **Destroyed**: When the serve daemon thread exits (runtime dropped).
- **Role**: Drives all async TLS accept/read/write for the receive service. Outbound sync pushes build their own short-lived `block_on` runtimes inside the push helpers — `run_tcp_push`/`run_workspace_tcp_push` (`backend/sync_push.rs:20`, `backend/sync_push.rs:173`) and `probe_target_status` (`backend/split_brain.rs:171`).

---

## 5. Network Listeners / Connections

### 5.1 TCP Listener (Receive Service)

```
codebaton-transport/src/lib.rs:1488-1491
```

```rust
pub struct TransportServer {
    listener: StdTcpListener,
    acceptor: TlsAcceptor,
}
```

- **Created**: `TransportServer::bind(addr, tls)` called from `ReceiveService::bind()`. `transport/src/lib.rs:1494-1498`
- **Destroyed**: When `TransportServer` is dropped (serve daemon exits via `ServeShutdownHandle`).
- **Binds**: `0.0.0.0:<receive_port>` (default: ephemeral or config-specified).

### 5.2 TLS Client Connections (TcpTransporter)

```
codebaton-transport/src/lib.rs:460-465
```

```rust
pub struct TcpTransporter {
    stream: tokio_rustls::client::TlsStream<TcpStream>,
    confirm_overwrite: bool,
}
```

- **Created**: `TcpTransporter::connect_addr()` per sync push / pairing request / file transfer. `transport/src/lib.rs:474`
- **Destroyed**: `TcpTransporter::shutdown()` sends TLS close_notify, then drops. `transport/src/lib.rs:528-529`
- **Lifetime**: Single sync operation (connect -> exchange -> shutdown -> drop).

### 5.3 mDNS ServiceDaemon

```
codebaton-discovery/src/lib.rs:513
```

- **Created**: `ServiceDaemon::new()` inside `MdnsDiscoverer::start()`. `discovery/src/lib.rs:513`
- **Destroyed**: `MdnsDiscoverer::stop()` calls `mdns.unregister(...)` then `mdns.shutdown()`. `discovery/src/lib.rs:232-236`
- **Role**: Registers this device's `_aisync._tcp.local.` service record and browses for peers.

---

## 6. File System Watchers

### 6.1 FsWatcher (notify::RecommendedWatcher wrapper)

```
codebaton-sync/src/watcher.rs:48-53
```

```rust
pub struct FsWatcher {
    watcher: RecommendedWatcher,
    stop_tx: Option<Sender<()>>,
    worker: Option<JoinHandle<()>>,
}
```

- **Created**: `FsWatcher::start(config, output)`. `watcher.rs:55`
- **Destroyed**: `FsWatcher::drop()` -> `self.stop()` -> sends stop signal, joins worker. `watcher.rs:106-111`
- **One instance per**: Each enabled project + each enabled workspace.
- **OS resource**: Uses kqueue (macOS) or inotify (Linux) file descriptors under the hood.

---

## 7. Timers / Polling

### 7.1 Session Mtime Scanner Loop

```
codebaton-app/src/backend/session_scanner.rs:120 (start_session_mtime_scanner)
```

- **Interval**: `config.refresh_interval_secs` (default typically 30-60s)
- **Mechanism**: `std::thread::sleep(Duration)` in the scanner thread's loop
- **Purpose**: Detects session file changes (Claude Code sessions) that the FS watcher might miss

### 7.2 Auto-Sync Cooldown Gate

```
codebaton-app/src/backend/auto_sync_gate.rs:23, 41 (auto_sync_cooldown, AutoSyncGate)
```

- **What**: `static AUTO_SYNC_COOLDOWN_OVERRIDE: OnceLock<Duration>` + per-project/workspace `AutoSyncGate { in_flight, cooldown_until }`
- **Default**: 90 seconds between consecutive auto-syncs for the same project/peer. `backend/auto_sync_gate.rs:23` (`auto_sync_cooldown`)
- **Storage**: `static AUTO_SYNC_GATES: OnceLock<Mutex<HashMap<String, AutoSyncGate>>>`. `backend/auto_sync_gate.rs:57` (`auto_sync_gates` accessor)

### 7.3 Incoming Sync Suppression Window

```
codebaton-app/src/backend/auto_sync_gate.rs:72 (incoming_suppress_window)
```

- **What**: After receiving a push from a peer, suppresses outbound auto-sync for the same root path for `incoming_suppress_window()` (max of cooldown, 5s).
- **Storage**: `static INCOMING_SYNC_SUPPRESSIONS: OnceLock<Mutex<HashMap<PathBuf, Instant>>>`. `backend/auto_sync_gate.rs:53` (`incoming_sync_suppressions` accessor)

### 7.4 mDNS Peer Stale Timeout

```
codebaton-discovery/src/lib.rs:27
```

- **Value**: `DEFAULT_OFFLINE_AFTER = Duration::from_secs(90)`
- **Mechanism**: `prune_stale_peers()` runs on every mDNS browser loop iteration. `discovery/src/lib.rs:565`

---

## 8. File Locks / Persistent State

### 8.1 Config File (`~/.aisync/config.toml`)

- **Read**: On every auto-sync trigger and on demand from commands. `backend/mod.rs:1220` (`start_project_watcher`)
- **Written**: `save_config()` after project/workspace/peer mutations.
- **No file lock**: Concurrent reads/writes are possible if multiple instances run.

### 8.2 State File (`~/.aisync/state.toml`)

- **Contains**: Sync snapshots (manifest hashes for split-brain detection).
- **Written**: After each successful sync push.

### 8.3 Pairing Store (`~/.aisync/paired_peers.json`)

```
codebaton-discovery/src/lib.rs:1596-1609
```

- **Written**: After `confirm_pairing()` / `unpair()`. Atomic write via `.tmp` rename. `discovery/src/lib.rs:1560-1575`
- **Read**: On `MdnsDiscoverer::new()` construction. `discovery/src/lib.rs:193`

### 8.4 Sync History Files (`~/.aisync/history/*.json`)

- **Written**: After each sync (manual or auto). `backend/history.rs:38` `record_sync_scoped()` / `backend/history.rs:346` `record_auto_sync_history()`
- **Limit**: 5 entries per file. `backend/mod.rs:163` (`HISTORY_FILE_LIMIT`)

### 8.5 TLS Certificate/Key (`~/.aisync/receiver.{cert,key}`)

- **Created**: `load_or_create_receiver_identity()` on first launch. `backend/identity.rs:92`
- **Lifetime**: Persistent across launches. Re-created only if missing.

### 8.6 Log File (`~/.aisync/logs/aisync.log`)

- **Written**: Append-only from `log_line()`. `backend/events.rs:138` (with `app_log` in `backend/mod.rs:964`) + `discovery/src/lib.rs:1461-1473`
- **No rotation**: Grows unbounded.

---

## 9. External Resources

### 9.1 macOS Keychain (Ed25519 Identity)

```
codebaton-discovery/src/lib.rs:115-140
```

- **Service name**: `"CodeBaton"`. `discovery/src/lib.rs:26`
- **Key format**: `"device:<uuid>:ed25519"`
- **Used by**: `ensure_local_ed25519_identity()` for pairing authentication.
- **Interface**: `keyring` crate -> macOS Security framework.

### 9.2 Tailscale CLI

```
codebaton-discovery/src/lib.rs:393-438
```

- **Command**: `tailscale status --json` to discover Tailscale peers.
- **Probing**: TCP connect to `<tailscale_ip>:<port>` with timeout. `discovery/src/lib.rs:1540-1542`
- **Invoked from**: `discover_tailscale_peers()` -- called on demand, not on a timer.

---

## 10. ServeShutdownHandle (Lifecycle Controller)

```
codebaton-app/src/backend/serve.rs:39
```

```rust
pub struct ServeShutdownHandle {
    stop: Arc<AtomicBool>,
    port: u16,
}
```

- **Created by**: `start_serve_daemon()`. `backend/serve.rs:60`
- **Stored in**: `Inner.serve_shutdown`. `backend/mod.rs:202` (`Inner`)
- **Consumed by**: `Backend::Drop` or `Backend::shutdown_serve()`. `backend/mod.rs:192` (`drop`), `backend/mod.rs:399` (`shutdown_serve`)
- **Mechanism**: Sets `stop` AtomicBool, then TCP-connects to localhost:port to unblock the `accept()` call in the serve loop. `backend/serve.rs:46` (`shutdown`)

---

## 11. Known Lifecycle Bugs

### BUG-1: Session Mtime Scanner Thread Has No Shutdown Handle

```
codebaton-app/src/backend/session_scanner.rs:120 (start_session_mtime_scanner)
```

`start_session_mtime_scanner()` spawns a `std::thread::spawn` with an infinite loop. No stop channel, no AtomicBool, no join handle is retained. The thread is effectively leaked until process exit.

**Impact**: In integration tests that construct/drop multiple `Backend` instances, orphan scanner threads accumulate and may access stale config paths.

### BUG-2: Sync Worker Threads Are Detached (No Join Handle)

```
codebaton-app/src/commands.rs:1520
```

`spawn_sync()` calls `thread::spawn(...)` and discards the `JoinHandle`. If the Tauri app quits while a sync is in progress, the thread may be killed mid-write.

**Impact**: Potential data corruption during app quit if a sync is active. Low probability in practice since syncs are fast.

### BUG-3 (FIXED): Serve Daemon Previously Had No Shutdown

The `ServeShutdownHandle` pattern (`backend/serve.rs:39`) was introduced to fix a prior bug where the serve daemon thread was orphaned after Backend drop. The current implementation correctly shuts it down via `Backend::Drop`.

### BUG-4: Static Global State for Auto-Sync Gates

```
codebaton-app/src/backend/auto_sync_gate.rs:53-65
```

Four `OnceLock<Mutex<HashMap<...>>>` process-global statics hold cross-Backend state (all in `backend/auto_sync_gate.rs`, accessed via these accessors):
- `INCOMING_SYNC_SUPPRESSIONS` (`incoming_sync_suppressions`, `backend/auto_sync_gate.rs:53`)
- `AUTO_SYNC_GATES` (`auto_sync_gates`, `backend/auto_sync_gate.rs:57`)
- `SESSION_BASELINE_SEEDS` (`session_baseline_seeds`, `backend/auto_sync_gate.rs:61`)
- `WORKSPACE_PROPAGATION_BYPASS` (`workspace_propagation_bypass`, `backend/auto_sync_gate.rs:65`)

**Impact**: In tests that create multiple Backend instances in the same process, these statics leak state between test runs. Mitigated by `set_auto_sync_cooldown_for_test()` (`backend/auto_sync_gate.rs:31`) but not fully isolated.

---

## 12. Entity Relationship Summary

```
Tauri Process
 |
 +-- Backend (Tauri managed state, singleton)
 |    |
 |    +-- Inner (Mutex-guarded)
 |    |    +-- MdnsDiscoverer
 |    |    |    +-- ServiceDaemon (mDNS multicast)
 |    |    |    +-- browser_thread (mDNS event loop)
 |    |    |
 |    |    +-- ServeShutdownHandle --> serve daemon thread
 |    |    |                              +-- tokio runtime (2 worker threads)
 |    |    |                              +-- ReceiveService
 |    |    |                              |    +-- TransportServer
 |    |    |                              |         +-- StdTcpListener (0.0.0.0:port)
 |    |    |                              |         +-- TlsAcceptor
 |    |    |                              +-- infinite accept loop
 |    |    |
 |    |    +-- project_watchers: HashMap<String, FsWatcher>
 |    |    |    +-- (per project) FsWatcher
 |    |    |         +-- RecommendedWatcher (kqueue/inotify)
 |    |    |         +-- debounce worker thread
 |    |    |         +-- auto-sync consumer thread
 |    |    |
 |    |    +-- workspace_watchers: HashMap<String, FsWatcher>
 |    |         +-- (per workspace) FsWatcher
 |    |              +-- RecommendedWatcher (kqueue/inotify)
 |    |              +-- debounce worker thread
 |    |              +-- auto-sync consumer thread
 |    |
 |    +-- pending_* queues (Arc<Mutex<VecDeque<...>>>)
 |         (shared between IPC commands and serve daemon callbacks)
 |
 +-- session mtime scanner thread (ORPHAN: no shutdown handle)
 |
 +-- [ephemeral] sync worker threads (per manual sync)
 +-- [ephemeral] file transfer ack threads
 +-- [ephemeral] TcpTransporter connections (per push/control message)
```
