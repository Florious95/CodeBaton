# 03 - Identity Model

系统中有 7 类"身份"概念，分属不同层次、不同生命周期。

---

## 1. 设备身份（DeviceId / device_name）

### 1.1 DeviceId

| 属性 | 值 |
|------|---|
| 类型 | `DeviceId(Uuid)` -- UUIDv4 newtype | `core/lib.rs:54-61` |
| 生成 | `Uuid::new_v4()` -- 首次创建 `SyncConfig` 时调用 `DeviceId::new()` | `config.rs:46` |
| 存储 | `~/.aisync/config.toml` -> `device.id` | `config.rs:159-160` |
| 跨重启 | **稳定** -- 只要 config.toml 存在就不变 |
| 其他消费方 | mDNS TXT record `device_id` (`discovery/lib.rs:1008`)、Hello 握手隐含（通过 device_name）、所有 Payload 的 `device: DeviceInfo` 字段 |

### 1.2 device_name

| 属性 | 值 |
|------|---|
| 生成逻辑 | `default_device_name()` (`backend/identity.rs:19`)：优先 env `AISYNC_DEVICE_NAME` -> `system_hostname()` (gethostname syscall，`backend/identity.rs:44`) -> fallback `"CodeBaton Device"` |
| 存储 | `config.toml` -> `device.name` | `config.rs:161` |
| 跨重启 | **稳定**，但旧版 BUG-007 可能残留 placeholder；`Backend::new` 检测并修复 (`backend/mod.rs:240`) |
| 关键角色 | **peer_name 的来源** -- 配对时 `persist_peer_connection()` 以 `peer.name` 作为 `config.peers` 的 HashMap key (`backend/peers.rs:466`)。此 key 在后续所有操作中作为 peer_name 使用 |

### 1.3 mDNS 实例名

mDNS 注册时用 `"{sanitized_device_name}-{short_device_id}"` 作为 service instance name (`discovery/lib.rs:998-1002`)。
- `sanitize_instance_name()`: 仅保留 ASCII 字母数字和 `-_`，其余替换为 `-`，最长 40 字符 (`discovery/lib.rs:1623-1634`)
- `short_device_id()`: UUID 前 8 字符 (`discovery/lib.rs:1619-1621`)
- hostname 用 `dns_label()` 进一步清洗为小写字母数字 (`discovery/lib.rs:1637-1654`)

这是一个 **派生身份**，不持久化，仅影响 mDNS 服务注册名。

### 1.4 Tailscale 设备身份

`devices_from_tailscale_status_json()` 为 Tailscale 发现的 peer 生成 **确定性 DeviceId**：`deterministic_device_id(seed)` 对 seed 做 blake3 hash 取前 16 字节构造 UUID (`discovery/lib.rs:1577-1582`)。seed 格式为 `"tailscale:{dns_name_or_host}:{first_reachable_ip}"` (`discovery/lib.rs:792-799`)。

同理，手动 IP peer 的 seed 为 `"manual:{socket_addr}"` (`discovery/lib.rs:823`)。

**风险**: Tailscale peer 的 DNS name 或 IP 变化会导致 DeviceId 变化，丢失配对关系。

---

## 2. 配对身份（pairing_code / request_id / PairedPeer）

### 2.1 配对码 (pairing_code)

| 属性 | 值 |
|------|---|
| 生成 | `derive_pairing_code_with_nonce()` (`discovery/lib.rs:653-687`): blake3 hash of `"aisync-pairing-v{version}:{sorted_id_1}:{sorted_id_2}:{nonce}"` -> 取前 4 字节 -> `u32 % 1_000_000` -> 6 位数字 |
| 特性 | 顺序无关（两个 DeviceId 排序后拼接）、每次 request 不同（nonce = request_id） |
| 生命周期 | **单次配对有效** -- 120 秒过期 (`discovery/lib.rs:269`) |
| 存储 | 仅运行时：`PairingSession.code` (`backend/peers.rs:24`) + `PairingRequestPayload.code` (`transport/lib.rs:183`) |

### 2.2 request_id

| 属性 | 值 |
|------|---|
| 生成 | `new_pairing_request_id()`: 当前纳秒时间戳的 hex 编码 (`discovery/lib.rs:661-667`) |
| 用途 | 唯一标识一次配对请求；同时用作 pairing code 的 nonce |
| 生命周期 | 随 `PairingSession` 存活 (`backend/peers.rs:24`)，配对完成或超时后清除 |

### 2.3 PairingSession（运行时）

| 属性 | 值 |
|------|---|
| 存储 | `Inner.pairing_sessions: HashMap<DeviceId, PairingSession>` (`backend/mod.rs:202`) |
| 字段 | peer: DeviceInfo, request_id, code, expires_at_unix_secs, connection, inbound (`backend/peers.rs:24`) |
| 生命周期 | 创建于 `begin_pairing` / 收到 PairingRequest；消费于 `confirm_pairing`；**不持久化** |

### 2.4 PairedPeer（持久化）

| 属性 | 值 |
|------|---|
| 结构 | `{ device: DeviceInfo, public_key: String, paired_at_unix_secs: u64 }` (`discovery/lib.rs:72-77`) |
| 存储 | `~/.aisync/paired_peers.json` (`discovery/lib.rs:1596-1609`)，原子写入（.tmp rename, `discovery/lib.rs:1560-1575`） |
| 写入 | `confirm_pairing()` 插入 `SharedState.paired_peers` 后调用 `persist_pairings()` (`discovery/lib.rs:306-312`) |
| 清除 | `unpair()` (`discovery/lib.rs:347-367`) |

### 2.5 配对身份的双重存储

配对成功后，peer 身份被写入 **两个独立存储**（S1 BUG-4）：

1. **Discovery 侧**: `~/.aisync/paired_peers.json` -- 存 PairedPeer（含 Ed25519 public_key）
2. **Config 侧**: `~/.aisync/config.toml` -> `peers` HashMap -- 存 PeerConfig（含 endpoint + TLS cert path）

`persist_peer_connection()` (`backend/peers.rs:466`) 以 `peer.name`（device_name）为 key 写入 `config.peers`；Discovery 侧以 `DeviceId` 为 key 写入 `paired_peers.json`。两次写入独立，可能部分失败导致不一致。`paired_peers()` 方法在读取时合并两个来源 (`backend/peers.rs:102`)。

---

## 3. 项目 / 工作区身份（project_name / workspace_name）

### 3.1 project_name

| 属性 | 值 |
|------|---|
| 定义位置 | `ProjectConfig.name: String` (`config.rs:199`) |
| 命名规则 | 用户自由指定的字符串 |
| 唯一性约束 | `validate_config()` 检查同 config 内无重复 (`config.rs:378-385`) |
| 引用方 | `SyncState.projects` key (`lib.rs:411-413`)、`sync_snapshot()` 的 project_name 参数 (`config.rs:130-135`)、`project_mapping()` 查找 (`config.rs:95-127`)、history.jsonl 的 `projectId` 字段、auto_sync_gate_key 的 `name` 段 (`backend/auto_sync_gate.rs:93`) |
| 跨设备 | **不要求跨设备一致**。ProjectMappingRequestPayload 携带 `project_name` (`transport/lib.rs:193`)，对端 accept 后以相同名称存储 -- 若对端已有同名项目则冲突 |

### 3.2 workspace_name

| 属性 | 值 |
|------|---|
| 定义位置 | `WorkspaceConfig.name: String` (`config.rs:216`) |
| 命名规则 | 用户自由指定 |
| 唯一性约束 | `validate_config()` 检查同 config 内无重复 (`config.rs:399-406`) |
| 引用方 | auto_sync_gate_key 的 `name` 段、watcher HashMap key、history.jsonl 的 `workspaceName` 字段 |

### 3.3 workspace child name

| 属性 | 值 |
|------|---|
| 定义位置 | `WorkspaceChildConfig.name: String` (`config.rs:243`) |
| 来源 | `scan_workspace` 发现的子目录名 |
| 唯一性 | 在同一 workspace 内由文件系统保证唯一（目录名即身份） |

---

## 4. 会话身份（session_id / encoded_dir_name）

### 4.1 session_id

| 属性 | 值 |
|------|---|
| 来源 | Claude Code 的 JSONL 文件名（不含 `.jsonl` 后缀）：`path.file_stem()` (`claude_code.rs:344-348`) |
| 格式 | Claude Code 生成的 UUID 或类似标识符 |
| 唯一性 | 在同一 encoded_dir_name 下由文件系统保证唯一 |
| 用途 | `ParsedSession.session_id` (`claude_code.rs:90`)、写回文件名 `{session_id}.jsonl` (`claude_code.rs:278`)、core Session 信封的 `id` 字段 (`lib.rs:30`) |

### 4.2 encoded_dir_name（编码目录名）

| 属性 | 值 |
|------|---|
| 生成 | `claude_project_dir_name(path)` (`backend/session_stage.rs:623`): 将路径中非 `[a-zA-Z0-9\-_.]` 的字符替换为 `-` |
| 示例 | `/Users/alice/code/中文项目` -> `-Users-alice-code---` |
| 存储 | 磁盘目录名：`~/.claude/projects/<encoded_dir_name>/` |
| 关键特性 | **有损编码** -- 不同原始路径可能编码为相同目录名（编码冲突，G4） |
| 冲突检测 | `SessionIndex.conflicts()` (`claude_code.rs:171-180`) 检测多个不同 `original_project_path` 映射到同一 `encoded_dir_name` |

### 4.3 original_project_path

| 属性 | 值 |
|------|---|
| 来源 | JSONL 记录中首个出现的顶层 `cwd` 字段 (`claude_code.rs:362-368`) |
| 用途 | 路径重写的基准（X8）、session 同步时的 `dir_filter` 匹配 |
| 编码目录名 vs 原始路径 | 编码目录名用于磁盘布局和文件传输；原始路径用于路径重写和语义匹配 |

### 4.4 同步时的目录名重写

推送会话到对端时，`prepare_claude_session_sync()` (`backend/session_stage.rs:53`) 将 `encoded_dir_name` 重写为 `claude_project_dir_name(remote_code_dir)` -- 即用对端项目路径的编码目录名替换本地的编码目录名。这是因为两端路径不同，编码目录名也不同。

---

## 5. 同步身份（sync_snapshot key = peer_name）

### 5.1 SyncSnapshot 索引

| 属性 | 值 |
|------|---|
| 存储 | `ProjectConfig.sync_snapshots: HashMap<String, SyncSnapshot>` (`config.rs:210-211`) |
| key | **peer_name** -- 即 `config.peers` 的 key（= 对端 device_name） |
| value | `{ peer_last_known_hash, self_last_synced_hash }` (`config.rs:187-195`) -- blake3 hex |
| 写入 | `set_sync_snapshot(project_name, peer_name, snapshot)` (`config.rs:138-149`) |
| 读取 | `sync_snapshot(project_name, peer_name)` (`config.rs:130-135`) -- 脑裂检测前读取 |

### 5.2 auto_sync_gate_key

| 属性 | 值 |
|------|---|
| 格式 | `"{scope}:{name}:{peer}"` (`backend/auto_sync_gate.rs:93`) |
| scope | `"project"` 或 `"workspace"` |
| name | project_name 或 workspace_name |
| peer | peer_name |
| 存储 | `AUTO_SYNC_GATES: OnceLock<Mutex<HashMap<String, AutoSyncGate>>>` (`backend/auto_sync_gate.rs:57`) |

### 5.3 SESSION_BASELINE_SEEDS key

| 属性 | 值 |
|------|---|
| key | session 文件的磁盘路径（字符串） (`backend/auto_sync_gate.rs:61`) |
| value | `SessionBaseline { mtime, content_fingerprint, sync_fingerprint }` (`backend/auto_sync_gate.rs:47`) |

### 5.4 SyncState.projects key

| 属性 | 值 |
|------|---|
| key | project_name（= `ProjectConfig.name`） |
| 存储 | `~/.aisync/state.toml` -> `projects` HashMap (`lib.rs:411-413`) |
| value | `ProjectVersionState` -- version counters + fingerprints (`lib.rs:466-473`) |

### 5.5 peer_name 作为统一索引键

下表汇总所有以 peer_name 为 key 的映射：

| 位置 | key 来源 | value |
|------|---------|-------|
| `SyncConfig.peers` | device_name | PeerConfig (`config.rs:19`) |
| `ProjectConfig.peers` | device_name | PathBuf (remote_code_dir) (`config.rs:202`) |
| `ProjectConfig.sync_snapshots` | device_name | SyncSnapshot (`config.rs:211`) |
| `ClaudeConfig.peers` | device_name | PathBuf (remote_session_dir) (`config.rs:183`) |
| `WorkspaceConfig.peers` | device_name | PathBuf (remote_root) (`config.rs:228`) |
| `AUTO_SYNC_GATES` | `{scope}:{name}:{device_name}` | AutoSyncGate (`backend/auto_sync_gate.rs:57`) |

**所有这些 key 都依赖 peer.name（= device_name）的稳定性。**

---

## 6. TLS 身份（receiver cert / client cert）

### 6.1 Receiver 身份（服务端）

| 属性 | 值 |
|------|---|
| 生成 | `load_or_create_receiver_identity()` (`backend/identity.rs:92`): 若 `~/.aisync/receiver.der` + `receiver.key.der` 存在则加载，否则 `generate_tls_identity("aisync-receiver")` 新建 |
| 证书生成 | `rcgen::CertificateParams::new(["aisync-receiver"])` -> self-signed (`transport/lib.rs:2298-2311`) |
| 存储 | `~/.aisync/receiver.der` (DER 证书) + `~/.aisync/receiver.key.der` (PKCS8 私钥) (`backend/identity.rs:84`) |
| 跨重启 | **稳定** -- 持久化到磁盘，仅在文件缺失时重建 |
| 分发 | mDNS TXT record 中以 180 字节 hex 分块发布 (`discovery/lib.rs:1180-1187`)；配对时在 `PairingRequestPayload.receiver_cert_der` 中传递 (`transport/lib.rs:186`) |

### 6.2 Peer Receiver 证书（pin 对端）

| 属性 | 值 |
|------|---|
| 存储 | `~/.aisync/peers/{device_id}-receiver.der` (`backend/identity.rs:78`) |
| 写入 | `persist_peer_connection()` 在配对成功时写入 (`backend/peers.rs:466`) |
| 引用 | `PeerConfig.server_cert: Option<PathBuf>` (`config.rs:171`) 指向该 DER 文件 |
| 使用 | `control_connection_for_peer()` 读取 DER 构造 `TlsConfig.with_pinned_peer_cert()` (`backend/mod.rs:866`) |

### 6.3 Client 身份（发起连接方）

| 属性 | 值 |
|------|---|
| 生成 | **每次连接都新建** -- `generate_tls_identity("aisync-client")` (`backend/mod.rs:866`) |
| 存储 | 不持久化，纯内存，连接结束即丢弃 |
| 验证 | 服务端 `with_no_client_auth()` (`transport/lib.rs:3050`) -- **不验证客户端证书** |

### 6.4 TLS server_name

| 属性 | 值 |
|------|---|
| 默认值 | `"aisync-receiver"` (`discovery/lib.rs:55`, `backend/peers.rs:466`) |
| 存储 | `PeerConfig.server_name` (`config.rs:173`)、`DiscoveryConfig.server_name` (`discovery/lib.rs:36`) |
| 用途 | TLS SNI 字段；`PinnedPeerCertVerifier` 不检查 server_name (`transport/lib.rs:3098`)，仅做 DER 字节比对 |

### 6.5 TLS 信任模型

```
发起方 (client)                       接收方 (server)
  |                                     |
  | -- TCP connect ------------------>  |
  | -- TLS ClientHello (SNI) -------->  | uses receiver.{der,key.der}
  | <-- ServerCertificate -----------   |
  |                                     |
  | PinnedPeerCertVerifier:             | with_no_client_auth():
  |   cert_der == pinned? PASS          |   不验证客户端
  |   cert_der != pinned? REJECT        |
```

- **服务端身份**: 通过 pin 的 DER 证书字节比对验证 (`transport/lib.rs:3102-3108`)。不使用 CA 信任链。
- **客户端身份**: 不验证。任何拥有对端证书 DER 的客户端都可以连接。
- **无证书轮换**: 证书一旦创建就持久化，无过期时间（rcgen 默认 not_after 远未来），无主动轮换机制。

---

## 7. Ed25519 身份（配对认证）

| 属性 | 值 |
|------|---|
| 生成 | `ensure_local_ed25519_identity_in_store()` (`discovery/lib.rs:598-613`): 首次生成 `SigningKey::generate(&mut OsRng)`，持久化私钥 |
| 存储 | macOS Keychain，service=`"CodeBaton"`，key=`"device:{device_id.uuid}:ed25519"` (`discovery/lib.rs:694-696`) |
| 跨重启 | **稳定** -- 绑定 Keychain，随系统账户存在 |
| 轮换 | `rotate_local_ed25519_identity()` (`discovery/lib.rs:594-596`): 删旧建新 |
| 用途 | 配对流程中互换 public_key，存入 `PairedPeer.public_key` (`discovery/lib.rs:74`) |
| 实际使用 | 当前代码 `confirm_pairing()` 传入硬编码 `"gui-local-key"` / `"gui-peer-key"` (`backend/peers.rs:300`)，**未真正使用 Ed25519 做密钥交换**。Ed25519 身份基础设施已就绪但认证流程未启用。 |

---

## 8. 身份混淆与 Bug 高发区

### 8.1 peer_name 作为 key 的脆弱性（**高风险**）

**问题**: `config.peers` 以 `device_name` (= `peer.name`) 为 HashMap key (`backend/peers.rs:466`)。如果两台设备恰好有相同的 hostname（例如都是 "MacBook-Air"），配对第二台时会覆盖第一台的 PeerConfig 条目。

**涉及范围**: 所有以 peer_name 为 key 的映射（见 Section 5.5 表）都会受到影响，包括 project.peers 的 remote path 映射、sync_snapshots 的脑裂检测、claude_config.peers 的 session 目录映射。

**根因**: DeviceId (UUID) 是唯一的，但代码选择用 device_name 做 key 而非 DeviceId。`persist_peer_connection()` 用 `config.peers.entry(peer.name.clone())` (`backend/peers.rs:466`)，不做重名检测。

### 8.2 paired_peers.json vs config.peers 双写不一致（**中风险**）

**问题**: Discovery 层的 `paired_peers.json` 以 DeviceId 为 key，Config 层的 `config.peers` 以 device_name 为 key。两者独立写入 (`discovery/lib.rs:306-312` vs `backend/peers.rs:300`)，可能一写成功一写失败。

**后果**: `paired_peers()` 合并两个来源时可能看到不一致状态 -- 例如 discovery 认为已配对但 config 中无该 peer 条目（缺 endpoint/cert），导致推送失败。

### 8.3 encoded_dir_name 编码冲突（**中风险**）

**问题**: `claude_project_dir_name()` (`backend/session_stage.rs:623`) 将所有非 ASCII 字母数字字符替换为 `-`，这是有损编码。多个包含不同中文字符的路径会编码为相同的目录名。

**示例**: `/Users/alice/code/项目一` 和 `/Users/alice/code/项目二` 都编码为 `-Users-alice-code---`。

**后果**: 两个不同项目的 session 文件混在同一个编码目录中。`SessionIndex.conflicts()` 可以检测到此情况 (`claude_code.rs:171-180`)，但同步时如何处理取决于调用方。

### 8.4 TLS client 证书每次重建（**低风险但浪费**）

**问题**: `control_connection_for_peer()` 每次连接都调用 `generate_tls_identity("aisync-client")` (`backend/mod.rs:866`)，但服务端 `with_no_client_auth()` 根本不验证客户端证书。

**后果**: 无安全影响（因为不验证），但每次连接都做 RSA 密钥生成是不必要的计算开销。

### 8.5 Ed25519 配对认证未实际使用（**中风险**）

**问题**: Ed25519 身份基础设施完整（Keychain 存储、生成、轮换），但 `Backend::confirm_pairing()` 传入硬编码字符串 `"gui-local-key"` / `"gui-peer-key"` (`backend/peers.rs:300`)。`PairedPeer.public_key` 存的是这个假值而非真正的 Ed25519 公钥。

**后果**: 配对认证名存实亡 -- 任何知道对端 IP 和端口的设备都可以发起配对请求，只需用户在 UI 上确认 6 位数字码。Ed25519 密钥交换本应提供额外的身份证明层但未启用。

### 8.6 Tailscale DeviceId 不稳定（**低风险**）

**问题**: Tailscale 发现的 peer 使用 `deterministic_device_id(seed)` (`discovery/lib.rs:1577-1582`)，seed 包含 IP 地址。Tailscale 节点 IP 变化（重新加入 tailnet、IP 回收等）会导致 DeviceId 变化。

**后果**: 已配对的 Tailscale peer 在 IP 变化后被视为新设备，需要重新配对。对于使用 `paired_peers.json`（以 DeviceId 为 key）的流程影响较大。

### 8.7 project_name 跨设备碰撞（**低风险**）

**问题**: project_name 由发起配对的一方在 `ProjectMappingRequestPayload.project_name` 中指定 (`transport/lib.rs:193`)，接收方 accept 后以相同名称存储。如果接收方已有同名项目，`validate_config()` 会报 `"duplicate project mapping"` (`config.rs:381-385`)。

**后果**: 必须手动解决命名冲突。但实际使用中项目名通常由路径的最后一段决定，碰撞概率取决于用户习惯。

### 8.8 sync_snapshot peer_name 与 peer 重命名不同步（**中风险**）

**问题**: `sync_snapshots` 的 key 是 peer_name (`config.rs:211`)。如果用户修改了对端设备名（导致 config.peers 的 key 变化），旧 peer_name 下的 snapshot 不会被迁移到新 key。

**后果**: `sync_snapshot(project, new_peer_name)` 返回 None，等效于从未同步过 -- 跳过脑裂检测。这不会造成数据丢失（首次同步总是安全的），但用户不会收到预期的脑裂警告。

---

## 9. 身份关系图

```
                     DeviceId (UUID, 持久化在 config.toml)
                       |
         +-------------+-------------+
         |                           |
    device_name                 Ed25519 keypair
    (hostname)                  (Keychain, key="device:{uuid}:ed25519")
         |                           |
         |                     [未启用: confirm_pairing 传硬编码值]
         |
    peer_name = remote device_name
         |
    +----+----+----+----+----+
    |    |    |    |    |    |
 config  project  sync  claude  workspace  auto_sync
 .peers  .peers   _snap .peers  .peers     gate_key
 (key)   (key)    shot  (key)   (key)      (含key)
                  (key)

    TLS receiver identity
    (receiver.der + receiver.key.der, 持久化)
         |
    mDNS TXT record: receiver_cert_{0..n} (hex chunks)
         |
    persist_peer_connection -> peers/{uuid}-receiver.der
         |
    PinnedPeerCertVerifier: DER 字节比对

    Claude Code session identity:
    original_project_path ---[有损编码]--> encoded_dir_name
         |                                   |
    cwd 字段 (JSONL)                    磁盘目录名
                                        (~/.claude/projects/<encoded>/)
```
