# CodeBaton 架构文档主索引（INDEX）

CodeBaton（原 AISync）状态架构的 12 维度参考。每篇用各自编号体系记录缺陷：01/02 用 `BUG-N`、03 用 `§8.N 热点`、08 用 `INV-NN`、09 用 `DIRTY-NN`、11 用 `RN`；同一缺陷常跨篇出现（交叉映射见 [review-final.md](review-final.md) §1.3）。

收到 bug/feature 时的检索流程见本文档 **§架构师检索路由**。全量交叉审查结论（17 矛盾 / 36 遗漏 / 优先修复清单）见 [review-final.md](review-final.md)。

> **代码结构变更（2026-06）**：后端已从单文件 god-file `backend.rs` 拆分为目录 `codebaton-app/src/backend/` = `mod.rs`（HUB：Backend/Inner/单 Mutex、构造器、薄方法、app_log，以及尚未模块化的 watcher gate 循环等）+ 20 个子模块。各篇文档若引用 `backend.rs:NNN` 现已陈旧，应改读 `backend/<module>.rs` 或对应符号。代码导航 / 子模块速查 + 文档阅读顺序见 [READING-GUIDE.md](READING-GUIDE.md)。

---

## 12 维度

### [01-state-model.md](01-state-model.md) — 状态层次模型
7 个存储层（config.toml / state.toml / 运行时内存 / discovery 存储 / history JSONL / 网络瞬态 / session 解析）谁是 truth 谁是 cache，及层间投影。**答**：有几个存储层、各装什么、谁权威、Inner.config 为何是 clone 非 live 引用、快照手动回填路径。
**缺陷**：BUG-1 快照内存/磁盘 desync、BUG-2 config 并发写无锁、BUG-3 全局静态不隔离、BUG-4 paired_peers/config 双写、BUG-5 watcher 陈旧 fallback、BUG-6 session dirty 记录非字节相同。

### [02-entities.md](02-entities.md) — 运行时实体清单
每个 live 实体（单 Tauri 进程、单例 struct、所有线程、tokio runtime、TCP/mDNS 监听、FsWatcher、定时器、文件锁、外部资源）的 create/destroy/cleanup-owner。**答**：有哪些线程/监听/watcher、谁管生命周期、各文件落点、实体关系树。
**缺陷**：BUG-1 scanner 线程无 shutdown handle 泄漏、BUG-2 sync worker 线程 detached、BUG-3（已修：serve daemon 曾孤儿）、BUG-4 全局静态跨实例泄漏。⚠ 含若干 stale 描述（见 review-final C03/C04/C06/C07）。

### [03-identity.md](03-identity.md) — 身份模型
7 个身份概念（DeviceId/device_name、配对码/PairedPeer、项目/workspace 名、session_id/encoded_dir_name、sync-snapshot peer_name key、TLS cert、Ed25519）的生成/存储/稳定性/混淆热点。**答**：谁唯一标识设备/peer/项目/session、为何用 device_name 而非稳定 DeviceId 做 key、Claude 路径如何有损编码、TLS pinning 如何信任、Ed25519 配对认证是否真用（否，硬编码占位）。
**缺陷**：热点 8.1 peer_name-as-key 脆弱、8.2 双写、8.3 编码冲突、8.4 client cert 每连接重建、8.5 Ed25519 名存实亡、8.6 Tailscale DeviceId 不稳、8.7 项目名碰撞、8.8 改名快照不迁移。

### [04-fields.md](04-fields.md) — 状态字段全景
逐字段：config 层(A)/运行时层(B)/传输协议层(C)/history-JSONL 层(D)/前端 DTO 层(E)，各字段类型/writer/reader/写清时机/依赖 + 字段 bug(F)。**答**：每个字段含义/谁读写/何时设清/依赖、三个 history 文件 JSONL schema、Message 枚举与常量、DTO→来源映射。
**缺陷**：BUG-007 device.name 占位符；§F 字段坑：auto_sync_paused 仅内存（重启丢）、confirm_overwrite default=false 兼容性数据安全隐患等。⚠ 缺 L7/L4/TLS 身份文件字段区（见 review-final §3.1）。

### [05-dependencies.md](05-dependencies.md) — 外部依赖拓扑
每个外部资源（文件、TCP 52000、mDNS、TLS pinning、keyring、tailscale、file-opener）的路径/端口、可用性假设、精确失败模式（panic/Err/静默退化/吞掉）+ 可用性矩阵 + 单点故障。**答**：某依赖缺失时崩溃还是降级、哪些启动致命、数据丢失防护（备份/回收站/50%阀/原子写/路径沙箱）在哪。
**缺陷**：SPOF-1..7：config/pairing 损坏启动崩、端口 52000 冲突禁收、无网络重试层、TLS pin 是唯一信任锚（重生成 cert 断所有连接）、session 解析 per-file 全或无、编码器漂移静默跳过 session。

### [06-operations.md](06-operations.md) — 操作矩阵
每个用户操作（A1-A12：配对/解配对/增删项目&workspace/push/pull/文件传输/设置/解脑裂）和系统操作（B1-B6：项目/workspace 自动同步/session scanner/mDNS/入站 push/workspace 扫描）的前置/状态变更/成功标准/失败处理/回滚边界 + push 完整链。**答**：操作逐步做什么、需要什么、改什么、失败时清理/孤儿、回滚边界在哪、check_split_brain→run_sync→run_tcp_push 链。
**缺陷**：§D3 引 S1 BUG-1/2/4 + S2 BUG-1；逐操作孤儿资源（cert .der、自动建目录、ft 临时文件、staging）。⚠ §B1 误称 auto_sync_paused 被 gate 检查（review-final C17，功能 bug）。

### [07-truth-hierarchy.md](07-truth-hierarchy.md) — 真相源层次
17 层权威表 + 10 条优先级规则（谁赢）+ 15 个 cache-vs-truth 冲突场景（每场景点名代码实际胜者，偏离即 bug）。**答**：同一状态在内存与磁盘谁权威、为何 config 内存领先磁盘但 snapshot 字段例外、为何后台线程把磁盘当唯一真相、为何 ConfigStore 是死代码、为何 state.toml/版本号不在生产权威链。
**缺陷**：拥有 10 优先级规则 + 15 冲突场景（§3.1-3.15）；从权威角度交叉引用 S1 BUG-1/2。

### [08-consistency-rules.md](08-consistency-rules.md) — 一致性规则/不变量
51 条不变量 INV-01..51，各标 enforced/PARTIAL/NONE/VIOLATED + 症状 + 严重度——bug 检测谓词清单。**答**：哪些约束实际强制 vs 未强制/违反、疑似 bug 破坏哪条不变量、如何检测。
**缺陷**：拥有 INV-01..51。high·未强制：INV-01/09/12/15/22/23/24/27/37。PARTIAL：INV-08/10/13/14/20/25/33/34/51。已强制基线：INV-26/28/29/30/31/32/35/47/49/50。

### [09-dirty-state.md](09-dirty-state.md) — 脏状态分类
33 类脏/不一致/损坏态 DIRTY-01..33，各含成因/检测(或 UNDETECTED)/修复(或 UNREPAIRED/manual/UNIMPLEMENTED)/严重度——运维调试症状索引。**答**：用户报"看似同步没生效""配对成功但 push 失败""config 重启回退"是哪类脏态、系统是否检测/修复。
**缺陷**：拥有 DIRTY-01..33。high：DIRTY-03 ack-before-save、11 PreferRemote 未实现、12 双向漂移脑裂、17 Tailscale DeviceId 漂移、22 cert 重生成断 pin。

### [10-concurrency.md](10-concurrency.md) — 并发与原子性模型
所有共享可变态（Mutex/AtomicBool/static）、执行上下文（线程+tokio）、锁顺序纪律、原子 vs 有竞态窗口、已知并发 bug 的具体交错重述。**答**：哪些锁、何序获取（绝不共持两锁规则）、哪些写原子（paired_peers/write_file_atomic/commit_two_dirs）vs 非原子（config/state truncate-in-place）、watcher/scanner 为何 bypass Inner、潜在 emit() 死锁、poison blast radius。
**缺陷**：重述 S1 BUG-1..6；两个 NEW：emit() callbacks-lock 潜在自死锁（今不可达）、receive_file_transfer_data 全局锁串行化所有入站块；候选-clone 整-config clobber、无 fsync durability gap。

### [11-migration.md](11-migration.md) — 版本迁移
config schema 演进（仅 serde-default、无 schema_version）、AISync→CodeBaton 改名标识符表、wire 协议兼容（严格相等 Hello、无降级）、数据目录兼容 + 启动 shim + 风险 R1-R13。**答**：旧 config/数据如何在新构建加载、改名改了/没改什么、为何两个 PROTOCOL_VERSION 分裂、启动修复 shim、跨版本同步是否可行（否，硬失败）。
**缺陷**：拥有 R1-R13。high：R1 Keychain 改新名断旧身份、R2 无迁移框架、R3 协议严格相等无跨版本、R4 双 PROTOCOL_VERSION、R5 新 MessageType 硬失败旧 peer。medium：R6-R10。含 BUG-007 启动自愈。

### [12-environment.md](12-environment.md) — 安装与运行环境
binary 如何定位路径（全 $HOME-anchored，从不相对自身）、环境变量表、构建/打包/DMG 流水线、ad-hoc 签名 & Gatekeeper/TCC 影响、DMG-vs-cargo-run-vs-dev 差异 + 首次启动要求。**答**：每个路径在哪解析、读哪些 env（HOME、AISYNC_* legacy、无 RUST_LOG/proxy）、DMG 如何构建/签名（ad-hoc '-'、无 notarization/entitlement/hardened-runtime）、三种安装模式差异、首启清单。
**缺陷**：CI workflow + clean-deploy skill 仍引 stale `aisync-app`（应 codebaton-app）→ CI 会失败；ad-hoc 签名使 TCC/通知授权不跨 cdhash-变化的重建存活；浏览器下载 DMG 首启 quarantine 阻拦。交叉引用 R1、端口 52000 SPOF。

---

## 架构师检索路由

收到 bug 报告或 feature 请求时，按下表顺序检索文档：

| 情境 | 检索顺序 |
|------|---------|
| **同步覆盖/销毁远端文件（数据丢失）** | 08 INV-49(50%阀+备份)/INV-28/INV-31/INV-15 → 05 §数据丢失防护+SPOF → 06 §A8(push 链/回滚边界)/§A12 → 09 DIRTY-30/12 → 07 §3.15 |
| **脑裂/双向冲突/假脑裂警告** | 08 INV-14/15/26/32 → 07 §3.2/§3.8+规则10(真相在 config.toml SyncSnapshot) → 09 DIRTY-12/13/01/11 → 01 BUG-1 → 06 §C1 |
| **配对成功但 peer-not-found / push 失败 / 无 endpoint/cert** | 09 DIRTY-23/24/22 → 08 INV-12/13/44 → 03 §2.5+§8.2 → 01 BUG-4 → 06 §A1 → 07 §3.11 |
| **TLS 握手失败（cert 不匹配 pin）** | 05 §TLS+SPOF-5 → 09 DIRTY-22/33 → 08 INV-29/13 → 03 §6/§8.4 → 11 R1/R11 |
| **config.toml 损坏/重启回退/两写互覆盖** | 10 §config RMW+并发交错+BUG-2/5 → 01 BUG-2/5 → 07 规则4/§3.6/§3.7 → 08 INV-24/23 → 09 DIRTY-02/29 → 05 SPOF-1 |
| **Claude session 同步到错目录/静默丢失/路径重写损坏内容** | 03 §4+§8.3 → 07 §3.12/3.13/3.14 → 08 INV-19/20/21/22 → 09 DIRTY-20/21/28 → 05 SPOF-6/7 → 01 BUG-6 |
| **peer 改名/重复 hostname → 重复/覆盖条目/丢映射** | 03 §8.1/§8.8/§5.5 → 08 INV-09/10/41/42 → 09 DIRTY-18/19/06 → 01 BUG-4 |
| **自动同步死循环/触发过频/watcher 不触发或触发错 peer** | 08 INV-35/36/37 → 06 §B1/§B2/§B3/§B5c → 02 §3.2-3.5+§7 → 10 §Rule3+BUG-3 → 09 DIRTY-14/16 → 04 §B1-B3 |
| **app 挂起/死锁/panic 拖垮 UI/线程泄漏** | 10 全篇(锁顺序/emit()死锁/poison blast radius/receive_file_transfer_data 全局锁) → 02 §3.5+§11 BUG-1/2+§10 → 01 BUG-3 → 05 §无网络重试+超时表(可能是 10s/60s 超时非死锁) |
| **feature：新增 config 字段** | 04 §A(匹配现有字段列)+§F(坑) → 11 §1(serde-default 唯一兼容)+§1.5+R2/R8/R10(须加 #[serde(default)]) → 08 INV-01..04(若是 peer-keyed map)+INV-39 → 01 §2-3(truth 还是 cache) → 07 规则1-3(后台线程读盘不读 Inner.config) |
| **feature：新增协议消息/Message 变体** | 04 §C1+§C7(常量) → 11 §3(严格相等 Hello)+R3/R5/R7(新 MessageType/变体硬失败旧 peer，须升版+全节点升级)+§3.5 → 08 INV-30/31/50 → 02 §3.1/§5 + 06 §B5(入站路由) |
| **迁移/升级：旧 AISync 升 CodeBaton 后行为异常（丢配对/身份重置/config 被拒）** | 11 全篇(§2 改名表/§5 启动 shim/§6 R1/R9/R6/R12) → 12 §代码签名(TCC 不跨 cdhash)+§首启+stale aisync-app CI bug → 03 §7/§6 → 05 §keyring+SPOF-1/2 |

> 找不到对应情境时：先看 [review-final.md](review-final.md) 的矛盾/遗漏清单（可能是已知文档缺口），或从 09 脏状态症状索引按"用户看到的现象"反查。
