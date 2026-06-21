# S1 State Hierarchy Model

## 1. Storage Layers

The system has **6** distinct storage layers:

### Layer 1: Config File (TOML on disk)
- **Path**: `~/.aisync/config.toml` (`config.rs:371`)
- **Structure**: `SyncConfig` (`config.rs:12-40`) -- device identity, peers, projects, workspaces, exclude rules, sync mode, sync snapshots, receive port
- **Key nested state**: `SyncSnapshot` per (project, peer) for split-brain detection (`config.rs:187-195`), embedded inside `ProjectConfig.sync_snapshots` (`config.rs:210-211`)
- **Read/Write**: `load_config()` / `save_config()` (`config.rs:345-367`), TOML serde round-trip with validation on both read and write

### Layer 2: Sync State File (TOML on disk)
- **Path**: `~/.aisync/state.toml` (`config.rs:373-374`)
- **Structure**: `SyncState` (`codebaton-sync/src/lib.rs:412-413`) -- per-project `ProjectVersionState` with `local_version`, `remote_version`, fingerprints, `last_synced_at_unix_secs`, `has_synced` flag (`lib.rs:466-473`)
- **Separate from config**: Tracks version counters and fingerprint history for the sync coordinator; not mixed into config

### Layer 3: Runtime Memory (Backend Inner)
- **Owner**: `Backend.inner: Mutex<Inner>` (`backend/mod.rs:178`)
- **Structure**: `Inner` (`backend/mod.rs:202`) holds an in-memory clone of `SyncConfig`, the discoverer, serve handles, pairing sessions, project/workspace watchers, mapping negotiation state
- **Parallel runtime state** (process-global statics):
  - `INCOMING_SYNC_SUPPRESSIONS`: `Mutex<HashMap<PathBuf, Instant>>` (`backend/auto_sync_gate.rs:53`) -- anti-loop suppression windows
  - `AUTO_SYNC_GATES`: `Mutex<HashMap<String, AutoSyncGate>>` (`backend/auto_sync_gate.rs:57`) -- in-flight + cooldown gates per (scope, name, peer)
  - `SESSION_BASELINE_SEEDS`: `Mutex<HashMap<String, SessionBaseline>>` (`backend/auto_sync_gate.rs:61`) -- mtime/fingerprint baselines for session change detection
  - `WORKSPACE_PROPAGATION_BYPASS`: `Mutex<HashSet<String>>` (`backend/auto_sync_gate.rs:65`) -- one-shot first-propagation flags
- **Pending queues** (Arc<Mutex<VecDeque>>):
  - pairing requests, project mapping requests/acks, workspace mapping requests/acks, text messages, file transfer requests/acks, file receive states (`backend/mod.rs:202`)

### Layer 4: Discovery Runtime (mDNS + Pairing Store)
- **In-memory**: `SharedState` (`codebaton-discovery/src/lib.rs:164-171`) -- live peer records (`peers: Mutex<HashMap<DeviceId, PeerRecord>>`), service name index, paired peers cache
- **On-disk**: `~/.aisync/paired_peers.json` (`discovery/lib.rs:1596-1608`) -- persisted paired peer identities + certificates, written by `persist_pairings()` (`discovery/lib.rs:478-487`)
- **Ephemeral**: `PeerRecord.last_seen: Instant` (`discovery/lib.rs:155`) -- not persisted, only runtime

### Layer 5: History / Log Files (append-only JSONL on disk)
- **Sync history**: `~/.aisync/history.jsonl` (`backend/history.rs:129`) -- one JSON line per sync event
- **Chat history**: `~/.aisync/chat_history.jsonl` (`backend/messaging.rs:47`)
- **File transfer history**: `~/.aisync/file_transfer_history.jsonl` (`backend/file_transfer.rs:310`)
- All append-only; read by parsing all lines then filtering (`backend/history.rs:170`)

### Layer 6: Network/Transport Ephemeral State
- **TLS connection**: `TcpTransporter.stream` (`transport/lib.rs:461`) -- per-sync-session TCP+TLS stream, discarded after each sync
- **Manifest exchange**: `SyncExchange { source_manifest, remote_manifest }` (`transport/lib.rs:468-471`) -- computed during sync, not persisted directly
- **Protocol state**: message-type framing (`MessageType` enum, `transport/lib.rs:120-145`), each connection is a state machine (Hello -> Manifest -> Signatures -> Deltas -> SyncComplete)
- **Staging directory**: temp dir for received files before atomic commit (`transport/lib.rs:2022`)

### Layer 7: Session Parse State (Claude Code JSONL)
- **On-disk**: `~/.claude/projects/<encoded-path>/<session-id>.jsonl` (`claude_code.rs:3-4`)
- **In-memory**: `ParsedSession` (`claude_code.rs:89-99`) with `RecordLine` per JSON line, preserving raw bytes + parsed Value + dirty flag (`claude_code.rs:37-44`)
- **Invariant**: write-back is byte-identical for unmodified records (`claude_code.rs:74-84`); only rewritten records get reserialized

---

## 2. Authority / Truth-Source Hierarchy

| Layer | Role | Authority |
|-------|------|-----------|
| Config File (L1) | **Truth source** for device identity, project/workspace mappings, peer definitions, sync snapshots | Ground truth; memory loads from it; all mutations must persist back |
| Sync State File (L2) | **Truth source** for version counters and fingerprint progression | Ground truth for the sync coordinator; independent of config |
| Runtime Memory (L3) | **Derived** -- loaded from L1 at startup, mutated in-memory, periodically saved back | Authoritative only for ephemeral state (gates, suppressions, pending queues); for config data it is a **cache** |
| Discovery Runtime (L4) | **Mixed** -- paired_peers.json is truth for pairing identity; live peer records are ephemeral | Pairing store is truth; online/offline status is ephemeral |
| History Files (L5) | **Write-only evidence** -- append-only audit trail | Read-only evidence for UI; never modified, never used as input to sync decisions |
| Network State (L6) | **Ephemeral** -- per-connection, per-sync | No persistence; manifest exchange results feed back into L1 (snapshot) and L2 (version state) |
| Session Parse (L7) | **Truth source** for Claude Code conversation data on the local machine | The .jsonl files are ground truth; ParsedSession is a faithful parse with byte-identical round-trip guarantee |

---

## 3. Sync / Projection Rules

### L1 (Config Disk) <-> L3 (Memory)

**Load at startup**: `Backend::new()` calls `load_config(&config_path)` (`backend/mod.rs:240`), stores clone in `Inner.config` (`backend/mod.rs:202`).

**Save on mutation**: Every config-changing operation (add_project, confirm_pairing, set_device_name, etc.) calls `save_config(&g.config_path, &g.config)` immediately (e.g. `backend/projects.rs:223` add_project, `backend/peers.rs:300` confirm_pairing, `backend/mod.rs:545` set_device_name).

**External edit detection**: `ConfigStore.reload_if_changed()` (`config.rs:334-342`) compares file mtime and reloads if changed. However, `Backend` does NOT use `ConfigStore`; it holds a bare `SyncConfig` clone. The watcher threads (`start_project_watchers`, `start_session_mtime_scanner`) each call `load_config(&config_path)` fresh from disk on every cycle (`backend/mod.rs:1220` start_project_watcher, `backend/session_scanner.rs:120` start_session_mtime_scanner), bypassing the in-memory config entirely.

**Snapshot writeback gap**: `run_tcp_push()` writes the sync snapshot to disk via `load_config` -> `set_sync_snapshot` -> `save_config` (`backend/sync_push.rs:20`), NOT through the in-memory `Inner.config`. The caller `run_sync()` then explicitly re-reads it back: `if let Ok(persisted) = load_config(...)` -> `g.config.set_sync_snapshot(...)` (`backend/split_brain.rs:73`). This is a **manual sync-back**, not automatic.

### L1 (Config Disk) <-> L2 (State Disk)

**No automatic coupling**. Config and state are separate files. The sync coordinator (`codebaton-sync/src/lib.rs:80`) loads state independently: `SyncState::load(&config.state_path())`. State is saved after successful sync: `self.state.save(&self.config.state_path())` (`lib.rs:325`).

### L3 (Memory) -> L5 (History)

**One-way append**: `record_sync()` appends a JSON line to `history.jsonl` (`backend/history.rs:23`). History is never read back to influence sync behavior. Same for chat and file transfer history.

### L4 (Discovery) <-> L1 (Config)

**Two separate stores**: Discovery has its own `paired_peers.json` for pairing identity; Backend also writes peer info into `config.peers` (`backend/peers.rs:466`). The `paired_peers()` method merges both sources: discoverer's paired peers + config.peers (`backend/peers.rs:102`). There is no automatic reconciliation -- they are merged at read time.

### L6 (Network) -> L1 (Config)

**One-way post-sync**: After successful push, the manifest hash is computed and written as a `SyncSnapshot` to the config file (`backend/sync_push.rs:20`). This is the only path from network state to persistent state.

### L7 (Session) <-> L6 (Network)

**Session sync is staged**: `prepare_claude_session_sync()` parses sessions from disk, applies path rewriting, writes to a staging directory, then the staging dir is pushed via transport (`backend/session_stage.rs:53`). After push, staging is cleaned up (`backend/sync_push.rs:20`). The original session files on disk are never modified during a push.

---

## 4. Known Layer-Inconsistency Bugs / Risks

### BUG-1: Snapshot Memory/Disk Desync (Mitigated but Fragile)

**Location**: `backend/split_brain.rs:73` (run_sync)

After `run_tcp_push()` writes a snapshot to disk, `run_sync()` re-reads it back into memory with an explicit `load_config()` -> `set_sync_snapshot()` call. The comment says:

> `run_tcp_push` 把成功同步的快照写到了磁盘 config；这里同步回内存 config，否则 check_split_brain 等读内存的逻辑看不到刚写的快照（in-memory/disk 不一致）。

This is a **manual workaround** for the fundamental problem that `Inner.config` is a clone, not a live reference to disk. If any other code path writes snapshots to disk (e.g., auto-sync in watcher threads), it does NOT sync back to the Backend's in-memory config. The auto-sync path uses its own `load_config` each cycle (`backend/mod.rs:1220` start_project_watcher, `backend/session_scanner.rs:120` start_session_mtime_scanner) so it always reads fresh disk state, but `check_split_brain()` (`backend/split_brain.rs:248`) reads from `g.config.sync_snapshot()` which is the in-memory clone -- potentially stale if auto-sync just completed.

### BUG-2: Config Concurrent Write Race

Multiple code paths call `save_config()` on the same file:
- Manual sync (`backend/sync_push.rs:20` run_tcp_push)
- Auto-sync watcher threads (`backend/mod.rs:977` refresh_and_save_workspaces)
- UI operations (add_project, confirm_pairing, etc.)
- `config_with_refreshed_workspaces()` (`backend/mod.rs:518`)

There is no file-level lock. The pattern `load_config -> modify -> save_config` is NOT atomic. If two threads interleave (e.g., a watcher refresh overlapping a user confirm_pairing), one write may clobber the other. The `ConfigStore` with its mtime check (`config.rs:334-342`) exists but is NOT used by the Backend -- it's only in the sync crate.

### BUG-3: Global Statics Not Scoped to Backend Instance

The four global statics (`INCOMING_SYNC_SUPPRESSIONS`, `AUTO_SYNC_GATES`, `SESSION_BASELINE_SEEDS`, `WORKSPACE_PROPAGATION_BYPASS` at `backend/auto_sync_gate.rs:53`) are process-global `OnceLock<Mutex<...>>`. In tests that create multiple Backend instances in the same process, these are shared across all instances. The `AUTO_SYNC_COOLDOWN_OVERRIDE` (`backend/auto_sync_gate.rs:23` auto_sync_cooldown) is similarly global. This is noted in the code comment for `set_auto_sync_cooldown_for_test` (`backend/auto_sync_gate.rs:31`): "all tests set the same value, no parallel race". But if production ever instantiates two Backends (e.g., multi-account), state would leak between them.

### BUG-4: Discovery Paired Peers vs Config Peers Dual-Write

Peer identity is stored in TWO places:
- `~/.aisync/paired_peers.json` (discovery crate, `discovery/lib.rs:1596-1608`)
- `config.peers` HashMap inside `~/.aisync/config.toml` (`config.rs:19`)

These are written independently: `persist_peer_connection()` writes to config.peers (`backend/peers.rs:466`); `discoverer.confirm_pairing()` writes to paired_peers.json (`discovery/lib.rs:308-312`). If one write succeeds and the other fails, the two stores diverge. The merge in `paired_peers()` (`backend/peers.rs:102`) handles this at read time but never repairs the inconsistency.

### BUG-5: Watcher Threads Use Stale Fallback Config

Each watcher thread holds a `fallback_config` clone taken at startup (`backend/mod.rs:1220` start_project_watcher). If `load_config` fails (e.g., file temporarily locked), it falls back to this stale snapshot: `load_config(&config_path).unwrap_or_else(|_ | fallback_config.clone())` (`backend/mod.rs:1220` start_project_watcher, `backend/session_scanner.rs:120` start_session_mtime_scanner). This means a watcher could use arbitrarily outdated project/peer mappings after a config file corruption or race.

### BUG-6: Session Trailing-Newline / Byte-Identical Risk on Rewrite

`ParsedSession` tracks `trailing_newline` (`claude_code.rs:98`) to ensure byte-identical round-trips. However, when `rewrite_structured_paths()` marks records as `dirty` (`claude_code.rs:307`), those records are reserialized by serde_json (`claude_code.rs:79`), which may produce different key ordering or whitespace than the original raw bytes. This is by design (dirty records MUST be reserialized), but means the "byte-identical" guarantee only holds for UNTOUCHED records -- the rewritten ones are content-equivalent but not byte-identical to their original form.
