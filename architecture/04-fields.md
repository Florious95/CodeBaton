# 4. 状态字段全景

按「配置层 -> 运行时层 -> 传输层 -> 历史层」分组，逐字段记录。

---

## A. 配置层（~/.aisync/config.toml）

文件: `codebaton-sync/src/config.rs`

### A1. SyncConfig（顶层结构）

| # | 字段 | 类型 | 行号 | 写入方 | 读取方 | 写入时机 | 清空时机 | 依赖 |
|---|------|------|------|--------|--------|----------|----------|------|
| 1 | `device` | `DeviceConfig` | :13 | `Backend::new` (首次生成), onboarding | 全局（mDNS 广播、Hello 握手、所有 Payload 里的 DeviceInfo） | 首次启动 / 修改设备名 | 不清空 | 无 |
| 2 | `onboarded` | `bool` | :15 | onboarding 完成时 save_config | UI 判断是否展示引导 | 首次配对成功 | 不清空（重置需删配置） | 无 |
| 3 | `receive_port` | `u16` | :17 | `Backend::new`（绑定实际端口后回写）, 用户手改 | start_serve_daemon, mDNS 广播 | 启动 / 端口冲突自动重选 | 不清空 | 无 |
| 4 | `peers` | `HashMap<String, PeerConfig>` | :19 | 配对流程 accept 后写入 | push/pull 时查 endpoint + cert | 配对成功 | 取消配对时移除 key | 无 |
| 5 | `claude_config` | `ClaudeConfig` | :21 | onboarding / 手动编辑 | `project_mapping()` 推导 session 目录 | 初始配置 | 无 | 依赖 peers 的 key |
| 6 | `projects` | `Vec<ProjectConfig>` | :23 | 新建项目映射 / accept ProjectMappingRequest | push/pull、watcher 启动、UI 列表 | 用户添加项目 | 用户删除项目 | peers key（ProjectConfig.peers） |
| 7 | `workspaces` | `Vec<WorkspaceConfig>` | :25 | workspace mapping 流程 | watcher、scan_workspace、UI | 用户添加 workspace | 用户删除 | peers key |
| 8 | `exclude_rules` | `Vec<String>` | :27 | 用户配置 / 默认值 | manifest 扫描时合并到 globset | 初始化 / 用户修改 | 不清空 | 无 |
| 9 | `default_sync_mode` | `SyncModeConfig` | :29 | 用户配置 | 新项目创建时继承 | 配置时 | 不清空 | 无 |
| 10 | `refresh_interval_secs` | `u64` | :31 | 用户配置，默认 30 | session_mtime_scanner 轮询间隔 | 配置时 | 不清空 | 无 |
| 11 | `default_file_receive_dir` | `Option<PathBuf>` | :33 | 用户配置 | 文件传输接收落点 | 配置时 | 置 None | 无 |
| 12 | `receive_dir_override` | `Option<PathBuf>` | :37 | 测试注入 | `receive_root()` 优先级最高 | 测试 setup | 测试 teardown | 无 |
| 13 | `state_path` | `Option<PathBuf>` | :39 | `Backend::new` 自动补全 | SyncCoordinator 加载 state.toml | 首次启动 | 不清空 | 无 |

### A2. DeviceConfig

文件: `codebaton-sync/src/config.rs:158-162`

| 字段 | 类型 | 写入方 | 读取方 | 备注 |
|------|------|--------|--------|------|
| `id` | `DeviceId` (UUID) | 首次生成，不变 | mDNS 广播、Hello、所有 Payload | 设备唯一标识 |
| `name` | `String` | 首次生成取 hostname; BUG-007 修复时重算 | UI 展示、mDNS、peer 列表 | 旧版曾留 placeholder |

### A3. PeerConfig

文件: `codebaton-sync/src/config.rs:164-176`

| 字段 | 类型 | 写入方 | 读取方 | 备注 |
|------|------|--------|--------|------|
| `id` | `DeviceId` | 配对成功时存 | push 时查找 peer | 对端唯一标识 |
| `name` | `String` | 配对时从 PairingRequestPayload 取 | UI、日志 | 人类可读名 |
| `endpoint` | `Option<SocketAddr>` | 配对时存对端 advertised endpoint | push 连接地址 | None 时退回 mDNS 发现 |
| `server_cert` | `Option<PathBuf>` | 配对时写 .der 到 peers/ | TLS pin 验证 | 指向 ~/.aisync/peers/{id}-receiver.der |
| `server_name` | `Option<String>` | 配对时存 | TLS SNI | 默认 "aisync-receiver" |
| `last_seen` | `Option<String>` | mDNS 发现更新 | UI 展示 | ISO 时间戳 |

### A4. ClaudeConfig

文件: `codebaton-sync/src/config.rs:178-184`

| 字段 | 类型 | 写入方 | 读取方 | 备注 |
|------|------|--------|--------|------|
| `local` | `PathBuf` | 用户配置 | `project_mapping()` — 空时退回 sibling_claude_dir | 本端 Claude session 目录 |
| `peers` | `HashMap<String, PathBuf>` | 用户配置 | `project_mapping()` — 无对应 key 时退回 sibling | key = peer name |

**依赖**: `peers` 的 key 必须存在于顶层 `SyncConfig.peers` 中，否则 project_mapping 不报错但取不到。

### A5. ProjectConfig

文件: `codebaton-sync/src/config.rs:197-212`

| 字段 | 类型 | 写入方 | 读取方 | 备注 |
|------|------|--------|--------|------|
| `name` | `String` | 用户创建 | 全局查找 key | 唯一（validate 检查） |
| `local` | `PathBuf` | 用户指定 | manifest 扫描、watcher | validate 检查 exists |
| `peers` | `HashMap<String, PathBuf>` | 创建/accept mapping | `project_mapping()` 取 remote_code_dir | key=peer name, val=remote path |
| `sync_mode` | `SyncModeConfig` | 创建时 / 用户修改 | push/pull 决策 | 默认 TwoWayAuto |
| `enabled` | `bool` | 用户开关 | sync_one_way 前置检查; UI 状态 | 默认 true |
| `exclude_rules` | `Vec<String>` | 用户配置 | 合并到项目级 exclude | 项目粒度排除 |
| `sync_snapshots` | `HashMap<String, SyncSnapshot>` | `set_sync_snapshot()` 同步成功后 | `sync_snapshot()` 脑裂检测 | key=peer name |

**依赖**: `peers` key 必须对应顶层 `SyncConfig.peers` key; `sync_snapshots` key 同理。

### A6. SyncSnapshot

文件: `codebaton-sync/src/config.rs:187-195`

| 字段 | 类型 | 写入方 | 读取方 | 备注 |
|------|------|--------|--------|------|
| `peer_last_known_hash` | `String` | 推送成功后存对端 manifest hash | 下次推送前脑裂检测比对 | blake3 hex |
| `self_last_synced_hash` | `String` | 推送成功后存本端 manifest hash | 未使用（预留） | blake3 hex |

### A7. WorkspaceConfig

文件: `codebaton-sync/src/config.rs:214-239`

| 字段 | 类型 | 写入方 | 读取方 | 备注 |
|------|------|--------|--------|------|
| `name` | `String` | 用户创建 | 查找 key, 唯一 | validate 检查 |
| `local_root` | `PathBuf` | 用户指定 | effective_local_root | 与 `local` 二选一 |
| `remote_root` | `PathBuf` | 用户/mapping | effective_remote_root | 对端根目录 |
| `peer` | `String` | mapping 时 | effective_peer | 单 peer 简写 |
| `children` | `Vec<WorkspaceChildConfig>` | scan_workspace + 用户确认 | 逐子目录同步 | 动态增长 |
| `local` | `PathBuf` | 兼容旧格式 | effective_local_root fallback | 废弃中 |
| `peers` | `HashMap<String, PathBuf>` | 多 peer 场景 | effective_remote_root | 与 remote_root 互斥 |
| `scan_depth` | `usize` | 配置，必须=1 | validate 检查 | MVP 限制 |
| `auto_enable_new` | `bool` | 用户配置 | scan_workspace 自动 enable | |
| `sync_mode` | `SyncModeConfig` | 配置 | 子项目继承 | |
| `enabled` | `bool` | 用户开关 | watcher/sync | 默认 true |
| `exclude_rules` | `Vec<String>` | 配置 | 合并排除 | |

### A8. WorkspaceChildConfig

文件: `codebaton-sync/src/config.rs:241-252`

| 字段 | 类型 | 写入方 | 读取方 | 备注 |
|------|------|--------|--------|------|
| `name` | `String` | scan 发现 | 子目录标识 | 目录名 |
| `local_dir` | `PathBuf` | scan 计算 | 同步源 | |
| `remote_dir` | `PathBuf` | scan 计算 / mapping ack | 同步目标 | |
| `enabled` | `bool` | 用户确认 | 是否参与同步 | 默认 true |
| `conflicted` | `bool` | 脑裂检测写入 | UI 展示冲突标记 | 默认 false |
| `last_fingerprint` | `Option<String>` | 同步成功后存 | 增量检测 | blake3 hex |

### A9. SyncModeConfig（枚举）

文件: `codebaton-sync/src/config.rs:282-303`

| 变体 | 映射 | 备注 |
|------|------|------|
| `OneWayPush` | `SyncMode::OneWayPush { direction: LocalToRemote }` | |
| `OneWayPull` | `SyncMode::OneWayPush { direction: RemoteToLocal }` | |
| `TwoWayAuto` | `SyncMode::TwoWayAuto` | 默认值 |

---

## B. 运行时层

### B1. 全局静态变量（auto_sync_gate）

文件: `codebaton-app/src/backend/auto_sync_gate.rs`（4 个进程级 static 均在此模块，非 Inner 字段）

| # | 变量 | 类型 | 写入方 | 读取方 | 生命周期 | 依赖 |
|---|------|------|--------|--------|----------|------|
| 1 | `AUTO_SYNC_COOLDOWN_OVERRIDE` | `OnceLock<Duration>` | `set_auto_sync_cooldown_for_test`（auto_sync_gate.rs:31） | `auto_sync_cooldown()`（auto_sync_gate.rs:23） | 进程级，仅首次写 | 无 |
| 2 | `INCOMING_SYNC_SUPPRESSIONS` | `OnceLock<Mutex<HashMap<PathBuf, Instant>>>` | `mark_incoming_sync_root`（auto_sync_gate.rs:77） | `incoming_sync_recent`（auto_sync_gate.rs:84） | 进程级，自动过期 | 无 |
| 3 | `AUTO_SYNC_GATES` | `OnceLock<Mutex<HashMap<String, AutoSyncGate>>>` | `try_begin_auto_sync` / `finish_auto_sync`（auto_sync_gate.rs:97, 168） | `try_begin_auto_sync`（auto_sync_gate.rs:97） | 进程级 | gate key = "{scope}:{name}:{peer}" |
| 4 | `SESSION_BASELINE_SEEDS` | `OnceLock<Mutex<HashMap<String, SessionBaseline>>>` | session_mtime_scanner（session_scanner.rs start_session_mtime_scanner） | session_mtime_scanner 比较 | 进程级 | key = session 文件路径 |
| 5 | `WORKSPACE_PROPAGATION_BYPASS` | `OnceLock<Mutex<HashSet<String>>>` | `enqueue_workspace_first_propagation`（auto_sync_gate.rs:178） | `workspace_first_propagation_pending`（auto_sync_gate.rs:195） | 进程级 | gate key 格式 |

### B2. AutoSyncGate

文件: `codebaton-app/src/backend/auto_sync_gate.rs:41`

| 字段 | 类型 | 含义 |
|------|------|------|
| `in_flight` | `bool` | 正在执行自动同步 |
| `cooldown_until` | `Instant` | cooldown 结束时间点 |

**写入**: `try_begin_auto_sync` (in_flight=true), `finish_auto_sync` (in_flight=false, cooldown_until=now+cooldown)
**读取**: `try_begin_auto_sync` 判断是否放行

### B3. SessionBaseline

文件: `codebaton-app/src/backend/auto_sync_gate.rs:47`

| 字段 | 类型 | 含义 |
|------|------|------|
| `mtime` | `SystemTime` | 上次扫描时的 mtime |
| `content_fingerprint` | `Option<String>` | 内容 hash（可选） |
| `sync_fingerprint` | `Option<String>` | 同步后的 fingerprint |

**写入**: session_mtime_scanner 每轮扫描后更新
**读取**: session_mtime_scanner 判断是否有变化触发自动同步

### B4. Backend（顶层 struct）

文件: `codebaton-app/src/backend/mod.rs:178`（HUB struct 定义留在 mod.rs）

| 字段 | 类型 | 说明 |
|------|------|------|
| `inner` | `Mutex<Inner>` | 主状态锁 |
| `pending_pairing_requests` | `Arc<Mutex<VecDeque<PairingRequestPayload>>>` | serve daemon 收到的配对请求队列 |
| `pending_project_mapping_requests` | `Arc<Mutex<VecDeque<ProjectMappingRequestPayload>>>` | 待处理项目映射请求 |
| `pending_project_mapping_acks` | `Arc<Mutex<VecDeque<ProjectMappingAckPayload>>>` | 待处理项目映射应答 |
| `pending_workspace_mapping_requests` | `Arc<Mutex<VecDeque<WorkspaceMappingRequestPayload>>>` | 待处理 workspace 映射请求 |
| `pending_workspace_mapping_acks` | `Arc<Mutex<VecDeque<WorkspaceMappingAckPayload>>>` | 待处理 workspace 映射应答 |
| `pending_text_messages` | `Arc<Mutex<VecDeque<TextMessagePayload>>>` | 待处理文本消息 |
| `pending_file_transfer_requests` | `Arc<Mutex<VecDeque<FileTransferRequestPayload>>>` | 待处理文件传输请求 |
| `pending_file_transfer_acks` | `Arc<Mutex<VecDeque<FileTransferAckPayload>>>` | 待处理文件传输应答 |
| `file_receive_states` | `Arc<Mutex<HashMap<String, FileReceiveState>>>` | 正在接收的文件状态，key=transfer_id |

**写入**: serve daemon 回调写入各 pending 队列; UI poll 消费后 pop
**读取**: Tauri command poll 各队列; UI 展示

### B5. Inner

文件: `codebaton-app/src/backend/mod.rs:202`（HUB struct 定义留在 mod.rs）

| 字段 | 类型 | 说明 |
|------|------|------|
| `config` | `SyncConfig` | 内存中的当前配置快照 |
| `config_path` | `PathBuf` | 配置文件路径（用于 save/reload） |
| `discoverer` | `MdnsDiscoverer` | mDNS 发现实例 |
| `auto_sync_paused` | `bool` | 用户暂停自动同步 |
| `serve` | `Option<ServeInfo>` | 本地 receive daemon 信息 |
| `serve_shutdown` | `Option<ServeShutdownHandle>` | 停止 daemon 的句柄 |
| `pairing_sessions` | `HashMap<DeviceId, PairingSession>` | 进行中的配对会话，key=peer DeviceId |
| `project_mapping_requests` | `HashMap<String, ProjectMappingRequestPayload>` | 入站项目映射请求，key=request_id |
| `outbound_project_mappings` | `HashMap<String, OutboundProjectMapping>` | 出站项目映射（等待 ack），key=request_id |
| `workspace_mapping_requests` | `HashMap<String, WorkspaceMappingRequestPayload>` | 入站 workspace 映射请求 |
| `outbound_workspace_mappings` | `HashMap<String, OutboundWorkspaceMapping>` | 出站 workspace 映射 |
| `file_transfer_requests` | `HashMap<String, FileTransferRequestPayload>` | 入站文件传输请求 |
| `outbound_file_transfers` | `HashMap<String, OutboundFileTransfer>` | 出站文件传输 |
| `project_watchers` | `HashMap<String, FsWatcher>` | 活跃的项目 watcher，key=project name |
| `workspace_watchers` | `HashMap<String, FsWatcher>` | 活跃的 workspace watcher，key=workspace name |

### B6. FileReceiveState

文件: `codebaton-app/src/backend/file_transfer.rs:29`

| 字段 | 类型 | 说明 |
|------|------|------|
| `target_path` | `PathBuf` | 最终落盘路径 |
| `tmp_path` | `PathBuf` | 临时写入路径 |
| `expected_size` | `u64` | 预期总字节数 |
| `bytes_written` | `u64` | 已接收字节数 |
| `filename` | `String` | 原始文件名 |
| `sender_name` | `String` | 发送方设备名 |
| `history_config_path` | `PathBuf` | 完成后写 history 用的 config 路径 |

### B7. SyncState（state.toml）

文件: `codebaton-sync/src/lib.rs:411-463`

路径: `~/.aisync/state.toml`（由 `config.state_path` 指定）

| 字段 | 类型 | 说明 |
|------|------|------|
| `projects` | `HashMap<String, ProjectVersionState>` | 按 project_id 索引的版本状态 |

#### ProjectVersionState

| 字段 | 类型 | 写入方 | 读取方 | 备注 |
|------|------|--------|--------|------|
| `local_version` | `u64` | `record_success` (指纹变化时+1) | UI、conflict 报告 | 单调递增 |
| `remote_version` | `u64` | `record_success` | UI、conflict 报告 | 单调递增 |
| `local_fingerprint` | `String` | `record_success` | `detect_conflict` 比较 | blake3(code+session) |
| `remote_fingerprint` | `String` | `record_success` | `detect_conflict` 比较 | blake3(code+session) |
| `last_synced_at_unix_secs` | `u64` | `record_success` | UI | unix epoch seconds |
| `has_synced` | `bool` | `record_success` 首次置 true | `detect_conflict` 跳过首次 | |

**依赖**: `projects` key 必须对应 `SyncConfig.projects[].name`。

### B8. ConfigStore（热重载）

文件: `codebaton-sync/src/config.rs:305-343`

| 字段 | 类型 | 说明 |
|------|------|------|
| `path` | `PathBuf` | 配置文件路径 |
| `config` | `SyncConfig` | 缓存的配置 |
| `last_modified` | `Option<SystemTime>` | 文件 mtime，reload_if_changed 比较 |

---

## C. 传输层

文件: `codebaton-transport/src/lib.rs`

### C1. Message 枚举

| 变体 | 字段 | 行号 | 用途 |
|------|------|------|------|
| `Hello` | `protocol_version: u32`, `device_name: String` | :299 | 握手版本协商 |
| `FileManifest` | `manifest: SyncManifest`, `remote_dir: Option<PathBuf>`, `confirm_overwrite: bool` | :303 | 交换文件清单 |
| `FileSignatures` | `signatures: Vec<SignatureEntry>` | :311 | rsync 签名列表 |
| `FileDelta` | `path: String`, `base_hash: Option<String>`, `target_hash: String`, `delta: Vec<u8>`, `size: u64` | :314 | 单文件增量 |
| `FileChunk` | `path: String`, `target_hash: String`, `offset: u64`, `data: Vec<u8>`, `size: u64`, `done: bool` | :322 | 大文件分块 |
| `FileBatch` | `tar_stream: Vec<u8>` | :331 | 小文件批量 tar 包 |
| `FileDelete` | `path: String` | :335 | 删除文件 |
| `SessionData` | `project_id: String`, `data: Vec<u8>` | :338 | session 数据 |
| `SyncComplete` | (unit) | :343 | 同步完成信号 |
| `PairingRequest` | `request: PairingRequestPayload` | :344 | 配对请求 |
| `PairingAck` | `request_id: String` | :347 | 配对应答 |
| `ProjectMappingRequest` | `request: ProjectMappingRequestPayload` | :350 | 项目映射请求 |
| `ProjectMappingAck` | `ack: ProjectMappingAckPayload` | :353 | 项目映射应答 |
| `WorkspaceMappingRequest` | `request: WorkspaceMappingRequestPayload` | :356 | workspace 映射请求 |
| `WorkspaceMappingAck` | `ack: WorkspaceMappingAckPayload` | :359 | workspace 映射应答 |
| `TextMessage` | `message: TextMessagePayload` | :362 | 文本消息 |
| `FileTransferRequest` | `request: FileTransferRequestPayload` | :365 | 文件传输请求 |
| `FileTransferData` | `data: FileTransferDataPayload` | :368 | 文件传输数据块 |
| `FileTransferAck` | `ack: FileTransferAckPayload` | :371 | 文件传输应答 |
| `TargetStatusRequest` | `request: TargetStatusRequestPayload` | :374 | 目标目录状态查询（脑裂） |
| `TargetStatusResponse` | `response: TargetStatusResponsePayload` | :377 | 目标目录状态响应 |
| `Error` | `message: String` | :380 | 错误传播 |

### C2. SyncManifest / FileEntry

文件: `codebaton-core/src/lib.rs:97-108`

#### SyncManifest
| 字段 | 类型 | 说明 |
|------|------|------|
| `files` | `Vec<FileEntry>` | 按 relative_path 排序 |

#### FileEntry
| 字段 | 类型 | 写入方 | 读取方 | 说明 |
|------|------|--------|--------|------|
| `relative_path` | `String` | `scan_manifest` (walkdir) | diff、transfer、commit | 正规化路径（/分隔） |
| `size` | `u64` | `scan_manifest` (metadata.len) | 进度计算、小文件判断 | |
| `blake3_hash` | `String` | `scan_manifest` (blake3全文) | diff 比较、完整性校验 | hex string |
| `mtime` | `u64` | `scan_manifest` (metadata.modified) | 未主动比较，仅传输 | unix secs |

### C3. 控制帧 Payload 结构

#### PairingRequestPayload (lib.rs:180-188)
| 字段 | 类型 | 说明 |
|------|------|------|
| `request_id` | `String` | 唯一 request 标识 |
| `code` | `String` | 配对验证码 |
| `expires_at_unix_secs` | `u64` | 过期时间 |
| `device` | `DeviceInfo` | 发起方设备信息 |
| `endpoint` | `Option<SocketAddr>` | 发起方 listen 地址 |
| `receiver_cert_der` | `Option<Vec<u8>>` | 发起方 TLS 证书 DER |
| `server_name` | `Option<String>` | TLS server name |

#### ProjectMappingRequestPayload (lib.rs:191-199)
| 字段 | 类型 | 说明 |
|------|------|------|
| `request_id` | `String` | |
| `project_name` | `String` | |
| `source_dir` | `PathBuf` | 发起方本地目录 |
| `mode` | `String` | 同步模式 |
| `device` | `DeviceInfo` | |
| `endpoint` | `Option<SocketAddr>` | |
| `receiver_cert_der` | `Option<Vec<u8>>` | |
| `server_name` | `Option<String>` | |

#### ProjectMappingAckPayload (lib.rs:201-210)
| 字段 | 类型 | 说明 |
|------|------|------|
| `request_id` | `String` | |
| `accepted` | `bool` | |
| `project_name` | `String` | |
| `remote_dir` | `Option<PathBuf>` | 接受方指定的目标目录 |
| `message` | `Option<String>` | 拒绝理由 |
| `device` | `DeviceInfo` | |

#### TargetStatusRequestPayload (lib.rs:213-219)
| 字段 | 类型 | 说明 |
|------|------|------|
| `request_id` | `String` | |
| `target_dir` | `PathBuf` | 查询的对端目录 |
| `device` | `DeviceInfo` | |

#### TargetStatusResponsePayload (lib.rs:221-232)
| 字段 | 类型 | 说明 |
|------|------|------|
| `request_id` | `String` | |
| `not_empty` | `bool` | 目录含文件 |
| `file_count` | `u64` | 文件数 |
| `manifest_hash` | `String` | 当前 manifest 指纹（脑裂比对） |
| `device` | `DeviceInfo` | |

#### WorkspaceMappingRequestPayload (lib.rs:234-247)
| 字段 | 类型 | 说明 |
|------|------|------|
| `request_id` | `String` | |
| `workspace_name` | `String` | |
| `source_root` | `PathBuf` | |
| `suggested_remote_root` | `PathBuf` | |
| `mode` | `String` | |
| `auto_enable_new` | `bool` | |
| `children` | `Vec<String>` | 子目录名列表 |
| `device` | `DeviceInfo` | |
| `endpoint/receiver_cert_der/server_name` | ... | TLS 连接信息 |

#### WorkspaceMappingAckPayload (lib.rs:249-257)
| 字段 | 类型 | 说明 |
|------|------|------|
| `request_id` | `String` | |
| `accepted` | `bool` | |
| `workspace_name` | `String` | |
| `remote_root` | `Option<PathBuf>` | |
| `message` | `Option<String>` | |
| `device` | `DeviceInfo` | |

#### TextMessagePayload (lib.rs:259-264)
| 字段 | 类型 | 说明 |
|------|------|------|
| `sender_name` | `String` | |
| `content` | `String` | 消息正文 |
| `timestamp` | `u64` | epoch millis |

#### FileTransferRequestPayload (lib.rs:266-276)
| 字段 | 类型 | 说明 |
|------|------|------|
| `transfer_id` | `String` | |
| `filename` | `String` | |
| `size` | `u64` | |
| `sender_name` | `String` | |
| `device` | `DeviceInfo` | |
| `endpoint/receiver_cert_der/server_name` | ... | TLS |

#### FileTransferDataPayload (lib.rs:278-285)
| 字段 | 类型 | 说明 |
|------|------|------|
| `transfer_id` | `String` | |
| `offset` | `u64` | 分块偏移 |
| `chunk` | `Vec<u8>` | 数据 |
| `done` | `bool` | 是否最后一块 |

#### FileTransferAckPayload (lib.rs:287-295)
| 字段 | 类型 | 说明 |
|------|------|------|
| `transfer_id` | `String` | |
| `accepted` | `bool` | |
| `ready` | `bool` | 准备接收数据 |
| `filename` | `String` | |
| `message` | `Option<String>` | |
| `device` | `DeviceInfo` | |

### C4. SignatureEntry

文件: `codebaton-transport/src/lib.rs:414-420`

| 字段 | 类型 | 说明 |
|------|------|------|
| `relative_path` | `String` | 文件路径 |
| `base_hash` | `String` | 当前文件 hash |
| `signature` | `Vec<u8>` | rsync 签名 bytes |

### C5. FileDiff

文件: `codebaton-transport/src/lib.rs:422-428`

| 字段 | 类型 | 说明 |
|------|------|------|
| `added` | `Vec<FileEntry>` | 新增文件 |
| `modified` | `Vec<FileEntry>` | 修改文件 |
| `deleted` | `Vec<FileEntry>` | 删除文件 |
| `unchanged` | `Vec<FileEntry>` | 未变文件 |

### C6. TlsConfig / TlsIdentity

文件: `codebaton-transport/src/lib.rs:92-118`

#### TlsIdentity
| 字段 | 类型 | 说明 |
|------|------|------|
| `cert_der` | `Vec<u8>` | 自签证书 DER |
| `private_key_der` | `Vec<u8>` | 私钥 DER |

#### TlsConfig
| 字段 | 类型 | 说明 |
|------|------|------|
| `identity` | `TlsIdentity` | 本端身份 |
| `pinned_peer_cert_der` | `Option<Vec<u8>>` | pin 的对端证书 |
| `server_name` | `String` | TLS SNI |

### C7. 协议常量

| 常量 | 值 | 行号 | 说明 |
|------|---|------|------|
| `PROTOCOL_VERSION` | 2 | :81 | Hello 中交换 |
| `SMALL_FILE_THRESHOLD` | 64KB | :82 | 小文件走 batch |
| `FILE_CHUNK_SIZE` | 1MB | :83 | 大文件分块 |
| `SMALL_FILE_BATCH_LIMIT` | 8MB | :84 | batch tar 上限 |
| `MAX_FRAME_SIZE` | 512MB | :86 | 帧最大体积 |
| `CONNECT_TIMEOUT` | 10s | :87 | TCP 连接超时 |
| `TLS_HANDSHAKE_TIMEOUT` | 10s | :88 | TLS 握手超时 |
| `FRAME_HEADER_TIMEOUT` | 10s | :89 | 帧头读取超时 |
| `FRAME_BODY_TIMEOUT` | 60s | :90 | 帧体读取超时 |

---

## D. 历史层

### D1. history.jsonl（同步历史）

路径: `~/.aisync/history.jsonl`
写入: `Backend::record_sync`（backend/history.rs:23） / `record_auto_sync_history`（backend/history.rs:346）
读取: `Backend::sync_history`（backend/history.rs:129）

每行 JSON schema:

```json
{
  "eventId": "string (hex)",
  "timestamp": "string (epoch millis)",
  "projectId": "string",
  "direction": "push | pull",
  "success": true,
  "files": 3,
  "bytes": 12345,
  "detail": "optional error message",
  "workspaceName": "optional",
  "childName": "optional",
  "trigger": "manual | auto",
  "role": "sender",
  "fileType": "code | session | mixed",
  "file_path": "optional first file path",
  "file_paths": ["array of paths, max 5"],
  "file_name": "optional first filename",
  "file_names": ["array of filenames"]
}
```

**依赖**: `projectId` 对应 `SyncConfig.projects[].name` 或 workspace child name.

### D2. chat_history.jsonl（聊天历史）

路径: `~/.aisync/chat_history.jsonl`
写入: `record_text_message_history`（backend/messaging.rs:47）
读取: `Backend::text_messages`（backend/messaging.rs:34；旧文档称 chat_messages，实际函数名为 text_messages）

每行 JSON schema:

```json
{
  "timestamp": 1782044725123,
  "peerName": "string",
  "senderName": "string",
  "content": "string",
  "mine": true
}
```

### D3. file_transfer_history.jsonl（文件传输历史）

路径: `~/.aisync/file_transfer_history.jsonl`
写入: `record_file_transfer_history`（backend/file_transfer.rs:594）
读取: `Backend::file_transfer_history`（backend/file_transfer.rs:310）

每行 JSON schema:

```json
{
  "timestamp": "string (epoch millis)",
  "transferId": "string (hex)",
  "direction": "in | out",
  "peer": "string (peer name)",
  "filename": "string",
  "path": "string (absolute path)",
  "bytes": 25581,
  "status": "sent | received | failed",
  "detail": null
}
```

---

## E. 前端 DTO 层

文件: `codebaton-app/src/dto.rs`

DTO 不持久化，仅在 Tauri IPC 层序列化传输。重点列出与配置/运行时字段的映射关系。

### E1. 核心展示 DTO

| DTO | 关键字段 | 数据来源 |
|-----|----------|----------|
| `PeerDto` | id, name, os, ip, status, kind, paired_at | `SyncConfig.peers` + mDNS discoverer |
| `ProjectDto` | id, name, local_dir, remote_dir, mode, status, progress, last_sync, history | `ProjectConfig` + state + history.jsonl |
| `WorkspaceDto` | id, name, local_root, remote_root, children, history | `WorkspaceConfig` + history |
| `WorkspaceChildDto` | name, local_dir, remote_dir, status, newly_discovered | `WorkspaceChildConfig` + scan |
| `SettingsDto` | device_name, port, global_excludes, refresh_interval_secs | `SyncConfig` 各字段 |
| `OverviewDto` | local, tools, projects, workspaces | 聚合上述所有 |
| `StatusBarDto` | primary_peer, status, syncing_project, last_sync, auto_sync_paused | Inner 运行时状态 |

### E2. 同步进度 DTO

| DTO | 关键字段 | 数据来源 |
|-----|----------|----------|
| `SyncProgressDto` | percent, phase, files_done/total, bytes_done/total, speed_bps, eta_secs, stages | 实时 push 进度回调 |
| `SyncResultDto` | success, files, bytes, elapsed_secs, rewritten_paths | SyncReport |
| `RewriteReportDto` | rewritten, skipped | RuleBasedRewriter 结果 |

### E3. 冲突 DTO

| DTO | 关键字段 | 数据来源 |
|-----|----------|----------|
| `ConflictDto` | project_id, local side, remote side | ConflictDetails + manifest diff |
| `SplitBrainStatusDto` | reachable, has_snapshot, peer_not_empty, split_brain | TargetStatusResponse + SyncSnapshot |

### E4. 配对/映射 DTO

| DTO | 数据来源 |
|-----|----------|
| `PairingDto` | PairingRequestPayload |
| `ProjectMappingRequestDto` | ProjectMappingRequestPayload |
| `WorkspaceMappingRequestDto` | WorkspaceMappingRequestPayload |

### E5. 文件传输 DTO

| DTO | 数据来源 |
|-----|----------|
| `FileTransferRequestDto` | FileTransferRequestPayload |
| `FileTransferHistoryDto` | file_transfer_history.jsonl |
| `TextMessageDto` | TextMessagePayload / chat_history.jsonl |

---

## F. 已知字段相关 Bug

| 编号 | 字段 | 描述 |
|------|------|------|
| BUG-007 | `device.name` | 旧版在 release sandbox 中 hostname lookup 失败，持久化了 placeholder "CodeBaton Device"。修复: Backend::new 检测并重算。 |
| - | `claude_config.local` | 空字符串时 `is_empty()` 判断走 sibling 分支，但 PathBuf 的 `is_empty` 使用 `as_os_str().is_empty()`，TOML 反序列化空串正确映射为空 PathBuf，行为正确但令人困惑。 |
| - | `sync_snapshots` | 旧配置无此字段（serde default 为空 HashMap），首次升级后推送不触发脑裂检测（无快照=跳过），符合预期但用户可能不知。 |
| - | `WorkspaceConfig.local` vs `local_root` | 存在两个字段做同一件事（兼容旧格式），effective_local_root 优先取 local_root，`local` 作为 fallback。新代码应只写 local_root。 |
| - | `workspace.scan_depth` | 硬编码必须=1，validate 强制检查，但字段类型为 usize 可配置任意值 -- MVP 遗留限制。 |
| - | `receive_dir_override` | 仅测试用途但暴露在公开 config struct 上，生产配置中设置此字段会绕过所有正常路径逻辑。 |
| - | `auto_sync_paused` (Inner) | 仅内存态，进程重启后恢复为 false -- 用户暂停后重启应用会丢失暂停状态。 |
| - | `confirm_overwrite` (FileManifest) | 旧版协议无此字段，serde default=false 保证向后兼容，但旧版接收端收到 true 时静默忽略（不做备份），存在数据安全隐患。 |
