# aisync-app — AISync 桌面应用 (Tauri v2)

Tauri v2 + React + TypeScript (Vite) 实现的 AISync 桌面 GUI。实现了 ui-design.md
中定义的全部页面 (P1–P3)、弹窗 (D1–D12)、系统托盘、底部状态栏、键盘快捷键与系统通知。

## 运行

需要联网环境（能访问 npm registry 与 crates.io）：

```bash
cd aisync-app
npm install          # 安装 react / vite / @tauri-apps/* / lucide-react
npm run tauri dev    # 启动应用（Vite dev server :1420 + Rust 后端）
```

> 注意：在受限沙箱中 npm registry / crates.io 可能被代理拦截（403），此时无法
> `npm install`。Rust 侧依赖（tauri 2.11.3 等）若已在 `~/.cargo` 缓存中可
> `cargo build --offline -p aisync-app` 离线验证后端编译。

构建发布版：`npm run tauri build`。

## 结构

```
aisync-app/
├── Cargo.toml            # Tauri 二进制 crate（依赖 aisync-core/sync/discovery）
├── tauri.conf.json       # 窗口 960×640、深色主题、bundle 配置
├── build.rs              # tauri-build
├── capabilities/         # IPC 权限（窗口控制 + notification）
├── icons/                # 应用 / 托盘图标
├── src/                  # Rust 后端
│   ├── main.rs
│   ├── lib.rs            # Builder：注册 commands、托盘、窗口事件（最小化到托盘）
│   ├── dto.rs            # IPC 数据结构（与 ui/types.ts 一一对应）
│   ├── state.rs          # 应用状态（当前为 mock 数据，见下文「接入真实后端」）
│   ├── commands.rs       # 所有 #[tauri::command]
│   └── tray.rs           # 系统托盘（X5：状态图标 + 暂停自动同步）
└── ui/                   # React 前端
    ├── main.tsx, App.tsx
    ├── store.tsx         # 全局状态 + 弹窗路由 + 事件订阅
    ├── ipc.ts            # invoke 封装（非 Tauri 环境优雅降级）
    ├── types.ts          # TS 类型（镜像 dto.rs）
    ├── Sidebar.tsx       # 设备面板（T11.2）
    ├── StatusBar.tsx     # 底部状态栏（空闲/同步中/冲突）
    ├── ProjectCard.tsx   # 项目卡片（可展开详情）
    ├── dialogs.tsx       # D1–D12 全部弹窗
    ├── shortcuts.ts      # 键盘快捷键（§10）
    ├── notifications.ts  # 系统通知（§9）
    └── pages/            # P1 Overview / P2 PeerDetail / P3 Settings
```

## 落地的 Phase 2→3 审查项

- **G7** 路径重写 debug 日志可查看 → D10 路径重写报告弹窗
  (`get_rewrite_report` + `RewriteReportDialog`)，区分「已重写 / 已跳过(低置信度)」。
- **G6** 敏感文件显式 include 需确认 → D6 批量同步确认弹窗中，匹配敏感模式的
  文件单列且默认不勾选，必须显式勾选「包含此文件」(`BatchPlan.sensitiveFiles`)。
- **X5** 系统托盘可见、可暂停自动同步 → `tray.rs` 托盘菜单「暂停所有自动同步」，
  状态在托盘 tooltip 与底部状态栏同步反映 (`set_auto_sync_paused`)。

## 真实后端接线（已完成）

`src/backend.rs` 的 `Backend` 持有真实的 `MdnsDiscoverer` 与 `SyncConfig`，操作类
command 全部走真实实现：

- `start_sync` → `Backend::run_sync` → 真实 `TcpTransporter::connect_to_peer()` /
  `ReceiveService`（对端 `aisync serve` 负责扫描 / 差异 / 暂存 / 原子提交）
- `begin/confirm_pairing` / `unpair` → 真实 `MdnsDiscoverer::begin_pairing` /
  `confirm_pairing` / `unpair`（含配对码派生）
- `scan_workspace` → 第一级子目录真实扫描，按 peer workspace 映射匹配
- `get_batch_plan` → 真实 `scan_sensitive_files`（不再硬编码文件数）
- `add_project` → 写入真实 `SyncConfig` 并持久化
- 会话路径重写能力保留在 `aisync-session`；当前 TCP push 先传输项目代码目录

**G6 敏感文件门控**（端到端）：`startSync` 携带 `confirmedSensitive` 列表 →
`Backend::run_sync` 用 `scan_sensitive_files` 找出敏感文件，把**未确认**的按精确相对
路径加入排除集（默认不同步）。集成测试 `tests/sync_g6.rs` 覆盖：`.env.local` 可被
识别为敏感文件；配置 peer endpoint 后，sync 通过 loopback TCP 写入 receiver 目录。

**当前限制**：`pull` 仍缺少远端控制通道，需由远端运行 `aisync send` 或后续控制协议触发。
剩余少量展示字段（同步历史文本、AI 工具会话数、冲突文件列表）仍由
`AppState` 种子数据填充，随后端结构化记录增长而收敛（`commands.rs` 已标注 seam）。

## 测试

```bash
cargo test -p aisync-app          # tests/sync_g6.rs：TCP endpoint + G6 sensitive scan
cargo build --workspace           # 全工作区编译
```
