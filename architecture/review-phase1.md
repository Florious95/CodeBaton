# Phase 1 Cross-Review: S1 / S2 / S4

审查范围: `01-state-model.md` (S1), `02-entities.md` (S2), `04-fields.md` (S4)

---

## 1. 通过项

### 1.1 S1 状态层 vs S4 字段归属 -- 一致

| S1 层 | S4 分区 | 验证 |
|-------|---------|------|
| Layer 1 Config File | A. 配置层 (A1-A9) | 完全对应; SyncConfig 13 个顶层字段、DeviceConfig、PeerConfig、ClaudeConfig、ProjectConfig、SyncSnapshot、WorkspaceConfig、WorkspaceChildConfig、SyncModeConfig 均有完整字段表 |
| Layer 2 Sync State File | B7. SyncState | 完全对应; state.toml 的 ProjectVersionState 6 字段与 S1 描述一致 |
| Layer 3 Runtime Memory | B1-B6 运行时层 | 5 个全局静态变量 (B1)、AutoSyncGate (B2)、SessionBaseline (B3)、Backend (B4)、Inner (B5)、FileReceiveState (B6) 均有字段表 |
| Layer 4 Discovery Runtime | S4 未单独列出 (见矛盾项 3.1) | -- |
| Layer 5 History/Log Files | D. 历史层 (D1-D3) | 3 种 JSONL 文件均有 schema |
| Layer 6 Network/Transport | C. 传输层 (C1-C7) | Message 枚举 21 个变体 + Payload 结构 + 常量完整 |
| Layer 7 Session Parse | S4 未单独列出 (见矛盾项 3.2) | -- |

### 1.2 S2 实体生命周期 vs S4 写入时机 -- 一致

| 实体 | S2 生命周期 | S4 写入时机 | 验证 |
|------|------------|------------|------|
| Backend | `Backend::new()` 创建 | B4: serve daemon 回调写入各 pending 队列 | 一致 |
| Inner | Mutex 保护, Backend 内部 | B5: 启动时从 config 加载, 运行时更新 | 一致 |
| FsWatcher | `start()` 创建, `drop()` 销毁 | A5: watcher 启动时读 ProjectConfig | 一致 |
| ServeShutdownHandle | `start_serve_daemon()` 创建 | B5: `serve_shutdown` 字段 | 一致 |
| Session Mtime Scanner | `Backend::new()` 启动, 无停止 | B3: SessionBaseline 每轮扫描更新 | 一致; S2 BUG-1 与 S1 BUG-3 对全局静态的描述互补 |

### 1.3 Bug 编号交叉 -- 一致但编号体系不同

S1 使用 BUG-1 到 BUG-6, S2 使用 BUG-1 到 BUG-4, S4 使用 BUG-007 + 无编号条目。三份文档描述的具体 bug 无矛盾, 例如:
- S1 BUG-3 (全局静态) = S2 BUG-4 (全局静态) -- 内容一致
- S1 BUG-4 (双写) -- S2/S4 均未重复描述, 但 S4 A3 PeerConfig 表中提及 `paired_peers.json` 的存在, 与 S1 描述兼容

### 1.4 SyncConfig 字段完整性

S1 Section 1 (Layer 1) 描述 SyncConfig 含 `device identity, peers, projects, workspaces, exclude rules, sync mode, sync snapshots, receive port`. S4 A1 表列出 13 个字段, 完整覆盖以上所有概念, 并额外列出 `onboarded`, `claude_config`, `refresh_interval_secs`, `default_file_receive_dir`, `receive_dir_override`, `state_path` -- 这些是细化展开, 不构成矛盾.

### 1.5 协议状态机

S1 Layer 6 描述协议状态机 `Hello -> Manifest -> Signatures -> Deltas -> SyncComplete`. S4 C1 Message 枚举包含对应变体 (`Hello`, `FileManifest`, `FileSignatures`, `FileDelta`, `SyncComplete`), 一致.

### 1.6 History 文件路径

S1 Layer 5 列出 3 个 JSONL 文件路径, S4 D1-D3 给出相同路径. 一致.

---

## 2. 矛盾项

### 2.1 层数计数: S1 说 6 层, 实际列了 7 层

- **S1 Section 1 开头**: "The system has **6** distinct storage layers"
- **S1 实际内容**: 列出 Layer 1 到 Layer 7, 共 7 层 (Layer 7 = Session Parse State)
- **S4**: 分为 A(配置)/B(运行时)/C(传输)/D(历史)/E(前端DTO)/F(Bug) 六个区, 其中 B 区合并了 S1 的 Layer 2 和 Layer 3, 传输层 C 对应 Layer 6, 历史层 D 对应 Layer 5. S4 没有为 Layer 7 (Session Parse) 单独建区.
- **结论**: S1 标题数字 "6" 与实际内容 "7 层" 自相矛盾. 应改为 7.

### 2.2 History 文件路径格式不一致

- **S2 Section 8.4**: 写 `~/.aisync/history/*.json`, 暗示 history 是目录下的多个 `.json` 文件, 且提到 `HISTORY_FILE_LIMIT = 5 entries per file`.
- **S1 Layer 5**: 写 `~/.aisync/history.jsonl`, 单文件.
- **S4 D1**: 写 `~/.aisync/history.jsonl`, 单文件.
- **结论**: S2 的路径格式 (`history/*.json`, 多文件, 5条上限) 与 S1/S4 (`history.jsonl`, 单文件 append-only) 矛盾. 需要查源码确认是 scoped history (多文件) 还是单文件. S4 同时提到写入方为 `record_sync` 和 `record_auto_sync_history`, 又在 S2 中提到 `record_sync_scoped()` -- 可能存在两种历史写入路径, 但文档描述不一致.

### 2.3 SyncState 内容描述偏差

- **S2 Section 8.2**: "Contains: Sync snapshots (manifest hashes for split-brain detection)"
- **S1 Layer 2**: "per-project `ProjectVersionState` with `local_version`, `remote_version`, fingerprints, `last_synced_at_unix_secs`, `has_synced` flag"
- **S4 B7**: 列出 ProjectVersionState 的 6 个字段, 包含 version counters + fingerprints + has_synced
- **结论**: S2 将 state.toml 描述为 "sync snapshots", 但实际内容是 version state (版本号 + 指纹), 不是 SyncSnapshot (manifest hash). SyncSnapshot 存在 config.toml 的 `ProjectConfig.sync_snapshots` 中 (S4 A5/A6). S2 混淆了 SyncSnapshot 和 SyncState 的概念.

### 2.4 ConfigStore 记录位置归类

- **S1 Section 3** (Sync/Projection Rules): 提到 `ConfigStore.reload_if_changed()` 但说 Backend 不使用它, 仅 sync crate 使用.
- **S4 B8**: 将 ConfigStore 放在 "运行时层", 但它在 `config.rs` (sync crate) 中, 并且 S1 明确说 Backend 不使用它.
- **结论**: 轻微不一致. ConfigStore 归为运行时层字段可能误导读者以为 Backend 使用它. S4 B8 应注明 "仅 SyncCoordinator 使用, Backend 直接持有 SyncConfig clone".

---

## 3. 遗漏项

### 3.1 Layer 4 (Discovery Runtime) 在 S4 中无对应字段区

- **S1 Layer 4**: 描述了 `SharedState` (`peers: Mutex<HashMap<DeviceId, PeerRecord>>`), `paired_peers.json` 持久化, `PeerRecord.last_seen: Instant` 等
- **S2 Section 5.3/8.3**: 描述了 mDNS ServiceDaemon 和 paired_peers.json
- **S4**: 没有为 Discovery 层的 `SharedState`, `PeerRecord`, `paired_peers.json` 内部结构单独建表
- **遗漏字段**:
  - `PeerRecord` 的字段 (device_id, name, addresses, port, last_seen, service_name 等)
  - `paired_peers.json` 的 JSON schema (每个 paired peer 含哪些字段)
  - `SharedState.service_name_index`, `SharedState.paired_peers_cache`
- **影响**: 中等. Discovery 是独立 crate, 但其字段直接影响配对流程和 peer 可达性判断.

### 3.2 Layer 7 (Session Parse State) 在 S4 中无对应字段区

- **S1 Layer 7**: 描述 `ParsedSession` (含 `RecordLine` per JSON line, `trailing_newline`, `dirty` flag)
- **S4**: 没有列出 `ParsedSession`, `RecordLine` 的字段
- **遗漏字段**:
  - `ParsedSession.records: Vec<RecordLine>`, `trailing_newline: bool`
  - `RecordLine.raw: Vec<u8>`, `parsed: Value`, `dirty: bool`
- **影响**: 低. Session parse 是只读(+路径重写)操作, 不参与同步决策. 但作为状态全景文档应覆盖.

### 3.3 TLS 证书/密钥持久化文件在 S4 中无记录

- **S2 Section 8.5**: 描述 `~/.aisync/receiver.{cert,key}` 由 `load_or_create_receiver_identity()` 创建
- **S4 C6**: 列出了 `TlsIdentity` 的内存结构 (`cert_der`, `private_key_der`), 但未记录磁盘文件路径和写入时机
- **S1 Layer 4**: 提到 `paired_peers.json` 含 certificates
- **遗漏**: 本端 TLS 身份文件的磁盘路径、格式、写入条件应在 S4 配置层或新增"安全/身份层"中记录.

### 3.4 日志文件 (`~/.aisync/logs/aisync.log`) 在 S1/S4 中无记录

- **S2 Section 8.6**: 描述了日志文件, 无 rotation, append-only
- **S1**: Layer 5 仅列 3 种 JSONL 历史文件, 未提及日志文件
- **S4**: D 区 (历史层) 仅列 3 种 JSONL, 未提及日志文件
- **影响**: 低 (日志不参与状态决策), 但完整性上应记录.

### 3.5 S4 E 区 (前端 DTO) 在 S1/S2 中完全未提及

- **S4 E1-E5**: 详细列出了 DTO 层 (PeerDto, ProjectDto, SyncProgressDto, ConflictDto 等)
- **S1**: 没有 "前端展示层" 的概念
- **S2**: 没有将 DTO 作为实体列出
- **影响**: 低. DTO 是派生视图, 不是独立状态层. 但 S1 的层次模型如果要覆盖完整数据流, 应提及 "前端通过 Tauri IPC 获取 DTO 快照, DTO 是 L1+L3+L5 的只读投影".

### 3.6 Tauri Event 通道在三份文档中均未覆盖

- Backend 通过 `app_handle.emit_all()` 向前端推送事件 (sync progress, pairing notifications 等)
- 这是一种临时状态通道 (事件队列), 不在 S1 的 7 层模型中, 也不在 S4 的字段表中
- **影响**: 低. 事件是瞬态的, 不持久化. 但作为 "数据如何流向前端" 的完整描述, 应在 S1 的 Projection Rules 中提及.

### 3.7 S2 提到的 Ed25519 Keychain 身份在 S4 中无记录

- **S2 Section 9.1**: macOS Keychain 中存储 `"device:<uuid>:ed25519"` 密钥
- **S4**: 无对应字段. `DeviceConfig.id` (A2) 是 UUID, 但 Ed25519 密钥对的存储和读取未在字段全景中出现
- **影响**: 中等. 这是安全关键状态 -- 设备身份的私钥.

### 3.8 S2 提到的 Tailscale 发现在 S1 中未建模

- **S2 Section 9.2**: `discover_tailscale_peers()` 调用 `tailscale status --json`
- **S1 Layer 4**: 仅描述 mDNS 发现, 未提及 Tailscale 作为第二种发现机制
- **影响**: 低. Tailscale 是按需调用, 不持有持久状态. 但 S1 Layer 4 标题 "Discovery Runtime (mDNS + Pairing Store)" 暗示发现仅含 mDNS, 不完整.

---

## 4. 总结

| 类别 | 数量 | 严重程度 |
|------|------|----------|
| 通过项 | 6 | -- |
| 矛盾项 | 4 | 2.1 低(文字错误), 2.2 中(路径/格式矛盾), 2.3 中(概念混淆), 2.4 低(归类偏差) |
| 遗漏项 | 8 | 3.1 中, 3.2 低, 3.3 中, 3.4 低, 3.5 低, 3.6 低, 3.7 中, 3.8 低 |

**必须修复** (阻塞后续文档):
1. S1 层数 "6" -> "7" (矛盾 2.1)
2. S2 Section 8.2 state.toml 描述修正 -- 不是 "sync snapshots" 而是 "version state" (矛盾 2.3)
3. S2 Section 8.4 history 路径格式需查源码确认后统一 (矛盾 2.2)

**建议补充** (不阻塞但影响完整性):
4. S4 新增 Discovery 字段区 (遗漏 3.1)
5. S4 新增 Session Parse 字段区 (遗漏 3.2)
6. S4 新增 TLS 身份文件记录 (遗漏 3.3)
7. S4 B8 ConfigStore 加注 "仅 SyncCoordinator 使用" (矛盾 2.4)
