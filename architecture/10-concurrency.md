# 10 - Concurrency & Atomicity Model

This document inventories every shared-mutable lock/static, every long-lived thread/task, the lock-ordering discipline that keeps the system deadlock-free, the transaction boundaries (what is atomic vs. what has a race window), and the concrete interleavings behind the known concurrency bugs. It cross-references 01-state-model.md (storage layers, BUG-1..6) and 06-operations.md (per-operation rollback boundaries). Accuracy over brevity: every claim carries a `file:line` reference verified against the current source. (The former 9667-line god-file `backend.rs` is now a directory `backend/` = `mod.rs` HUB + 20 submodules; references below cite `backend/<module>.rs`.)

---

## Shared Mutable State Inventory

All shared mutable state in the running process. There is **no `RwLock` anywhere** in the workspace; every guard is a `std::sync::Mutex` or an `AtomicBool`. Poison handling is **inconsistent** (see §"Poisoning" below): `Backend.inner` and discovery `SharedState` use `.unwrap()`/`.expect()` (panic on poison); `EVENT_*` and the watcher-gate accessors that the daemon touches use `.ok()`/`if let Ok` (silent skip).

### Process-global statics (NOT scoped to a Backend instance — cross-ref 01-state-model.md BUG-3)

| Name | Type | Guards | Decl |
|------|------|--------|------|
| `INCOMING_SYNC_SUPPRESSIONS` | `OnceLock<Mutex<HashMap<PathBuf, Instant>>>` | anti-loop suppression: received-root → expiry instant | `backend/auto_sync_gate.rs:35` |
| `AUTO_SYNC_GATES` | `OnceLock<Mutex<HashMap<String, AutoSyncGate>>>` | per-`scope:name:peer` in-flight + cooldown gate | `backend/auto_sync_gate.rs:36` |
| `SESSION_BASELINE_SEEDS` | `OnceLock<Mutex<HashMap<String, SessionBaseline>>>` | mtime/content/sync fingerprint baselines for session change detection | `backend/auto_sync_gate.rs:37` |
| `WORKSPACE_PROPAGATION_BYPASS` | `OnceLock<Mutex<HashSet<String>>>` | one-shot first-propagation gate-key flags | `backend/auto_sync_gate.rs:38` |
| `AUTO_SYNC_COOLDOWN_OVERRIDE` | `OnceLock<Duration>` (NOT a Mutex) | test-only cooldown override; set-once, idempotent | `backend/auto_sync_gate.rs:21` |
| `EVENT_COUNTERS` | `OnceLock<Mutex<HashMap<String, u64>>>` | test-observability event counters | `backend/events.rs:18` |
| `EVENT_LOG` | `OnceLock<Mutex<Vec<RecordedEvent>>>` | test-observability ring buffer (cap 2000, drains oldest 1000 on overflow) | `backend/events.rs:39` |
| `ENV_LOCK` (test-only) | `OnceLock<Mutex<()>>` | serialize env-var manipulation in one test | `backend/mod.rs:3314` (`#[cfg(test)] mod tests`) |

All four gate/suppression map statics plus the cooldown override now live in `backend/auto_sync_gate.rs` (they are **process-global statics, not `Inner` fields**). Each map static has a lazy `get_or_init` accessor: `incoming_sync_suppressions()` (`backend/auto_sync_gate.rs:53`), `auto_sync_gates()` (`backend/auto_sync_gate.rs:57`), `session_baseline_seeds()` (`backend/auto_sync_gate.rs:61`), `workspace_propagation_bypass()` (`backend/auto_sync_gate.rs:65`). `AutoSyncGate` is `Clone + Copy` (`backend/auto_sync_gate.rs:41`); `SessionBaseline` is `Clone` (`backend/auto_sync_gate.rs:47`). The override is read by `auto_sync_cooldown()` (`backend/auto_sync_gate.rs:23`) which feeds both `finish_auto_sync`'s cooldown (`backend/auto_sync_gate.rs:168`) and `incoming_suppress_window()` (`backend/auto_sync_gate.rs:72`).

### Backend instance state

| Name | Type | Guards | Decl |
|------|------|--------|------|
| `Backend.inner` | `Mutex<Inner>` | the **central big-lock**: `config`, `config_path`, `discoverer`, `auto_sync_paused`, `serve`/`serve_shutdown`, 9 negotiation/watcher HashMaps | `backend/mod.rs:178` (`Backend` struct) |
| `pending_pairing_requests` | `Arc<Mutex<VecDeque<PairingRequestPayload>>>` | daemon→UI producer/consumer queue | `backend/mod.rs:178` (`Backend` struct) |
| `pending_project_mapping_requests` | `Arc<Mutex<VecDeque<…>>>` | daemon→UI queue | `backend/mod.rs:178` (`Backend` struct) |
| `pending_project_mapping_acks` | `Arc<Mutex<VecDeque<…>>>` | daemon→UI queue | `backend/mod.rs:178` (`Backend` struct) |
| `pending_workspace_mapping_requests` | `Arc<Mutex<VecDeque<…>>>` | daemon→UI queue | `backend/mod.rs:178` (`Backend` struct) |
| `pending_workspace_mapping_acks` | `Arc<Mutex<VecDeque<…>>>` | daemon→UI queue | `backend/mod.rs:178` (`Backend` struct) |
| `pending_text_messages` | `Arc<Mutex<VecDeque<…>>>` | daemon→UI queue | `backend/mod.rs:178` (`Backend` struct) |
| `pending_file_transfer_requests` | `Arc<Mutex<VecDeque<…>>>` | daemon→UI queue | `backend/mod.rs:178` (`Backend` struct) |
| `pending_file_transfer_acks` | `Arc<Mutex<VecDeque<…>>>` | daemon→UI queue | `backend/mod.rs:178` (`Backend` struct) |
| `file_receive_states` | `Arc<Mutex<HashMap<String, FileReceiveState>>>` | **three-way shared**: Backend methods, daemon request handler, data-write free fn | `backend/mod.rs:178` (`Backend` struct) |

The `Backend` struct + the single `Mutex<Inner>` live in `backend/mod.rs` (struct decl `backend/mod.rs:178`). `Inner` fields are declared at `backend/mod.rs:202` (the 9 HashMaps: `pairing_sessions`, `project_mapping_requests`, `outbound_project_mappings`, `workspace_mapping_requests`, `outbound_workspace_mappings`, `file_transfer_requests`, `outbound_file_transfers`, `project_watchers`, `workspace_watchers`). `self.inner.lock().unwrap()` is called at 60+ sites; every call uses `.unwrap()`, so a poisoned `inner` panics the caller. `Backend::Drop` (`backend/mod.rs:192`) uses `if let Ok(mut inner)` to avoid a double-panic during unwind.

### Per-call local Mutex (not long-lived shared state)

`analysis_slot: Arc<Mutex<Option<WorkspaceConflictAnalysis>>>` is created per workspace-sync call inside `run_workspace_tcp_push` (`backend/sync_push.rs:173`) to smuggle conflict analysis out of the async preflight closure: written `*preflight_slot.lock().unwrap() = Some(analysis.clone())`, read back via `analysis_slot.lock().unwrap().take()` (all within `run_workspace_tcp_push`, `backend/sync_push.rs:173…`). Held only for the assign/take.

### Discovery `SharedState` — four independent sibling Mutexes behind one `Arc`

| Name | Type | Guards | Decl |
|------|------|--------|------|
| `peers` | `Mutex<HashMap<DeviceId, PeerRecord>>` | live mDNS peer records (ephemeral) | `codebaton-discovery/src/lib.rs:167` |
| `service_names` | `Mutex<HashMap<String, DeviceId>>` | mDNS service-fullname → DeviceId index | `codebaton-discovery/src/lib.rs:168` |
| `callbacks` | `Mutex<Vec<PeerChangeCallback>>` | registered peer-change callbacks | `codebaton-discovery/src/lib.rs:169` |
| `paired_peers` | `Mutex<HashMap<DeviceId, PairedPeer>>` | paired-peer cache, mirrored to `paired_peers.json` | `codebaton-discovery/src/lib.rs:170` |

All four `.expect("… poisoned")` on lock (panic on poison), e.g. lock sites `peers` (`codebaton-discovery/src/lib.rs:1493-1497`), `service_names` (`codebaton-discovery/src/lib.rs:1501-1505`), `callbacks` (`codebaton-discovery/src/lib.rs:1534`), `paired_peers` (`codebaton-discovery/src/lib.rs:307-311, 478-487`). `shared: Arc<SharedState>` is cloned into the browser thread at `codebaton-discovery/src/lib.rs:525`.

### Stop flags (AtomicBool, not Mutex)

| Name | Type | Purpose | Decl |
|------|------|---------|------|
| `ServeShutdownHandle.stop` | `Arc<AtomicBool>` (SeqCst) | signal serve accept-loop to stop | `backend/serve.rs:39` (`ServeShutdownHandle`) |
| serve daemon `stop`/`stop_loop` | `Arc<AtomicBool>` (SeqCst) | loop-guard checked at top of accept loop | `backend/serve.rs:60` (`start_serve_daemon`) |
| `MdnsDiscoverer.stop_browser` | `Arc<AtomicBool>` (Relaxed) | signal mDNS browse loop to stop | `codebaton-discovery/src/lib.rs:178, 511, 531` |

### Transport — essentially no shared mutable lock state

`codebaton-transport` carries **no** global `Mutex`/`RwLock` for connection state. `ReceiveService { server, target_dir }` (`codebaton-transport/src/lib.rs:2110-2113`) owns its state per-instance; connection concurrency is tokio task-per-connection, not shared locks. The only `Arc` in the production path is the immutable rustls `Arc<CryptoProvider>` held read-only by `PinnedPeerCertVerifier` (`codebaton-transport/src/lib.rs:3090`), installed once as the process default by `ensure_crypto_provider` (`codebaton-transport/src/lib.rs:3074-3085`). The crate contains **no `Mutex` of any kind** — `grep -n Mutex` over `codebaton-transport/src/lib.rs` (its only source file) returns zero matches, which strengthens the no-shared-lock-state claim. (The `secrets: Mutex<HashMap<String, Vec<u8>>>` test secret-store double lives in a different crate, at `codebaton-discovery/src/lib.rs:1737` inside `#[cfg(test)] struct MemorySecretStore`.)

---

## Execution Contexts

### Threads spawned at `Backend::new()`

`Backend::new()` (`backend/mod.rs:240`) starts these long-lived contexts in order: `start_serve_daemon` (`backend/serve.rs:60`), `MdnsDiscoverer::new` + `.start()` (discovery crate), `start_project_watchers` (`backend/watchers.rs:8`), `start_workspace_watchers` (`backend/watchers.rs:18`), `start_session_mtime_scanner` (`backend/session_scanner.rs:120`).

| Context | Spawn | Lifetime / shutdown | Worker threads |
|---------|-------|---------------------|----------------|
| **Serve daemon accept-loop** | `std::thread::spawn` running `runtime.block_on`, inside `start_serve_daemon` (`backend/serve.rs:60`) | stop = `AtomicBool` (SeqCst) checked at loop top (in `start_serve_daemon`, `backend/serve.rs:60…`); woken by self-poke (`ServeShutdownHandle::shutdown`, `backend/serve.rs:46`). Cleaned by `Backend::Drop` (`backend/mod.rs:192`). | 1 OS thread + a dedicated **2-worker** multi-thread tokio runtime (in `start_serve_daemon`, `backend/serve.rs:60…`) |
| **mDNS browse loop** | `thread::spawn` at `codebaton-discovery/src/lib.rs:530` | `while !stop.load(Relaxed)` (`codebaton-discovery/src/lib.rs:531`); `stop()` stores true, shuts down mDNS daemon, **joins** the thread (`codebaton-discovery/src/lib.rs:229-242`); `Drop` calls `stop()` (`codebaton-discovery/src/lib.rs:491-495`). **Cleanly shut down.** | 1 OS thread (no tokio) |
| **Project watcher consumer** (per enabled project) | `std::thread::spawn` inside the **singular** `start_project_watcher` (`backend/mod.rs:1220`) — the gate-CONSUMER loop that stayed in mod.rs (the `start_project_watchers` spawn wrapper is in `backend/watchers.rs:8`) | `while let Ok(batch) = rx.recv()` (in `start_project_watcher`, `backend/mod.rs:1220…`); terminates **only** when the `FsWatcher` holding `tx` is dropped (channel disconnect). **No stop flag, no JoinHandle stored.** | 1 OS thread each |
| **Workspace watcher consumer** (per workspace) | `std::thread::spawn` inside the **singular** `start_workspace_watcher` (`backend/mod.rs:1439`) — gate-consumer loop in mod.rs (spawn wrapper `start_workspace_watchers` in `backend/watchers.rs:18`) | `while let Ok(batch) = rx.recv()` (in `start_workspace_watcher`, `backend/mod.rs:1439…`); same channel-disconnect-only termination. **No stop flag.** | 1 OS thread each |
| **Session-mtime scanner** | `std::thread::spawn` inside `start_session_mtime_scanner` (`backend/session_scanner.rs:120`) | infinite `loop { … sleep(interval_secs) }`; returns unit, **no JoinHandle, no stop flag**. **LEAKED until process exit** (cross-ref 06-operations.md B3, S2 BUG-1). | 1 OS thread (no tokio) |

`Backend::Drop` cleans up **only** the serve daemon (`backend/mod.rs:192`). It does NOT touch the session-mtime scanner (no handle) and does NOT explicitly stop watcher consumer threads — those die only when their `FsWatcher` is dropped as `Inner` is dropped, i.e. after `inner.lock()` succeeds and the struct goes away. The mDNS browse thread is stopped via `MdnsDiscoverer::Drop`. The test constructors `with_config` (`backend/mod.rs:350`) / `with_config_serving` (`backend/mod.rs:410`) start watchers but **not** the scanner.

### Watcher / FsWatcher internal thread

Each `FsWatcher` (codebaton-sync) owns a debounce worker thread spawned in `FsWatcher::start` (`codebaton-sync/src/watcher.rs:86-87`) with a proper `(stop_tx, stop_rx)` channel; `stop()` sends and **joins** the worker (`codebaton-sync/src/watcher.rs:96-103`); `Drop` calls `stop()` (`codebaton-sync/src/watcher.rs:106-111`). The debounce loop also breaks on raw-event `Disconnected`. This is the **only** watcher thread with an explicit join. Restarting a watcher on config change `remove`s the old entry from `Inner.project_watchers`/`workspace_watchers` (dropping the old `FsWatcher`, closing the channel, ending the old consumer thread) then inserts a fresh one (in the project/workspace CRUD methods, e.g. `add_project` in `backend/projects.rs:223`); a delete-only path just `remove`s (e.g. `delete_project` in `backend/projects.rs:290`).

### Ephemeral per-send threads + runtimes

| Context | Spawn | Join? |
|---------|-------|-------|
| `run_control_future` (all control sends: project/workspace mapping req+ack, text msg, file-transfer req) | `std::thread::spawn` + 2-worker runtime (`run_control_future`, `backend/transport.rs:89`) | **Yes** — synchronous `.join()` (in `run_control_future`, `backend/transport.rs:89…`) |
| `send_pairing_request_async` (pairing request) | `std::thread::spawn` + 2-worker runtime (`send_pairing_request_async`, `backend/transport.rs:54`) | **No** — fire-and-forget |
| File-transfer auto-accept ack | `std::thread::spawn` inside daemon request handler (`start_serve_daemon`, `backend/serve.rs:60…`) | **No** — fire-and-forget |
| Workspace-sync preflight runtime | 2-worker runtime `block_on` (in `run_workspace_tcp_push`, `backend/sync_push.rs:173…`) | n/a — runtime owned by caller |

The fire-and-forget threads (`send_pairing_request_async`, auto-accept) are **untracked and unbounded in count** — nothing limits how many can be in flight.

### GUI (Tauri) command-layer threads

`commands.rs` spawns short-lived threads that re-enter `Backend` via `app.state::<Backend>()`: a file-picker send that spawns+joins per path (`commands.rs:944-951`), and a fire-and-forget background sync thread for manual sync (`commands.rs:1520`) running push/pull off the UI thread.

### Thread → shared-state contact map

| Thread | Touches |
|--------|---------|
| GUI command threads | `Backend.inner`, pending queues (consume), `file_receive_states`, discovery `SharedState` (via `inner.discoverer`) |
| Serve daemon | the 8 pending `VecDeque`s (push, inside `start_serve_daemon` `backend/serve.rs:60…`) + `file_receive_states` (insert/remove, also in `start_serve_daemon`); writes disk directly via `record_receiver_sync_history` (`backend/history.rs:453`) |
| Project/workspace watcher threads | the 4 global statics only; `load_config`/`save_config` directly to disk (in the singular `start_project_watcher` `backend/mod.rs:1220` / `start_workspace_watcher` `backend/mod.rs:1439`). **Never touch `Inner`.** |
| Session-mtime scanner | the 4 global statics; `load_config`/`save_config` directly (in `start_session_mtime_scanner`, `backend/session_scanner.rs:120…`). **Never touches `Inner`.** |
| mDNS browse thread | discovery `SharedState.peers` + `service_names` (and `callbacks` on prune via `emit`) |

The critical structural fact: **the watcher/scanner threads bypass `Backend.inner` entirely** — they read and write `config.toml` on disk as free functions, so the `inner` Mutex does NOT serialize them against UI config mutations (this is the root of BUG-2; see §Transaction Boundaries).

---

## Lock Ordering & Deadlock Analysis

The design's deadlock-freedom rests on **never co-holding two locks**. Each lock is acquired in its own scope that ends before the next is taken.

### Rule 1: `Backend.inner` is never held across a network sync

The codebase consistently scopes `self.inner.lock()` inside a `{ … }` block that ends before any TCP push/sync runs. Example: `process_workspace_mapping_acks` (`backend/workspaces.rs:349`) computes a `(candidate, config_path, workspace, peer)` tuple inside a block that drops the `inner` guard, briefly re-locks `inner` for `live_connection_for_config_peer` (`backend/mod.rs:861`) (drops again), and only then runs `run_workspace_auto_sync_outcome` (`backend/auto_sync_orchestration.rs:167`) unlocked. Same shape in `process_file_transfer_acks` (`backend/file_transfer.rs:324`) — block ends, then network (per 06-operations.md A10). This is why `inner` and the gate/queue locks are never simultaneously held.

**The documented EXCEPTION (Rule-1 exception):** `run_sync` (`backend/split_brain.rs:73`) holds the `Inner` guard **across** the call into `run_tcp_push` (`backend/sync_push.rs:20`) network I/O. The lock is acquired in `split_brain.rs` and held while execution crosses the module boundary into `sync_push.rs` — a cross-file lock span. (By contrast `run_workspace_sync` `backend/split_brain.rs:25` and `probe_target_status` `backend/split_brain.rs:171` drop the guard before any network call.)

### Rule 2: queue lock → `inner` lock, never nested

In every `take_pending_*` / consumer method the pending-queue Mutex guard is dropped (the `pop_front` statement ends) **before** `self.inner.lock()` is acquired:
- `take_pending_file_transfer_request` (`backend/file_transfer.rs:187`): `pop_front()?` (guard dropped), THEN `inner.lock()`.
- `pending_file_transfers` (`backend/file_transfer.rs:199`): pops drain inside a `{ … }` block that drops the queue guard, THEN `inner.lock()`.
- `accept_file_transfer` (`backend/file_transfer.rs:217`): locks `inner` for the filename lookup and `file_receive_states` in separate sequential statements — never together.

Consistent global ordering: queue → inner, with no overlap.

### Rule 3: gate/suppression statics are point-mutations, never held across work

`AUTO_SYNC_GATES` is locked only inside `try_begin_auto_sync` (`backend/auto_sync_gate.rs:97`) / `begin_auto_sync_bypass_cooldown` (`backend/auto_sync_gate.rs:135`) / `finish_auto_sync` (`backend/auto_sync_gate.rs:168`) — a `retain` + `insert`, microseconds. The `in_flight` boolean **inside** the gate, not the held mutex, is the cross-thread guard: `try_begin_auto_sync` inserts `in_flight:true` and drops the lock; the sync then runs unlocked; `finish_auto_sync` re-locks to set `in_flight:false, cooldown_until:now+cooldown`. `INCOMING_SYNC_SUPPRESSIONS` (via `mark_incoming_sync_root` `backend/auto_sync_gate.rs:77` / `incoming_sync_recent` `backend/auto_sync_gate.rs:84`), `WORKSPACE_PROPAGATION_BYPASS` (via `enqueue_workspace_first_propagation` `backend/auto_sync_gate.rs:178` / `workspace_first_propagation_pending` `:195` / `clear_workspace_first_propagation` `:202`), and `SESSION_BASELINE_SEEDS` (accessed from `seed_session_baselines_for_workspace`, which STAYED in the HUB at `backend/mod.rs:1799`) are likewise locked only for a single map op — `seed_session_baselines_for_workspace` re-acquires the lock each loop iteration, never holding it across the loop.

### Rule 4: discovery — `service_names` and `peers` never co-held; three-lock sequences are sequential

- `remove_peer` (`codebaton-discovery/src/lib.rs:1492-1516`): lock `peers`, `remove` (guard dropped at statement end, `:1493-1497`); conditionally lock `service_names`, `remove` (`:1501-1505`); then `emit()` (`:1508`) which locks `callbacks`. Three locks **in sequence, never two at once**.
- `prune_stale_peers` (`codebaton-discovery/src/lib.rs:1518-1531`): collects stale ids under the `peers` lock inside a block (`:1519-1526`), drops it, THEN loops `remove_peer` — so `peers` is NOT held across `remove_peer`.
- `confirm_pairing`/`unpair`: lock `paired_peers` for the insert/remove (`:307-311` / `:348-353`), drop it, call `persist_pairings()` (which **re-locks** `paired_peers` to snapshot, `:478-487`, then `save_pairings` does the fs write with the lock dropped), then `emit()`. `paired_peers` is locked twice per op but **never held across the file write**.
- The `ServiceResolved` browser-thread arm locks `service_names` to insert (`:538-542`) then `upsert_peer` which locks `peers` — sequential, not nested.

### Latent deadlock: `emit()` holds `callbacks` while invoking callbacks (NOT reachable in production today)

`emit()` (`codebaton-discovery/src/lib.rs:1533-1538`) locks `shared.callbacks` and invokes every registered callback **while still holding the lock**: `let callbacks = shared.callbacks.lock()…; for callback in callbacks.iter() { callback(change.clone()); }`. `on_peer_change` (`codebaton-discovery/src/lib.rs:580-587`) also locks `callbacks` to push. If any callback re-entered the discoverer to register another callback or triggered another `emit`, it would self-deadlock. `emit` is reached from `confirm_pairing` (`:313`), `unpair` (`:357`), `remove_peer` (`:1508`), `upsert_peer`, and prune — all of which can run on the browser thread.

**Reachability:** verified that **no production code registers a peer-change callback**. `on_peer_change` is only called at `codebaton-discovery/src/lib.rs:2003` and `:2230`, both inside the crate's `#[cfg(test)]` module (`mod tests` begins at `:1711`, under the `#[cfg(test)]` attribute on `:1710`); `grep` for `on_peer_change`/`PeerChangeCallback` across `codebaton-app/src` and `codebaton-cli/src` returns nothing. So `callbacks` is always empty in production and the loop body never runs. This is a **latent** deadlock that becomes live the moment a GUI peer-change callback is wired in — flagged here so future work does not reintroduce it. (NEW, beyond 01-state-model.md BUG set.)

---

## Transaction Boundaries & Atomicity

This section states, per write path, **what is atomic and what is not**, with the exact race window.

### Atomicity primitives — three different commit implementations exist

| Writer | File(s) | Atomicity | Ref |
|--------|---------|-----------|-----|
| `save_config` | `config.toml` | **NON-atomic** — direct `fs::write` truncate-in-place, no tmp+rename, no fsync | `codebaton-sync/src/config.rs:358-365` |
| `SyncState::save` | `state.toml` | **NON-atomic** — direct `fs::write`, same as config | `codebaton-sync/src/lib.rs:427-433` |
| `save_pairings` | `paired_peers.json` | **Atomic** — tmp + `fs::rename` (Windows remove-then-rename), no fsync | `codebaton-discovery/src/lib.rs:1560-1574` |
| `write_file_atomic` | individual received file | **Atomic per file** — `.aisync-tmp` + `fs::rename`, no fsync | `codebaton-transport/src/lib.rs:3582-3593` |
| `commit_staging_with_options` | receiver target dir | **Per-file atomic, NOT directory-atomic** — loop of `write_file_atomic` then loop of `trash_file` | `codebaton-transport/src/lib.rs:3346-3401` |
| `commit_two_dirs` | sync-coordinator target dirs | **Directory-atomic WITH rollback** — rename-aside-backup + rename-into-place + restore on failure | `codebaton-sync/src/lib.rs:623-654` |

**Zero file-locking infrastructure** exists (no flock/fs2/fd_lock/advisory locks anywhere). All cross-process and cross-thread serialization of `config.toml` relies solely on the in-process `Backend.inner` Mutex — which the watcher/scanner threads bypass.

**Durability caveat (all "atomic" paths):** none of `write_file_atomic`, `save_pairings`, or `commit_two_dirs` calls `sync_all`/`sync_data` before the rename. They guarantee crash-consistent **visibility** of each file's contents but NOT **durability** against power loss (the tmp data may not be flushed before the rename's directory entry lands). (Gap noted by scout, confirmed: no `sync_all`/`sync_data` in these write paths.)

### Config read-modify-write — the dominant non-atomic pattern (BUG-2)

The prevailing config mutation is the **candidate-clone** pattern:
```
candidate = g.config.clone();   // snapshot the in-memory image
modify(candidate);
save_config(config_path, &candidate);   // writes the FULL snapshot to disk
g.config = candidate;
```
e.g. workspace-ack apply in `process_workspace_mapping_acks` (`backend/workspaces.rs:349`) (`let mut candidate = g.config.clone()` → `replace_workspace` `backend/mod.rs:1698` → `save_config` → `g.config = candidate.clone()`); same shape across the project/workspace/peer CRUD methods in `backend/projects.rs`, `backend/workspaces.rs`, and `backend/peers.rs`. **None of these re-reads disk first.** Because `save_config` writes a *whole-config snapshot* taken earlier, any field another thread persisted to disk between the clone and the save is **silently reverted** — this is worse than a single-field race: it reverts the entire on-disk config to a stale memory image.

The snapshot-writeback path is the other half: `run_tcp_push` (`backend/sync_push.rs:20`) does `load_config → set_sync_snapshot → save_config` with no lock around the load/save pair. `run_tcp_push` runs from both manual sync (`run_sync` `backend/split_brain.rs:73`) and auto-sync (`run_project_auto_sync` `backend/auto_sync_orchestration.rs:155`).

### What is atomic vs. not — summary

**Atomic:**
- A single received file is never observed half-written (`write_file_atomic`).
- `paired_peers.json` is never observed half-written (`save_pairings`).
- The sync-coordinator's two target dirs commit-or-rollback together (`commit_two_dirs`), modulo a tiny window between the two renames inside one call.
- The transport handshake: commit runs **before** `SyncComplete` is sent; on commit failure the staging dir is removed and `Message::Error` is returned to the client (`codebaton-transport/src/lib.rs:2055-2065`), and only on success is `SyncComplete` written (`:2081`). So the client never records a snapshot for a commit that did not land (protocol-level atomicity preserved even though the FS commit is per-file).
- Session staging dirs are unique per `unix_nanos_now()` and cleaned in all paths (06-operations.md A8 D3).

**NOT atomic (race/partial-failure windows):**
1. **`config.toml` truncate-in-place** — a crash mid-`fs::write` can leave a corrupt/truncated file (`config.rs:365`).
2. **load→modify→save** — separate syscalls; last-writer-wins between them (BUG-2).
3. **candidate-clone whole-config clobber** — reverts on-disk fields written by another thread between clone and save.
4. **`commit_staging_with_options` mid-loop** — if `write_file_atomic` fails at file *k* (`codebaton-transport/src/lib.rs:3381`), files `0..k` are already renamed in, `k+1..N` are not, and no deletions have run; no overall rollback (the safety-valve pre-check and recycle bin make it *recoverable*, not *atomic*). A backup is taken only when `confirm_overwrite=true` (`:3356`).
5. **commit_two_dirs → state.save split** — in the sync coordinator, `commit_two_dirs` (`codebaton-sync/src/lib.rs:304-309`) and `state.save` (`:325`) are sequential independent `?`-points; a crash after commit but before `state.save` leaves the filesystem updated while the version counter lags (idempotent re-sync, but disk and state diverge). config.toml and state.toml are saved as two uncoupled operations.
6. **pairing tri-write** — `confirm_pairing` touches `paired_peers.json` + `peers/<id>-receiver.der` + `config.toml` as three independent non-atomic operations (BUG-4, below).

### Concurrent-writer interleavings on `config.toml`

The independent writers of `config.toml` are: UI command threads (under `inner`), the manual-sync worker thread (`run_tcp_push` `backend/sync_push.rs:20`, NOT under `inner` during the load/save), the project/workspace watcher threads (free-function `load_config`/`save_config`, never under `inner`), `refresh_and_save_workspaces` (`backend/mod.rs:977`), `run_workspace_auto_sync_outcome` (`backend/auto_sync_orchestration.rs:167`), and the session-mtime scanner (`start_session_mtime_scanner` `backend/session_scanner.rs:120`). Because the watcher/scanner threads do not hold `inner`, the `inner` Mutex does **not** mediate UI-vs-watcher or watcher-vs-watcher config writes — they can interleave freely. (Whether two Tauri command invokes can truly run concurrently was not inspected; the `inner` Mutex serializes UI-vs-UI within a Backend regardless.)

---

## Known Concurrency Bugs & Races

Cross-referenced with 01-state-model.md BUG-1..6, each restated with the concrete interleaving and verified line numbers, plus one new latent deadlock.

### BUG-1: Snapshot memory/disk desync (01-state-model.md BUG-1)

`run_tcp_push` (`backend/sync_push.rs:20`) writes the post-sync `SyncSnapshot` **only to disk** via `load_config → set_sync_snapshot → save_config`, never to `Backend.inner.config`. The in-memory copy is reconciled only by `run_sync`'s explicit manual sync-back (in `run_sync`, `backend/split_brain.rs:73`), which exists solely for the manual path and runs only `if result.is_ok()`.

**Interleaving:** auto-sync (project watcher) calls `run_tcp_push` via `run_project_auto_sync` (`backend/auto_sync_orchestration.rs:155`) and **never** executes `run_sync`'s sync-back. So after an auto-sync, `Inner.config` holds a stale snapshot while disk holds the fresh one. `check_split_brain` (`backend/split_brain.rs:248`) reads the snapshot from the **in-memory** clone (`g.config.sync_snapshot`) and decides split-brain at `resp.manifest_hash != snap.peer_last_known_hash`. If a user triggers a split-brain check immediately after an auto-sync completed, it compares the peer's fresh manifest against the **stale in-memory** `peer_last_known_hash` → potential **false split-brain** (or a missed real one). The disk snapshot written by `run_tcp_push` (`backend/sync_push.rs:20`) is invisible to this check unless the manual sync-back ran.

### BUG-2: Config concurrent-write race / whole-config clobber (01-state-model.md BUG-2)

No file lock; `load_config → modify → save_config` is non-atomic and the candidate-clone pattern writes a full stale snapshot.

**Interleaving (clobber):** Thread A (UI `confirm_pairing` `backend/peers.rs:300`) does `candidate = g.config.clone()` then `save_config`. Concurrently Thread B (project watcher's `run_tcp_push` `backend/sync_push.rs:20`) does `load_config`, `set_sync_snapshot`, `save_config`. If A clones before B's save and A saves after B's save, A's write **reverts the snapshot B just persisted** (A's candidate never contained it). Symmetrically, B's `load_config` reading before A's save and B saving after A's save reverts A's new peer entry. The watcher threads never hold `inner`, so the `inner` Mutex does not prevent this — it only serializes the UI side. (See §Transaction Boundaries "candidate-clone whole-config clobber".)

### BUG-3: Global statics not scoped to Backend instance (01-state-model.md BUG-3)

The four gate/suppression maps plus `AUTO_SYNC_COOLDOWN_OVERRIDE` (all in `backend/auto_sync_gate.rs`), `EVENT_COUNTERS`, and `EVENT_LOG` (both in `backend/events.rs`) are `OnceLock` process-globals, not per-Backend fields. In tests that construct multiple Backends in one process, all instances share them.

**Interleaving:** with two Backends sharing `AUTO_SYNC_GATES`, gate keys are `scope:name:peer` (`auto_sync_gate_key`, `backend/auto_sync_gate.rs:93`) with no Backend identity. If two Backends sync the same project to the same peer name, their gates collide: Backend X's `try_begin_auto_sync` (`backend/auto_sync_gate.rs:97`) inserts `in_flight:true`, and Backend Y's `try_begin_auto_sync` then sees the live gate and **suppresses Y's sync** as `in_flight`. A code comment near the override (`backend/auto_sync_gate.rs`, around `auto_sync_cooldown` `:23`) notes the override is set once early with the same value across tests "so no parallel race"; tests sidestep gate collisions by using **unique project names** for isolation (mirrored in the `EVENT_COUNTERS` composite `event:<project|name>` keying in `record_event`, `backend/events.rs:41`). If production ever instantiated two Backends (multi-account), gate/suppression/baseline state would leak between them.

> Note: older docs (e.g. 01-state-model.md) cite these statics at `backend.rs:62-66`; after the Phase-1 refactor all four maps + the cooldown override now live in `backend/auto_sync_gate.rs:21,35-38`, no longer in the former `backend.rs`.

### BUG-4: Discovery paired-peers vs config-peers dual-write (01-state-model.md BUG-4)

`confirm_pairing` writes peer identity to **two independent stores in sequence with no transaction**, and `persist_peer_connection` adds a **third** write:
1. `g.discoverer.confirm_pairing(peer_id, …)` (inside `confirm_pairing` `backend/peers.rs:300`) → `persist_pairings()` writes `paired_peers.json` (atomically, `codebaton-discovery/src/lib.rs:312, 1560-1574`).
2. `persist_peer_connection(&mut g.config, …)` (`backend/peers.rs:466`) writes the receiver cert `.der` via `fs::write` (within `persist_peer_connection`, per 06-operations.md A1).
3. `save_config(&config_path, &cfg)` (inside `confirm_pairing` `backend/peers.rs:300`) writes `config.toml` (non-atomic).

**Interleaving / divergence window:** if the process dies or `save_config` fails between (1) and (3), `paired_peers.json` has the peer but `config.peers` does not — **permanent divergence with no repair** (the `paired_peers()` merge `backend/peers.rs:102` at read time handles it cosmetically but never reconciles to disk). The inverse (config has peer, json does not) is an **intended branch**: if `discoverer.confirm_pairing` fails, the code logs "persisting anyway" (in `confirm_pairing` `backend/peers.rs:300`) and still writes config. A partial failure can also leave a cert `.der` file with no config entry, or vice-versa (the `.der` path is only stored into `config.peers` and persisted by the later `save_config`).

### BUG-5: Watcher threads use stale fallback config (01-state-model.md BUG-5)

Each watcher/scanner thread holds a `fallback_config` clone taken at startup (in the singular `start_project_watcher` `backend/mod.rs:1220`, `start_workspace_watcher` `backend/mod.rs:1439`, and `start_session_mtime_scanner` `backend/session_scanner.rs:120`). On `load_config` failure it falls back: `load_config(&config_path).unwrap_or_else(|_| fallback_config.clone())` (same three functions). The same stale fallback appears in the post-sync re-reads (within the singular watcher consumer loops, per 01-state-model.md / scout). If `config.toml` is momentarily corrupt (a real possibility given BUG-1/BUG-2's truncate-in-place writes) the watcher proceeds with **arbitrarily outdated** project/peer mappings, and never updates `Inner.config` either way.

**Interleaving:** Thread A's `save_config` truncates `config.toml` (mid-`fs::write`, `config.rs:365`). Thread B (watcher) `load_config`s in that instant, hits a TOML parse error, and silently uses its startup `fallback_config` — potentially syncing with a deleted peer or a renamed project. The corruption is transient (A's write completes), but B already acted on stale state for that cycle.

### BUG-6: Session byte-identical-on-rewrite (01-state-model.md BUG-6)

Not a lock race; included for completeness. `ParsedSession`'s byte-identical round-trip holds only for **untouched** records; records marked `dirty` by `rewrite_structured_paths` are reserialized by serde_json (may reorder keys/whitespace). This is concurrency-adjacent only in that session staging is done off the UI thread (`prepare_claude_session_sync`); the original `.jsonl` files are never mutated during a push (01-state-model.md §3 L7), so there is no read/write race on session files. See 01-state-model.md BUG-6.

### NEW: `emit()` callbacks-lock self-deadlock (latent)

Described in detail in §Lock Ordering "Latent deadlock". `emit()` invokes callbacks while holding `shared.callbacks` (`codebaton-discovery/src/lib.rs:1533-1538`); a callback that re-enters `on_peer_change` or triggers `emit` self-deadlocks. **Currently unreachable** — no production callback is registered (`on_peer_change` only in `#[cfg(test)]`). Flagged so a future GUI peer-change subscription does not reintroduce a live deadlock.

### NEW: `receive_file_transfer_data` serializes ALL inbound chunks under one global lock

`receive_file_transfer_data` (`backend/file_transfer.rs:518`) locks `file_receive_states` and holds it **across the entire append-to-disk write** (`get_mut` → `OpenOptions::append` → `write_all` → `state.bytes_written += …`, all within `receive_file_transfer_data`). This is the **one** place a data-plane lock is held across an fs write. It serializes all concurrent inbound file chunks for **all** transfers (the lock is on the whole `HashMap`, not per-transfer), not just the one being written. Not a deadlock (single lock, dropped at function end) but a **throughput serialization point** under parallel inbound transfers. (Gap: the actual degree of inbound parallelism depends on how many data streams the daemon accepts concurrently via `receive_once_with_control_handlers`, which was not exhaustively mapped.)

---

## Poisoning — inconsistent blast radius

| Consumer | On poison |
|----------|-----------|
| `Backend.inner` (60+ `.lock().unwrap()` sites) | **panics** the caller |
| discovery `SharedState` (`.expect("… poisoned")`) | **panics** the locking thread (incl. browser thread) |
| gate/suppression accessors used by daemon & watchers (`mark_incoming_sync_root` `.unwrap()`, etc.) | **panics** |
| `EVENT_COUNTERS`/`EVENT_LOG` (`if let Ok` / `.ok()`, in `backend/events.rs` — `record_event` `:41`, `log_line` `:138`, `events_for` `:96`) | **silently skipped** |
| `Backend::Drop` (`if let Ok(mut inner)`, `backend/mod.rs:192`) | **silently skipped** (avoids double-panic during unwind) |

A thread that panics while holding `inner` or any discovery lock poisons it; the **next** `inner.lock().unwrap()` from any GUI command then panics too — a single mid-critical-section panic cascades to the whole UI. The event-log statics degrade gracefully instead. (Gap: not every panic-on-poison consumer was enumerated to fully bound the blast radius.)
