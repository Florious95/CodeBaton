# CodeBaton 架构文档 · 阅读导航（READING-GUIDE）

本篇是新手与调试者的"入口指南"：教你**遇到 bug 从哪读起**、**加功能在哪插入**、
**代码落在哪个子模块**，以及 12 篇文档之间的**先读后读顺序**。

> 主路由器仍是 [INDEX.md](INDEX.md)。本篇只做导航，不复述事实/缺陷内容（BUG-N / INV-NN / DIRTY-NN / §
> 编号一律以各自专篇为准）。机器可读的符号→落点索引见文末 [_refactor-symbol-map.txt](_refactor-symbol-map.txt)。

---

## 1. 遇到 bug，从哪个文档开始读

**总是先回到 [INDEX.md](INDEX.md) §架构师检索路由**：那张"情境 → 检索顺序"表把每类症状映射到一串按
优先级排好的文档跳转链，是排障的主路由器，本篇不重复它。

路由哲学三句话：

1. **按"用户看到的现象"反查，而不是按你以为的根因。** 先从症状定位文档，再顺链收敛到代码。
2. **运行时"看似同步了却没生效" / 脏态 / 重启回退** → 直接跳 [09-dirty-state.md](09-dirty-state.md)
   的 DIRTY-NN 症状索引（它就是为"用户报现象"建的反查表）。
3. **数据丢失（远端文件被覆盖/销毁）** → [08-consistency-rules.md](08-consistency-rules.md)（50% 安全阀
   / 备份 / 回收站等不变量）配合 [05-dependencies.md](05-dependencies.md)（数据丢失防护与单点故障）一起读。

实在对不上情境，就看 [review-final.md](review-final.md) 的矛盾/遗漏清单——可能是已知文档缺口。

---

## 2. 要加新功能，从哪个文档找插入点

先用下表确认"要改哪些文档/不变量"，再确认"代码落到哪个子模块"。

| 功能类型 | 先读的文档（找插入点） | 代码落在哪个子模块 |
|---------|----------------------|------------------|
| **新增 config 字段** | 04-fields §A（字段列）+ 11-migration §1（`#[serde(default)]` 是唯一兼容手段） | `mod.rs` 的 Inner/config 投影 + 写盘点；字段语义按用途散落 |
| **新增协议消息 / Message 变体** | 04-fields §C + 11-migration §3（严格相等 Hello，须升版 + 全节点升级）+ 02-entities §5（入站路由） | `transport.rs`（无 Inner 的发送/描述符）+ 入站路由所在的业务模块 |
| **新增用户/系统操作** | 06-operations（前置/状态变更/成功标准/失败回滚） | 按操作归属：项目 `projects.rs` / workspace `workspaces.rs` / 配对 `peers.rs` |
| **新增后台线程** | 02-entities（生命周期 owner）+ 10-concurrency（锁顺序、是否 bypass Inner） | 线程 spawn 包装放 `watchers.rs` 或 `session_scanner.rs`；gate 消费循环留 `mod.rs` |
| **新增同步引擎逻辑** | 06-operations §push 链 + 08 不变量 + 07 真相层次 | `split_brain.rs`（run_sync / 脑裂检测 / 解决） |
| **新增传输发送方** | 04-fields §C + 05-dependencies §TLS | `transport.rs`（无状态 sender / 描述符构建器） |
| **新增 history 记录类型** | 04-fields §D（JSONL schema） | `history.rs`（append/read 原语 + record_* 记录器） |
| **新增自动同步驱动/指纹** | 06-operations §B + 08 INV-35/36/37 | `auto_sync_orchestration.rs`（驱动 + 指纹）；门控统计在 `auto_sync_gate.rs` |

---

## 3. 20 个子模块速查表（+ mod.rs HUB = 21 行）

> 一句话职责取自重构落点说明；关键符号供 grep。完整逐符号清单见
> [_refactor-symbol-map.txt](_refactor-symbol-map.txt)。

| 子模块 | 职责一句话 | 关键符号 |
|--------|-----------|---------|
| `mod.rs`（HUB） | Backend/Inner/单 Mutex/Drop/构造器/点锁方法 + 未模块化的 watcher 门控消费循环等 | Backend, Inner, with_config, app_log, start_project_watcher（单数） |
| `time_util.rs` | 纯 SystemTime 包装 | unix_secs_now, epoch_millis, unix_nanos |
| `events.rs` | 事件计数 + RecordedEvent 环形缓冲 + 日志 sink | record_event, log_line, RecordedEvent |
| `identity.rs` | TLS 接收端身份 + 设备名叶子 | load_or_create_receiver_identity, default_device_name |
| `auto_sync_gate.rs` | 4 个进程级 static 门控/抑制/基线 + 冷却覆盖（非 Inner 字段） | try_begin_auto_sync, AUTO_SYNC_GATES, incoming_sync_recent |
| `workspace_conflict.rs` | 纯 split-brain 分析 + manifest 指纹 | analyze_workspace_conflicts, manifest_fingerprint, child_manifest |
| `transport.rs` | 无状态 sender + 无 Inner 的描述符构建器 | peer_transport_connection, run_control_future, pairing_tls_config |
| `history.rs` | JSONL 原语 + record_sync 系列 + 摘要器 | append_json_line, read_jsonl, sync_history, HistoryFileSummary |
| `auto_sync_orchestration.rs` | 自动同步驱动 + 指纹 + hash_prefix | run_project_auto_sync, project_auto_sync_fingerprint, WorkspaceSyncOutcome |
| `session_stage.rs` | FS session 暂存（原件从不改，BUG-6） | prepare_claude_session_sync, SessionSyncPlan, project_rewriter |
| `sync_push.rs` | 网络 push（不持 Inner 锁，按快照写回） | run_tcp_push, run_workspace_tcp_push |
| `claude_paths.rs` | claude 路径 helper（watcher/scanner/staging 共享） | local_claude_projects_root, remote_claude_projects_dir |
| `watchers.rs` | FsWatcher spawn 包装 + 监视/排除规则（门控消费循环不在此） | start_project_watchers（复数）, exclude rules |
| `session_scanner.rs` | session-mtime 扫描线程 + 分类（扫描顺序行为敏感） | start_session_mtime_scanner, classify_session_mtime, SessionMtimeTarget |
| `messaging.rs` | 文本消息 impl Backend 方法 | send_text_message, text_messages, record_text_message_history |
| `file_transfer.rs` | 自包含文件收发流程 | request_file_transfer, receive_file_transfer_data, OutboundFileTransfer |
| `peers.rs` | peer/配对 CRUD（confirm_pairing 在锁内做 fs I/O，故意） | add_peer_endpoint, confirm_pairing, PairingInfo |
| `projects.rs` | 项目 CRUD + 映射 impl Backend | add_project, confirm_project_mapping_request |
| `workspaces.rs` | workspace CRUD + 映射 impl Backend | add_workspace, scan_workspace_path, confirm_workspace_mapping_request |
| `serve.rs` | serve 守护进程（serve_info/shutdown 方法留 mod.rs） | start_serve_daemon, ServeInfo, ServeShutdownHandle |
| `split_brain.rs` | 同步引擎 impl Backend（run_sync 跨网络 I/O 持锁，Rule1 例外） | run_sync, run_workspace_sync, check_split_brain, resolve_split_brain |

> ⚠ 注意单复数：`watchers.rs` 里的 `start_project_watchers`（复数）是 spawn 包装；真正含门控消费循环的
> `start_project_watcher`（单数）留在 `mod.rs`。`split_brain.rs` 的 `run_sync` 在网络 I/O 期间持有 Inner
> 锁（文档化的 Rule1 例外），而 `run_workspace_sync` / `probe_target_status` 在网络前已释放 guard。

---

## 4. 文档之间的依赖关系（先读什么后读什么）

整体地基是 **[01-state-model.md](01-state-model.md)**（谁是 truth、谁是 cache）——没有它，后面所有篇的
"权威/缓存"论述都悬空。在此之上：

- **真相与实体层**：[07-truth-hierarchy.md](07-truth-hierarchy.md)（权威规则）+ [02-entities.md](02-entities.md)（live 实体/线程）。
- **参考查表层**（按需跳查，不必通读）：[03-identity.md](03-identity.md)（身份）、[04-fields.md](04-fields.md)（字段全景）。
- **失败/并发层**：[05-dependencies.md](05-dependencies.md)（外部依赖失败模式/SPOF）+ [10-concurrency.md](10-concurrency.md)（锁顺序/原子性）。
- **操作枢纽**：[06-operations.md](06-operations.md) 把每个操作串到上面所有篇。
- **bug 检测/症状层**：[08-consistency-rules.md](08-consistency-rules.md)（不变量谓词）+ [09-dirty-state.md](09-dirty-state.md)（脏态症状索引）。
- **升级/部署层**：[11-migration.md](11-migration.md)（版本迁移）+ [12-environment.md](12-environment.md)（安装运行环境）。

### 新手通读路径（建立全局心智）
1. 01-state-model → 2. 07-truth-hierarchy → 3. 02-entities → 4. 03-identity（速览）→
5. 04-fields（速览）→ 6. 05-dependencies → 7. 10-concurrency → 8. 06-operations →
9. 08-consistency-rules → 10. 09-dirty-state → 11. 11-migration → 12. 12-environment。

### 排障快路径（已知症状，只想定位）
1. [INDEX.md](INDEX.md) §架构师检索路由 选情境 → 2. 顺链读（多以 09 / 08 起步）→
3. 命中代码模块后回本篇 §3 速查表查"哪个子模块" → 4. 用 [_refactor-symbol-map.txt](_refactor-symbol-map.txt)
按符号名 grep 出 `backend/<file>:<line>` 精确落点。

---

## 5. 符号→落点的机器可读索引

需要"某函数/struct 现在在哪个文件第几行"时，不要靠记忆——直接 grep
[_refactor-symbol-map.txt](_refactor-symbol-map.txt)（格式 `backend/<file>:<line>  <symbol>`）。
留在 HUB 的符号会指向 `backend/mod.rs`。重构后所有 `backend.rs:NNN` 旧引用均已失效，以该文件为准。
