# 11 - 版本迁移 (Version Migration)

CodeBaton 由旧品牌 AISync 改名而来（改名提交日 2026-06-19）。本文档是版本迁移的权威参考，覆盖四个维度的兼容性：**配置 schema 演进**、**二进制/标识符改名**、**wire 协议兼容**、**磁盘数据目录兼容**，并枚举启动时实际存在的迁移修复逻辑与已知缺口。

核心结论先行：CodeBaton **没有任何显式的版本号、迁移框架或一次性迁移标记**。所有向后兼容能力都依赖两个隐式机制 ——（1）serde 的 `#[serde(default)]` 字段兜底，（2）JSONL 历史文件的逐行容错读取。改名采取「表层换皮、兼容层冻结旧名」策略，但其中 Keychain service 名是唯一被改成新品牌值的兼容关键标识符，构成最高风险点。

---

## 1. Config Schema 演进

### 1.1 无显式 schema_version

`SyncConfig` 结构体没有任何 `version` / `schema_version` / `config_version` 字段。其字段全集为 `device`、`onboarded`、`receive_port`、`peers`、`claude_config`、`projects`、`workspaces`、`exclude_rules`、`default_sync_mode`、`refresh_interval_secs`、`default_file_receive_dir`、`receive_dir_override`、`state_path`，无任何版本标记 (`config.rs:11-40`)。这意味着配置文件本身不携带版本号，启动时**无法据此判断 schema 版本、也无法按版本号路由迁移逻辑**。

### 1.2 唯一兼容机制：serde default

schema 演进完全是隐式的：新增字段全部带 `#[serde(default)]`（无参，取类型默认）或 `#[serde(default = "fn")]`（自定义默认函数），读旧配置时缺失字段自动取默认值。**这是唯一的兼容手段**，不存在任何显式的版本化迁移函数（无 `migrate_config` / `upgrade` 之类）(`config.rs:14-39`)。

带无参 `#[serde(default)]` 的字段及其旧配置缺失时的兜底值 (`config.rs:14-39`)：

| 字段 | 行 | 旧配置缺失时默认值 | 引用 |
|------|----|------|------|
| `onboarded` | :14 | `false` | `config.rs:14` |
| `peers` | :18 | 空 `HashMap` | `config.rs:18` |
| `claude_config` | :20 | `ClaudeConfig::default()` | `config.rs:20` |
| `projects` | :22 | 空 `Vec` | `config.rs:22` |
| `workspaces` | :24 | 空 `Vec` | `config.rs:24` |
| `default_sync_mode` | :28 | `TwoWayAuto` | `config.rs:28` |
| `default_file_receive_dir` | :32 | `None` | `config.rs:32` |
| `receive_dir_override` | :36 | `None` | `config.rs:36` |
| `state_path` | :38 | `None` | `config.rs:38` |

带自定义默认函数的字段 (`codebaton-sync/src/config.rs:16-31`)：

| 字段 | 行 | 默认函数 | 兜底值 | 引用 |
|------|----|------|------|------|
| `receive_port` | :16 | `default_receive_port()` | `52000` | `config.rs:456-462` |
| `exclude_rules` | :26 | `crate::watcher::default_exclude_rules()` | 内置排除规则 | `config.rs:16-31` |
| `refresh_interval_secs` | :30 | `default_refresh_interval_secs()` | `30`（秒） | `config.rs:456-462` |

因此旧配置缺 `refresh_interval_secs` 时自动取 30 秒、缺 `receive_port` 时取 52000，加载无碍。

### 1.3 唯一的必填项：[device]

`device` 字段（`DeviceConfig`）是 `SyncConfig` 中唯一**没有** `#[serde(default)]` 的字段，因此 `[device]` 段是配置文件的必填项；若旧配置缺 `[device]` 则反序列化失败。`DeviceConfig` 内 `id` 和 `name` 两个字段也都无 default，均为必填 (`config.rs:13`、`config.rs:158-162`)。任何 AISync 写过的配置都含 `[device]` 段，故实际升级不会触发此失败。

### 1.4 后加兼容垫片的直接证据

schema 隐式演进有若干带注释的实证字段：

| 垫片字段 | 所属结构 | 说明 | 引用 |
|------|------|------|------|
| `sync_snapshots` | `ProjectConfig` | 注释明确：「按对端名索引的同步快照（脑裂检测）。旧配置无此字段，默认空。」 | `config.rs:209-211` |
| `default_file_receive_dir` / `receive_dir_override` / `state_path` | `SyncConfig` | 三个后加的 `Option<T>` 字段，`#[serde(default)]` 默认 `None`；`receive_dir_override` 注释说明用于让每个 Backend 实例有独立接收目录、消除并行测试对全局 env 的依赖 | `config.rs:34-39` |
| `endpoint` / `server_cert` / `server_name` / `last_seen` | `PeerConfig` | 四个 `#[serde(default)]` 的 `Option<T>`，旧配置缺失默认 `None` | `config.rs:168-176` |
| `local_root`/`local`、`peer`/`peers` 等 | `WorkspaceConfig` | 新旧两套字段并存，运行时由 `effective_local_root` / `effective_peer` / `effective_remote_root` 做择优；另有 `scan_depth` 默认 `default_scan_depth()=1`、`enabled` 默认 `default_true()=true`。`workspaces` 本身在 `SyncConfig` 层 default 为空 `Vec`，旧配置完全没有 `[[workspaces]]` 时加载干净 | `config.rs:215-239` |

### 1.5 加载路径：解析 + 校验，无迁移分支

`load_config()` 用 `toml::from_str` 反序列化，失败时返回 `AisyncError::Config("parse TOML config: ...")`；成功后立即调用 `validate_config` 做语义校验。**load 路径里完全没有版本探测或迁移分支**，schema 演进全靠 serde 默认值兜底 (`config.rs:345-356`)。

`validate_config` 是唯一的加载后校验（load 与 save 都调用），它会拒绝 (`config.rs:377-440`)：重复的 project 名（:380-385）、enabled 项目的 local 路径不存在（:386-392）、重复的 workspace 名（:401-406）、enabled workspace 的 local 路径不存在（:408-414）、enabled workspace 无 peer（:415-420）、`workspace.scan_depth != 1`（:421-426）、`exclude_rules` 含空白项（:429-437）。

> **迁移风险（medium）**：`validate_config` 会因 enabled 项目/工作区的本地路径不存在而拒绝**整个**配置 (`config.rs:386-392`、`config.rs:377-440`)。跨机/换机迁移时，若旧 config 引用的本地代码目录在新机器上路径不同或已被删除/移动，新构建加载旧 `config.toml` 会**直接报错失败**而非跳过该条目。

### 1.6 读时迁移并落盘（仅写操作时）

`save_config` 用 `toml::to_string_pretty` 序列化**整个** `SyncConfig` 写回，会把所有 serde-default 填充的字段（如 `onboarded`、`receive_port`、`refresh_interval_secs`）显式写出 (`config.rs:358-367`)。因此一旦新版本读入旧配置再 save，配置文件会被补全为新 schema 全字段 —— 这是一次隐式的「读时迁移并落盘」，但**只发生在有写操作时，纯读不会改写文件**。

---

## 2. 二进制改名影响 (AISync → CodeBaton)

下表枚举每一处旧名 vs 新名标识符。**COMPAT-CRITICAL** 列标记跨版本/跨设备兼容关键项（改动会破坏新旧互通或旧安装升级）。

| # | 标识符 | 当前值 | 新名? | COMPAT-CRITICAL | 后果 | 引用 |
|---|------|------|------|:---:|------|------|
| 1 | 数据目录 | `~/.aisync/`（旧） | 否 | 是 | 新二进制仍读旧目录，同机原地升级无需迁移；若改 `.codebaton` 旧 config/identity 全丢 | `config.rs:370` |
| 2 | 状态文件 | `~/.aisync/state.toml`（旧） | 否 | 是 | 整个数据目录维持旧名 `.aisync`，新二进制不去别处找 | `config.rs:374` |
| 3 | TLS 接收端证书 | `~/.aisync/receiver.der`（旧） | 否 | 是 | pinned cert 不失效，已配对设备无需重配对 | `backend/identity.rs:84` |
| 4 | env `AISYNC_RECEIVE_DIR` | 旧前缀 | 否 | 相关 | 用户脚本/launchd 旧变量在新版仍生效，无 `CODEBATON_` 双读层 | `backend/mod.rs:169` |
| 5 | env `AISYNC_DEVICE_NAME` | 旧前缀 | 否 | 相关 | `default_device_name()` 最高优先级来源，无新别名 | `backend/identity.rs:19` |
| 6 | env `AISYNC_LOG_FILE` | 旧前缀 | 否 | 相关 | 日志路径覆盖沿用旧命名（backend 与 discovery 各一处） | `backend/events.rs:155`、`discovery/lib.rs:1476` |
| 7 | env `AISYNC_CODEX_SESSIONS_DIR` | 旧前缀 | 否 | 相关 | Codex 会话扫描目录覆盖沿用旧名 | `backend/mod.rs:2179` |
| 8 | mDNS 服务类型 `AISYNC_SERVICE_TYPE` | `_aisync._tcp.local.`（旧） | 否 | 是 | 局域网发现标签，改 `_codebaton._tcp` 则新旧版互相发现不到 | `discovery/lib.rs:24` |
| 9 | Keychain service `AISYNC_KEYRING_SERVICE` | **`"CodeBaton"`（新值）** | **是** | 是 | **唯一值已迁移到新名的标识符**；旧版若用 `"aisync"` 作 service 写过私钥，新版读不到 → 身份重生成、需重配对 | `discovery/lib.rs:25`、`discovery/lib.rs:690` |
| 10 | 配对码前缀 | `aisync-pairing-v{V}`（旧字面量） | 否 | 是 | 两端必须同前缀才能算出相同 6 位配对码 | `discovery/lib.rs:677` |
| 11 | discovery `PROTOCOL_VERSION` | `1`（配对码用） | n/a | 是 | 与 transport v2 是不同常量，升版需分别评估 | `discovery/lib.rs:26` |
| 12 | transport `PROTOCOL_VERSION` | `2`（握手用） | n/a | 是 | 与品牌无关，改名不影响握手版本 | `transport/lib.rs:81` |
| 13 | TLS server_name / CN（接收端） | `"aisync-receiver"`（旧） | 否 | 是 | 客户端按此 SNI 验证服务端证书，改名则名校验失败、连接被拒 | `discovery/lib.rs:55`、`backend/identity.rs:92` |
| 14 | TLS CN（客户端） | `"aisync-client"`（旧） | 否 | 是 | 客户端身份 CN 旧品牌残留 | `backend/mod.rs:866` |
| 15 | CLI server_name 默认 | `"aisync-receiver"`（旧，两处 clap default） | 否 | 是 | CLI 与 GUI 默认 SNI 一致 | `cli/main.rs:37` |
| 16 | 占位设备名 `PLACEHOLDER_DEVICE_NAME` | **`"CodeBaton Device"`（新值）** | 是 | cosmetic | 展示性字符串；改占位符则 `is_placeholder_device_name` 需同时识别旧占位符 | `backend/identity.rs:13` |
| 17 | Tauri bundle id | `"com.aisync.app"`（旧） | 否 | 是 | macOS TCC/Keychain ACL/LaunchServices 身份键，保旧名让 TCC 授权（辅助功能/屏幕录制）升级后沿用 | `tauri.conf.json:5` |
| 18 | Tauri productName | `"CodeBaton"`（新值） | 是 | cosmetic | `.app` 显示名/可执行名，不影响数据/权限 | `tauri.conf.json:3` |
| 19 | Cargo 包名/二进制名 | `codebaton-app` / `codebaton_app_lib` / bin `codebaton`（新） | 是 | 编译期 | 源码层 crate 命名无旧名残留，不影响运行时兼容 | `Cargo.toml:13` |
| 20 | 核心错误枚举 | `AisyncError`（旧名类型） | 否 | cosmetic | 纯内部类型名，不跨进程/不持久化；全仓库引用，改名工作量大但无兼容风险 | `core/lib.rs:12`、`session/lib.rs:9` |

**整体策略**（`discovery/lib.rs:24-25`）：品牌改名只完成了表层（Cargo 包名、productName、Keychain service 值、占位设备名）；所有跨版本/跨设备兼容关键标识符（数据目录 `.aisync`、mDNS `_aisync._tcp`、配对码前缀 `aisync-pairing`、TLS CN `aisync-receiver`/`aisync-client`、bundle id `com.aisync.app`、env `AISYNC_*`）一律冻结旧名。这是正确的渐进式改名 —— **唯一例外是第 9 项 Keychain service 值改成 `CodeBaton`，与其他冻结项不一致，是唯一可能导致旧安装设备身份失效的破坏性变更。**

---

## 3. 协议兼容性

### 3.1 两个独立的 PROTOCOL_VERSION

系统存在**两个语义已分裂的协议版本常量**，升级时需分别维护，极易遗漏：

| 常量 | 值 | 用途 | 引用 |
|------|----|------|------|
| transport `PROTOCOL_VERSION` | `2` | wire 握手协商的唯一基准值 | `transport/lib.rs:81` |
| discovery `PROTOCOL_VERSION` | `1` | 配对码哈希输入字符串 | `discovery/lib.rs:26` |

> **风险（high）**：配对码字符串实际是 `"aisync-pairing-v1:..."`（用 discovery v1），而握手用的是版本 2。两个版本号语义已分裂 (`discovery/lib.rs:26`)。

### 3.2 握手：严格相等，无降级

握手是双向 Hello 交换：客户端先 write `Hello` 再 `expect_hello` 读对端 Hello（多处调用点如 :539-547）；服务端先 `expect_hello` 读客户端 Hello 再 write 自己的 Hello。两侧都跑严格相等检查 (`transport/lib.rs:1588-1605`)。

握手版本检查是**严格相等（==）**，不是范围或最小版本：仅当远端 `protocol_version` 恰好等于本地 `PROTOCOL_VERSION` 时返回 Ok，否则报错并断开 (`transport/lib.rs:3016-3023`)。版本不匹配时 `expect_hello` 构造 `"protocol version mismatch: local {PROTOCOL_VERSION}, remote {protocol_version}"` 并返回 Err，连接随即失败，**无降级回退** (`transport/lib.rs:3021-3022`)。

全程无版本协商/降级路径：Hello 只携带单一固定 `PROTOCOL_VERSION` 常量，无 `min`/`max` 或 `supported_versions` 列表，`expect_hello` 只有相等/不相等两种结果 (`transport/lib.rs:3014-3030`)。

### 3.3 版本号在消息中的位置

唯一携带 `protocol_version` 的消息变体是 `Message::Hello`（字段 `protocol_version: u32, device_name: String`），其余 20 个 `Message` 变体均不带协议版本号 (`transport/lib.rs:299-302`)。协议版本号也通过 `DeviceInfo.protocol_version` 嵌入各类 payload（如 `PairingRequestPayload.device`、各 `*Request`/`*Ack` 的 `device` 字段），由 `server_device_info()` 用 `PROTOCOL_VERSION` 填充，但**该字段不参与握手校验** (`transport/lib.rs:3032-3039`)。

发现层不做版本拒绝：mDNS 侧对端 version 属性解析失败时回退到本地 discovery `PROTOCOL_VERSION(=1)`，即把未知/缺失版本的 peer 当作同版本对待，所有版本校验推迟到 TCP 握手的 `expect_hello` (`discovery/lib.rs:1070-1073`)。

### 3.4 消息帧与序列化

帧格式：**4 字节大端长度 + 1 字节类型标记 + serde_json body**。`write_message` 用 `serde_json::to_vec` 编码 body，前置 `message_type() as u8` 类型字节 (`transport/lib.rs:2570-2593`)。`read_message` 先把首字节 `frame[0]` 经 `MessageType::try_from` 解析，剩余字节用 `serde_json::from_slice` 反序列化为 `Message`，类型字节与 body 必须一致否则报 `frame type does not match payload` (`transport/lib.rs:2648-2656`)。

### 3.5 字段级前向兼容（有限）

payload **字段层面**具备一定前向兼容能力：

- 全代码库**无任何** `#[serde(deny_unknown_fields)]` 标注，因此 `serde_json` 默认忽略 struct 中多出的未知字段，旧 peer 能读取新版 payload 新增字段（被丢弃）—— 字段增删对结构体层面是**软兼容**的（前提：Message 变体已知且握手通过）(`transport/lib.rs:2649-2650`)。
- 部分新增字段标注 `#[serde(default)]`，旧版缺失时用默认值。例如 `FileManifest.confirm_overwrite` 注释明确「旧版无此字段，默认 false」(`transport/lib.rs:307-309`)。

### 3.6 版本兼容契约（明确声明）

> **契约**：两个不同 wire 版本（`PROTOCOL_VERSION` 不等）的 CodeBaton 实例之间是**硬失败（hard fail）**，在 Hello 握手阶段即被 `expect_hello` 严格相等检查拒绝并断连，无任何优雅降级 (`transport/lib.rs:3016-3023`)。同版本实例之间，payload 字段层面因无 `deny_unknown_fields` 且部分字段带 `#[serde(default)]` 而具备**有限的前/后向字段兼容**；但任何新增 `Message` 变体 / `MessageType` 编号都会让旧 peer 硬失败。**版本升级必须同时升级所有节点。**

---

## 4. 数据目录兼容

CodeBaton 改名后数据目录**沿用旧的 `~/.aisync`**，因此旧版本写入的文件路径与新版本读取路径完全一致，无需迁移数据目录 (`config.rs:369-375`)。下表枚举 `~/.aisync` 下每个 artifact 及其加载兼容性：

| Artifact | 路径 | 磁盘 schema | 旧数据加载行为 | 容错级别 | 引用 |
|------|------|------|------|------|------|
| 主配置 | `~/.aisync/config.toml` | `SyncConfig`（toml） | serde-default 补齐缺失字段，旧配置无损加载（但 `[device]` 必填、enabled 路径必须存在） | serde-default | `config.rs:370`、`config.rs:14-39`、`config.rs:158-162`、`config.rs:386-392` |
| 状态 | `~/.aisync/state.toml` | `SyncState { projects: HashMap<String, ProjectVersionState> }` | `SyncState::load`：不存在返回空，存在用 `toml::from_str` **严格解析**（失败即 Err，无字段级容错） | 严格 | `config.rs:374`、`sync/lib.rs:417-424` |
| 配对存储 | Unix `~/.aisync/paired_peers.json`；**Windows `%APPDATA%\CodeBaton\paired_peers.json`** | `PairingFile { peers: Vec<PairedPeer> }`，`PairedPeer{ device, public_key, paired_at_unix_secs }` | `load_pairings` 用 `serde_json::from_slice` **严格解析**整个文件（任何字段不可反序列化即 Err）；文件不存在返回空 map。`DeviceInfo` 字段未变，旧文件可加载 | 严格（结构未变故安全） | `discovery/lib.rs:1596-1609`、`discovery/lib.rs:72-77`、`discovery/lib.rs:1544-1557` |
| 对端固定证书 | `~/.aisync/peers/{device_id}-receiver.der` | DER 二进制 | 基于 `config_path.with_file_name` 派生，改名不变；旧 pinned cert 原地被新构建读取 | n/a（原地复用） | `backend/identity.rs:78` |
| 本机 TLS 身份 | `~/.aisync/receiver.der` + `receiver.key.der` | DER 二进制 | 两文件都可读则复用，否则用 CN=`"aisync-receiver"` 重新生成并写回；旧身份无损复用，TLS pinning 不因改名失效 | create-if-missing | `backend/identity.rs:92` |
| 同步历史 | `~/.aisync/history.jsonl` | 单一 JSONL，每行一条 JSON | `sync_history` 逐行 `serde_json::from_str::<Value>` 并 `filter_map(.ok())`，无法解析的行**静默丢弃**而非整文件失败 | 逐行容错 | `backend/history.rs:129` |
| 聊天历史 | `~/.aisync/chat_history.jsonl` | JSONL | 基于 `with_file_name` 派生，改名不变 | 逐行容错 | `backend/messaging.rs:34` |
| 文件传输历史 | `~/.aisync/file_transfer_history.jsonl` | JSONL | 经通用 `read_jsonl`（逐行 `from_str::<Value>::ok()` 容错），旧文件无损加载 | 逐行容错 | `backend/file_transfer.rs:310`、`backend/history.rs:170` |
| 日志 | `~/.aisync/logs/aisync.log`（旧目录+旧文件名） | 文本 | 可被 `AISYNC_LOG_FILE` 覆盖；改名后仍沿用 `aisync`，旧日志被追加而非新建 | 追加 | `backend/events.rs:155` |
| Claude 会话 | `~/.claude/projects`（外部只读依赖） | 外部 | 与改名无关；未配置 peer 时硬编码回退此路径 | n/a | `backend/claude_paths.rs:61`、`claude_code.rs:230-233` |

**通用 JSONL 读取器** `read_jsonl` 与 `sync_history` 一样按行容错（空行过滤 + `from_str::<Value>::ok()`），三个 `*_history.jsonl` 文件的旧数据都不会因 schema 漂移导致整体加载失败 (`backend/history.rs:170`)。

**Keychain 命名空间与磁盘目录分裂**：Keychain 服务名为新名 `"CodeBaton"`，而磁盘数据目录仍为旧名 `.aisync`。同一升级后端的密钥（如 ed25519 私钥 `device:<id>:ed25519`）会去 `CodeBaton` 服务名下查找；若旧 AISync 曾用不同 keychain 服务名写入，则升级后读不到旧密钥（keychain 命名空间漂移），与磁盘 `.aisync` 目录的兼容形成不一致 (`discovery/lib.rs:25`)。

mDNS 服务类型仍为旧名 `_aisync._tcp.local.`，保证新旧版本在局域网上仍能互相发现（运行期兼容）(`discovery/lib.rs:24`)。

---

## 5. 启动时迁移逻辑

`Backend::new()` 启动时存在的**完整**修复/补全 shim 集合（除此之外**全代码库无任何迁移函数、版本字段或一次性迁移标记文件**）：

### 5.1 BUG-007：占位设备名自愈（唯一真正的迁移性修复）

`Backend::new()` 检测 `config.device.name` 为占位符并就地重写为真实主机名：命中后设置 `changed` 标志并触发 `save_config` 回写 (`backend/mod.rs:240`)。这是唯一针对旧磁盘数据的主动修复逻辑。

占位符判定集合（三类都被视为需重新派生）(`backend/identity.rs:33`)：

| 判定 | 来源 | 引用 |
|------|------|------|
| 空字符串 / 纯空白 | 异常写入 | `backend/identity.rs:33` |
| `"CodeBaton Device"` | 新占位符 `PLACEHOLDER_DEVICE_NAME` | `backend/identity.rs:13` |
| `"aisync-device"` | **旧品牌字面量**（legacy placeholder） | `backend/identity.rs:33` |

`PLACEHOLDER_DEVICE_NAME` 即新名 `"CodeBaton Device"`；`default_device_name()` 在 hostname 解析失败时回退到该占位符，与 BUG-007 修复逻辑形成闭环 (`backend/identity.rs:13`)。即旧版本写入的 legacy 名字会在下次启动被治愈。这是少数主动做了向后兼容的点。

### 5.2 state_path 字段补全

`Backend::new()` 第二处启动期补全：若 `config.state_path` 为 `None`（旧配置缺该字段）则填入 `default_state_path()` 并标记 `changed=true` 回写 (`backend/mod.rs:240`)。属字段补默认，非数据搬迁。

### 5.3 load-or-create（无 legacy 路径探测）

- **配置**：`Backend::new()` 仅在 config 路径不存在时新建内存默认 config（`SyncConfig::new`），**不会去任何旧路径**（如 `.codebaton` 或其它目录）寻找 legacy 配置来拷贝；路径硬编码 `~/.aisync/config.toml` (`backend/mod.rs:240`)。
- **配置加载**：`load_config()` 只做 TOML 反序列化 + `validate_config`，无版本判别或字段迁移分支 (`config.rs:345-356`)。
- **TLS 接收端身份**：`load_or_create_receiver_identity()` 是纯 create-if-missing —— 读到 `receiver.der` + `receiver.key.der` 则复用，否则新生成并写入，**不会从任何旧路径/旧文件名迁移**已有身份 (`backend/identity.rs:92`)。
- **ed25519 设备身份**：`ensure_local_ed25519_identity_in_store()` 同为 create-if-missing —— 按 `key=device:{uuid}:ed25519` 在 keychain 取，取不到才生成，**不会从旧 keychain service 名迁移到 `"CodeBaton"`** (`discovery/lib.rs:602-612`)。

### 5.4 全库迁移代码搜索结果

全代码库（所有 `*.rs`）grep `migrat` / `schema_version` 仅命中 BUG-007 注释中的 `legacy placeholder` 字样和无关的 receive-dir legacy fallback 注释；**不存在任何迁移函数、版本字段或一次性迁移标记文件** (`backend/identity.rs:33`)。

---

## 6. 已知风险与缺口

| # | 缺口 | 严重度 | 后果 | 引用 |
|---|------|:---:|------|------|
| R1 | **Keychain service 值改成 `CodeBaton`，与所有其他冻结的兼容标识符不一致** | **high** | 若旧 AISync 曾用不同 service 名（如 `aisync`/`AISync`）写过 ed25519 私钥，升级后去 `CodeBaton` 下读不到 → 设备身份静默重生成、配对失效，无任何迁移代码覆盖 | `discovery/lib.rs:25`、`discovery/lib.rs:602-612`、`discovery/lib.rs:24-25` |
| R2 | **磁盘 artifact 全部无 schema_version、无迁移框架** | **high** | 所有 artifact（config.toml/state.toml/paired_peers.json/*.jsonl/receiver.der/peers/*）靠 serde-default 与逐行 JSONL 容错隐式兼容。一旦未来某字段从**非容错路径**（`DeviceConfig`/`SyncState`/`PairingFile` 的 `from_str`/`from_slice` 严格解析）发生破坏性变更，旧 `~/.aisync` 数据将无声或报错失败，且无回滚机制 | `config.rs:11-13`、`sync/lib.rs:417-424`、`discovery/lib.rs:1544-1557` |
| R3 | **协议严格相等 = 无跨版本同步** | **high** | 两个 `PROTOCOL_VERSION` 不等的实例在 Hello 握手即硬失败、无降级；新增 `Message` 变体 / `MessageType` 编号会让旧 peer 在帧类型解析或 enum 反序列化阶段硬失败。升级必须同时升级所有节点 | `transport/lib.rs:3016-3023`、`transport/lib.rs:174`、`transport/lib.rs:297-298` |
| R4 | **两个 PROTOCOL_VERSION 语义分裂**（配对码用 discovery v1，握手用 transport v2） | **high** | 升版时需分别维护两个独立常量，极易遗漏；配对码字符串实际是 `aisync-pairing-v1:...` 而握手是 v2 | `discovery/lib.rs:26`、`transport/lib.rs:81`、`discovery/lib.rs:676-681` |
| R5 | **新增 MessageType 编号对旧 peer 是硬错误** | **high** | `MessageType::try_from` 对未知类型字节直接返回 `Err("unknown message type {other}")`；即便 TLS 与握手版本相同，旧 peer 收到新增类型消息会在帧类型解析阶段就失败 | `transport/lib.rs:174` |
| R6 | **配对存储平台间路径基名不一致**（Unix `~/.aisync` vs Windows `%APPDATA%\CodeBaton`） | medium | Windows 用户从旧 AISync 升级时，旧 `paired_peers.json` 若位于 `%APPDATA%\AISync` 则不会被读取（跨改名路径漂移），且无旧→新迁移 | `discovery/lib.rs:1596-1609` |
| R7 | **新增 Message enum 变体破坏旧 peer** | medium | `Message` 是 externally-tagged serde enum，旧 peer 的 `from_slice` 遇未知变体名反序列化失败，serde 不静默忽略未知 enum 变体 | `transport/lib.rs:297-298` |
| R8 | **字段级兼容依赖逐个 `#[serde(default)]` 标注，非全局保证** | medium | 并非所有 payload 字段都带 default（如 `TargetStatusResponsePayload` 的 `not_empty`/`file_count` 无 default）；新版给某无 default 字段改名或新增必填字段，旧 peer 反序列化会因缺字段报错 | `transport/lib.rs:229-230` |
| R9 | **validate 因本地路径不存在拒绝整个配置** | medium | 迁移到路径不同的新机器时，旧 config 的 enabled 项目/工作区本地路径若已不存在，`validate_config` 会让 `load_config` **整体失败** | `config.rs:386-392`、`config.rs:377-440` |
| R10 | **无 deny_unknown_fields + 无版本号 → 无法表达破坏性 schema 变更** | medium | 未来若需做无法用「缺字段填默认」表达的破坏性变更（字段语义翻转、重命名、嵌套改形），现有机制无任何挂钩点识别旧版本；且被重命名/废弃的旧字段会被静默丢弃而非告警，可能静默数据丢失 | `config.rs:11-40` |
| R11 | **ed25519 / TLS 身份均为 create-if-missing，无 legacy 迁移** | medium | 若旧密钥位于不同 keychain service 或旧文件名，启动时不会迁移，而是静默重新生成新身份 | `discovery/lib.rs:602-612`、`backend/identity.rs:92` |
| R12 | **数据目录未随改名迁移（设计选择，非纯缺陷）** | low | 当前正确（保旧名 = 原地升级数据连续）；但若**将来**改成 `.codebaton`，旧安装的 config/identity/cert 全部丢失且无拷贝逻辑 | `config.rs:369-375`、`backend/mod.rs:240` |
| R13 | **state.toml / paired_peers.json 严格解析（无字段级容错）** | low | 当前 `ProjectVersionState` / `DeviceInfo` 字段未变故安全；一旦增删必填字段，旧文件加载会整体 Err | `sync/lib.rs:417-424`、`discovery/lib.rs:1544-1557` |

> **未经验证的存疑点（仅供后续核查，非已确认事实）**：本次核查的所有 findings 均为 verified（无 rejected 项），故无被否决的存疑结论需在此标注。唯一需后续确认的是 R1/R11 中「旧 AISync 实际使用的 keychain service 名」—— 代码仅能证明新版用 `"CodeBaton"`，但旧版的历史 service 名未在当前代码中留存，需查阅旧版本源码或实测旧安装的 keychain 项才能定量评估升级失败影响面。

---

## 迁移机制总览图

```
┌──────────────────────────────────────────────────────────────────────────┐
│                  CodeBaton 版本迁移机制总览                                  │
└──────────────────────────────────────────────────────────────────────────┘

显式迁移框架:        ✗ 无 (无 schema_version / 无 migrate_*() / 无 marker 文件)
隐式兼容机制:        ① serde #[serde(default)]   ② JSONL 逐行 from_str::<Value>().ok()

┌─ 配置层 (config.rs) ───────────────────────────────────────────────────────┐
│  config.toml ──load_config()── toml::from_str ──validate_config()           │
│       缺字段 → serde default 兜底 (port 52000 / interval 30 / None / 空集)   │
│       [device] 必填; enabled 本地路径不存在 → 整个配置 Err  ⚠ R9             │
│       save 时全字段写回 = 隐式读时迁移落盘 (仅写操作)                        │
└────────────────────────────────────────────────────────────────────────────┘

┌─ 数据目录 ~/.aisync/ (旧名冻结, 原地升级) ─────────────────────────────────┐
│  config.toml ┐                                                              │
│  state.toml  ├─ TOML 严格解析 (state 无字段容错 ⚠ R13)                       │
│  paired_peers.json ─ JSON 严格 (Unix=.aisync / Win=CodeBaton ⚠ R6)          │
│  receiver.der / peers/*.der ─ DER, create-if-missing, 原地复用              │
│  *_history.jsonl (sync/chat/file) ─ 逐行容错, schema 漂移安全               │
│  logs/aisync.log ─ 追加                                                      │
└────────────────────────────────────────────────────────────────────────────┘
        ↕ 命名空间分裂 ⚠ R1
┌─ Keychain service = "CodeBaton" (唯一改新名的兼容关键项) ──────────────────┐
│  device:{uuid}:ed25519  create-if-missing, 旧 service 名不迁移 ⚠ R11        │
└────────────────────────────────────────────────────────────────────────────┘

┌─ 启动期 shim (Backend::new) ───────────────────────────────────────────────┐
│  ① BUG-007: device.name ∈ {"", 空白, "CodeBaton Device", "aisync-device"}   │
│             → default_device_name() 重算 + changed → save_config           │
│  ② state_path == None → default_state_path() + changed → save_config       │
│  ③ config 不存在 → SyncConfig::new (不探测任何 legacy 路径)                 │
└────────────────────────────────────────────────────────────────────────────┘

┌─ 协议层 (transport v2 / discovery v1, 两个独立常量 ⚠ R4) ──────────────────┐
│  Hello 双向交换 ── expect_hello ── 严格 == 检查                             │
│       version 不等 → "protocol version mismatch" → 断连, 无降级 ⚠ R3       │
│  帧 = [4B len][1B type][serde_json body]                                    │
│       未知 MessageType 编号 → Err  ⚠ R5                                     │
│       未知 enum 变体名 → 反序列化 Err  ⚠ R7                                  │
│       未知 struct 字段 → 忽略 (软兼容); 部分新字段带 default               │
│  契约: 不同 wire 版本 = hard fail, 升级必须全节点同步                       │
└────────────────────────────────────────────────────────────────────────────┘

改名策略: 表层换皮 (Cargo/productName/Keychain值/占位名) + 兼容层冻结旧名
          (.aisync / _aisync._tcp / aisync-pairing / aisync-receiver|client /
           com.aisync.app / AISYNC_*) ── 唯一例外 Keychain 值 ⚠ R1
```
