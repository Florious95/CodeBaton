# 08 - 一致性规则（Consistency Rules / Invariants）

系统正确运行须**同时满足**的约束清单。每条不变量都是一个**潜在 bug 检测点**：违反它即对应一类缺陷。每条标注：是否被代码**强制**（`enforced` / `PARTIAL` / `NONE` / `VIOLATED`）、违反时的可观测症状、严重度。

所有声明经源码 `file:line` 核实（对抗性 verifier 逐条复核）。引用路径相对仓库根。共 **51 条**（INV-01~51）。

> 检测点用法：把每条 `statement` 当作可检查谓词。`enforced_by=NONE/VIOLATED` 的条目是已知漏洞，应优先写测试或加防护。

---

## 0. 速查表（按严重度）

| 等级 | 条目 |
|------|------|
| **high · 未强制/被违反** | INV-01（project.peers key 不验证）、INV-09（config.peers 用 device_name 做 key）、INV-12（双存储无原子性）、INV-15（self_last_synced_hash 从不读）、INV-22（编码冲突仅检测不防）、INV-23（非候选模式失败无回滚）、INV-24（config 写无锁无原子）、INV-27（ack-before-save）、INV-37→ 见 medium |
| **high · 已强制（防退化基线）** | INV-05、INV-19、INV-26、INV-28、INV-29、INV-30、INV-31、INV-32、INV-35、INV-47、INV-49、INV-50 |
| **high · PARTIAL** | INV-08、INV-10、INV-13、INV-14、INV-20 |
| **medium** | INV-02/03/04/16/21/25/33/34/36/37/39/40/41/42/43/44/46/51 |
| **low** | INV-06/07/11/17/38/45/48 |

---

## 1. 配置引用完整性（config / state）

### INV-01 · high · **NONE**
**约束**：`ProjectConfig.peers` 中每个 peer_name key 必须存在于 `SyncConfig.peers`。
**强制**：`validate_config` (`config.rs:377-440`) 只查项目/workspace 名唯一、local 路径存在、workspace peer 存在、scan_depth、空 exclude——**从不**遍历 `ProjectConfig.peers` key 验证其在 config.peers 的成员资格。`project_mapping` 在 sync 时惰性 `project.peers.get(peer_name)` (`config.rs:101-106`)，`peer_transport_connection` 报 "peer not found" (`backend/transport.rs:286`)。
**症状**：项目映射引用未配对/已删 peer；sync 运行时失败而非 config 加载时被抓。
引用：`config.rs:101-106`, `config.rs:377-440`, `backend/transport.rs:286`

### INV-02 · medium · **NONE**
**约束**：`ClaudeConfig.peers` 每个 key 必须存在于 `SyncConfig.peers`。
**强制**：`validate_config` 从不查 `claude_config.peers`。`project_mapping` 读 `claude_config.peers.get(peer_name)`，缺失时**静默退回** `sibling_claude_dir(remote_code_dir)` (`config.rs:107-112`)，孤儿 key 从不被检测。
**症状**：孤儿 `claude_config.peers` 条目残留；remote session-dir 解析静默用错误 sibling 默认值，session 落到错误目录。
引用：`config.rs:107-112`, `config.rs:183`, `config.rs:377-440`

### INV-03 · medium · **NONE**
**约束**：`ProjectConfig.sync_snapshots` 每个 key 必须存在于 `SyncConfig.peers`。
**强制**：sync_snapshots 按 peer_name 读写 (`config.rs:130-149`)，但 validate 从不交叉检查；`unpair` 从 config.peers/claude_config.peers/projects.peers/workspaces.peers 移除 peer，但**不**从 `projects.sync_snapshots` 移除 (`backend/peers.rs:392`)。
**症状**：unpair+同名重配对后，陈旧快照以错误基线驱动脑裂检测，造成假脑裂或抑制真冲突。
引用：`config.rs:130-149`, `config.rs:209-211`, `backend/peers.rs:392`

### INV-04 · medium · **NONE**
**约束**：`WorkspaceConfig.peers`（及 `.peer`）每个 key 必须存在于 `SyncConfig.peers`。
**强制**：validate 只要求 `effective_peer().is_some()` (`config.rs:415-420`)，从不确认所选 peer 名是真实 config.peers 条目。`peer_transport_connection` 后续报 "peer not found"。
**症状**：workspace 指向不存在 peer；sync 中途失败而非验证时。
引用：`config.rs:263-269`, `config.rs:415-420`, `backend/transport.rs:286`

### INV-05 · high · **enforced**
**约束**：`SyncConfig.projects` 中项目名唯一。
**强制**：`validate_config` 用 HashSet 插入每个 `project.name`，冲突报错 (`config.rs:378-385`)。
**症状（若失效）**：按名查找（`.find(|p| p.name==..)`）只匹配第一个，第二个不可达。
引用：`config.rs:378-385`, `config.rs:97-100`

### INV-06 · medium · **enforced**
**约束**：`SyncConfig.workspaces` 中 workspace 名唯一。
**强制**：`validate_config` HashSet 去重 (`config.rs:399-406`)。
**症状（若失效）**：`replace_workspace`/查找歧义，改动持久化到错误 workspace。
引用：`config.rs:399-406`

### INV-07 · low · **NONE**
**约束**：`SyncState.projects` key 必须对应现存 `ProjectConfig.name`（state 不引用已删项目）。
**强制**：`SyncState.projects` 按 project_id keyed (`lib.rs:411-414`)，`record_success` 按 id 插入 (`lib.rs:447-450`)，与 config.projects 无 reconciliation，删项目时不剪 state。
**症状**：已删项目的版本/指纹孤儿累积；复用项目名继承陈旧指纹，误触发或抑制冲突检测 (`lib.rs:355-360`)。
引用：`lib.rs:411-414`, `lib.rs:447-462`, `lib.rs:355-360`

### INV-38 · low · **enforced（双重）**
**约束**：`WorkspaceConfig.scan_depth` 必须 = 1（MVP）。
**强制**：`validate_config` 拒绝 != 1 (`config.rs:421-426`)，`scan_workspace` 再查 != 1 报错 (`lib.rs:136-140`)。
引用：`config.rs:421-426`, `lib.rs:136-140`

### INV-39 · medium · **enforced**
**约束**：enabled 的项目/workspace 的 local 路径在 config 加载/保存时必须存在；disabled 豁免。
**强制**：`validate_config` 在 `project.enabled && !local.exists()` (`config.rs:386-392`) 及 workspace 同理 (`:408-414`) 报错。
**症状**：对缺失 local dir 同步会扫出空 manifest，经 <50% commit 路径可能大删 remote；存在性检查阻止 config 加载。注意 disabled 项目缺 peer/cert 仍过验证。
引用：`config.rs:386-392`, `config.rs:408-414`

### INV-40 · medium · **NONE**
**约束**：`WorkspaceConfig` 须有单一权威 remote-root 源；`remote_root` 与 `peers` map 对活跃 peer 不得矛盾。
**强制**：`effective_remote_root` 仅当 peer 空或匹配时优先 remote_root，否则退回 `peers.get(peer)`；二者都存在时不查是否一致 (`config.rs:271-279`)。
**症状**：remote_root 给 peer A 但 peers map 给 A 不同路径时，解析条件相关、可能选陈旧值，sync 到错误 remote dir。
引用：`config.rs:271-279`

---

## 2. 身份稳定性（identity）

### INV-08 · high · **PARTIAL**
**约束**：DeviceId 须跨进程重启稳定。
**强制**：DeviceId 持久化在 config.toml (`config.rs:159-162`)、`load_config` 重载，一旦写入即跨重启。但 `DeviceId::new()` 每次铸新随机 v4 (`core/lib.rs:57-61`)，`server_device_info()` **每次调用**铸全新 DeviceId (`transport lib.rs:3032-3039`)——传输层服务端身份**不稳定**，重生成的 config 得新身份。
**症状**：config 丢失/重置 → 设备得新 id，所有 peer 的 paired_peers.json/config.peers（及以旧 id 命名的 cert 文件）失配，破坏配对与 cert pinning。
引用：`config.rs:46`, `config.rs:159-162`, `core/lib.rs:57-61`, `transport lib.rs:3032-3039`

### INV-09 · high · **NONE**
**约束**：config.peers 的 HashMap key 须为每设备稳定唯一标识；用 device_name（可变、不唯一）违反此约。
**强制**：`persist_peer_connection` 用 `peer.name` 做 key (`backend/peers.rs:466`)，**非**稳定 DeviceId（config.peers 是 `HashMap<String, PeerConfig>`，`config.rs:19`）。无任何名唯一性/稳定性强制。
**症状**：两 peer 同显示名互相覆盖 config.peers 条目（last-writer-wins），静默丢一方 endpoint/cert；改名 peer 产生重复孤儿条目而 project.peers 仍指旧名。
引用：`backend/peers.rs:466`, `config.rs:19`

### INV-10 · high · **PARTIAL**
**约束**：config.peers 按 device_name keyed，但 `PeerConfig.id` 须等于该设备 DeviceId；name→id 与 id→name 查找须一致。
**强制**：`persist_peer_connection` 每次覆盖 `entry.id = peer.id` (`backend/peers.rs:466`) 保持 id 新鲜，但无双射检查。`connection_from_config` 按 id 在 values() 查 (`backend/peers.rs:441`) 而 `peer_transport_connection` 按 name key 查 (`backend/transport.rs:286`)；name/id 偏斜使两查找不一致。
**症状**：两条目共享 id（名复用）或名映射到错 id 时，id-based 与 name-based 查找选不同 PeerConfig，同一逻辑 peer 得失配的 endpoint/cert。
引用：`backend/peers.rs:466`, `backend/peers.rs:441`, `backend/transport.rs:286`

### INV-41 · medium · **NONE**
**约束**：修复 placeholder device_name 不得改变 peer 用来引用本设备的 key。
**强制**：启动时覆盖 `device.name` 修复 (`backend/mod.rs:350`)；该名也是对端 config.peers 的 key（`persist_peer_connection` 按名 key），无跨设备 rename reconciliation。
**症状**：本设备改名后，对端 config.peers 仍 key 旧名；下次连接对端在新名下插**新**条目，重复/分裂 peer 映射。
引用：`backend/mod.rs:350`, `backend/peers.rs:466`

### INV-42 · medium · **PARTIAL**
**约束**：peer cert .der 按稳定 DeviceId 写盘，而指向它的 config.peers 条目按可变 device_name keyed。
**强制**：cert 文件写在 `<peer_id>-receiver.der` (`backend/peers.rs:466`)，但 PeerConfig 存在 `peer.name` key 下 (`backend/peers.rs:466`)。文件耐久但索引按名。
**症状**：改名留下按名 key 的条目其 server_cert 指向正确 id 命名文件，但第二个名条目无 cert；信任解析歧义。
引用：`backend/peers.rs:466`

---

## 3. 双存储一致性 / 持久化原子性（config / discovery / transport）

### INV-11 · low · **PARTIAL**
**约束**：paired_peers.json 须按 DeviceId 可加载，无两记录共享同一 DeviceId。
**强制**：`load_pairings` 经 `.map(|peer| (peer.device.id, peer)).collect()` 收进 `HashMap<DeviceId, PairedPeer>` (`discovery lib.rs:1549-1553`)，盘上 Vec 有重复 id 时静默 last-wins 去重；无显式唯一性校验。
**症状**：手改/合并的 paired_peers.json 含重复 id 时加载静默丢全部但一条。
引用：`discovery lib.rs:1549-1553`, `:479-487`, `:170`

### INV-12 · high · **NONE**
**约束**：双存储一致——paired_peers.json（按 DeviceId）配对的设备必须也在 config.peers（按 device_name）中，反之亦然。
**强制**：配对经**两条独立代码路径**写两存储：discoverer 配对持久化 paired_peers.json (`persist_pairings`，`discovery lib.rs:478-488`)，`persist_peer_connection`+`save_config` 写 config.peers (`backend/peers.rs:466`)。两写**非原子**，跨 DeviceId vs device_name 两 key 空间无 reconciliation。unpair 从 config.peers 移除但 cert .der 留下。
**症状**：discoverer 配对与 save_config 间崩溃使一存储已配对另一未；UI 显示已配对但 sync 失败（peer 不在 config.peers），或 config.peers 有 discovery 已不认为配对的 peer。
引用：`discovery lib.rs:478-488`, `backend/peers.rs:466`, `backend/peers.rs:392`

### INV-13 · high · **PARTIAL/INCONSISTENT**
**约束**：`PeerConfig.server_cert` 指向的 cert 文件必须存在于盘。
**强制**：`peer_transport_connection` 在 `fs::read(server_cert_path)` 失败时**硬报错** "server certificate not found" (`backend/transport.rs:286`)，但 `connection_from_config` 把缺失 cert **静默映射为 None**（`fs::read(path).ok()`，`backend/peers.rs:441`）。validate 从不查 cert 存在。`persist_peer_connection` 存路径前先写 cert (`backend/peers.rs:466`)，但后续手删不被检测直到使用。
**症状**：删/缺 peer cert 使一路径大声中止 sync，另一路径静默产生无 pin（None）连接随后 TLS 失败 "not pinned"——同一根因不同失败模式。
引用：`backend/transport.rs:286`, `backend/peers.rs:441`, `backend/peers.rs:466`, `config.rs:377-440`

### INV-23 · high · **INCONSISTENT**
**约束**：save_config 成功后内存 Inner.config 必须等于写盘内容；save_config 失败时 Inner.config 必须未被改（候选/回滚模式）。
**强制**：**正确**候选模式先 `save_config(&candidate)?` 再仅成功时 `g.config = candidate`（例如 `confirm_project_mapping_request` `backend/projects.rs:39`、`confirm_workspace_mapping_request` `backend/workspaces.rs:210`、`add_project` `backend/projects.rs:223`、`add_workspace` `backend/workspaces.rs:79`、`complete_onboarding` `backend/mod.rs:561`）。**被违反**的 mutate-then-save 路径：`persist_peer_connection` 原地改 `g.config` 再 save (`backend/peers.rs:466`)；`persist_workspace_update` 经 `replace_workspace` 改后保存 (`backend/split_brain.rs:62`)；`unpair` 改 `g.config` 后用 `let _ =` 忽略 save 错 (`backend/peers.rs:392`)。
**症状**：save_config 失败（盘满/权限/校验）时内存与盘背离：peer 在运行 app 里显示已配对/workspace 已更新但重启丢失，无回滚。
引用：`backend/projects.rs:39`, `backend/peers.rs:466`, `backend/split_brain.rs:62`, `backend/peers.rs:392`

### INV-24 · high · **NONE**
**约束**：config 持久化须原子且串行——并发 save_config 不得交错产生部分/损坏的 config.toml，读者不得见半写文件。
**强制**：`save_config` 裸 `fs::write(path, text)`，**无临时文件+rename、无 advisory lock、无进程内 path mutex** (`config.rs:358-367`)。多调用者（Backend g.config 保存、`backend/sync_push.rs:20` run_tcp_push 快照持久化、`backend/mod.rs:977` refresh_and_save_workspaces 独立 workspace 刷新）写同一 config.toml。对比：discovery 的 `save_pairings` 用 tmp+rename (`discovery lib.rs:1560-1574`)。
**症状**：两线程竞 config.toml 互相截断/覆盖；并发 `load_config` 读部分文件 TOML 解析失败，或一写者改动静默丢失（last-writer-wins）。
引用：`config.rs:358-367`, `backend/mod.rs:977`, `discovery lib.rs:1560-1574`

### INV-46 · medium · **enforced**
**约束**：paired_peers.json 写须原子（无读者见半写配对存储）。
**强制**：`save_pairings` 写 `path.tmp` 后 `fs::rename`（Windows 预删）原子替换 (`discovery lib.rs:1560-1574`)。
**症状（若失效）**：无 tmp+rename 的崩溃会损坏 paired_peers.json、下次加载丢全部配对；此路径已正确保护（不同于 config.toml — INV-24）。
引用：`discovery lib.rs:1560-1574`

---

## 4. 同步正确性（sync / state / transport）

### INV-14 · high · **PARTIAL**
**约束**：脑裂比较须 like-for-like——存储的 `snapshot.peer_last_known_hash` 须与探测的 `manifest_hash` 用相同文件集、相同 hash 函数计算。
**强制**：两侧都用 `manifest_hash`（排序 relative_path + blake3，`transport lib.rs:3288-3299`）。探测用默认 excludes 扫服务端 remote_code_dir (`:1670-1672`)；存储的 `peer_last_known_hash` 是 `manifest_hash(&code_manifest)`，code_manifest 是**客户端** source_manifest（`:1312-1316`, 持久化在 `backend/sync_push.rs:20`）。干净全镜像后匹配，但服务端若残留额外文件则两 hash 背离。无代码断言相等；`check_split_brain` 只比 `resp.manifest_hash != snap.peer_last_known_hash` (`backend/split_brain.rs:248`)。
**症状**：push 在服务端留残留文件时，其 manifest_hash 偏离存储的 peer_last_known_hash，下次 `check_split_brain` 即使无独立 peer 编辑也假报 split_brain=true。
引用：`backend/sync_push.rs:20`, `backend/split_brain.rs:248`, `transport lib.rs:1670-1672`, `:1312-1316`, `:3288-3299`

### INV-15 · high · **NONE**
**约束**：须读 `self_last_synced_hash` 以检测自上次同步以来的**本地**漂移；忽略它的脑裂检测不完整。
**强制**：`self_last_synced_hash` 是已定义快照字段 (`config.rs:193-194`)、由 `run_tcp_push` 写 (`backend/sync_push.rs:20`)，但 `check_split_brain` **只读** `peer_last_known_hash` (`backend/split_brain.rs:248`)，从不读 `self_last_synced_hash`。
**症状**：若本地与 remote 自上次同步都变了但 remote==last-known 而 local 移动了，`check_split_brain` 报无脑裂并静默覆盖 remote。
引用：`config.rs:193-194`, `backend/sync_push.rs:20`, `backend/split_brain.rs:248`

### INV-16 · medium · **enforced**
**约束**：`manifest_hash` 须顺序无关且内容敏感（同文件→同 hash 不论扫描顺序；任何内容变→不同 hash）。
**强制**：`manifest_hash` 先按 relative_path 排序再 hash，带分隔符折入 path + blake3_hash (`transport lib.rs:3289-3298`)。
**症状（若失效）**：两相同目录不同扫描顺序得不同 hash，每次检查假脑裂。
引用：`transport lib.rs:3289-3298`

### INV-17 · low · **enforced**
**约束**：指纹版本单调——`local_version`/`remote_version` 须非减，且仅当对应指纹真变时才增。
**强制**：`record_success` 仅当 `!has_synced` 或指纹不同才增 (`lib.rs:451-456`)；u64 从不减 (`:466-473`)。
引用：`lib.rs:451-456`, `:466-473`

### INV-25 · medium · **PARTIAL**
**约束**：`run_tcp_push` 中快照持久化须基于最新盘 config 而非陈旧内存 clone，以免覆盖并发 config 编辑。
**强制**：`run_tcp_push` 写快照前先 `load_config` 重读盘 (`backend/sync_push.rs:20`)，收窄丢更新窗口。但 save_config 本身非原子无锁（INV-24），此 load 与 write 间落地的 config 编辑仍丢失；且 load_config 失败时 `if let Ok(mut persisted)` 分支静默跳过快照持久化 (`backend/sync_push.rs:20`)。
**症状**：push 期间的 config 改动被快照保存的 load-modify-write 覆盖，或快照静默不持久化，下次脑裂检查无基线。
引用：`backend/sync_push.rs:20`, `config.rs:358-367`

### INV-26 · high · **enforced**
**约束**：`run_tcp_push` 把快照写盘后，须把 Backend 内存 config 重新同步以使 `check_split_brain` 看到。
**强制**：`run_sync` 在 `result.is_ok()` 时从盘重载刚写快照进 g.config (`backend/split_brain.rs:73`)，使 `check_split_brain`（读内存 g.config，`backend/split_brain.rs:248`）见新 `peer_last_known_hash`。
**症状（若缺）**：`check_split_brain` 看不到刚写快照，下次 push 假报脑裂。
引用：`backend/split_brain.rs:73`, `backend/split_brain.rs:248`

### INV-32 · high · **enforced**
**约束**：`detect_conflict` 须仅把"相对已记录 post-sync 基线（`state.has_synced`）的背离"当冲突——从未同步的项目不得假报冲突，且 `(local_changed && remote_changed)` 须被标记。
**强制**：`detect_conflict` 在 state 缺失或 `!has_synced` 时早返 Ok，仅当 local 与 remote 指纹都异于存储基线才报 `ConflictDetected` (`lib.rs:349-373`)；`run_auto_sync_once` 对 (true,true) 返回 ConflictDetected (`:114-127`)。
**症状（若失效）**：无 has_synced guard 时，两端预存文件的首次同步被标冲突；无 (true,true) 分支时同时编辑静默覆盖一侧。
引用：`lib.rs:349-373`, `:114-127`

### INV-33 · medium · **PARTIAL**
**约束**：`detect_conflict` 的指纹比较须用与 `record_success` 存储时相同的 exclude 模式，否则 local/remote_changed 假为真。
**强制**：`sync_one_way_impl` 两处 `project_snapshot` 都用 `exclude_rules_for_project(project_id)` (`lib.rs:241-247`, `:362-363`)；但 `run_sync` 经 `inject_excludes` 注入 per-run 敏感 excludes 并在后恢复 (`backend/split_brain.rs:73`)，sync 中途改了模式集。
**症状**：带注入 excludes 的 sync 存指纹后，不带这些 excludes 的后续 sync 算出不同指纹 → 假冲突/假变更。
引用：`lib.rs:241-247`, `:362-363`, `backend/split_brain.rs:73`

### INV-34 · medium · **PARTIAL**
**约束**：`record_success` 须把 SyncState 写盘与内存版本递增一并完成。
**强制**：`sync_one_way_impl` 调 `record_success` 后 `state.save` (`lib.rs:322-325`)；但 `record_success` 在 save 前改内存 map (`:447-461`)，`SyncState::save` 用裸 `fs::write` 无锁无 temp-rename (`:427-435`)。
**症状**：state.save 在 record_success 后失败时，内存显示已同步而盘未；重启重载陈旧 state，可能重同步或误判冲突。
引用：`lib.rs:322-325`, `:447-461`, `:427-435`

### INV-35 · high · **enforced**
**约束**：incoming-sync 抑制窗须 ≥ watcher 防抖，以免接收端写回触发反向 push（无回声循环）。
**强制**：`incoming_suppress_window = max(auto_sync_cooldown, MIN_WINDOW=5s)`，注释说明 5s > DEFAULT_DEBOUNCE(2s) (`backend/auto_sync_gate.rs:72`)；DEFAULT_DEBOUNCE 是 2s (`watcher.rs:10`)。
**症状（若失效）**：窗 < 防抖时，incoming 写的 watcher 事件在抑制过期后到达 → 误读为本地变更 → 反向 push → 无限同步循环。
引用：`backend/auto_sync_gate.rs:72`, `watcher.rs:10`

### INV-36 · medium · **enforced**
**约束**：每 (scope,name,peer) 至多一个 auto-sync 在飞；并发触发须合并。
**强制**：`try_begin_auto_sync` 按 key 门控 `AutoSyncGate{in_flight}`，in_flight 或 cooldown 时返 None，否则插 in_flight=true (`backend/auto_sync_gate.rs:97`)；`finish_auto_sync` 清 in_flight 设 cooldown (`backend/auto_sync_gate.rs:168`)。
**症状（若失效）**：两并发 push 到同一 peer 竞同一 staging/target → commit 损坏或双备份。
引用：`backend/auto_sync_gate.rs:97`, `backend/auto_sync_gate.rs:168`

### INV-37 · medium · **NONE**
**约束**：项目 auto-sync watcher 假设每项目恰好一个 peer（取 `project.peers.keys().next()`）。
**强制**：`start_project_watcher` 任意取首个 peer key (`backend/mod.rs:1220`)；无验证 project.peers 恰一条目。
**症状**：映射到多 peer 的项目只 auto-sync 到一个（HashMap 迭代顺序不确定），其余从不收 watcher 驱动更新。
引用：`backend/mod.rs:1220`

### INV-47 · high · **enforced**
**约束**：原子双目录 commit 须全或无——若第一目录（code）已换、提交第二目录（session）失败，code 目录须回滚到备份。
**强制**：`commit_two_dirs` 每目录 rename target→backup 再 stage→target；rename 失败时从备份恢复刚失败的 target 并调 `rollback_committed` 恢复先前已提交目录 (`lib.rs:623-663`)。
**症状（若失效）**：部分 commit 留 code 更新但 session 陈旧（或反），项目快照不一致随后在 record_success 误指纹。
引用：`lib.rs:623-663`

### INV-48 · low · **enforced**
**约束**：staging 路径分配须在有限尝试内找到不存在的唯一 sibling，否则大声失败。
**强制**：`unique_staging_path` 尝试至多 1000 个时间戳候选后返 Err (`lib.rs:677-701`)。
引用：`lib.rs:677-701`

### INV-51 · medium · **PARTIAL**
**约束**：`resolve_split_brain` 无 PreferRemote 路径——仅 PreferLocal（confirm_overwrite push）实现；PreferRemote 返回错误。
**强制**：PreferLocal 调 `run_sync(LocalToRemote, confirm_overwrite=true)` 触发备份 (`backend/split_brain.rs:304`)；PreferRemote 返 Err "not yet implemented" (`backend/split_brain.rs:304`)。
**症状**：想保留 remote 的用户无支持路径，可能强用 PreferLocal 销毁 remote-only 工作（虽备份 .bak-* 但易忽略）。
引用：`backend/split_brain.rs:304`

---

## 5. 会话与路径重写（session / history）

### INV-19 · high · **enforced**
**约束**：未被路径重写的记录 session round-trip 须字节相同（未触碰记录重发原字节；trailing newline 保留）。
**强制**：`RecordLine.emit` 在 `!dirty` 时返原 raw 字节，仅 dirty 时重序列化 (`claude_code.rs:73-84`)；`parse_session_file` 记 `trailing_newline = raw.ends_with('\n')` (`:326`)；`serialize_session` 重应用 trailing_newline (`:379-381`)；`rewrite_structured_paths` 仅对改动记录设 dirty=true (`:306-308`)。
**症状（若失效）**：若 emit 重序列化未变记录，JSON key 顺序/格式/数字精度漂移，manifest hash 每次同步变（无限重传/假变更检测），Claude session 文件损坏。
引用：`claude_code.rs:73-84`, `:326`, `:379-381`, `:306-308`

### INV-20 · high · **PARTIAL**
**约束**：结构化路径重写须可逆——rewrite(SourceToTarget) 后 rewrite(TargetToSource) 还原原路径。
**强制**：`directional_rules` 通过交换 prefix 派生反向规则集 (`path_rewriter.rs:139-148`；`PathRule::reversed` `:55-62`)，`validate_rules` 禁空/循环/重复 source prefix (`:271-294`)；round-trip 由 `reversible_round_trip_unix/windows` 测试覆盖 (`:467-493`)。**但** `validate_rules` **不**禁重叠/嵌套 TARGET prefix，故两条 target 嵌套的 forward 规则可使反向歧义、不可逆。
**症状**：两条规则映射不同 source 到嵌套/重叠 target 时，TargetToSource 把路径重写回错误 source（A→B→A' ≠ A）；session cwd 路径永久损坏。
引用：`path_rewriter.rs:139-148`, `:271-294`, `:467-493`

### INV-21 · medium · **PARTIAL**
**约束**：自由文本（启发式）路径重写**不**保证可逆，且标 Medium/Low confidence，区别于 High-confidence 结构化重写。
**强制**：`rewrite_text` 把应用的替换标 `Confidence::Medium`、未匹配候选 skip (`path_rewriter.rs:222-235`)，结构化标 `Confidence::High` (`:181-186`)。但 `PathRewriter::rewrite` trait 入口把**所有**内容经 `rewrite_text` 路由 (`:247-248`)，任何 trait 消费方都得启发式（不可逆）行为，无"仅触碰结构化字段"的强制。
**症状**：含路径状子串（实非文件系统路径）的文件被启发式重写且不可逆，同步时静默篡改用户内容。
引用：`path_rewriter.rs:222-235`, `:181-186`, `:247-248`

### INV-22 · high · **DETECTED-ONLY（不防）**
**约束**：编码目录名冲突——两个不同原始项目路径不得编码为同一 Claude projects 目录名（G4）。
**强制**：`SessionIndex.conflicts()` 报告映射到 >1 原始路径的 `encoded_dir_name` (`claude_code.rs:171-180`)，但 `claude_project_dir_name` 是**有损**映射（每个非 `[alnum-_.]` 字符→`-`，`backend/session_stage.rs:623`），故 `/a/b` 与 `/a-b` 冲突且无阻塞；`write_session` 无条件写入 `target_dir.join(encoded_dir_name)` (`claude_code.rs:275-282`)。
**症状**：两不同 local 项目路径编码相同时，在同一 remote 子目录互相覆盖 session 文件。
引用：`claude_code.rs:171-180`, `backend/session_stage.rs:623`, `claude_code.rs:275-282`

### INV-27 · high · **VIOLATED**
**约束**：ack-before-save 排序——peer 被告知项目映射已接受**先于**本地已持久化。
**强制**：`confirm_project_mapping_request` 先把 `ProjectMappingAck` 发上线 (`backend/projects.rs:39`)，**再** `save_config(&candidate)?` 持久化 (`backend/projects.rs:39`)。save_config 若失败，remote 已信映射建立（并会记录/推送）而本地盘上无任何记录。（一个 duplicate-project guard 在 ack 前，`backend/projects.rs:39`。）
**症状**：remote 认为项目已映射并开始推送；本地失败/重启无映射记录，incoming sync 指向未配置项目——非对称配对态。
引用：`backend/projects.rs:39`

---

## 6. 传输与安全（transport）

### INV-18 · medium · **PARTIAL**
**约束**：客户端 session-staging 目录须总是被清理，不论成功失败。
**强制**：客户端 session staging 经 `fs::remove_dir_all(plan.staging_root)` 移除 (`backend/sync_push.rs:20`)，**仅在** `runtime.block_on` 返 Ok 的成功路径后；若 block_on 出错（`backend/sync_push.rs:20` 的 `?`），清理循环不达，泄漏 staging_root。服务端 `commit_staging_with_options` 成功时移除 (`transport lib.rs:3397-3399`)，receive 循环 commit/receive 失败时移除 (`:2056`, `:2095`)。
**症状**：中止的客户端 push 在 config/项目旁留孤儿 session-stage 目录，累积磁盘占用。
引用：`backend/sync_push.rs:20`, `transport lib.rs:3397-3399`, `:2056`

### INV-28 · high · **enforced**
**约束**：服务端 commit（含 50% 安全阀检查）须成功完成**先于**向客户端发 SyncComplete。
**强制**：`commit_staging_with_options` 须返 Ok 才 `write_message(SyncComplete)`；commit 错时服务端发 `Message::Error` 后 shutdown 返 Err (`transport lib.rs:2055-2081`)。
**症状（若失效）**：若 SyncComplete 先于 commit 发，ack 后的安全阀中止或盘错会使客户端为从未落地的文件持久化 peer_last_known_hash，造成未来假脑裂或背离。
引用：`transport lib.rs:2055-2081`

### INV-29 · high · **enforced**
**约束**：TLS 客户端连接须 pin peer 确切证书；未 pin 的客户端 config 须被拒而非退回系统信任。
**强制**：`client_config` 在 `pinned_peer_cert_der` 为 None 时报错 "TLS peer certificate is not pinned" (`transport lib.rs:3055-3059`) 并限 TLS1.3 (`:3067`)；`PinnedPeerCertVerifier.verify_server_cert` 仅在确切 DER 字节相等时接受 (`:3102-3108`)。
**症状（若失效）**：允许未 pin config 或非确切匹配时，持任何看似有效 cert 的 MITM 可冒充接收方、截获/伪造同步文件。
引用：`transport lib.rs:3055-3059`, `:3067`, `:3102-3108`

### INV-30 · high · **enforced**
**约束**：两 peer 须同意 PROTOCOL_VERSION；版本失配须中止握手。
**强制**：`expect_hello` 仅当 peer 的 `Hello.protocol_version == PROTOCOL_VERSION` 才返 Ok，否则报 "protocol version mismatch" (`transport lib.rs:3014-3023`)；每消息盖 PROTOCOL_VERSION (`:81`, `:542`)。
**症状（若失效）**：旧客户端与新服务端交换不兼容消息/manifest 帧，损坏传输。
引用：`transport lib.rs:3014-3023`, `:81`

### INV-31 · high · **enforced**
**约束**：接收文件须先经 blake3 校验和核对发送方 manifest 才 commit 到 target。
**强制**：`receive_changes` 结果链 `.and_then(verify_manifest_checksums(&staging, &source_manifest))`，仅 Ok 才 commit；否则移除 staging 返 Error (`transport lib.rs:2028-2030`, `:2094-2104`)。`verify_manifest_checksums` 在 `:2482`。
**症状（若失效）**：损坏/截断传输被 commit，静默写损坏文件到 peer。
引用：`transport lib.rs:2028-2030`, `:2094-2104`, `:2482`

### INV-49 · high · **enforced**
**约束**：50% 删除安全阀须中止会把 >半数 target 文件移入 trash 的未确认 commit；confirm-overwrite commit 须在任何破坏性写**前**先建全量备份。
**强制**：`commit_staging_with_options` 算 `delete_ratio = to_delete/target_files`，`!confirm_overwrite` 且 >0.5 时报错 (`transport lib.rs:3357-3368`)；confirm_overwrite 时先调 `backup_target_dir` (`:3355-3356`)，其 create/write 失败经 `?` 在任何 target 写前传播 (`:3424-3427`)。
**症状（若失效）**：禁用阀或备份后于写会让不完整 staging 集抹掉多数 peer 文件且无可恢复副本——P0 数据丢失路径。
引用：`transport lib.rs:3357-3368`, `:3355-3356`, `:3424-3427`

### INV-50 · high · **enforced**
**约束**：所有接收的相对路径须沙箱在 staging/target 根下（无绝对、`..`、根、prefix 组件）。
**强制**：`checked_relative_path` 拒绝绝对路径及 `ParentDir`/`RootDir`/`Prefix` 组件 (`transport lib.rs:3611-3633`)；`session_data_path` 另拒含 `/`,`\`,`.`,`..` 的 id (`:3635-3648`)；`verify_manifest_checksums` 把每条目经 `checked_relative_path` (`:2484`)。
**症状（若失效）**：路径逃逸时，恶意 peer 可写到项目目录外（路径遍历）。
引用：`transport lib.rs:3611-3633`, `:3635-3648`, `:2484`

---

## 7. 发现与配对（discovery）

### INV-43 · medium · **enforced**
**约束**：配对码派生须对两 device id 顺序无关，并依赖协议版本 + 可选 nonce。
**强制**：`derive_pairing_code_from_parts` 先排序两 device-id 字符串再 hash，输入含 PROTOCOL_VERSION 和可选 nonce (`discovery lib.rs:674-687`)。
**症状（若失效）**：顺序依赖使两设备显示不同 6 位码，手动确认总失败。
引用：`discovery lib.rs:674-687`

### INV-44 · medium · **enforced**
**约束**：`confirm_pairing` 须要求 peer 当前在线/已发现才记录配对。
**强制**：`confirm_pairing` 调 `self.peer(peer_id)?`，缺失时报 "peer is not online"，先于插入 paired_peers 和持久化 (`discovery lib.rs:294-296`, `:307-312`)。
**症状（若失效）**：若能为未见 DeviceId 记录配对，伪造 id 可被信任；此强制把配对绑到 live discovery record。
引用：`discovery lib.rs:294-296`, `:307-312`

### INV-45 · low · **enforced（by construction）**
**约束**：`prune_stale_peers` 须只移除 last_seen 超 offline_after 的 live discovery-map peer，不得丢持久化 paired-peer 记录。
**强制**：`prune_stale_peers` 按 `now.duration_since(last_seen) > offline_after` 过滤 `shared.peers` 经 `remove_peer` 移除 (`discovery lib.rs:1518-1531`)；`paired_peers` 是独立 Mutex map (`:170`)，prune/remove_peer 不碰 (`:1492-1516`)。
**症状（若失效）**：若 remove_peer 也清 paired_peers，暂时离线的已配对设备会丢配对；当前分离防止此，但靠构造非断言不变量。
引用：`discovery lib.rs:1518-1531`, `:1492-1516`, `:170`

---

> 配套：层权威与冲突胜负见 [07-truth-hierarchy.md](07-truth-hierarchy.md)，这些不变量被违反后形成的脏状态见 [09-dirty-state.md](09-dirty-state.md)。
