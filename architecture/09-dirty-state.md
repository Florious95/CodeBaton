# 09 - 脏状态分类（Dirty-State Classification）

系统可能进入的所有**脏/不一致/损坏状态**类型。每类标注：**如何产生**（成因）、**如何检测**（或 UNDETECTED）、**如何修复**（或 UNREPAIRED/manual/UNIMPLEMENTED）、严重度。

所有声明经源码 `file:line` 核实（对抗性 verifier 逐条复核）。引用路径相对仓库根。共 **33 类**（DIRTY-01~33）。

> 这是运维/调试的索引：用户遇到"看起来同步了但没生效""配对显示成功但推送失败"等问题时，先在此表按症状定位脏状态类型，再看其检测/修复策略。

---

## 0. 速查表（按检测能力分组）

| 检测能力 | 脏状态 |
|---------|--------|
| **完全未检测（UNDETECTED）** | DIRTY-02, 03, 04, 05, 06, 07, 08, 14, 15, 16, 18, 19, 25, 32 |
| **部分检测（PARTIAL）** | DIRTY-01, 13, 22, 23, 30 |
| **已检测但修复受限** | DIRTY-11（PreferRemote 未实现）, 12（仅手动）, 17（仅手动重配对）, 20（仅警告不防）, 27（明确报错但功能缺失）, 33（自愈） |

| 修复能力 | 脏状态 |
|---------|--------|
| **UNREPAIRED / 须重启或手动** | DIRTY-02, 05, 06, 07, 08, 14, 15, 16, 18, 19, 24, 25, 26, 31, 32 |
| **UNIMPLEMENTED** | DIRTY-11(PreferRemote), 27(Gemini converter) |
| **部分/自愈** | DIRTY-01, 09, 13, 22, 23, 33 |
| **手动重建** | DIRTY-03, 04, 17, 18, 20 |

---

## 1. 内存/磁盘 config 不一致

### DIRTY-01 · medium · 快照/config 磁盘-内存 desync（push 后 + 后台刷新）
**描述**：`run_tcp_push` 经 load_config→set_sync_snapshot→save_config 把 post-sync 快照直接写盘**不碰** live `g.config`。`refresh_and_save_workspaces`（mtime scanner 调）也只 load+save 写盘。Backend 内存 config 因此落后磁盘（陈旧/缺失快照、陈旧 workspace 子项目），而 `check_split_brain` 读内存快照。
**成因**：`backend/sync_push.rs:20`（run_tcp_push 独立 load+set+save）；`refresh_and_save_workspaces` 只写盘从不更 g.config (`backend/mod.rs:977`)。
**检测**：PARTIAL——`run_sync` 在成功 push 后把 persisted 快照重读回 g.config，但只这一字段、只 `is_ok()` 时 (`backend/split_brain.rs:73`)。其余无通用 reconciliation。
**修复**：`run_sync` 成功时回填快照；scanner/watcher 每轮重读盘读到新鲜盘态。其它内存漂移持续到 Backend 重建。
引用：`backend/sync_push.rs:20`, `backend/split_brain.rs:73`, `backend/mod.rs:977`

### DIRTY-02 · medium · 失败 save 后非回滚的内存 config 改动
**描述**：多个 config setter 在 save_config **前**改 g.config 且 save 失败时不回滚，内存与盘背离。对比 add_project 用候选-clone-save-commit 模式。重启时内存改动静默丢失；会话中漂移持续。
**成因**：`update_device_name_locked`（设 name 再 save，无 revert，`backend/mod.rs:922`）；`set_refresh_interval_secs`（`backend/mod.rs:550`）；`set_default_file_receive_dir`（`backend/mod.rs:491`）；`add_peer_endpoint`（插 g.config.peers 再 save，`backend/peers.rs:75`）；`persist_workspace_update`（`backend/split_brain.rs:62`）；`config_with_refreshed_workspaces`（`g.config=refreshed` 后 `let _ =` 吞 save 错，`backend/mod.rs:518`）。
**检测**：UNDETECTED——错误被返回（或 `config_with_refreshed_workspaces` 吞掉）但内存态不回滚，无 memory-vs-disk reconciliation 除全量重载。
**修复**：UNREPAIRED——仅进程重启（重载盘）丢弃内存漂移。
引用：`backend/mod.rs:922`, `backend/mod.rs:550`, `backend/mod.rs:491`, `backend/peers.rs:75`, `backend/split_brain.rs:62`, `backend/mod.rs:518`

### DIRTY-29 · medium · watcher/scanner 加载失败静默退回陈旧快照 config
**描述**：每个后台循环（项目 watcher、workspace watcher、mtime scanner）经 `load_config(config_path).unwrap_or_else(|_| fallback_config.clone())` 加载。fallback 是循环创建时的 config 快照。config 文件瞬时不可读/损坏/校验失败（如某项目 local 被删使 validate_config 报错）时，循环继续对**旧**捕获 config auto-sync，无报错浮现。
**成因**：`load_config` 跑 `validate_config`，enabled 项目 local 缺失即报错 (`config.rs:386-392`, 经 `:345-356`)。循环用 `unwrap_or_else(|_| fallback_config.clone())`（项目 watcher `backend/mod.rs:1220`、workspace watcher `backend/mod.rs:1439`、mtime scanner `backend/session_scanner.rs:120`）。
**检测**：UNDETECTED——加载错误被 unwrap_or_else 吞掉。
**修复**：UNREPAIRED——对陈旧 config 运行直到进程重启。
引用：`backend/mod.rs:1220`, `backend/mod.rs:1439`, `backend/session_scanner.rs:120`, `config.rs:345-356`, `:386-392`

### DIRTY-31 · low · 内存 exclude 注入在 early-return/panic 时不恢复
**描述**：`run_sync` 经 `inject_excludes` 给 g.config 注入 per-run 敏感 exclude，仅经直线 `restore_excludes` 在 push 后恢复。无 RAII/Drop guard；inject 与 restore 间任何 early return（`?`）或 panic 会把临时 per-run exclude 永久留在内存项目 config，后续 save 可能持久化它。
**成因**：`inject_excludes` 改 g.config 的 project.exclude_rules (`backend/split_brain.rs:362`)；`restore_excludes` 仅在 result block 后跑 (`backend/split_brain.rs:376`)。无 Drop guard。
**检测**：UNDETECTED。
**修复**：仅正常路径恢复；early-exit/panic 时 UNREPAIRED。
引用：`backend/split_brain.rs:362`, `backend/split_brain.rs:376`

---

## 2. Ack-before-save 非对称态

### DIRTY-03 · high · 接收端 ack-before-save：peer 被告知映射接受先于接收端持久化
**描述**：`confirm_project_mapping_request` **先**把 ProjectMappingAck（accepted=true，含 remote_dir）发给请求 peer，**再**获锁 save_config。save_config 失败时请求方已被告知接受（会记录/推送），而接收方从未持久化其侧、未启 watcher——非对称映射。
**成因**：`send_project_mapping_ack` (`backend/transport.rs:117`) 先于 confirm_project_mapping_request (`backend/projects.rs:39`) 内的 `save_config(&config_path,&candidate)?`；save 错在 ack 已上线后返 Err。
**检测**：UNDETECTED——ack 是 fire-and-forget，无确认接收方已持久化的 reconciliation 握手。
**修复**：UNREPAIRED/manual——用户须重建映射，无自动重试或 ack 回滚。
引用：`backend/transport.rs:117`, `backend/projects.rs:39`

### DIRTY-04 · medium · 发送端 outbound 映射在 save 前移除（save 错时丢映射）
**描述**：`process_project_mapping_acks` 在持久化新项目**前**移除 `outbound_project_mappings` 条目。save_config 失败时 outbound 条目已没（不能重 ack）且项目未持久化——发送方丢映射而接收方已建。
**成因**：`process_project_mapping_acks` 内 `g.outbound_project_mappings.remove(&ack.request_id)` 在 `candidate.projects.push` + `save_config(&path,&candidate)?` 之前 (`backend/projects.rs:160`)；`?` 在 remove 后传播。
**检测**：UNDETECTED。
**修复**：UNREPAIRED/manual。
引用：`backend/projects.rs:160`

---

## 3. 孤儿资源

### DIRTY-05 · low · unpair 后孤儿 pinned cert .der
**描述**：unpair 从 config 和 discovery 配对存储移除 peer，但从不删 `persist_peer_connection` 配对时写的 `<config>/peers/<peer-id>-receiver.der`。cert 滞留盘上。
**成因**：unpair 移除 config.peers/claude_config.peers/project.peers/workspace.peers (`backend/peers.rs:392`) 但无 `fs::remove_file`。persist_peer_connection 经 `fs::write` 写该文件 (`backend/peers.rs:466`)，路径由 `peer_receiver_cert_path` 构造 (`backend/identity.rs:78`)。
**检测**：UNDETECTED。
**修复**：UNREPAIRED——文件留到手动清理；后续重配对经 `fs::write` 覆盖。
引用：`backend/peers.rs:392`, `backend/peers.rs:466`, `backend/identity.rs:78`

### DIRTY-06 · low · unpair/peer 移除后孤儿 sync_snapshots key
**描述**：sync_snapshots 是每 ProjectConfig 内按 peer **名** keyed 的 HashMap。unpair 移除 project.peers[name] 但从不移除 project.sync_snapshots[name]。已移除 peer 的快照持续，peer 名后续复用时可能误触发脑裂检查。
**成因**：unpair 遍历项目调 `project.peers.remove(&name)` (`backend/peers.rs:392`) 但从不 `project.sync_snapshots.remove(&name)`。sync_snapshots 按名 keyed (`config.rs:211`)；set_sync_snapshot 仅插入 (`:138-149`)。
**检测**：UNDETECTED。
**修复**：UNREPAIRED——陈旧快照 key 持续；仅同名 peer 重配对并同步时覆盖。
引用：`backend/peers.rs:392`, `config.rs:211`, `:138-149`

### DIRTY-07 · low · 失败文件传输的孤儿 .part 文件
**描述**：incoming 文件传输数据追加到 sibling `<name>.<transfer_id>.part`，仅完成时 rename 到 target。offset 失配或最终 size 失配时函数返 Err **不**移除部分 .part，留在盘上。.part 仅在传输**开始**时移除，非中途失败时。
**成因**：`receive_file_transfer_data` 在 `data.offset != state.bytes_written` 及 done 时 size 失配 (`backend/file_transfer.rs:518`) 返 Err，已追加到 state.tmp_path 后那些错误分支无清理。`file_transfer_tmp_path` 构 `<name>.<id>.part` (`backend/file_transfer.rs:510`)；唯一 remove_file 在传输开始（request_file_transfer，`backend/file_transfer.rs:40`）。
**检测**：UNDETECTED——遗留 .part 不被追踪或回收。
**修复**：部分——同 target 的**新**传输开始时移除 tmp。否则 UNREPAIRED。
引用：`backend/file_transfer.rs:518`, `backend/file_transfer.rs:510`, `backend/file_transfer.rs:40`

### DIRTY-08 · low · 失败映射添加后留下的自动创建 local dir
**描述**：add_project 与 confirm_project_mapping_request 在持久化前 `mkdir -p` local 目标。后续步骤失败（save_config 校验错，或 confirm 路径的"已存在"检查）时新建空目录留盘。config 回滚（clone-then-commit）保护 config 不保护文件系统目录。
**成因**：add_project：`fs::create_dir_all(&local)`（create_local_dir 分支），再 `save_config(&path,&candidate)?` (`backend/projects.rs:223`)。confirm_project_mapping_request：`fs::create_dir_all(&local_dir)` 先于项目已存在检查和 save_config (`backend/projects.rs:39`)。
**检测**：UNDETECTED。
**修复**：UNREPAIRED——孤儿目录留存。
引用：`backend/projects.rs:223`, `backend/projects.rs:39`

### DIRTY-09 · medium · 连接中断时孤儿接收 staging dir
**描述**：`prepare_staging` 在接收前建 sibling `.aisync-staging-<nanos>`（target 的全量副本）。commit 错与 receive 错时移除，但 prepare_staging 与 match 臂间任务死亡（panic、killed worker、transport drop）时 staging 副本被孤儿化。session sync 路径类似建 `.aisync-session-stage-<nanos>`，仅成功路径移除。
**成因**：`prepare_staging` 复制 target 进 `.aisync-staging-<nanos>` (`transport lib.rs:3245-3266`) 在 `:2022` 调；显式 `fs::remove_dir_all` 仅在 commit-error 臂 (`:2055-2056`) 与 receive-error 臂 (`:2094-2095`)。session staging 根在 prepare_claude_session_sync 建 (`backend/session_stage.rs:53`)，仅成功时 `let _ = fs::remove_dir_all`（run_tcp_push，`backend/sync_push.rs:20`）。
**检测**：UNDETECTED——无启动时对陈旧 `.aisync-staging-*`/`.aisync-session-stage-*` 的清扫。
**修复**：突死场景 UNREPAIRED；正常错误路径清理。prepare_staging 每次用新 nanos 名，从不复用陈旧的。
引用：`transport lib.rs:3245-3266`, `:2022`, `:2055-2056`, `:2094-2095`, `backend/session_stage.rs:53`, `backend/sync_push.rs:20`

### DIRTY-32 · low · 崩溃时孤儿原子写临时文件（.aisync-tmp / .tmp）
**描述**：`write_file_atomic` 写 `<path>.aisync-tmp` 后 rename；`save_pairings` 写 `<path>.tmp` 后 rename。write 与 rename 间崩溃留陈旧 .aisync-tmp/.tmp 从不清理；下次写用新 tmp 名覆盖 target 但崩溃的 temp 滞留。
**成因**：`write_file_atomic`：`tmp = path.with_extension("aisync-tmp")`; `fs::write(&tmp)`; `fs::rename(tmp,path)` (`transport lib.rs:3587-3592`)。`save_pairings`：`tmp_path = path.with_extension("tmp")`; write; rename (`discovery lib.rs:1565-1573`)。
**检测**：UNDETECTED。
**修复**：UNREPAIRED——孤儿临时文件持续。
引用：`transport lib.rs:3582-3594`, `discovery lib.rs:1560-1575`

### DIRTY-25 · low · 已删项目/peer 的陈旧 history.jsonl 行
**描述**：sync 与文件传输 history 是追加 JSONL（history.jsonl、file_transfer_history.jsonl）。delete_project/unpair 移除 config 条目但从不剪引用已删项目/peer 的 history 行，history 累积对已不存在实体的引用。
**成因**：`record_sync_scoped` 按 project_id 追加 history.jsonl (`backend/history.rs:38`)；record_file_transfer_history 追加 file_transfer_history.jsonl (`backend/file_transfer.rs:594`)；delete_project 只编辑 config (`backend/projects.rs:290`)；sync_history 按 project_id 过滤但不验证项目仍存 (`backend/history.rs:129`)。
**检测**：UNDETECTED。
**修复**：UNREPAIRED/manual——文件随孤儿引用无界增长。
引用：`backend/history.rs:38`, `backend/file_transfer.rs:594`, `backend/projects.rs:290`, `backend/history.rs:129`

### DIRTY-26 · medium · 已删项目的孤儿 SyncState（state.toml）条目（名/id 复用）
**描述**：delete_project 从 config 移除 ProjectConfig（及其嵌入 sync_snapshots），但独立 state.toml 中按 project_id keyed 的 ProjectVersionState 从不剪。同名重建项目复活陈旧版本/指纹态，detect_conflict 随后读它，造成假/抑制冲突检测。
**成因**：`SyncState.projects` 按 project_id keyed (`lib.rs:411-413`)，load/save 到 state_path (`:417-435`)；detect_conflict 经 `self.state.project(project_id)` 读先前指纹 (`:355-371`)。delete_project 只编辑 config 移 watcher (`backend/projects.rs:290`)，无 state.toml 清理。
**检测**：UNDETECTED。
**修复**：UNREPAIRED——名复用时陈旧 state 静默复用。
引用：`lib.rs:411-435`, `:355-371`, `backend/projects.rs:290`

---

## 4. 脑裂 / 冲突

### DIRTY-11 · high · 脑裂 PreferRemote 未实现；workspace 子项目卡 conflicted
**描述**：`resolve_split_brain` 只实现 PreferLocal（强制覆盖 push 带 remote 备份）。PreferRemote（反向 pull）返 "not yet implemented"，用户想倒向 remote 解决的脑裂无法自动解决。另外，被标 conflicted 的 workspace 子项目卡 conflicted 直到 local 与 remote 指纹和解，期间被排除出 safe_children/pushes。
**成因**：`resolve_split_brain` PreferRemote 返 Err (`backend/split_brain.rs:304`)；PreferLocal 调 run_sync forced-overwrite (`backend/split_brain.rs:73`)。`analyze_workspace_conflicts`：`if split_brain || (child.conflicted && local!=remote) { child.conflicted=true; continue; }`，匹配时清 (`backend/workspace_conflict.rs:18`)。
**检测**：DETECTED——check_split_brain 标 split_brain=true (`backend/split_brain.rs:248`)；child.conflicted 持久化在 `WorkspaceChildConfig.conflicted` (`config.rs:249`)。
**修复**：PARTIAL/manual——PreferLocal 可用；PreferRemote UNIMPLEMENTED。workspace 子项目在指纹再匹配时自动清 (`backend/workspace_conflict.rs:18`)，否则手动。
引用：`backend/split_brain.rs:304`, `backend/workspace_conflict.rs:18`, `config.rs:249`

### DIRTY-12 · high · 脑裂：两侧自上次同步都背离
**描述**：同步后 local 项目与 remote target 被独立修改。存储快照不再匹配 remote 当前 manifest hash，后续 push 会覆盖独立 remote 工作。SyncCoordinator 类比在 local 与 remote 指纹都变时标 ConflictDetected。
**成因**：check_split_brain 探测 remote live manifest_hash 比 `snapshot.peer_last_known_hash` (`backend/split_brain.rs:248`)。`detect_conflict` 在 local_changed && remote_changed 时返 ConflictDetected (`lib.rs:362-371`)。
**检测**：DETECTED——check_split_brain 返 split_brain=true；detect_conflict 返 ConflictDetected。
**修复**：手动经 resolve_split_brain（PreferLocal push 带 remote 备份）。peer 不可达时 split_brain 假报 false（见 DIRTY-13）。
引用：`backend/split_brain.rs:248`, `lib.rs:349-374`

### DIRTY-13 · medium · 脑裂假阴性：peer 不可达或快照缺失
**描述**：check_split_brain 在 peer 不可达（探测失败）**或**无本地快照时返 split_brain=false，把未知当安全。调用方无法区分"无冲突"与"无法检查"。check_target_not_empty 也在探测错时返 Ok(false)，覆盖保护检查也 fail open。
**成因**：探测错时 check_split_brain 记 check_split_brain_unreachable 返 split_brain:false；快照 None 时返 split_brain=false (`backend/split_brain.rs:248`)。check_target_not_empty 探测错返 Ok(false) (`backend/split_brain.rs:218`)。
**检测**：PARTIAL——reachable=false 被浮现，但 split_brain 在这些路径硬编码 false。
**修复**：依赖调用方/前端特殊对待 unreachable/unknown；无后端保障。
引用：`backend/split_brain.rs:248`, `backend/split_brain.rs:218`

### DIRTY-30 · medium · 安全阀中止：客户端可能为接收方拒绝的 commit 持久化快照
**描述**：接收方在收完所有变更**后**算 >50% 删除比；中止时移除 staging 写 Message::Error 后 shutdown。客户端仅在 sync_directory_to 返 Ok 后持久化快照，但依赖 Error 帧在连接 drop 前到达客户端是脆弱的（代码注释标此风险）。若只见 close_notify/EOF，客户端可能为接收方实际拒绝的 commit 持久化快照，产生与接收方盘态不符的快照 hash。
**成因**：commit_staging_with_options 在 delete_ratio>0.5 时返 Err (`transport lib.rs:3357-3368`)；接收方写 Message::Error 后 shutdown，注释指 commit 须先于 SyncComplete 以免客户端在失败 commit 上存快照 (`:2051-2066`)。客户端仅成功 push 后持久化快照（run_tcp_push，`backend/sync_push.rs:20`）。
**检测**：接收方经安全阀检测；仅当 Error 帧在连接 drop 前被读到才浮现给客户端。
**修复**：接收方态保留（staging 移除，target 不动）。客户端快照在错误未传播时 UNREPAIRED。
引用：`transport lib.rs:3357-3368`, `:2051-2066`, `backend/sync_push.rs:20`

---

## 5. 陈旧/泄漏的运行时态

### DIRTY-14 · medium · 进程全局 auto-sync 静态跨 Backend 实例泄漏/碰撞
**描述**：AUTO_SYNC_GATES、INCOMING_SYNC_SUPPRESSIONS、SESSION_BASELINE_SEEDS、WORKSPACE_PROPAGATION_BYPASS 是按 `scope:name:peer`（或 path）keyed 的进程全局 `OnceLock<Mutex<...>>`。多 Backend 实例（测试，或一进程两 config）共享它们，一实例的在飞 gate/suppression/baseline 可抑制或误 baseline 另一实例的同步；前一 Backend 的陈旧条目持续。
**成因**：模块级静态（auto_sync_gates/incoming_sync_suppressions/session_baseline_seeds/workspace_propagation_bypass，`backend/auto_sync_gate.rs:53`）；`auto_sync_gate_key = format!("{scope}:{name}:{peer}")` (`backend/auto_sync_gate.rs:93`)，非按 config-path 限定；try_begin_auto_sync 仅按 in_flight||cooldown retain 剪除 (`backend/auto_sync_gate.rs:97`)。
**检测**：UNDETECTED。
**修复**：过期 gate 条目部分自剪；Backend drop 时不清。态持续整进程生命周期。
引用：`backend/auto_sync_gate.rs:53`, `backend/auto_sync_gate.rs:93`, `backend/auto_sync_gate.rs:97`

### DIRTY-15 · medium · 孤儿 session-mtime scanner 线程跨 Backend 生命周期泄漏
**描述**：`start_session_mtime_scanner` 在 Backend 构造时 spawn 无限循环的 detached std::thread；不存 JoinHandle 或 stop 信号。Backend::drop 只停 serve daemon。Backend drop 后 scanner 继续运行，重读盘 config 并可能 auto-sync。测试/多实例时每次构造泄漏一个 live scanner。
**成因**：`std::thread::spawn(move || loop { ... })` 在 start_session_mtime_scanner (`backend/session_scanner.rs:120`)，循环结束无 handle；构造器 with_config 调 (`backend/mod.rs:350`)；Inner 无其字段 (`backend/mod.rs:202`)；Backend::drop 只停 serve_shutdown (`backend/mod.rs:192`)。
**检测**：UNDETECTED。
**修复**：UNREPAIRED——仅进程退出停它。
引用：`backend/session_scanner.rs:120`, `backend/mod.rs:350`, `backend/mod.rs:192`

### DIRTY-16 · medium · auto_sync_paused 重启丢失且 scanner 不强制
**描述**：auto-sync 暂停开关是纯内存 Inner 字段，从不持久化到 SyncConfig；每次构造重置 false。且 scanner/watcher 循环从不读它，暂停可能实际不停 scanner 触发的 auto-sync。
**成因**：`Inner.auto_sync_paused: bool` (`backend/mod.rs:202`) 构造时初始化 false (`backend/mod.rs:350`)；仅经 backend 方法 get/set (`backend/mod.rs:634`) 和命令；SyncConfig 无此字段 (`config.rs:11-40`)；scanner 循环无读 auto_sync_paused（仅 `backend/mod.rs:634` set/get）。
**检测**：UNDETECTED——无暂停被丢的警告。
**修复**：UNREPAIRED——每次重启后用户须重暂停。
引用：`backend/mod.rs:202`, `backend/mod.rs:350`, `backend/mod.rs:634`, `config.rs:11-40`

---

## 6. 身份漂移

### DIRTY-17 · high · Tailscale DeviceId 因 IP 变化漂移 → 丢配对
**描述**：Tailscale 发现的 peer 的 DeviceId 由含 reachable[0]（首个可达 IP）的 seed 确定性 blake3-hash。peer IP 变时派生 DeviceId 变，paired_peers.json（按 DeviceId keyed）和 cert 路径不再匹配重发现的设备——配对显得丢失。manual peer 类似 hash socket addr。
**成因**：`devices_from_tailscale_status_json` 构 `id_seed = format!("tailscale:{dns}:{}", reachable[0])` (`discovery lib.rs:792-799`) 再 `deterministic_device_id(&id_seed)` (`:801`)；manual：`deterministic_device_id(&format!("manual:{address}"))` (`:823`)；deterministic_device_id blake3-hash seed (`:1577-1582`)；配对存储按 device.id keyed (`:1550-1553`)。
**检测**：UNDETECTED——旧 DeviceId 只是不再被见，显为新未配对设备。
**修复**：UNREPAIRED/manual——用户须以新身份重配对。
引用：`discovery lib.rs:792-806`, `:823`, `:1577-1582`, `:1550-1553`

### DIRTY-18 · medium · peer 改名孤儿化 config/快照/映射条目（peers 按名 keyed）
**描述**：config.peers、project.peers、claude_config.peers、workspace.peers 及每项目 sync_snapshots 全按 peer **名**（String）keyed，非稳定 DeviceId。peer（同 DeviceId）改名时 persist_peer_connection 在新名下建新条目，孤儿化旧名的 peer 条目、项目映射和快照；project_mapping 随后报 "no mapping for peer"。
**成因**：persist_peer_connection 做 `config.peers.entry(peer.name.clone()).or_insert_with(...)` (`backend/peers.rs:466`)。project.peers 和 sync_snapshots 按名 keyed (`config.rs:202`, `:211`)。project_mapping 按名查 `project.peers.get(peer_name)` (`config.rs:101-112`)。
**检测**：UNDETECTED——改名在 sync 时产生 "project has no mapping for peer"。
**修复**：UNREPAIRED/manual——无 rekeying；孤儿按名 key 条目留存。
引用：`backend/peers.rs:466`, `config.rs:202`, `:211`, `:95-112`

### DIRTY-19 · medium · 重复 hostname 覆盖 peer config 条目
**描述**：因 config.peers 按 peer 名 keyed，两不同设备共享同一 hostname 坍缩为一条目：persist_peer_connection 的 `entry(peer.name)` 覆盖先前同名 peer 的 id/endpoint/cert。validate_config 只去重项目/workspace 名，不去重 peers。
**成因**：`config.peers.entry(peer.name.clone()).or_insert_with(...)` 然后 `entry.id = peer.id`/endpoint/server_cert 被覆盖 (`backend/peers.rs:466`)。validate_config 只去重项目名 (`config.rs:380`) 和 workspace 名 (`:401`)，无 peer guard。
**检测**：UNDETECTED。
**修复**：UNREPAIRED/manual。
引用：`backend/peers.rs:466`, `config.rs:377-401`

---

## 7. 会话与路径重写

### DIRTY-20 · medium · 编码 session 目录名冲突（多原始路径 → 一目录）
**描述**：Claude session 目录按有损路径编码命名（每个非 `[A-Za-z0-9-_.]` 字符坍缩为 `-`）。两不同原始项目路径可编码为同一目录名。write_session 写入 `target_dir.join(encoded_dir_name)`，不同项目的 session 落入同一目录。SessionIndex::conflicts() 能检测但在运行 app 中**不被调用**（仅 session 测试中）。
**成因**：`claude_project_dir_name` 字符映射 (`backend/session_stage.rs:623`)；session sync 从 `claude_project_dir_name(remote_code_dir)` 设 session.encoded_dir_name 并在该名下 write_session（prepare_claude_session_sync，`backend/session_stage.rs:53`）。SessionIndex::from_sessions/conflicts() 在一编码目录映射 >1 原始路径时报 EncodingConflict (`claude_code.rs:159-180`)。
**检测**：仅经 SessionIndex::conflicts() 作**警告**检测；未接入 app sync 路径。
**修复**：UNREPAIRED——write_session 不去冲突；盘上目录仍冲突。
引用：`backend/session_stage.rs:623`, `backend/session_stage.rs:53`, `claude_code.rs:159-180`

### DIRTY-21 · low · dirty session 记录重序列化非字节相同
**描述**：session round-trip 仅对**未修改**记录字节相同（重放 raw 字节）。path 字段被重写的记录标 dirty 并经 `serde_json::to_string` 重序列化，可能相对原行重排 key/改空白——故重写记录除预期 path 改动外不与源字节相同。
**成因**：`RecordLine::emit` 在 clean 时返 self.raw、dirty 时返 `serde_json::to_string(&self.value)` (`claude_code.rs:74-84`)；`rewrite_structured_paths` 对任何结构化 path 改动设 record.dirty=true (`:297-308`)。
**检测**：UNDETECTED 字节布局（按设计——只要求 path 内容可逆，由 record_values 相等而非字节相等验证）。
**修复**：N/A by design——内容可逆，但改动记录的字节保真有意不保证。
引用：`claude_code.rs:74-84`, `:297-308`

### DIRTY-28 · medium · 不可逆启发式文本路径重写（Confidence::Medium）可改 session 散文
**描述**：RuleBasedRewriter 文本路径（经 PathRewriter trait / sync crate 的 copy_with_rewrite 用）扫自由文本找路径状候选，替换任何 prefix 匹配规则的，标命中 Confidence::Medium。这是启发式：合法出现在散文的 prefix，或嵌在更大 token 的路径，可被重写，且 trait 边界丢弃 report 故应用的重写不被标记审查。
**成因**：`rewrite_text` 替换每匹配候选并 push Confidence::Medium applied 记录 (`path_rewriter.rs:222-228`)；未匹配候选记 skipped (`:230-235`)；trait `rewrite()` 只返重写字符串、丢 report (`:247-249`)；copy_with_rewrite 把此应用于整文件内容 (`sync lib.rs:604-621`)。
**检测**：部分——未匹配候选记 skipped，但应用的 Medium 重写不浮现（report 在 trait 边界丢弃）。
**修复**：UNREPAIRED。
引用：`path_rewriter.rs:204-241`, `:247-249`, `sync lib.rs:604-621`

### DIRTY-27 · low · 跨工具转 Gemini 未实现（session 留不可转换）
**描述**：`ClaudeToGeminiConverter::convert` 返 "gemini converter not yet implemented"。任何把 Claude session 转 Gemini 格式的尝试失败，Gemini 目标工具无法接收转换的 session。
**成因**：convert 返 `Err(AisyncError::Session("gemini converter not yet implemented..."))` (`converter.rs:187-191`)。
**检测**：DETECTED——convert 返明确错误字符串。
**修复**：UNIMPLEMENTED——无转换路径；仅手动 workaround。
引用：`converter.rs:187-191`

---

## 8. TLS / 双存储

### DIRTY-22 · high · 接收方 cert 重生成但 peer 仍 pin 旧 cert（TLS pinning 失配）
**描述**：`load_or_create_receiver_identity` 在 receiver.der **或** receiver.key.der 任一不可读时（重装、清空 ~/.aisync）重生成新 TLS 身份，start_serve_daemon 每次启动把该 cert 写 receiver.der。无 live discovery 的连接 peer 退回存储的 `peers/<id>-receiver.der`（或 config peer.server_cert），仍持**旧** cert。pinned-cert verifier 做确切字节比较、拒绝握手。
**成因**：load_or_create_receiver_identity 仅两读都 Ok 才返持久化 cert，否则生成新 (`backend/identity.rs:92`)；start_serve_daemon 每次启动 `fs::write` cert (`backend/serve.rs:60`)。peer_transport_connection 优先 live discovery cert 但 discovery 无 receiver_cert_der 时退回缓存 peers .der/config cert (`backend/transport.rs:286`)。verifier 确切比 end_entity 字节 (`transport lib.rs:3102-3106`)。
**检测**：PARTIAL——仅浮现为 TLS 握手错误 "server certificate does not match pinned peer certificate"；push 日志 cert_source（'discovery' vs 'config'）。
**修复**：PARTIAL/automatic——仅当 peer 经 live mDNS discovery 取到新鲜 cert（receiver_cert_der 已填）才修复。Tailscale/manual peer 带 receiver_cert_der=None 总退回存储 cert；重配对重写 pinned cert 前 UNREPAIRED。
引用：`backend/identity.rs:92`, `backend/serve.rs:60`, `backend/transport.rs:286`, `transport lib.rs:3102-3106`

### DIRTY-23 · medium · discovery 已配对但 config 无 endpoint/cert（部分双写）
**描述**：配对持久化到两存储：discovery 的 paired_peers.json 和 config.toml 的 peers map。peer 在配对时不可达时，persist_peer_connection 以 endpoint=None/cert=None 记录 peer 而 paired_peers.json 仍列其为已配对。sync 随后失败 "peer has no endpoint" 或 "server certificate not found"，即使 UI 显示 peer 已配对。Tailscale/manual discovery 也以 receiver_cert_der=None upsert PeerConnectionInfo。
**成因**：confirm_pairing 即使连接信息 None 也持久化 peer 条目（注释 "None is fine — we still persist the peer entry"，`backend/peers.rs:300`）；endpoint/cert 仅存在时设。PeerConfig.endpoint/server_cert 是 Option (`config.rs:169`, `:171`)。Tailscale/manual upsert receiver_cert_der:None (`discovery lib.rs:428-432`, `:447-451`)。peer_transport_connection 在无 endpoint 或不可读 cert 时报错 (`backend/transport.rs:286`)。
**检测**：PARTIAL——push 在 sync 时惰性失败为 AisyncError；两存储间无主动 reconciliation。
**修复**：PARTIAL——后续可达 discovery + 重 persist 填 endpoint/cert；否则手动重配对。
引用：`backend/peers.rs:300`, `backend/transport.rs:286`, `config.rs:164-176`, `discovery lib.rs:428-432`, `:447-451`

### DIRTY-24 · medium · 部分 unpair 失败时 paired_peers.json vs config.peers 背离
**描述**：unpair 做两独立持久化删除：discoverer.unpair（persist_pairings 写 paired_peers.json，经 `?` 传播）和 config peer 移除（save_config 带吞掉的 `let _ =`）。discovery 侧 persist 失败在 config 清理前早返（config 仍列 peer）；config save 失败被静默忽略（paired_peers.json 更新但 config 陈旧）。两路任一使两存储背离。
**成因**：unpair 调 `g.discoverer.unpair(peer_id)?`，再改 config 并 `let _ = save_config(&path,&cfg)` (`backend/peers.rs:392`)。discoverer.unpair 从 paired_peers 移除后 `persist_pairings()?` (`discovery lib.rs:347-367`)；persist_pairings → save_pairings (`:478-488`)。
**检测**：UNDETECTED——无 config.peers 与 paired_peers 一致性交叉检查。
**修复**：UNREPAIRED/manual——吞掉的 `let _ = save_config` 丢失错误。
引用：`backend/peers.rs:392`, `discovery lib.rs:347-367`, `:478-488`

### DIRTY-33 · low · prune_stale_peers 丢 live discovery 态但留 config/cert（瞬时已配对但缺席）
**描述**：prune_stale_peers 在 offline_after 后从内存 discovery map 移除 peer，发 Lost。仅影响易失 discovery 缓存；config.peers、paired_peers.json 和 pinned cert 留存。peer 随后在 config 中"已配对"但缺席 live discovery，sync 退回存储的（可能陈旧）endpoint/cert 路径（喂给 DIRTY-22/DIRTY-23）。
**成因**：prune_stale_peers 过滤超 offline_after 的 peers 调 remove_peer (`discovery lib.rs:1518-1531`)；remove_peer 从 shared.peers drop 并发 `PeerChangeKind::Lost` (`:1492-1516`)。paired_peers 和 config 不动。
**检测**：检测为 Lost PeerChange 事件；非错误。
**修复**：peer 重发现时自愈；在此前 sync 用存储 fallback 连接信息。
引用：`discovery lib.rs:1518-1531`, `:1492-1516`

### DIRTY-10 · medium · 双目录原子 commit 第二 rename 失败时部分态（sync crate）
**描述**：commit_two_dirs 每目录（code 然后 session）rename target→backup 再 stage→target。若**第一**目录已 commit 但**第二** rename 失败，它恢复当前备份并对先前目录调 rollback_committed，但每步 rollback 是 best-effort（`let _ = fs::remove_dir_all`/`fs::rename`）吞掉错误。失败的恢复可留 target 已删和遗留 `.aisync-backup-*` 目录。
**成因**：commit_two_dirs：stage-rename 错时 `let _ = fs::rename(&backup,target)` 再 rollback_committed (`lib.rs:638-643`)；rollback_committed pop 并 `let _ = fs::remove_dir_all(&target)` 再 `let _ = fs::rename(backup,target)` (`:656-663`)。
**检测**：UNDETECTED——rollback 忽略自身失败。
**修复**：best-effort 自动 rollback；遗留 `.aisync-backup-*` 目录不保证清理。transport commit_staging 路径（增量，非整目录 rename）是生产路径；此 sync-crate 路径是旧 coordinator。
引用：`lib.rs:623-663`

---

> 配套：层权威与冲突胜负见 [07-truth-hierarchy.md](07-truth-hierarchy.md)，每个脏状态对应被违反的不变量见 [08-consistency-rules.md](08-consistency-rules.md)。
