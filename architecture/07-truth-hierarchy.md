# 07 - 真相源层次（Truth-Source Hierarchy）

当同一份逻辑状态同时存在于多个层（内存缓存、磁盘文件、运行时临时态）时，**谁是权威？缓存与实际不一致时以谁为准？** 本文档逐层定义，并给出每个"缓存 vs 真相"冲突场景在代码中的实际胜负。

所有声明均经源码 `file:line` 核实（对抗性 verifier 逐条复核）。引用路径相对仓库根。

> 取证方法：每个维度 2 个独立 reader 直接读 Rust 源码 → 1 个对抗性 verifier 逐条 OPEN 文件核对行号是否属实 → 汇总。

---

## 1. 存储层清单（按权威级别）

| 层 | 类型 | 权威 | 从何加载 | 持久化到 | 关键引用 |
|----|------|------|---------|---------|---------|
| **config.toml on disk** | disk-truth | **truth** | `load_config` 读盘+解析 | 自身（`save_config`），被前台命令和后台线程共同写 | `config.rs:345-356`, `config.rs:358-367` |
| Inner.config（内存 SyncConfig） | memory-cache | **cache** | `Backend::new` 启动时 `load_config` 一次 (`backend/mod.rs:350`)，存入 `backend/mod.rs:350` | 每次变更后同锁内 `save_config` 全量写盘 | `backend/mod.rs:202`, `backend/peers.rs:75` |
| **SyncSnapshot**（`ProjectConfig.sync_snapshots`，嵌在 config.toml） | disk-truth | **truth** | config.toml 一部分；`sync_snapshot()` 读 | `run_tcp_push` 内 `set_sync_snapshot`+`save_config` (`backend/sync_push.rs:20`) | `config.rs:130-149`, `config.rs:186-195` |
| **state.toml**（`ProjectVersionState`：版本号、指纹、has_synced） | disk-truth | **truth（仅 SyncCoordinator 路径）** | `SyncState::load` 仅在 `SyncCoordinator::new` 调用 (`lib.rs:80`) | `record_success` 后 `state.save` (`lib.rs:322-325`) | `lib.rs:411-435`, `backend/sync_push.rs:20` |
| **paired_peers.json** | disk-truth | **truth（配对身份）** | `load_pairings` 反序列化 (`discovery lib.rs:1544-1558`) | `save_pairings` 原子 tmp+rename (`lib.rs:1560-1575`)；仅存 id/name/os、public_key、paired_at，**无 endpoint/cert** | `discovery lib.rs:73-77`, `discovery lib.rs:1544-1575` |
| Discovery `SharedState.paired_peers` | memory-cache | **cache** | `MdnsDiscoverer::new` 时 `load_pairings` 装入 | `persist_pairings`→`save_pairings`（confirm/unpair 时） | `discovery lib.rs:170`, `discovery lib.rs:478-488` |
| **config.peers**（`PeerConfig`：endpoint/server_cert/server_name/last_seen） | disk-truth | **truth（传输路由）** | config.toml 一部分 | config.toml；`last_seen` **永远写 None**，从不填充 | `config.rs:164-176`, `backend/peers.rs:75`, `backend/mod.rs:1073` |
| Discovery `SharedState.peers`（live `PeerRecord`，含 `last_seen: Instant`、`removed`） | ephemeral | **evidence** | mDNS `ServiceResolved`→`upsert_peer` 运行时填充 | 不持久化；`prune_stale_peers` 按 last_seen 老化 | `discovery lib.rs:149-157`, `discovery lib.rs:1518-1531` |
| `PeerConnectionInfo`（live endpoint/cert/server_name，在 `peers[].connection`） | ephemeral | **evidence** | mDNS TXT/探测经 `upsert_peer` 设置 | 不持久化；配对时快照进 config.peers，sync 时作为 live 覆盖优先 | `discovery lib.rs:468-476`, `backend/transport.rs:286` |
| `ConfigStore`（path/config/last_modified，mtime 门控） | memory-cache | **cache（死代码）** | `ConfigStore::load`；`reload_if_changed` 仅 mtime 变化时重载 | `ConfigStore::save`。**仅被 re-export，app/runtime 从不实例化**；Backend 用裸 Inner.config + load/save | `config.rs:305-343`, `sync lib.rs:19` |
| Workspace 子项目指纹（`WorkspaceChildConfig.last_fingerprint`/`conflicted`，嵌 config.toml） | disk-truth | **truth** | config.toml；`refresh_workspaces_in_config` 重算 | `refresh_and_save_workspaces`（独立 load+save）/ `run_workspace_auto_sync_outcome` | `config.rs:241-252`, `backend/mod.rs:977` |
| **Session .jsonl on disk**（`~/.claude/projects/*/*.jsonl`） | disk-truth | **truth** | 自身：`parse_session_file` 逐行解析 | 自身：`write_session`/`serialize_session` 重发 | `claude_code.rs:323-360`, `claude_code.rs:275` |
| `ParsedSession`（内存解析：`RecordLine{raw,value,dirty}`） | derived | **derived** | `parse_session_file` 保留每行 raw bytes + parsed Value，dirty=false | `serialize_session`：clean 记录原样发 raw，dirty 记录重新序列化，保留 trailing_newline | `claude_code.rs:35-85`, `claude_code.rs:370-383` |
| 传输接收 staging dir（`.aisync-staging-<nanos>`） | ephemeral | **derived** | `prepare_staging` 把 target_dir 现有内容复制进新 sibling | `commit_staging_with_options` 增量逐文件覆盖 + trash 删除后移除 staging | `transport lib.rs:3245-3266`, `transport lib.rs:3346-3401` |
| 全局静态 map（`AUTO_SYNC_GATES`/`INCOMING_SYNC_SUPPRESSIONS`/`SESSION_BASELINE_SEEDS`/`WORKSPACE_PROPAGATION_BYPASS`） | ephemeral | **evidence** | 懒初始化为空 | 不持久化；纯运行时协调，重启丢失 | `backend/auto_sync_gate.rs:53`, `backend/auto_sync_gate.rs:57` |
| Session-mtime scanner 本地 map（`seen`/`content_seen`/`sync_seen`）+ scanner `fallback_config` | ephemeral | **cache** | 每线程 HashMap；spawn 时传入 fallback_config clone | 不持久化；scanner 每轮重读盘 config，从不碰 Inner.config | `backend/session_scanner.rs:120`, `backend/mod.rs:977` |
| `chat_history.jsonl` / `history.jsonl` | append-only-evidence | **evidence** | `read_jsonl` | 追加写；过去事件的证据日志，非 config 缓存 | `backend/history.rs:170`, `backend/history.rs:157` |

---

## 2. 优先级规则（权威链，按场景）

> 这些是从代码实际行为归纳的"谁赢"规则。每条都是判断 bug 的基准——若观察到与规则不符的行为，即为缺陷。

### 规则 1：config 的常态——内存领先，磁盘跟随（全量写）

每个会改 config 的 Backend 方法都在 mutex 内改 `Inner.config`，并**立即把整份内存 config `save_config` 写盘**——磁盘是内存的序列化。之后**从不**把磁盘重读回 Inner.config。前台读用 `config() = inner.config.clone()` (`backend/mod.rs:514`)，直接读 `g.config`。
- 证据：`connect_peer`/`add_peer`（clone→改→save）`backend/peers.rs:75`；`config_with_refreshed_workspaces`（`g.config=refreshed; save_config`）`backend/mod.rs:518`。

### 规则 2（规则 1 的例外）：SyncSnapshot 字段——磁盘权威，内存从磁盘回填

`run_tcp_push` 不持有 Inner 锁，无法改 Inner.config，于是它用**自己的** `load_config + set_sync_snapshot + save_config`（脱离 Inner mutex）把快照写盘 (`backend/sync_push.rs:20`)。调用者 `run_sync` 随后**重读磁盘，仅把 snapshot 这一个字段拷回** Inner.config (`backend/split_brain.rs:73`)。
- 理由：否则 `check_split_brain`（读 `g.config.sync_snapshot`，`backend/split_brain.rs:248`）会因内存快照陈旧而误判。
- **注意**：除 snapshot 外的其它字段**不回填**——`run_tcp_push`（或并发写者）对其它字段的磁盘改动不会反映到 Inner.config。

### 规则 3：后台线程——磁盘权威，每轮重读

项目 watcher、workspace watcher、session-mtime scanner 都把 **config.toml 磁盘视为权威**，每次事件/循环 `load_config` 重读，**完全无视 Inner.config**，仅在读失败时退回 spawn 时冻结的 `fallback_config` clone。
- 证据：`backend/mod.rs:1220`（`load_config(&config_path).unwrap_or_else(|_| fallback_config.clone())`）、`backend/session_scanner.rs:120`、`backend/mod.rs:977`。
- 含义：未刷盘的内存编辑对 auto-sync 不可见；磁盘是它们与 app 其余部分的唯一共享通道。

### 规则 4：磁盘整文件 last-writer-wins

`save_config` 序列化**整个** SyncConfig 后 `fs::write`，**无字段级合并、无临时文件+rename、无锁** (`config.rs:358-367`)。最近一个写者的整份快照成为磁盘真相。多数写者写前会先 `load_config`，只**收窄**（不消除）竞态窗口。

### 规则 5：peer 在线/离线——live discovery 主，config endpoint 探测兜底

`online = live_match.is_some() || 活跃配对会话.is_some() || endpoint.is_some_and(endpoint_online)`，其中 `endpoint_online` 对 endpoint 做 250ms `TcpStream::connect_timeout` 真探测 (`backend/mod.rs:1073`, `backend/mod.rs:2341`)。
- `paired_peers.json` 不带可达性；`PeerConfig.last_seen` 是死字段（永远 None，`backend/peers.rs:75`）——从不作为离线时间戳来源。

### 规则 6：peer 身份/配对成员——两个持久化存储**合并**去重，互不覆盖

`paired_peers()` 先遍历 `discoverer.paired_peers()`，再遍历 `config.peers`、跳过已发出的 id（按 DeviceId 去重，`backend/peers.rs:102`）。
- `paired_peers.json` 拥有**加密配对身份**（public_key、paired_at）；`config.peers` 拥有**传输路由**（endpoint、server_cert、server_name）。两者互补，不是一个覆盖另一个。

### 规则 7：sync 时的 endpoint/cert/server_name 选择——live 覆盖 config，逐字段优先

`peer_transport_connection` 取 `live: Option<PeerConnectionInfo>`，每字段独立：`live.endpoint.or(peer.endpoint)`；live cert（cert_source=discovery）否则 config server_cert（cert_source=config）；`live.server_name.or(peer.server_name)` (`backend/transport.rs:286`)。

### 规则 8：接收的文件内容——target 磁盘是真相，staging 是派生快照+delta

未确认覆盖时若将删除 >50% target 文件则**中止**（安全阀）；被删文件入 trash 非永久删；确认覆盖时**先**把整个 target 备份到 `<name>.bak-<stamp>` 再合并 (`transport lib.rs:3355-3368`, `:3384-3393`, `:3403-3433`)。target 被当作不可替代的真相。

### 规则 9：session 文件——磁盘字节是真相，ParsedSession 是无损视图

未触碰行字节级 round-trip（重发 raw），仅被显式路径重写的行才重新序列化。**此保证只对"从文件解析"的路径成立，对 `RecordLine::from_value` 重建不成立**（后者 raw 是紧凑重序列化，丢失原格式，`claude_code.rs:55-67`）。

### 规则 10：state.toml 与 ConfigStore 不在生产权威链内

`SyncState::load` 只被 `SyncCoordinator::new` 调用；`ConfigStore` 只 re-export、app 从不构造；`run_tcp_push` 只持久化 sync_snapshot 并把 report 版本号**硬编码为 0** (`backend/sync_push.rs:20`)；`check_split_brain` 读 `g.config.sync_snapshot`。**生产脑裂真相在 config.toml 的 SyncSnapshot 指纹里，不在 state.toml 版本号里。**

---

## 3. "缓存 vs 真相"冲突场景（谁实际赢）

每行是一个具体可触发的不一致窗口。`who_wins_in_code` 列是该场景下代码的实际胜负——任何偏离即 bug。

### 3.1 后台线程改盘而 Inner.config 不知
- **场景**：后台线程（workspace 刷新 / snapshot / peer 连接）经 `load_config+save_config` 改盘，自上次前台命令后无人动 Inner.config。前台读（`config()`、`check_split_brain` 读 `g.config.sync_snapshot`、`paired_peers` 读 `g.config.peers`）返回陈旧内存副本。
- **cache**：Inner.config（陈旧内存）— **truth**：config.toml（被后台线程更新）
- **谁赢**：**Inner.config 赢**（每个前台读者）。**无任何 Backend 端通用 reconciler**——`ConfigStore.reload_if_changed` 从不实例化。磁盘更新对内存不可见，直到某命令既 `load_config` 又重赋 `g.config`。
- 引用：`backend/mod.rs:514`, `backend/split_brain.rs:248`, `backend/mod.rs:977`, `config.rs:334-342`

### 3.2 push 后 snapshot 内存/磁盘窗口
- **场景**：`run_tcp_push` 经**独立** load+save 把新 SyncSnapshot 写盘（不走 Inner 锁）。此窗口内 Inner.config 持旧快照、磁盘持新快照。`run_sync` 随后重读磁盘，**只**把 snapshot 拷回。
- **cache**：Inner.config snapshot（陈旧，直到 `backend/split_brain.rs:73` 手动回填）— **truth**：config.toml snapshot（`backend/sync_push.rs:20` 写）
- **谁赢**：snapshot 字段**磁盘赢**然后拷回内存；**非 snapshot 的任何字段不回填**——`run_tcp_push`（或并发写者）对其它字段的磁盘改动不反映到 Inner.config。
- 引用：`backend/sync_push.rs:20`, `backend/split_brain.rs:73`, `backend/split_brain.rs:248`

### 3.3 run_tcp_push 读旧盘、内存编辑丢失
- **场景**：`run_tcp_push` `load_config`→`save_config` 写快照。若 Inner.config 有未刷盘的内存编辑（如 `backend/split_brain.rs:362` 注入的 per-run excludes，`backend/split_brain.rs:376` 恢复），`run_tcp_push` 读到**旧**盘状态、叠加 snapshot 写回——内存编辑永不到盘，`run_sync` 只拷回 snapshot。
- **谁赢**：**按字段分裂**：snapshot→磁盘赢（`backend/split_brain.rs:73` 拷回）；其它字段→Inner.config 仍是内存权威且从不重载。注入 excludes 故意内存-only 且 `backend/split_brain.rs:376` 恢复，从不持久化。
- 引用：`backend/split_brain.rs:362`, `backend/split_brain.rs:376`, `backend/sync_push.rs:20`

### 3.4 receive_port 绑定后内存赢、磁盘陈旧
- **场景**：`Backend::new` 把 daemon 绑到可能不同的端口、内存里设 `config.receive_port = serve.port` (`backend/mod.rs:410`)，但构造器里唯一的 `save_config` 跑在更早处（daemon 启动前）。该处到 `new()` 结束之间**无** save。
- **谁赢**：运行时**内存赢**：mDNS 广播内存里的 receive_port (`backend/mod.rs:410`)。下次重启 `load_config` 读到**陈旧磁盘端口**。磁盘陈旧直到某后续变更命令重写整份 config。
- 引用：`backend/mod.rs:410`

### 3.5 watcher 用磁盘、fallback 更陈旧
- **场景**：长生命 watcher 线程 spawn 时抓了 `fallback_config = config.clone()` (`backend/mod.rs:1220`)，每批文件事件 `load_config`（从不读 Inner.config）。命令改了 Inner.config 并 save 则 watcher 下次事件见新盘；若 save 失败则 watcher 读陈旧盘并退回**更陈旧**的启动 clone。
- **谁赢**：watcher **磁盘赢**：`load_config(&config_path).unwrap_or_else(|_| fallback_config.clone())`。watcher 从不咨询 Inner.config，未刷盘的内存编辑对 auto-sync 不可见。
- 引用：`backend/mod.rs:1220`, `backend/mod.rs:1439`

### 3.6 scanner 写盘竞 config 全量保存
- **场景**：session-mtime scanner（`Backend::new` spawn 一次）永久循环 `load_config`，经 `refresh_and_save_workspaces`（独立 load+save）重写盘。它与 Inner.config 无共享态，可与持 Inner 锁的命令**并发 save()**。
- **谁赢**：scanner **磁盘赢**；且 scanner 自己**写盘**（`refresh_and_save_workspaces` 内 `save_config`，`backend/mod.rs:977`）**不持 Inner 锁**——并发命令保存整份 config 会与 scanner 的 save 竞态（磁盘最后写者赢，无协调）。Inner.config 从不被 scanner 的写刷新。
- 引用：`backend/session_scanner.rs:120`, `backend/session_scanner.rs:120`, `backend/mod.rs:977`

### 3.7 workspace 确认：candidate 覆盖并发盘改
- **场景**：workspace mapping 确认：锁内取 `candidate = g.config.clone()` (`backend/peers.rs:466`)，`persist_peer_connection` 改 candidate，**释放锁**发网络 ACK (`backend/peers.rs:466`)，再获锁 `save_config(candidate)` + `g.config=candidate` (`backend/peers.rs:466`)。若后台线程在 ACK 往返期间写了 config.toml，该盘改被覆盖。
- **谁赢**：**candidate（缓存）赢**：`save_config(&path, &candidate)` 覆盖整文件，`g.config=candidate` 替换内存，丢弃并发盘写。整 config last-writer-wins 无字段级合并。
- 引用：`backend/peers.rs:466`, `backend/peers.rs:466`, `config.rs:358-367`

### 3.8 生产 push 不写 state.toml
- **场景**：生产 GUI push（`start_sync`→`spawn_sync`→`run_sync`→`run_tcp_push`）完成。state.toml（版本号/指纹）**从不更新**，只更 config.toml sync_snapshots。期待版本号的读者见 0。
- **谁赢**：生产路径 **config.toml snapshots 赢**：`run_tcp_push` 硬编码 `local_version=0, remote_version=0` (`backend/sync_push.rs:20`)，从不调 `SyncState::record_success`。state.toml 仅在 `SyncCoordinator::sync_one_way_impl` (`lib.rs:322-325`) 下权威，而 GUI 从不调它。app 的脑裂用 config.toml snapshots，非 state.toml 版本。
- 引用：`backend/sync_push.rs:20`, `lib.rs:322-325`, `commands.rs:1454-1504`

### 3.9 离线 peer 被查询本身"复活"
- **场景**：peer 离线（mDNS `ServiceRemoved` 置 `record.removed=true`，`lib.rs:1403`）。随后 UI 调 `Discoverer::peers()`，内部 `refresh_peers_for_query` 对**每条** record 置 `last_seen=now` 且 `removed=false` (`lib.rs:1364-1368`)，复活刚移除的 peer 并重置老化时钟。
- **谁赢**：**查询变更覆盖移除标记**：`peers()` 非纯读——只要有东西在轮询 `peers()`，`prune_stale_peers` 就不会按 last_seen 剪除。真实可达性再由 `Backend::paired_peers` 的 TCP 探测 (`endpoint_online`) 重新判定，非仅凭 mDNS record。
- 引用：`discovery lib.rs:1403`, `:574-577`, `:1357-1383`, `backend/mod.rs:1073`

### 3.10 已配对 peer 在线判定靠真探测
- **场景**：`SharedState.peers` 无 live record（peer 退出 mDNS），但 config.peers 仍有存储 endpoint。`Backend::paired_peers` 须判在线/离线。
- **谁赢**：**真 TCP 探测兜底**：`online = live_match || 活跃会话 || endpoint.is_some_and(endpoint_online)`，后者 250ms connect。陈旧但可达的 config endpoint 即使无 live mDNS record 也报在线；live mDNS peer 不探测即报在线。`PeerConfig.last_seen` 永远 None，从不作离线时间戳源。
- 引用：`backend/mod.rs:1073`, `backend/mod.rs:2341`, `backend/peers.rs:75`

### 3.11 paired_peers.json 与 config.peers 同一 peer
- **场景**：同一逻辑 peer 同时在持久化配对存储和 config.peers，可能 name/endpoint 不同；或只在其一（如手改 config）。
- **谁赢**：**谁都不单独赢**——`paired_peers()` 先遍历 `discoverer.paired_peers()` 再遍历 config.peers、跳过已发 id（去重，`backend/peers.rs:102`）。两持久化存储合并而非覆盖。endpoint 解析顺序 `live_match→session→configured`，config endpoint 是最后兜底，配对存储身份占主导。
- 引用：`backend/peers.rs:102`, `backend/peers.rs:102`, `discovery lib.rs:73-77`

### 3.12 session 单行重写字节保真
- **场景**：某 .jsonl 记录 path 字段被重写（dirty=true），同文件其它记录未触碰；写回须除该字段外字节相同。
- **谁赢**：每个非 dirty 记录 **raw bytes 赢**（emit 返回 `self.raw.clone()`，`claude_code.rs:82`）；只有 dirty 记录 **value 赢**（重序列化，`:79`）。trailing_newline 保留（`:379-381`），`write_back_is_byte_identical` 测试验证。**例外**：经 `RecordLine::from_value` 重建的 session，raw 是紧凑 `serde_json::to_string`（`:56-61`），干净记录**不**复现原盘格式——字节一致仅对"从文件解析"路径成立。
- 引用：`claude_code.rs:73-84`, `:370-383`, `:55-67`

### 3.13 编码目录名冲突
- **场景**：两个不同原始 cwd 路径编码为同一 `projects/<encoded-dir>` 名。
- **谁赢**：`SessionIndex.conflicts()` **检测**（不解决）：映射 `encoded_dir_name → {original_project_paths}`，标记 >1 路径者 (`claude_code.rs:171-180`)。`original_project_path` 来自文件内 cwd（`extract_original_path`），仅 cwd 缺失时退回目录名（`:350-351`, `:363-367`）。**记录的 cwd 权威于编码目录名。**
- 引用：`claude_code.rs:171-180`, `:350-351`, `:363-367`

### 3.14 backend 编码 vs parser 编码必须一致
- **场景**：`claude_project_dir_name`（backend）与 parser 产出的 `encoded_dir_name` 须一致，否则过滤扫描（`encoded == local_encoded_dir`）匹配零 session、push 一个不发。
- **谁赢**：**盘上目录名是匹配目标**，backend 派生是谓词（`encoded == local_encoded_dir`，`backend/session_stage.rs:53`）。若二者背离，`prepare_claude_session_sync` 返回 `Ok(None)` 理由 `no_matching_sessions` (`backend/session_stage.rs:53`)，push **静默略过 session**（非报错）。两编码器靠约定保持同步（`claude_code.rs:224-225`）。
- 引用：`backend/session_stage.rs:53`, `backend/session_stage.rs:53`, `claude_code.rs:224-225`

### 3.15 接收 commit：staging 不完整但 target 已有大量文件
- **场景**：incoming staging 不完整/空，但 target 已有许多文件；staging 会删 >50% target。
- **谁赢**：无 confirm_overwrite 时**现有 target 经安全阀赢**：`delete_ratio = to_delete/target_files > 0.5` 时中止报错 (`transport lib.rs:3358-3368`)。confirm_overwrite=true 时 staging 赢，但**先** `backup_target_dir` 把整 target 复制到 `<name>.bak-<stamp>` (`:3355-3356`, `:3403-3433`)。正常 commit 删除的文件入 trash 非永久删 (`:3384-3393`)。背离保守地倒向盘上真相，除非用户确认。
- 引用：`transport lib.rs:3355-3368`, `:3384-3393`, `:3403-3433`

---

## 4. 一句话总结

- **config**：内存领先磁盘（规则 1），唯一例外是 **snapshot 字段磁盘权威回填内存**（规则 2）。后台线程则把磁盘当唯一真相（规则 3）。
- **无任何通用内存↔磁盘 reconciler**：`ConfigStore.reload_if_changed` 是死代码，后台线程改盘对前台 Inner.config 不可见，直到下一个变更命令整份重写。
- **peer**：身份靠两持久化存储合并（规则 6），可达性靠 live discovery + 真 TCP 探测（规则 5），`PeerConfig.last_seen` 是死字段。
- **session**：盘上字节是真相，内存解析无损（规则 9），但仅对解析路径、且仅对未触碰记录字节一致。
- **state.toml/ConfigStore 不在生产权威链内**（规则 10）——生产脑裂真相在 config.toml 的 SyncSnapshot。

> 配套：约束清单见 [08-consistency-rules.md](08-consistency-rules.md)，脏状态分类见 [09-dirty-state.md](09-dirty-state.md)。
