# 全量交叉审查（review-final）

对 architecture/ 下 12 篇维度文档（01–12）的全量交叉审查：检查维度间**一致性**、**矛盾**、**遗漏**。

> 方法：Workflow 编排 26 个 agent —— 5 个跨维度 reviewer（含完整性批判：存储/字段/真相源、身份/TLS/改名、Bug-INV-DIRTY 交叉映射、操作/并发/真相源、完整性批判）→ 对每条"矛盾"逐条对抗性核对（打开两篇文档读原文）排除误报 → 12 维度逐篇摘要合成。初筛 46 一致项 / 20 矛盾 / 36 遗漏；矛盾经对抗性核对后 **17/20 存活**（3 条误报已剔除）。
>
> ID 体系说明：每篇文档用各自的编号体系——01/02 用 `BUG-N`、03 用 `§8.N 热点`、08 用 `INV-NN`、09 用 `DIRTY-NN`、11 用 `RN`。同一缺陷常跨多篇出现于不同编号下（见 §1.3 交叉映射）。

---

## 0. 总体结论

**12 篇文档整体高度一致、互相印证**。46 个跨文档一致项显示核心模型（真相源层次、不变量、脏状态、并发）在多篇之间收敛于相同机制和相同 `file:line`。

矛盾集中在 **02-entities.md 携带若干 stale 描述**（state.toml 内容、history 文件格式、receiver cert 文件名、端口默认值、Ed25519 caveat 缺失），以及 review-phase1.md 早先标记但**至今未修**的 3 个问题（01 层数 6→7、02 state.toml 描述、02 history 路径格式）。这些是局部文字订正，不动摇架构结论。

最值得注意的两类系统性问题：
1. **一个真正的功能性 bug 被交叉审查暴露**：`auto_sync_paused` 暂停开关实际不 gate scanner/watcher/gate 三者（矛盾 C17，high）——06 §B1 误称"scanner checks"，被 08/09/10 三篇否证。
2. **交叉引用图稀疏**（遗漏 G-link，high）：01/02/03/04/11 五篇对外零交叉引用，只有 05/07/08/09/12 互链。读 03 身份模型的人不会被指向 08 的身份不变量或 09 的身份漂移脏状态。

---

## 1. 一致性（46 项，文档互相印证的证据）

### 1.1 真相源 / config 缓存模型——4–6 篇收敛
- **config 是真相、Inner.config 是缓存、无通用 disk→memory reconciler**：01 §2 + §3、07 §1 + 规则2/4 一致（ConfigStore 是死代码）。
- **SyncSnapshot 磁盘权威例外**（push 后回填内存）：01-BUG-1、06 §A8 step h、07 规则2/§3.2、08-INV-26 四篇一致，包括"仅 snapshot 字段回填、仅 is_ok() 时"caveat，行号一致（backend/sync_push.rs:20 写、回填均在 run_tcp_push 内）。
- **config.toml save 非原子、无锁、last-writer-wins**：01-BUG-2、07 规则4、08-INV-24、10 atomicity 表四篇收敛于 config.rs:358-367，并一致对比 save_pairings 的 tmp+rename。
- **后台线程把磁盘当唯一真相、每轮重读、无视 Inner.config**：01 §3+BUG-5、07 规则3/§3.5 机制与行号一致（backend/session_scanner.rs:120）。
- **生产 push 不写 state.toml；脑裂真相在 config.toml SyncSnapshot**：07 规则10/§3.8、08 §3、10 point 5 一致（run_tcp_push 硬编码 version=0，backend/sync_push.rs:20）。

### 1.2 身份 / TLS / 改名——3–4 篇收敛
- **Keychain service 值已迁移为新名 `CodeBaton`**（唯一不一致标识符）：03 §7、11 §2#9+R1、05、02 §9.1 四篇一致，account key `device:{uuid}:ed25519` 格式一致。
- **所有跨版本/跨设备兼容关键标识符冻结旧 aisync 名**：TLS CN `aisync-receiver`/`aisync-client`、mDNS `_aisync._tcp.local.`、数据目录 `~/.aisync/`、配对码前缀 `aisync-pairing-v{V}`——03/11/05 一致。
- **TLS 信任 = 确切 DER pinning、无 CA、不校验 server_name、不验证客户端证书**：03 §6.4/§6.5、05 §Network、07 规则7、08-INV-29 一致。
- **Tailscale DeviceId 因 seed 含 IP 而不稳定**：03 §1.4/§8.6、09 DIRTY-17 cause/机制/严重度/修复全一致。
- **接收方 cert 重生成破坏对端 pin**：03 §6.1/§6.5、09 DIRTY-22、05 SPOF#5 一致，且都指出 mDNS 供新 cert 是唯一自愈路径。

### 1.3 Bug / 不变量 / 脏状态交叉映射——多篇编号体系收敛

> 这是各文档编号体系的 Rosetta：同一缺陷在不同文档的不同编号下出现，描述/行号一致。

| 缺陷 | 01 | 10 | 08 | 09 | 其它 |
|------|----|----|----|----|------|
| 快照内存/磁盘 desync | BUG-1 | BUG-1（显式交叉引用） | INV-26/INV-25 | DIRTY-01 | 06 §D3.2、07 规则2 |
| config 并发写 clobber/无锁 | BUG-2 | BUG-2 | INV-24 | （缺，见 G-09 矛盾） | 05 §FS、07 规则4 |
| 全局静态不按 Backend 隔离 | BUG-3 | BUG-3（显式交叉引用） | （缺，见遗漏） | DIRTY-14 | — |
| paired_peers vs config 双写 | BUG-4 | BUG-4（扩为三写含 cert） | INV-12 | DIRTY-23/24 | 03 §2.5/§8.2 |
| watcher 陈旧 fallback config | BUG-5 | BUG-5（显式交叉引用） | （缺，见遗漏） | DIRTY-29 | — |
| session dirty 记录非字节相同 | BUG-6 | BUG-6（非锁竞态） | INV-19 | DIRTY-21 | 07 规则9 |

- 10-BUG-3 还**主动调和了行号漂移**：注明"01 引用 backend.rs:62-65（旧单文件，现为 backend/auto_sync_gate.rs:53 起的全局静态块），用新位置"——自洽的已知调和，非未标记矛盾。
- 其它一致对：脑裂 PreferRemote 未实现（08-INV-51=09-DIRTY-11）、ack-before-save（08-INV-27=09-DIRTY-03，行号 backend/projects.rs:160 一致）、孤儿 sync_snapshots key（08-INV-03=09-DIRTY-06）、50% 安全阀（08-INV-49=09-DIRTY-30=10 atomicity item）、scanner 线程泄漏（02 §3.5=06 §B3=09-DIRTY-15=10 thread 表）。

### 1.4 操作 / 并发——一致且不冲突
- **锁顺序规则1（inner 从不跨网络持有）与 06 操作序列一致**：10 引用 process_workspace_mapping_acks 在 :2497 放锁后于 :2536 才网络，06 §A6/§A10 描述同序，ack-before-save 不违反规则1（ack 网络发送与 save_config 是顺序非共持）。
- **候选-clone 整-config clobber 模式**：06 §D2、07 §3.7/规则4、10 BUG-2、09 DIRTY-02 描述同一非原子整写。

---

## 2. 矛盾（17 条经对抗性核对存活）

> 每条都已打开两篇文档核对原文。"修复方"列指出哪篇该改。绝大多数是 02-entities.md 的 stale 描述。

### 2.1 高严重度

**C17 · high · `auto_sync_paused` 暂停实际不生效（功能 bug）**
06 §B1 前置6（line 361）称"Inner.auto_sync_paused is checked implicitly via gate (scanner checks…)"——断言 scanner honor 暂停标志且 gate 会查它。但 09-DIRTY-16、08-INV-36、10 §shared-state 三篇否证：scanner/watcher 循环从不读它（只 backend/mod.rs:634 get/set），try_begin_auto_sync 只 gate on in_flight/cooldown，且 scanner 结构上 bypass Inner.inner 根本读不到该字段。**06 自身 §B3 scanner 流水线也没列暂停检查，自相矛盾。**
→ **修 06**：改 §B1 前置6 为"auto_sync_paused 不被 gate / scanner / watcher 读取，仅 backend get/set，对停止 auto-sync 实际无效（交叉引用 09-DIRTY-16、08-INV-36、10）"。**应作为功能 bug 上报**：切换"暂停自动同步"不会停 scanner/watcher 触发的自动同步。

**C01 · high · PeerConfig.last_seen 语义：live ISO 时间戳 vs 死字段永远 None**
04 §A3（line 49）称 last_seen 由 mDNS 发现写入、UI 读取、存 ISO 时间戳；07 §1 row 21 + 规则5（4 处一致）称它是**死字段、永远写 None（backend/mod.rs:1073）、从不填充/读取**。07 正确（带精确 backend/mod.rs:1073 引用且内部一致）。04 把独立的 live runtime 字段 `PeerRecord.last_seen: Instant`（07 row 22，mDNS 填充、prune 老化）误植到 config 字段上，并臆造了 07 明确否认的"UI 展示"读者。
→ **修 04**：§A3 line 49 改为"永远写 None（backend/mod.rs:1073）/ 读取方 无（死字段）/ 备注 类型为 ISO 时间戳但从不填充；live 可达性来自 PeerRecord.last_seen:Instant，非此字段"。

### 2.2 中严重度

**C03 · medium · state.toml 装什么：版本计数 SyncState vs manifest-hash SyncSnapshot**（=review-phase1 §2.3，未修）
02 §8.2（line 295）称 state.toml "Contains: Sync snapshots (manifest hashes for split-brain detection)"。01/04/07 一致：manifest-hash SyncSnapshot 在 **config.toml** 的 ProjectConfig.sync_snapshots；state.toml 装 ProjectVersionState（版本计数+指纹+has_synced）。02 把两者颠倒。
→ **修 02**：§8.2 改为"version state（local/remote_version 计数+指纹+has_synced）。注意：脑裂用的 manifest-hash SyncSnapshot 在 config.toml，不在此"；并修"Written: after each successful push"——生产 GUI push 从不写 state.toml（只 SyncCoordinator::record_success 路径写，GUI 不调）。

**C04+C16 · medium · 同步历史磁盘格式：单文件 history.jsonl vs 目录 history/*.json（5 条上限）**（=review-phase1 §2.2，未修）
02 §8.4（line 307-310）独家称 `~/.aisync/history/*.json` 目录、每文件 5 条上限（HISTORY_FILE_LIMIT, 旧 backend.rs:62 const）。01 §L5、04 §D1、05、09-DIRTY-25、11 §4 **五篇一致**为单一 append-only `~/.aisync/history.jsonl`。09-DIRTY-25 决定性指出 `record_sync_scoped` 本身就追加到单一 history.jsonl（backend/history.rs:38，与 01 引用同符号）。02 §8.4 的旧 backend.rs:62 是行号混淆——02 自己 §L3 把 INCOMING_SYNC_SUPPRESSIONS 也放在同处（现 backend/auto_sync_gate.rs:53）。
→ **修 02**：§8.4 改为单一 append-only history.jsonl（每行一事件、无每文件条目上限、writer backend/history.rs:23 / history.rs:38），删 HISTORY_FILE_LIMIT/`*.json` 说法。注意 04 §D1 的"file_paths max 5"是单事件字段截断，非文件级保留上限，勿混淆。

**C06 · medium · Ed25519 配对认证在出货 app 里是否真用**
02 §9.1（line 334）把 Keychain Ed25519 列为"Used by: ensure_local_ed25519_identity() for pairing authentication"——无 caveat，读起来像在用。03 §7/§8.5 + 05 §OS 称：confirm_pairing 传硬编码 `gui-local-key`/`gui-peer-key`（backend/peers.rs:300），Ed25519 路径在出货 GUI 里 unwired/latent。
→ **修 02**：§9.1 加 caveat"意图用于配对认证，但出货 app 中 LATENT/未接线——live 配对传硬编码占位 key（backend/peers.rs:300）；见 03 §8.5、05 §OS"。另 02 引 `ensure_local_ed25519_identity()` 而 03/05 引 `_in_store` 变体，命名漂移待调和。

**C12+C15 · medium · session staging 清理：06 称"所有路径都清"vs 09-DIRTY-09 称仅成功路径清**
06 §A8 step g/§D1/§D3.4 多处称 staging "cleaned in all paths (success and error) / even on failure"（backend/sync_push.rs:20）。09-DIRTY-09 用**同一行**指出 `let _ = fs::remove_dir_all` 仅成功路径跑；transport Err 经 `?` 早返、或 panic/killed worker/transport drop 时 `.aisync-session-stage-*` 被孤儿化，UNDETECTED（无启动清扫）。06 自身 step g 序列就在 `?`-传播的 sync 调用之后，自我否证。
→ **修 06**：改为"正常成功/错误返回路径清理，但 transport 早错或任务突死时不清、且启动不清扫（见 09-DIRTY-09）"；严重度从 low 提到 medium。10 line 196 直接引 06 继承同一 overclaim，须同步改。

**C09 · medium · INV-26 标"enforced"但 auto-sync 路径绕过（10-BUG-1/09-DIRTY-01）**
08-INV-26 标 enforced 并列入"已强制（防退化基线）"高行。但其强制文本只描述 manual run_sync 路径；10-BUG-1 + 09-DIRTY-01 指出 auto-sync（run_project_auto_sync, backend/auto_sync_orchestration.rs:155→run_tcp_push, backend/sync_push.rs:20）**从不**执行回填 → auto-sync 后 check_split_brain 读陈旧内存快照 → 潜在假脑裂。
→ **修 08**：INV-26 从 enforced 降为 PARTIAL（manual 强制、auto-sync 不），移出"已强制"高行，把假设性"症状（若缺）"换成 live auto-sync 症状，交叉引用 10-BUG-1/09-DIRTY-01。

### 2.3 低严重度（文字/行号订正）

| ID | 矛盾 | 修复方 |
|----|------|--------|
| C02+C13+C14 | 01 头部"6 distinct storage layers"但正文列 Layer 1–7（=review-phase1 §2.1 未修） | **修 01** line 5：6→7 |
| C07 | 02 §9.1 把 Keychain service 常量引到 discovery/lib.rs:26，实为 :25（:26 是 discovery PROTOCOL_VERSION=1）；11/05 都印证 | **修 02** §9.1：:26→:25 |
| C11 | 文件传输接收临时文件名：06 称 `.aisync-ft-{id}.tmp`（with_extension 会替换扩展名），09-DIRTY-07 称 `<name>.<id>.part`（file_transfer_tmp_path, backend/file_transfer.rs:510）。行为一致（失败孤儿），仅命名分歧 | **修 06** §A10/§D1：用 `<name>.<id>.part` |
| （cert 文件名） | 02 §8.5 用 `receiver.{cert,key}`（展开为 receiver.cert/receiver.key，PEM 风格误导）；03/05/12 一致为 `receiver.der`/`receiver.key.der`（backend/identity.rs:84） | **修 02** §8.5：改为 receiver.der/receiver.key.der |
| （端口默认） | 02 §5.1 称默认端口"ephemeral"；05/12 一致为固定 52000（default_receive_port, config.rs:456-458），ephemeral 仅测试用、生产无 fallback | **修 02** §5.1：默认 52000 |

> **3 条被对抗性核对剔除的误报**（未列入上表）：reviewer 初筛的 20 条中，3 条经打开原文核对发现两篇实际一致或属同篇内不同粒度（如 07 的 17 行表 vs 01 的 7 层是细粒度分解，非矛盾）。

---

## 3. 遗漏（36 项，按类）

### 3.1 04-fields.md 缺三个存储层的字段区（=review-phase1 §3.1/3.2/3.3 未修）
- **L7 Session Parse**：ParsedSession/RecordLine 字段从未列表，而 08-INV-19/22 推理 RecordLine.dirty 与字节保真（low）。
- **L4 Discovery runtime**：SharedState/PeerRecord/paired_peers.json schema 缺，而 08-INV-11/44/45 推理 PeerRecord.last_seen 与 DeviceId-keyed map（medium）。
- **本机 TLS 身份文件** `~/.aisync/receiver.{der,key.der}`：04 §C6 只列内存 TlsIdentity，磁盘文件+写时机缺（medium）。
- 另：04 §B8 ConfigStore 未注"死代码、Backend 不实例化"（=review-phase1 §2.4，low）；传输 staging 目录未作字段区（low）。

### 3.2 缺不变量/脏状态条目（跨 08/09 的覆盖空洞）
- **09 缺 BUG-2 并发 config clobber/corruption 的 high 脏状态**（G-09，high）：09 §1 的 DIRTY-01/02/29 都 medium 且为相邻态，没有"两线程交错 load→modify→save、一写覆盖另一 / 读到半写损坏 config.toml"这一 08-INV-24+10-BUG-2 定为 high 的态。建议加 DIRTY-34（high）。
- **08 缺 BUG-3（全局静态不隔离）和 BUG-5（watcher 陈旧 fallback）的不变量**（各 medium）：09-DIRTY-14/29 的页脚都声称"对应被违反的不变量见 08"，但 08 无对应条目。
- **08/09 缺 11 的迁移风险 R1–R13 对应项**（medium）：尤其 R1/R11（keychain 身份静默重生成→配对失效）是具体脏状态，09 §6 应与 DIRTY-17 并列；R9（validate 因缺路径拒整 config）是一致性属性，08 无 INV。
- **10 的两个 NEW 发现无 INV/DIRTY**（low）：emit() callbacks-lock 潜在自死锁、receive_file_transfer_data 全局锁串行化所有入站块。
- **缺 Ed25519 占位认证的安全缺口 INV/DIRTY**（medium）：03 §8.5 散文指出"任何知 IP/端口的设备都能发起配对，只需 UI 确认 6 位码"，08/09 无对应条目。
- **缺 Keychain service 名跨版本稳定性不变量**（medium）：11-R1 是改名最高风险，08 §2 身份稳定性（INV-08..12）止于磁盘层，无对应。

### 3.3 交叉引用稀疏（最高优先级结构问题）
- **G-link · high · 01/02/03/04/11 五篇对外零交叉引用**（grep 验证）。只有 05/07/08/09/12 互链，且单向：07↔08↔09 三元，05→01/02/06，10→01/06，但四大基础清单文档（01 state、02 entities、03 identity、04 fields）+ 11 migration 被引而不引。读 03 的人不会被指向 08 身份不变量（INV-08..12/41/42）或 09 身份漂移（DIRTY-17/18/19）。
  → 建议：03 §8 链 08/09 身份条目；04 §F 链 08/09；01 BUG-1..6 链 10 BUG-1..6（同 bug 重编号）；06 §D3/§B1-3 链 10；08-INV-24/36 链 10 锁顺序。
- **G-link2 · high · 无文档反向引用 10-concurrency.md**，即便直接 load-bearing：06 §B1-3/§D3 描述的正是 10 分析的竞态，却从不指向 10。

### 3.4 完全未覆盖的横切关注（新维度候选）
- **可观测性/可诊断性**（medium）：09 的 33 脏状态 / 08 的 51 违反无任何映射到具体日志行/trace_stage。鉴于用户 CLAUDE.md §5（"任何'看似执行但没生效'必须有日志解释"），缺 log-vs-symptom 映射是显著空洞。建议 09 的 detection 列点名实际日志串（如 `check_split_brain_unreachable`、`cert_source`）。
- **前端/Tauri IPC 层**（medium）：04 §E 只列 DTO 即止。pending_* 队列轮询节奏、Inner.config 陈旧时 UI 显示什么（07 cache-vs-truth 全后端视角）、sync-progress 事件可靠性（丢事件/乱序/窗口关闭）无人分析。"UI 显示已配对但 push 失败"（09-DIRTY-23）无前端侧维度。
- **时间/时钟语义**（medium）：mtime 变更检测、unix_secs、paired_at、120s 配对过期、cooldown Instant、trash 7 天保留——双机时钟偏移从未分析，而脑裂/冲突/配对码过期都隐含可比时钟。04 混用 epoch-secs/millis 无人调和单位。
- **多 peer 拓扑一致性**（medium）：08-INV-37 只点出项目 auto-sync 假设单 peer，无维度端到端分析 N-peer（fan-out 顺序、per-peer 快照分歧、A/B 同时 push、50% 安全阀/脑裂在 >1 peer 是否自洽）。整个架构隐含 2 设备但从未声明为 scope 边界。
- **资源上界/背压**（low）：10 标 fire-and-forget 线程"untracked and unbounded"，02 §8.6/12 标日志无界，但无统一"什么有界什么无界"维度（线程、日志、history.jsonl、无 cap 的 pending_* VecDeque）。
- **持久化 durability（fsync）**（low）：仅 10 指出原子 tmp+rename 路径都不调 sync_all/sync_data（crash-consistency ≠ 掉电 durability），05/01/11 把原子写当掉电安全。
- **CI/构建 breakage**（low）：12 §CI 标 build-dmg.yml 仍引 stale `aisync-app` 工作目录（应为 codebaton-app）会致 CI 失败，但 11（改名影响专篇）R1-R13 未列此条。

---

## 4. 优先修复清单

| 优先级 | 动作 | 类别 |
|--------|------|------|
| **P0** | 上报功能 bug：`auto_sync_paused` 不 gate scanner/watcher/gate，暂停无效（C17）；修 06 §B1 前置6 | 矛盾·high |
| **P0** | 加交叉引用：01/02/03/04/11 对外补链，全篇补链 10-concurrency（G-link/G-link2） | 遗漏·high |
| **P1** | 修 04 §A3 PeerConfig.last_seen 为死字段（C01） | 矛盾·high |
| **P1** | 修 02 §8.2（state.toml 内容颠倒）、§8.4（history 单文件）、§9.1（Ed25519 caveat + :25）、§8.5（cert 文件名）、§5.1（端口 52000）——清掉 02 的 stale 描述群 | 矛盾·medium/low |
| **P1** | 加 09-DIRTY-34（high）：并发 config clobber/corruption（G-09） | 遗漏·high |
| **P2** | 修 06 staging"all paths"overclaim（C12/C15）+ 同步改 10 line 196；降 INV-26 为 PARTIAL（C09） | 矛盾·medium |
| **P2** | 补 08/09 缺失条目：BUG-3/BUG-5 不变量、R1-R13/keychain 安全缺口对应项 | 遗漏·medium |
| **P3** | 修 01 line 5（6→7）、02 §9.1 行号、06 ft 临时文件名 | 矛盾·low |
| **P3** | 评估新维度：可观测性（log-vs-symptom）、前端 IPC、时钟偏移、多 peer——按需补 | 遗漏·medium |

> 配套：12 维度摘要 + 检索路由指南见 [INDEX.md](INDEX.md)。
