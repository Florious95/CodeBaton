# 06 - Operations Matrix

All user-initiated and system-initiated operations, with preconditions, state changes, success criteria, failure handling, side effects, and rollback boundaries.

---

## A. User Operations

### A1. Pairing (begin_pairing + confirm_pairing)

**Modules touched**: `peers.rs` (`pairing_code`, `confirm_pairing`, `persist_peer_connection`) + `transport.rs` (`send_pairing_request_async`).

**Entry**: `commands.rs:735` `begin_pairing` -> `backend/peers.rs:174` `pairing_code()`; then `commands.rs:1211` `confirm_pairing` -> `backend/peers.rs:300` `confirm_pairing()`

**Preconditions**:
- `peer_id` must be a valid UUID (`commands.rs:741`)
- Peer must be discoverable via mDNS OR present in `config.peers` (fallback in `backend/peers.rs` `pairing_code`)
- Local serve daemon must be running for pairing request payload (`backend/peers.rs` `pairing_code`)

**State Changes (begin_pairing)**:
1. Insert `PairingSession` into `Inner.pairing_sessions` keyed by peer DeviceId (`backend/peers.rs:174` `pairing_code`)
2. Fire-and-forget TCP send of `PairingRequestPayload` to peer endpoint via `transport.rs` `send_pairing_request_async` (`backend/transport.rs:54`)

**State Changes (confirm_pairing)**:
1. Call `discoverer.confirm_pairing()` -- writes to `~/.aisync/paired_peers.json` (best-effort, `backend/peers.rs:300` `confirm_pairing`)
2. Call `persist_peer_connection()` -- writes peer cert `.der` to `~/.aisync/peers/{id}-receiver.der` (`backend/peers.rs:466` `persist_peer_connection`)
3. Insert/update `config.peers[name]` with id, endpoint, server_cert, server_name (`backend/peers.rs:300` `confirm_pairing`)
4. `save_config()` to `~/.aisync/config.toml` (`backend/peers.rs:300` `confirm_pairing`)

**Success Criterion**: `config.peers` contains the new peer entry; `paired_peers.json` updated (best-effort).

**Failure Handling**:
- `discoverer.confirm_pairing()` failure is non-fatal -- logs warning, proceeds with config-only persistence (`backend/peers.rs:300` `confirm_pairing`)
- If peer not found anywhere (discovery + session + config), returns `AisyncError::Discovery` (`backend/peers.rs:300` `confirm_pairing`)
- `save_config()` failure propagates as `Err`; in-memory config already mutated (no rollback of `config.peers`)

**Side Effects**: Tray menu refreshed (`commands.rs:1235`)

**Rollback Boundary**: None explicit. If `save_config()` fails after `persist_peer_connection()` wrote the `.der` file, the cert file remains orphaned on disk.

**Resources Created**: `~/.aisync/peers/{id}-receiver.der` (cert file)

---

### A2. Unpair

**Modules touched**: `peers.rs` (`unpair`).

**Entry**: `commands.rs:1243` `unpair` -> `backend/peers.rs:392` `unpair()`

**Preconditions**: Peer must exist in discoverer or config.

**State Changes**:
1. `discoverer.unpair(peer_id)` -- removes from `paired_peers.json` (`backend/peers.rs:392` `unpair`)
2. Remove peer from `config.peers`, `config.claude_config.peers` (`backend/peers.rs:392` `unpair`)
3. Remove peer key from all `ProjectConfig.peers` and `WorkspaceConfig.peers` (`backend/peers.rs:392` `unpair`)
4. `save_config()` (`backend/peers.rs:392` `unpair`)

**Success Criterion**: Peer absent from both `paired_peers.json` and `config.toml`.

**Failure Handling**: `discoverer.unpair()` failure propagates as `Err`. `save_config()` failure is swallowed (`let _ =`).

**Side Effects**: Tray refresh. Project/workspace mappings to this peer become orphaned (no peer entry for the key in `ProjectConfig.peers`).

**Rollback Boundary**: None. If `discoverer.unpair()` succeeds but `save_config()` fails, discovery store and config diverge.

**Resources Cleaned**: Peer cert `.der` file NOT deleted (orphaned).

---

### A3. Add Project Mapping (request_project_mapping + process_project_mapping_acks)

**Modules touched**: `mod.rs` (`request_project_mapping` HUB) -> `transport.rs` (`send_project_mapping_request`); ack side in `projects.rs` (`process_project_mapping_acks`) -> `mod.rs` (`start_project_watcher` HUB).

**Entry**: `commands.rs:1787` `add_project` -> `backend/mod.rs:721` `request_project_mapping()`

**Preconditions**:
- Project name must not already exist in config (`backend/mod.rs:721` `request_project_mapping`)
- Local directory must exist OR `create_local_dir=true` (`backend/mod.rs:721` `request_project_mapping`)
- Peer must exist in config (`control_connection_for_peer`, `backend/mod.rs:866`)
- Local serve daemon must be running (`backend/mod.rs:721` `request_project_mapping`)

**State Changes (request)**:
1. Optionally `fs::create_dir_all(local)` if `create_local_dir=true` (`backend/mod.rs:721` `request_project_mapping`)
2. Insert `OutboundProjectMapping` into `Inner.outbound_project_mappings[request_id]` (`backend/mod.rs:721` `request_project_mapping`)
3. Send `ProjectMappingRequestPayload` to peer over TCP via `transport.rs` `send_project_mapping_request` (`backend/transport.rs:106`)

**State Changes (ack processing, `process_project_mapping_acks` at `backend/projects.rs:160`)**:
1. Remove from `outbound_project_mappings` (`backend/projects.rs:160` `process_project_mapping_acks`)
2. Create `ProjectConfig` with name, local, peers, mode (`backend/projects.rs:160`)
3. Clone config -> add project -> `save_config()` (`backend/projects.rs:160`)
4. Update `Inner.config` to candidate (`backend/projects.rs:160`)
5. Start project watcher (`backend/mod.rs:1220` `start_project_watcher`)

**Success Criterion**: Project appears in `config.projects`; watcher running.

**Failure Handling**:
- Send failure: remove from `outbound_project_mappings`, return `Err` (`backend/mod.rs:721` `request_project_mapping`)
- Ack rejected: return `AisyncError::Config` with rejection message (`backend/projects.rs:160` `process_project_mapping_acks`)
- `save_config()` failure: propagates; in-memory config NOT updated (candidate pattern)

**Side Effects**: Tray refresh on ack processed.

**Rollback Boundary**: On send failure, `outbound_project_mappings` entry cleaned. On `save_config()` failure in ack processing, in-memory config unchanged.

---

### A4. Confirm Project Mapping Request (receiver side)

**Modules touched**: `projects.rs` (`confirm_project_mapping_request`) + `transport.rs` (`send_project_mapping_ack`) -> `mod.rs` (`start_project_watcher` HUB).

**Entry**: `commands.rs:823` `confirm_project_mapping_request` -> `backend/projects.rs:39`

**Preconditions**:
- `request_id` must exist in `Inner.project_mapping_requests` (`backend/projects.rs:39` `confirm_project_mapping_request`)
- Project name must not already exist (`backend/projects.rs:39`)

**State Changes**:
1. Auto-create local dir if missing (`backend/projects.rs:39` `confirm_project_mapping_request`)
2. Build candidate config with new `ProjectConfig` (`backend/projects.rs:39`)
3. Send `ProjectMappingAckPayload` to peer via `transport.rs` `send_project_mapping_ack` (`backend/transport.rs:117`)
4. `save_config()` candidate (`backend/projects.rs:39`)
5. Update `Inner.config` (`backend/projects.rs:39`)
6. Remove from `project_mapping_requests` (`backend/projects.rs:39`)
7. Start project watcher for new project (`backend/mod.rs:1220` `start_project_watcher`)

**Success Criterion**: Project in config; ack sent; watcher started.

**Failure Handling**: Ack send failure propagates before config is saved -- config stays unchanged.

**Rollback Boundary**: Ack is sent BEFORE config is saved (see D2 Ack-Before-Save Pattern, both steps in `backend/projects.rs:39` `confirm_project_mapping_request`). If ack succeeds but `save_config()` fails, peer thinks mapping is established but receiver has no record. The auto-created local dir remains.

---

### A5. Delete Project

**Modules touched**: `projects.rs` (`delete_project`).

**Entry**: `commands.rs:1940` `delete_project` -> `backend/projects.rs:290`

**Preconditions**: Project must exist in config (`backend/projects.rs:290` `delete_project`).

**State Changes**:
1. Clone config -> retain projects where name != target (`backend/projects.rs:290` `delete_project`)
2. `save_config()` (`backend/projects.rs:290`)
3. Update `Inner.config` (`backend/projects.rs:290`)
4. Remove project watcher from `Inner.project_watchers` (`backend/projects.rs:290`)

**Success Criterion**: Project absent from config; watcher stopped.

**Failure Handling**: `save_config()` failure propagates; in-memory config NOT updated (candidate pattern).

**Rollback Boundary**: Clean -- candidate pattern ensures no partial state.

**Resources NOT Cleaned**: Local files, sync snapshots in config for this project (orphaned keys), history entries. Peer's project mapping is NOT notified/removed.

---

### A6. Add Workspace (request + ack flow)

**Modules touched**: `workspaces.rs` (`add_workspace`, `process_workspace_mapping_acks`) + `transport.rs` (`send_workspace_mapping_request`) + `mod.rs` (`workspace_children`, `start_workspace_watcher`, `seed_session_baselines_for_workspace` HUB) + `auto_sync_orchestration.rs` (`run_workspace_auto_sync_outcome`) -> `sync_push.rs` (`run_workspace_tcp_push`) + `history.rs` (`record_auto_workspace_child_history`) + `auto_sync_gate.rs` (`finish_auto_sync`).

**Entry**: `commands.rs:1838` `add_workspace` -> `backend/workspaces.rs:79`

**Preconditions**:
- `local_root` must be a directory (`backend/workspaces.rs:79` `add_workspace`)
- Workspace name must not exist (`backend/workspaces.rs:79`)
- Peer must exist in config (`backend/workspaces.rs:79`)
- Local serve daemon running (`backend/workspaces.rs:79`)

**State Changes (request)**:
1. Scan children via `workspace_children()` (`backend/mod.rs:1192` `workspace_children`)
2. Insert `OutboundWorkspaceMapping` in `Inner.outbound_workspace_mappings` (`backend/workspaces.rs:79` `add_workspace`)
3. Send `WorkspaceMappingRequestPayload` to peer via `transport.rs` `send_workspace_mapping_request` (`backend/transport.rs:128`)

**State Changes (ack processing, `process_workspace_mapping_acks` at `backend/workspaces.rs:349`)**:
1. Remove from `outbound_workspace_mappings` (`backend/workspaces.rs:349` `process_workspace_mapping_acks`)
2. Create `WorkspaceConfig` with children, modes, root dirs (`backend/workspaces.rs:349`)
3. Replace workspace in candidate config -> `save_config()` (`backend/workspaces.rs:349`)
4. Update `Inner.config` (`backend/workspaces.rs:349`)
5. Start workspace watcher (`backend/mod.rs:1439` `start_workspace_watcher`)
6. Run initial auto-sync via `run_workspace_auto_sync_outcome()` (`backend/auto_sync_orchestration.rs:167`)
7. Seed session baselines for workspace (`backend/mod.rs:1799` `seed_session_baselines_for_workspace`)
8. Record auto-sync history for workspace and children (`backend/history.rs:580` `record_auto_workspace_child_history`)
9. `finish_auto_sync()` releases gate (`backend/auto_sync_gate.rs:168` `finish_auto_sync`)

**Success Criterion**: Workspace in config; watcher running; initial sync completed.

**Failure Handling**:
- Send failure: remove `outbound_workspace_mappings` entry, return `Err` (`backend/workspaces.rs:79` `add_workspace`)
- Ack rejected: return error (`backend/workspaces.rs:349` `process_workspace_mapping_acks`)
- Initial sync failure: records failure history, returns `Err`, `finish_auto_sync()` releases gate (`backend/workspaces.rs:349` + `backend/auto_sync_gate.rs:168`)

**Side Effects**: Initial sync triggered automatically on requester side after ack. History entries created.

**Rollback Boundary**: Config is saved before initial sync. If initial sync fails, workspace exists in config but has no sync history.

---

### A7. Confirm Workspace Mapping Request (receiver side)

**Modules touched**: `workspaces.rs` (`confirm_workspace_mapping_request`) + `peers.rs` (`persist_peer_connection`) + `transport.rs` (`send_workspace_mapping_ack`) + `mod.rs` (`start_workspace_watcher` HUB) + `auto_sync_gate.rs` (`enqueue_workspace_first_propagation`).

**Entry**: `commands.rs:1173` `confirm_workspace_mapping_request` -> `backend/workspaces.rs:210`

**Preconditions**: Request ID must exist in `Inner.workspace_mapping_requests` (`backend/workspaces.rs:210` `confirm_workspace_mapping_request`).

**State Changes**:
1. Auto-create `local_root` if missing (`backend/workspaces.rs:210` `confirm_workspace_mapping_request`)
2. Persist peer connection (cert + endpoint) via `peers.rs` `persist_peer_connection` (`backend/peers.rs:466`)
3. Build `WorkspaceConfig` with child names from request (`backend/workspaces.rs:210`)
4. Replace workspace in candidate config (`backend/workspaces.rs:210`)
5. Send `WorkspaceMappingAckPayload` via `transport.rs` `send_workspace_mapping_ack` (`backend/transport.rs:139`)
6. `save_config()` (`backend/workspaces.rs:210`)
7. Update `Inner.config` (`backend/workspaces.rs:210`)
8. Remove from `workspace_mapping_requests` (`backend/workspaces.rs:210`)
9. Start workspace watcher (`backend/mod.rs:1439` `start_workspace_watcher`)
10. Enqueue first propagation for auto-sync bypass via `auto_sync_gate.rs` `enqueue_workspace_first_propagation` (`backend/auto_sync_gate.rs:178`)

**Failure Handling**: Ack send failure propagates before config saved.

**Rollback Boundary**: Same as A4 -- ack sent before config saved.

---

### A8. Push (Manual Sync)

**Modules touched**: `split_brain.rs` (`run_sync`, `inject_excludes`/`restore_excludes`) -> `sync_push.rs` (`run_tcp_push`) -> `transport.rs` (`peer_transport_connection`) + `session_stage.rs` (`prepare_claude_session_sync`/`prepare_codex_session_sync`). NOTE: `run_sync` holds the `Inner` lock ACROSS the `run_tcp_push` network I/O (documented Rule1 exception in split_brain.rs).

**Entry**: `commands.rs:1454` `start_sync` -> `commands.rs:1504` `spawn_sync()` (worker thread) -> `backend/split_brain.rs:73` `run_sync()`

**Preconditions**:
- Project must exist with a peer mapping (`backend/split_brain.rs:73` `run_sync`)
- Direction `LocalToRemote` only for TCP push (pull not implemented, `backend/split_brain.rs:73`)
- Peer must be reachable (endpoint resolution in `transport.rs` `peer_transport_connection`, `backend/transport.rs:286`)

**State Changes (full push chain)**:

1. **Sensitive file exclusion** (`backend/split_brain.rs:73` `run_sync`, via `inject_excludes` `backend/split_brain.rs:362`): Scan sensitive files, compute unconfirmed set, inject into project exclude rules
2. **Resolve peer transport** (`backend/split_brain.rs:73` `run_sync`): Resolve endpoint + TLS cert from config/mDNS
3. **`run_tcp_push()`** (`backend/sync_push.rs:20`):
   a. `peer_transport_connection()` -- resolve connection details (`backend/transport.rs:286`)
   b. `prepare_claude_session_sync()` (`backend/session_stage.rs:53`) -- parse sessions, rewrite paths, stage to temp dir
   c. `prepare_codex_session_sync()` -- same for Codex (`backend/session_stage.rs:190`)
   d. Create ephemeral tokio runtime (`backend/sync_push.rs:20` `run_tcp_push`)
   e. **Code sync**: `TcpTransporter::connect_to_peer()` -> `sync_directory_to(source, remote_code_dir)` -> `shutdown()` (`backend/sync_push.rs:20`)
   f. **Session sync** (per tool): new TLS connection -> `sync_directory_to(staged_project_dir, remote_project_dir)` -> `shutdown()` (`backend/sync_push.rs:20`)
   g. **Staging cleanup**: `fs::remove_dir_all(staging_root)` for each session plan (`backend/sync_push.rs:20`)
   h. **Snapshot persistence**: Compute `manifest_hash(code_manifest)`, write `SyncSnapshot` to disk config via `load_config -> set_sync_snapshot -> save_config` (`backend/sync_push.rs:20`)
4. **Restore exclude rules** in memory (`backend/split_brain.rs:376` `restore_excludes`)
5. **Sync snapshot back to memory** (`backend/split_brain.rs:73` `run_sync`): `load_config()` -> `set_sync_snapshot()` on in-memory config
6. **Record sync history** (`commands.rs:1665-1675`): Append to `history.jsonl`
7. **Emit progress events** to frontend (`commands.rs:1637-1658`)

**Success Criterion**: Code and session files transferred to peer; snapshot persisted; history recorded.

**Failure Handling**:
- Transport error (connect/send): propagates as `Err`; staging directories cleaned in all cases (session plans loop in `backend/sync_push.rs:20` `run_tcp_push`)
- `save_config()` for snapshot failure: logged but NOT fatal (`backend/sync_push.rs:20` `run_tcp_push`)
- Failure history recorded (`commands.rs:1707-1737`)
- Exclude rules always restored regardless of outcome (`backend/split_brain.rs:376` `restore_excludes`)

**Side Effects**: Tauri `sync-progress` events emitted. History appended. Snapshot updated on disk.

**Rollback Boundary**:
- **Session staging**: Always cleaned (`remove_dir_all`), even on failure
- **Exclude injection**: Always restored
- **Snapshot**: Written ONLY on success; failure leaves previous snapshot intact
- **Transferred files on peer**: NOT rolled back on partial failure -- peer may have received some files via `sync_directory_to` before error

**Resources Created/Destroyed**:
- Created then destroyed: `~/.aisync/.aisync-session-stage-{nanos}/` (staging dir)
- Created on success: Updated `SyncSnapshot` in config

---

### A9. Pull (Manual Sync)

**Entry**: `commands.rs:1454` `start_sync` with direction=pull

**Modules touched**: `split_brain.rs` (`run_sync`).

**Current State**: NOT IMPLEMENTED. Returns `AisyncError::Transport("pull over TCP requires a remote control channel...")` (`backend/split_brain.rs:73` `run_sync`).

---

### A10. File Transfer

**Modules touched**: `file_transfer.rs` (`request_file_transfer`, `process_file_transfer_acks`, `prepare_default_file_transfer_accept`, `receive_file_transfer_data`, `record_file_transfer_history`) + `transport.rs` (`send_file_transfer_request`/`send_file_transfer_data`/`send_file_transfer_ack`). Receiver side driven from `serve.rs` `start_serve_daemon`.

**Entry**: `commands.rs:889` `request_file_transfer` -> `backend/file_transfer.rs:40` `request_file_transfer()`

**Preconditions**:
- Path must point to a file (not directory) (`backend/file_transfer.rs:40` `request_file_transfer`)
- Must have a filename (`backend/file_transfer.rs:40`)
- Sensitive file check must pass (user-confirmed or non-sensitive) (`backend/file_transfer.rs:40`)
- Peer endpoint resolvable (`backend/file_transfer.rs:40`)
- Local serve daemon running (`backend/file_transfer.rs:40`)

**State Changes**:
1. Insert `OutboundFileTransfer` in `Inner.outbound_file_transfers[transfer_id]` (`backend/file_transfer.rs:40` `request_file_transfer`)
2. Send `FileTransferRequestPayload` to peer via `transport.rs` `send_file_transfer_request` (`backend/transport.rs:161`)

**State Changes (ack processing, `process_file_transfer_acks` at `backend/file_transfer.rs:324`)**:
1. Remove from `outbound_file_transfers` (`backend/file_transfer.rs:324` `process_file_transfer_acks`)
2. `send_file_transfer_data()` -- stream file content to peer (`backend/transport.rs:264`)
3. Record history to `file_transfer_history.jsonl` via `record_file_transfer_history` (`backend/file_transfer.rs:594`)

**State Changes (receiver side, in serve daemon)**:
1. `prepare_default_file_transfer_accept()` -- compute target path, create `FileReceiveState` (`backend/file_transfer.rs:378`)
2. Auto-accept: send `FileTransferAckPayload` back to sender via `transport.rs` `send_file_transfer_ack` (`backend/transport.rs:253`)
3. `receive_file_transfer_data()` -- write chunks to tmp file, rename to target on completion (`backend/file_transfer.rs:518`)

**Failure Handling**:
- Send failure: remove from `outbound_file_transfers`, return `Err` (`backend/file_transfer.rs:40` `request_file_transfer`)
- Ack rejected: return `AisyncError::Transport` (`backend/file_transfer.rs:324` `process_file_transfer_acks`)
- Auto-accept failure on receiver: fall back to `pending_file_transfer_requests` queue for manual UI acceptance (`backend/file_transfer.rs:378` `prepare_default_file_transfer_accept`)
- Receiver removes `FileReceiveState` on ack send failure (`backend/serve.rs:60` `start_serve_daemon`)

**Resources Created**: Tmp file at `target_path.with_extension("aisync-ft-{id}.tmp")`; final file at target_path on success. Tmp file NOT explicitly cleaned on failure.

---

### A11. Settings Changes

**Modules touched**: all point-lock setters stayed in the `mod.rs` HUB; A11b's scanner effect is observed in `session_scanner.rs` (`start_session_mtime_scanner`).

#### A11a. Set Device Name
**Entry**: `commands.rs:635` `save_settings` -> `backend/mod.rs:545` `set_device_name()`

**State Changes**: Update `config.device.name`, `save_config()` (`backend/mod.rs:545` `set_device_name`). Also updates `discoverer` local device name.

#### A11b. Set Refresh Interval
**Entry**: `backend/mod.rs:550` `set_refresh_interval_secs()`

**State Changes**: Update `config.refresh_interval_secs`, `save_config()` (`backend/mod.rs:550` `set_refresh_interval_secs`). Effect on running session scanner: scanner reads config from disk each cycle (`backend/session_scanner.rs:120` `start_session_mtime_scanner`).

#### A11c. Set Default File Receive Dir
**Entry**: `commands.rs:1123` -> `backend/mod.rs:491` `set_default_file_receive_dir`

**State Changes**: `fs::create_dir_all(path)`, update `config.default_file_receive_dir`, `save_config()` (`backend/mod.rs:491` `set_default_file_receive_dir`).

#### A11d. Toggle Auto-Sync Paused
**Entry**: `commands.rs:1399` `set_auto_sync_paused` -> `backend/mod.rs:638`

**State Changes**: `Inner.auto_sync_paused = paused` (memory only, `backend/mod.rs:638` `set_auto_sync_paused`). NOT persisted -- lost on restart.

#### A11e. Complete Onboarding
**Entry**: `commands.rs:670` `complete_onboarding` -> `backend/mod.rs:561`

**State Changes**: Update device name, set `config.onboarded = true`, `save_config()` (`backend/mod.rs:561` `complete_onboarding`).

---

### A12. Resolve Split Brain

**Modules touched**: `split_brain.rs` (`resolve_split_brain`, `check_split_brain`) -> `split_brain.rs` (`run_sync`) -> `sync_push.rs` (`run_tcp_push`).

**Entry**: `backend/split_brain.rs:304` `resolve_split_brain()`

**Preconditions**: `check_split_brain()` (`backend/split_brain.rs:248`) returned `split_brain=true`.

**State Changes (PreferLocal)**:
- Delegates to `run_sync(..., confirm_overwrite=true)` (`backend/split_brain.rs:73` `run_sync`)
- Same as A8 but with `confirm_overwrite=true` -- peer backs up target dir to `.bak-{timestamp}` before merge

**PreferRemote**: NOT IMPLEMENTED. Returns `Err` (`backend/split_brain.rs:304` `resolve_split_brain`).

---

## B. System Operations

### B1. Auto-Sync (Project Watcher)

**Modules touched**: the gate-consumer loop is in `mod.rs` (`start_project_watcher`, HUB) -> `auto_sync_gate.rs` (`try_begin_auto_sync`/`finish_auto_sync`/`incoming_sync_recent`) -> `auto_sync_orchestration.rs` (`run_project_auto_sync`) -> `sync_push.rs` (`run_tcp_push`) + `history.rs` (`record_auto_sync_history`). (The FsWatcher spawn wrappers live in `watchers.rs`, but the gate consumer loop stayed in mod.rs.)

**Entry**: `backend/mod.rs:1220` `start_project_watcher()` spawns the consumer thread containing the gate loop

**Trigger**: FsWatcher detects file changes in project dir (after 2s debounce, `watcher.rs:10`)

**Preconditions (checked per event batch)**:
1. Project must be enabled (checked at watcher startup, `backend/mod.rs:1220` `start_project_watcher`)
2. Not within cooldown window (`suppress_until`, `backend/mod.rs:1220`)
3. Fingerprint must differ from last known (`backend/mod.rs:1220`)
4. No recent incoming sync for this root (`incoming_sync_recent()`, `backend/auto_sync_gate.rs:84`)
5. Auto-sync gate available (`try_begin_auto_sync()`, `backend/auto_sync_gate.rs:97`)
6. `Inner.auto_sync_paused` is checked implicitly via gate (scanner checks, watcher does not directly check `auto_sync_paused`)

**State Changes**:
1. `try_begin_auto_sync()` sets gate `in_flight=true` (`backend/auto_sync_gate.rs:97`)
2. `run_project_auto_sync()` -> `run_tcp_push()` (same chain as A8 without sensitive file exclusion, `confirm_overwrite=false`) (`backend/auto_sync_orchestration.rs:155` -> `backend/sync_push.rs:20`)
3. Post-sync: update `last_fingerprint` baseline (`backend/mod.rs:1220` `start_project_watcher`)
4. `record_auto_sync_history()` -- append to `history.jsonl` (`backend/history.rs:346`)
5. `finish_auto_sync()` sets gate `in_flight=false, cooldown_until=now+cooldown` (`backend/auto_sync_gate.rs:168`)
6. Set `suppress_until = now + cooldown` (`backend/mod.rs:1220` `start_project_watcher`)

**Failure Handling**: Record failure history (`backend/history.rs:346` `record_auto_sync_history`). Gate released. `suppress_until` set. Fingerprint updated to avoid immediate re-trigger.

**Side Effects**: Snapshot written to disk on success. History appended.

---

### B2. Auto-Sync (Workspace Watcher)

**Modules touched**: gate-consumer loop in `mod.rs` (`start_workspace_watcher`, HUB) -> `auto_sync_gate.rs` (`workspace_first_propagation_pending`) -> `auto_sync_orchestration.rs` (`run_workspace_auto_sync_outcome`) -> `sync_push.rs` (`run_workspace_tcp_push`) + `workspace_conflict.rs` (`analyze_workspace_conflicts`, per-child conflict analysis) + `split_brain.rs` (`persist_workspace_update`).

**Entry**: `backend/mod.rs:1439` `start_workspace_watcher()` spawns the consumer thread containing the gate loop

Same pattern as B1 but for workspaces. Key differences:
- Checks `workspace_first_propagation_pending()` for bypass cooldown (`backend/auto_sync_gate.rs:195`)
- Calls `run_workspace_auto_sync_outcome()` -> `run_workspace_tcp_push()` (`backend/auto_sync_orchestration.rs:167` -> `backend/sync_push.rs:173`)
- Workspace sync includes conflict analysis per child via `workspace_conflict.rs` `analyze_workspace_conflicts` (`backend/workspace_conflict.rs:18`)
- Conflicted children are skipped; safe children synced individually (`backend/sync_push.rs:173` `run_workspace_tcp_push`)
- Updates `WorkspaceChildConfig.last_fingerprint` and `conflicted` flags
- Persists updated workspace config to disk via `split_brain.rs` `persist_workspace_update` (`backend/split_brain.rs:62`)

---

### B3. Session Mtime Scanner

**Modules touched**: `session_scanner.rs` (`start_session_mtime_scanner`, `refresh_workspaces_in_config`, `session_mtime_targets`) + `mod.rs` HUB helpers (`refresh_and_save_workspaces`, `run_pending_workspace_first_propagations`); on trigger fans into the same B1/B2 chain (`auto_sync_orchestration.rs` -> `sync_push.rs`).

**Entry**: `backend/session_scanner.rs:120` `start_session_mtime_scanner()` -- infinite loop thread, no shutdown handle

**Trigger**: Polling loop with `sleep(config.refresh_interval_secs)` (default 30s)

**What it does each cycle** (`backend/session_scanner.rs:120` `start_session_mtime_scanner`):
1. `load_config()` from disk (NOT from in-memory cache) (`backend/session_scanner.rs:120`)
2. `refresh_and_save_workspaces()` -- detect new workspace children, save if changed (`backend/mod.rs:977` `refresh_and_save_workspaces`, backed by `refresh_workspaces_in_config` at `backend/session_scanner.rs:18`)
3. `run_pending_workspace_first_propagations()` -- trigger initial sync for newly mapped workspaces (`backend/mod.rs:1840` `run_pending_workspace_first_propagations`)
4. Enumerate `session_mtime_targets()` -- all (project+peer+tool) combinations needing session monitoring (`backend/session_scanner.rs:434` `session_mtime_targets`)
5. For each target: check mtime -> content fingerprint -> sync fingerprint -> incoming suppression -> auto-sync gate -> trigger sync if changed

**State Changes**: Same as B1/B2 on trigger. Additionally updates `seen`, `content_seen`, `sync_seen` HashMaps (local to scanner thread).

**Failure Handling**: Config load failure: use `fallback_config` clone from startup (`backend/session_scanner.rs:120` `start_session_mtime_scanner`).

**Known Bug**: No shutdown handle -- thread leaked until process exit (`S2 BUG-1`).

---

### B4. Device Discovery (mDNS)

**Entry**: `discovery/src/lib.rs:530` mDNS browser thread

**Trigger**: Continuous mDNS event listening + periodic stale peer pruning

**State Changes**:
- Update `SharedState.peers` (in-memory HashMap) with new/updated peer records
- Prune peers where `last_seen` exceeds `DEFAULT_OFFLINE_AFTER` (90s) (`discovery/lib.rs:27`)

**No disk persistence**: Peer discovery is ephemeral. Only pairing writes to `paired_peers.json`.

---

### B5. Incoming Push (Receive Daemon)

**Modules touched**: `serve.rs` (`start_serve_daemon` -- the daemon loop and all control-message handlers) + `history.rs` (`record_receiver_sync_history`) + `mod.rs` HUB (`refresh_and_save_workspaces`, `mark_incoming_session_roots`) + `auto_sync_gate.rs` (`mark_incoming_sync_root`) + `messaging.rs` (`record_text_message_history`) + `file_transfer.rs` (`prepare_default_file_transfer_accept`/`receive_file_transfer_data`).

**Entry**: `backend/serve.rs:60` `start_serve_daemon()` loop -> `ReceiveService::receive_once_with_control_handlers()`

**Trigger**: Inbound TLS TCP connection from peer

**State Changes (sync push received)**:
1. Transport layer: accept TLS, exchange Hello, receive manifest, compute diff, receive deltas/chunks/batches, commit staging to target dir
2. `record_receiver_sync_history()` (`backend/history.rs:453`):
   a. `refresh_and_save_workspaces()` -- detect new children (`backend/mod.rs:977`)
   b. `mark_incoming_sync_root()` -- set suppression window to prevent outbound auto-sync loop (`backend/auto_sync_gate.rs:77`)
   c. If session files received, `mark_incoming_session_roots()` (`backend/mod.rs:1001`)
   d. Append receiver history to `history.jsonl` (`backend/history.rs:453` `record_receiver_sync_history`)

**State Changes (control messages received -- all handlers in `backend/serve.rs:60` `start_serve_daemon`)**:
- `PairingRequest`: pushed to `pending_pairing_requests` queue
- `ProjectMappingRequest`: pushed to `pending_project_mapping_requests` queue
- `ProjectMappingAck`: pushed to `pending_project_mapping_acks` queue
- `WorkspaceMappingRequest`: pushed to `pending_workspace_mapping_requests` queue
- `WorkspaceMappingAck`: pushed to `pending_workspace_mapping_acks` queue
- `TextMessage`: normalize timestamp, record to `chat_history.jsonl` via `messaging.rs` `record_text_message_history` (`backend/messaging.rs:47`), push to `pending_text_messages` queue
- `FileTransferRequest`: auto-accept if default dir configured (`file_transfer.rs` `prepare_default_file_transfer_accept`, `backend/file_transfer.rs:378`), else push to `pending_file_transfer_requests`
- `FileTransferAck`: push to `pending_file_transfer_acks` queue
- `FileTransferData`: write chunk to `FileReceiveState.tmp_path`, rename to `target_path` on completion (`file_transfer.rs` `receive_file_transfer_data`, `backend/file_transfer.rs:518`)

**Failure Handling**: Transport errors logged, daemon continues loop (`backend/serve.rs:60` `start_serve_daemon`).

---

### B6. Workspace Scan (Auto-Discovery of Children)

**Modules touched**: `session_scanner.rs` (`refresh_workspaces_in_config`) + `mod.rs` HUB (`scan_workspace_direct`).

**Entry**: `refresh_workspaces_in_config()` (`backend/session_scanner.rs:18`) called from multiple places; also `scan_workspace_direct()` at `backend/mod.rs:1090`

**Trigger**: Called during session mtime scanner cycle, get_overview, workspace sync

**State Changes**: New subdirectories under `workspace.local_root` are added as `WorkspaceChildConfig` entries. If `auto_enable_new=true`, they start with `enabled=true`. Config saved to disk if changed.

---

## C. Push Complete Chain Detail

### C1. check_split_brain → check_target_not_empty → run_sync → run_tcp_push

The UI-driven push flow (manual sync from P2 detail page). Module crossing:
`split_brain.rs` (`check_split_brain`/`probe_target_status`/`run_sync`/`inject_excludes`/`restore_excludes`) ->
`sync_push.rs` (`run_tcp_push`) -> `transport.rs` (`peer_transport_connection`) + `session_stage.rs` (`prepare_claude_session_sync`).
`run_sync` holds the `Inner` lock across `run_tcp_push` (Rule1 exception); `probe_target_status` drops the guard before network.

```
UI calls check_split_brain(project, peer)     [commands.rs:1437]
  |                                            [backend/split_brain.rs:248 check_split_brain]
  +-> probe_target_status()                    [backend/split_brain.rs:171]
  |     Connect to peer -> send TargetStatusRequest -> receive TargetStatusResponse
  |     Response contains: not_empty, file_count, manifest_hash
  |
  +-> Compare manifest_hash vs SyncSnapshot.peer_last_known_hash  [backend/split_brain.rs:248 check_split_brain]
  |     No snapshot (first sync): split_brain=false
  |     Hash mismatch: split_brain=true
  |     Hash match: split_brain=false
  |
  +-> Return SplitBrainStatus to UI

UI interprets:
  split_brain=true -> show "以哪端为准" dialog
  peer_not_empty + no_snapshot -> show "目标非空，确认覆盖" dialog
  else -> proceed

UI calls start_sync(project, direction, confirmed_sensitive, confirm_overwrite)  [commands.rs:1454]
  |
  +-> spawn_sync() on worker thread                               [commands.rs:1520]
        |
        +-> backend.run_sync(project, peer, direction, confirmed, confirm_overwrite) [backend/split_brain.rs:73]
              |
              +-> Lock Inner, get project mapping                  [backend/split_brain.rs:73 run_sync]
              +-> Scan sensitive files, exclude unconfirmed         [backend/split_brain.rs:73 run_sync]
              +-> Inject per-run excludes                          [backend/split_brain.rs:362 inject_excludes]
              +-> run_tcp_push()                                   [backend/sync_push.rs:20]
              |     |
              |     +-> peer_transport_connection()                [backend/transport.rs:286]
              |     |     Resolve: config endpoint -> mDNS -> Tailscale
              |     |     Load pinned cert from ~/.aisync/peers/{id}-receiver.der
              |     |
              |     +-> prepare_claude_session_sync()              [backend/session_stage.rs:53]
              |     |     Parse sessions -> filter by project -> rewrite paths -> stage
              |     |     Creates: ~/.aisync/.aisync-session-stage-{nanos}/
              |     |
              |     +-> [tokio runtime]
              |     |     1. TLS connect -> sync_directory_to(code)   -> shutdown
              |     |     2. TLS connect -> sync_directory_to(session) -> shutdown
              |     |
              |     +-> Cleanup staging dirs                       [backend/sync_push.rs:20 run_tcp_push]
              |     +-> Persist SyncSnapshot to disk                [backend/sync_push.rs:20 run_tcp_push]
              |           load_config -> set_sync_snapshot -> save_config
              |
              +-> Restore exclude rules                            [backend/split_brain.rs:376 restore_excludes]
              +-> Sync snapshot back to in-memory config            [backend/split_brain.rs:73 run_sync]
              +-> Return SyncReport

        +-> Record history to history.jsonl                        [commands.rs:1665]
        +-> Emit sync-progress / sync-result events                [commands.rs:1637-1658]
```

### C2. Transport Protocol (inside sync_directory_to)

```
Client                          Server (ReceiveService)
  |  --- TLS Handshake ---         |
  |  --> Hello(version)            |
  |  <-- Hello(version)            |
  |  --> FileManifest(local)       |
  |  <-- FileManifest(remote)      |
  |  <-- FileSignatures(changed)   |
  |  --> FileDelta / FileChunk     |  (for modified files)
  |  --> FileBatch (tar)           |  (for small new files)
  |  --> FileDelete                |  (for files only on remote)
  |  --> SyncComplete              |
  |  --- TLS close_notify ---      |
```

Server-side commit_staging: received files are written to a staging directory, then atomically moved to the target directory on `SyncComplete`. If `confirm_overwrite=true`, the entire target directory is backed up to `{name}.bak-{timestamp}` before the merge.

---

## D. Failure Cleanup Summary

### D1. Resources Created Per Operation

| Operation | Resources Created | Cleaned on Failure |
|-----------|------------------|--------------------|
| confirm_pairing | `.der` cert file in `~/.aisync/peers/` | NO -- orphaned |
| add_project (local) | local dir (if create_local_dir=true) | NO -- left in place |
| run_tcp_push | staging dir `~/.aisync/.aisync-session-stage-{nanos}/` | YES -- `remove_dir_all` in all paths |
| run_tcp_push | SyncSnapshot in config | NOT written on failure |
| file_transfer (receiver) | tmp file `.aisync-ft-{id}.tmp` | NO -- orphaned on failure |
| workspace confirm | local_root dir (auto-created) | NO -- left in place |
| workspace ack + initial sync | workspace config entry | Left in config even if initial sync fails |
| serve daemon | TLS identity files `receiver.{cert,key}.der` | Persistent, not cleaned |

### D2. Rollback Patterns

**Candidate Config Pattern** (used by `add_project` `backend/projects.rs:223`, `delete_project` `backend/projects.rs:290`, `confirm_project_mapping_request` `backend/projects.rs:39`, `process_project_mapping_acks` `backend/projects.rs:160`, `process_workspace_mapping_acks` `backend/workspaces.rs:349`):
- Clone config -> mutate clone -> `save_config(clone)` -> if success, `Inner.config = clone`
- On `save_config()` failure: in-memory config unchanged

**Non-Rollback Pattern** (used by `confirm_pairing` `backend/peers.rs:300`, `set_device_name` `backend/mod.rs:545`, `set_refresh_interval_secs` `backend/mod.rs:550`):
- Mutate `Inner.config` in place -> `save_config()`
- On failure: in-memory already mutated, disk may be stale

**Ack-Before-Save Pattern** (used by project/workspace mapping confirm on receiver side, `backend/projects.rs:39` `confirm_project_mapping_request` and `backend/workspaces.rs:210` `confirm_workspace_mapping_request`):
- Send ack to peer BEFORE saving config locally
- Risk: peer receives ack (thinks mapping established) but local `save_config()` fails

### D3. Cross-Operation Atomicity Gaps

1. **Pairing dual-write** (`S1 BUG-4`): `discoverer.confirm_pairing()` writes `paired_peers.json`; `persist_peer_connection()` writes `config.toml`. Either can fail independently.

2. **Snapshot memory/disk desync** (`S1 BUG-1`): `run_tcp_push()` writes snapshot to disk via fresh `load_config()`; caller manually syncs back to memory. Auto-sync paths in watcher threads use their own `load_config()` each cycle, bypassing the Backend's in-memory config entirely.

3. **Config concurrent write** (`S1 BUG-2`): No file lock. `save_config()` from watcher thread, manual sync, and UI operations can interleave. The `load_config -> modify -> save_config` pattern is non-atomic.

4. **Session staging is safe**: Each sync creates a unique staging dir keyed by `unix_nanos_now()`, and cleanup runs in all code paths (success and error).
