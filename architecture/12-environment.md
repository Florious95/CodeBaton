# 12 - Install & Runtime Environment

How CodeBaton finds its binary, resolves all of its filesystem paths, reads its environment, is built/bundled/signed, and how those differ across DMG-installed vs `cargo run` vs `cargo tauri dev`. See `01-state-model.md §1` for the storage-layer view of these same paths; this document is the install/runtime-environment view.

---

## Executable & Resource Paths

### The binary does not locate anything relative to itself

CodeBaton never resolves any path relative to its own executable. There is no use of `std::env::current_exe`, no Tauri `path_resolver` / `resolve_resource` / `resource_dir` / `app.path()`, anywhere in the workspace (grep across all `*.rs` excluding `/target/` returns no matches). The only Tauri bundle macro used is `generate_context!()` inside `run()` (`lib.rs:18,124`). Consequence: **the location of the binary — `/Applications/CodeBaton.app/Contents/MacOS/codebaton` vs `target/debug/codebaton` vs the `cargo tauri dev` debug binary — is irrelevant to where CodeBaton reads and writes data.** Everything is anchored to `$HOME` (or an `AISYNC_*` env override).

There are also no bundled runtime resources to resolve: `bundle` in `tauri.conf.json` declares no `resources` key and no `externalBin` (`tauri.conf.json:32-45`); the bundle block carries only `active`, `targets`, `icon`, and `macOS` keys. Frontend assets come from `build.frontendDist = "dist"` (`tauri.conf.json:7`); in `cargo tauri dev` they are served from `build.devUrl = "http://localhost:1420"` (`tauri.conf.json:8`).

### Binary name vs product name

The Cargo binary is named `codebaton` (`[[bin]] name = "codebaton"` — `Cargo.toml:12-14`; `default-run = "codebaton"` — `Cargo.toml:6`). The library is `codebaton_app_lib` (`Cargo.toml:8-10`). The product/app/DMG name `CodeBaton` and the bundle id `com.aisync.app` come from `tauri.conf.json` (`tauri.conf.json:3,5`). The bundle id retains the legacy `aisync` namespace even though the product was renamed to CodeBaton (`tauri.conf.json:5`). The binary was renamed from `aisync-app` to `codebaton` specifically to fix the name shown in macOS TCC permission dialogs (`.claude/skills/clean-deploy/SKILL.md:15-19`).

### Home-directory resolution

Every path below hangs off a hand-rolled `home_dir()` that reads `HOME`, falling back to `USERPROFILE`. There is no `dirs` / `dirs_next` / `directories` crate dependency anywhere (grep over all `Cargo.toml` returns nothing). The helper is duplicated independently in four crates with identical semantics:

| Crate | `home_dir()` location |
|-------|----------------------|
| codebaton-app | `backend/mod.rs:819` (`HOME` else `USERPROFILE`) |
| codebaton-sync | `codebaton-sync/src/config.rs:472-476` (`HOME` else `USERPROFILE`) |
| codebaton-transport | `codebaton-transport/src/lib.rs:3280-3284` (`HOME` else `USERPROFILE`) |
| codebaton-discovery | inline `HOME` lookups at `codebaton-discovery/src/lib.rs:1479-1481,1604-1607` |

If both `HOME` and `USERPROFILE` are unset, `home_dir()` returns `None` and each call site degrades to a relative-path fallback (see below) rather than erroring.

### The `~/.aisync` config directory

The config dir is `$HOME/.aisync` (legacy name retained; the binary/product is `codebaton`). Path resolvers:

| Artifact | Default path | Resolver | Fallback if `HOME` unset |
|----------|--------------|----------|--------------------------|
| Config | `$HOME/.aisync/config.toml` | `default_config_path()` (`codebaton-sync/src/config.rs:369-371`) | `Backend::new` substitutes relative `.aisync/config.toml` (`backend/mod.rs:240`) |
| Sync state | `$HOME/.aisync/state.toml` | `default_state_path()` (`codebaton-sync/src/config.rs:373-375`) | relative `state.toml` sibling of config, ultimately `.aisync-state.toml` (`backend/mod.rs:240`, `config.rs` `state_path()`) |
| Sync history | `$HOME/.aisync/history.jsonl` | `config_path.with_file_name("history.jsonl")` (`backend/history.rs:23`) | sibling of config_path |
| Chat history | `$HOME/.aisync/chat_history.jsonl` | `config_path.with_file_name(...)` (`backend/messaging.rs:47`) | sibling of config_path |
| File-transfer history | `$HOME/.aisync/file_transfer_history.jsonl` | `with_file_name(...)` (`backend/file_transfer.rs:310`) | sibling of config_path |
| Logs | `$HOME/.aisync/logs/aisync.log` | `log_file_path()` (`backend/events.rs:155`) / `discovery_log_file_path()` (`codebaton-discovery/src/lib.rs:1475-1483`) | none (returns `None`, file sink disabled) |

When `HOME` is unset, `Backend::new()` loads config from the relative path `.aisync/config.toml` (cwd-relative), and if that file does not exist it builds an in-memory default `SyncConfig::new(default_device_name())` (`backend/mod.rs:240`).

The startup banner is written to the file sink before the config even loads, so qa can confirm logging works even when the DMG is launched via `open -a` (stderr → /dev/null): `"[codebaton-app] starting — logs at ~/.aisync/logs/aisync.log"` (`lib.rs:19-21`). App and discovery both write to the **same** default log file, so their lines interleave (`backend/events.rs:155`, `codebaton-discovery/src/lib.rs:1476-1482`).

### TLS cert / key placement

The local receiver's self-signed identity is written as siblings of `config.toml`:

| Artifact | Path | Resolver |
|----------|------|----------|
| Receiver cert | `$HOME/.aisync/receiver.der` | `receiver_cert_path()` = `config_path.with_file_name("receiver.der")` (`backend/identity.rs:84`) |
| Receiver private key | `$HOME/.aisync/receiver.key.der` | `receiver_key_path()` = `config_path.with_file_name("receiver.key.der")` (`backend/identity.rs:88`) |
| Pinned peer cert | `$HOME/.aisync/peers/<peer_id>-receiver.der` | `peer_receiver_cert_path()` (`backend/identity.rs:78`) |
| Paired-peers store | `$HOME/.aisync/paired_peers.json` (macOS) / `%APPDATA%/CodeBaton/paired_peers.json` (Windows) | `default_pairing_store_path()` (`codebaton-discovery/src/lib.rs:1596-1610`) |

`load_or_create_receiver_identity()` reads the cert+key if both exist, else calls `generate_tls_identity("aisync-receiver")` and `fs::write`s both, `create_dir_all`-ing the parent first (`backend/identity.rs:92`). `start_serve_daemon` re-writes the cert via `fs::write(&cert_out, &identity.cert_der)` on every daemon start (`backend/serve.rs:60`). The Windows pairing-store branch falls back to `.` (cwd) when `APPDATA` is unset; the macOS branch falls back to `.` when `HOME` is unset (`codebaton-discovery/src/lib.rs:1597-1609`).

### Staging / temp / trash dirs

Staging dirs are placed next to the data they touch, not under `~/.aisync` (except session staging, which is config-sibling):

| Stage | Location | Resolver |
|-------|----------|----------|
| Incoming-push landing | `$HOME/.aisync/received/` (default) | `receive_root()` (`backend/mod.rs:169`); created by `start_serve_daemon` (`backend/serve.rs:60`) |
| Transport file staging | `<target parent>/.aisync-staging-<unix_nanos>` | `prepare_staging()` (`codebaton-transport/src/lib.rs:3245-3254`) |
| Transport trash/recycle | `<target_dir>/.aisync-trash/<timestamp>/` (7-day retention) | `codebaton-transport/src/lib.rs:3489-3517` |
| Sync code stage | `<target>/.aisync-stage-code` | `unique_sibling()` (`codebaton-sync/src/lib.rs:277`) |
| Sync session stage | `<target>/.aisync-stage-session` | `unique_staging_path()` (`codebaton-sync/src/lib.rs:287-291`) |
| Sync backup | `<target>/.aisync-backup` | `unique_sibling()` (`codebaton-sync/src/lib.rs:631`) |
| Backend session convert stage | `$HOME/.aisync/.aisync-session-stage-<nanos>` (and `.aisync-codex-session-stage-*`, `.aisync-workspace-session-stage-*`) | `config_path.with_file_name(...)` (`backend/session_stage.rs:53`; codex variant `backend/session_stage.rs:190`; workspace variant `backend/session_stage.rs:293`) |

### AI-tool source dirs and Downloads landing

| Artifact | Default | Resolver |
|----------|---------|----------|
| Claude projects source | `$HOME/.claude/projects` (or `config.claude_config.local/projects`) | `local_claude_projects_root()` (`backend/claude_paths.rs:24`); `home_projects = home_dir()?.join(".claude").join("projects")` (`backend/mod.rs:819` `home_dir`) |
| Codex sessions source | `$HOME/.codex/sessions` (only if `is_dir()`) | `local_codex_sessions_dir()` (`backend/mod.rs:2179`) |
| File-transfer download dir | `$HOME/Downloads/CodeBaton` | `default_downloads_receive_dir()` (`backend/mod.rs:918`) |

### No hardcoded absolute paths

There are no hardcoded absolute filesystem paths (`/Applications/...`, `/Users/...`) in non-test source. All runtime paths are `$HOME`-relative or env-overridable; absolute paths appear only in `#[cfg(test)]` temp-dir tests.

---

## Environment Variables

CodeBaton reads no `RUST_LOG`, `RUST_BACKTRACE`, `TMPDIR`, or any HTTP-proxy variable (`http_proxy` / `https_proxy` / `no_proxy` / `all_proxy`). Logging is a hand-rolled file/`eprintln!` sink with no env-driven level (`backend/events.rs:138`); there is no `env_logger` / `tracing-subscriber`. There is no HTTP client (`reqwest` / `hyper` / `ureq`) in the workspace, so proxy variables have no effect — all networking is raw TCP+TLS (`codebaton-transport`), mDNS (`codebaton-discovery`), and a `tailscale` subprocess (`codebaton-discovery/src/lib.rs` `Command::new("tailscale")`).

| Var | Reader (`file:line`) | Default if unset | Effect |
|-----|----------------------|------------------|--------|
| `HOME` | `backend/mod.rs:819`; `codebaton-sync/src/config.rs:472-476`; `codebaton-transport/src/lib.rs:3280-3284`; `codebaton-discovery/src/lib.rs:1604-1607` | falls back to `USERPROFILE`, then `None` | Root of all `~/.aisync` paths, `~/.claude`, `~/.codex`, `~/Downloads/CodeBaton`. Unset → relative-path fallbacks, features degrade. |
| `USERPROFILE` | same `home_dir()` helpers as above | `None` | Windows fallback for `HOME`. |
| `AISYNC_RECEIVE_DIR` | `receive_root()` (`backend/mod.rs:169`) | `<config dir>/received/` | Legacy fallback for incoming-push landing dir. Overridden by `config.receive_dir_override` (preferred, per-instance, parallel-safe — `codebaton-sync/src/config.rs:34-37`). |
| `AISYNC_LOG_FILE` | `log_file_path()` (`backend/events.rs:155`); `discovery_log_file_path()` (`codebaton-discovery/src/lib.rs:1476`) | `$HOME/.aisync/logs/aisync.log` | Redirects the (single, shared) log file for both app and discovery. |
| `AISYNC_CODEX_SESSIONS_DIR` | `local_codex_sessions_dir()` (`backend/mod.rs:2179`) | `$HOME/.codex/sessions` | Overrides Codex sessions source dir. Even when set, the path must exist as a directory or the function returns `None` (`backend/mod.rs:2179`). |
| `AISYNC_DEVICE_NAME` | `default_device_name()` (`backend/identity.rs:19`) | system hostname, then `"CodeBaton Device"` placeholder | Overrides the auto-derived device display name on first run. Empty/whitespace value is ignored. |
| `APPDATA` | `default_pairing_store_path()` (`codebaton-discovery/src/lib.rs:1598`) | `.` (cwd) | Windows-only: selects the `%APPDATA%/CodeBaton/paired_peers.json` dir. |
| `USER` | `commands.rs:228-230` | empty string (also tries `USERNAME`) | Display-only: populates `LocalInfoDto.user` for the UI. No runtime behavior depends on it. |
| `USERNAME` | `commands.rs:229` | empty string | Fallback for `USER` (display-only). |
| `COMPUTERNAME` | `system_hostname()` Windows branch (`backend/identity.rs:44`); CLI (`codebaton-cli/src/main.rs:759`) | `None` | Windows hostname source. On unix the app uses the `gethostname(2)` syscall, not an env var (`backend/identity.rs:44`). |
| `HOSTNAME` | CLI `default_device_name()` (`codebaton-cli/src/main.rs:758`) | falls back to `COMPUTERNAME`, then `"aisync-device"` | CLI-only device-name source. Independent of the GUI app's hostname logic. |

Notes:
- `AUTO_SYNC_COOLDOWN_OVERRIDE` is **not** an env var — it is a process-global `OnceLock<Duration>` set only by the test hook `set_auto_sync_cooldown_for_test()`; the production cooldown is `DEFAULT_AUTO_SYNC_COOLDOWN = 90s` (`backend/auto_sync_gate.rs:23`).
- The serve port is **not** env-controllable. `default_receive_port() = 52000` (`codebaton-sync/src/config.rs:456-458`) is a config field (`SyncConfig.receive_port`, serde default) persisted in `~/.aisync/config.toml`; the daemon binds `0.0.0.0:<port>` and writes the actually-bound port back to `config.receive_port` (`backend/serve.rs:60`, `backend/serve.rs:60`).
- The only compile-time `env!()` reads are `env!("CARGO_BIN_EXE_codebaton-cli")` in CLI integration tests (`codebaton-cli/tests/commands.rs:9`); none drive production config.

---

## Build & Bundle Pipeline

### `build.rs`

`codebaton-app/build.rs` is a 3-line shim: `fn main() { tauri_build::build(); }` (`build.rs:1-3`). It reads no env vars and emits no `cargo:rustc-env` or feature config; all Tauri build-time codegen comes from `tauri-build` (pinned `=2.6.3`, no features — `Cargo.toml:17`).

There are no application-defined Cargo `[features]` tables. The only `features` entries in any `Cargo.toml` select third-party dependency features (`tauri` with `tray-icon` + `image-png` — `Cargo.toml:30`; `tauri-plugin-notification =2.3.3` — `Cargo.toml:31`; `tauri-plugin-dialog 2` — `Cargo.toml:32`). The only conditional compilation is platform `cfg` (`cfg!(windows)`, `#[cfg(unix)]`, `#[cfg(test)]`), never feature flags.

### Tauri bundle config

`tauri.conf.json` bundle block (`tauri.conf.json:32-45`): `active = true`, `targets = "all"` (builds both `.app` and `.dmg`), an `icon` array (`icons/32x32.png`, `128x128.png`, `128x128@2x.png`, `icon.icns`, `icon.ico` — `tauri.conf.json:35-41`), and `macOS.signingIdentity = "-"` (`tauri.conf.json:42-44`). `plugins` is `{}` (`tauri.conf.json:46`) and `app.security.csp` is `null` (`tauri.conf.json:28-30`).

### Tauri capabilities

`capabilities/default.json` grants the `main` window only: `core:default`, window show/hide/set-focus/close, `notification:default`, `dialog:default`, `dialog:allow-open` (`capabilities/default.json:6-15`). No `fs` / `shell` / `http` plugin permissions — all file access goes through the custom Rust backend, not the Tauri fs plugin.

### DMG post-processing (`hide-dmg-volicon.sh`)

After Tauri builds the DMG, `codebaton-app/scripts/hide-dmg-volicon.sh <dmg>` re-masters the image so no phantom volume icon appears in the Finder install window (BUG 331 / ISS-014). Tauri's `bundle_dmg.sh` drops a `.VolumeIcon.icns` for a custom disk icon; setting the macOS hidden flag is unreliable when the user has "show hidden files" on, so the script deletes the file outright (`hide-dmg-volicon.sh:30-31`). It also deletes `.DS_Store`, because Tauri baked an icon-layout entry for `.VolumeIcon.icns` into it and Finder otherwise renders a stale top-left icon (`hide-dmg-volicon.sh:32-37`). Flow: `hdiutil convert` UDZO→UDRW (`:25`), attach to a temp mountpoint (`:28`), delete `.VolumeIcon.icns` (`:31`) and `.DS_Store` (`:37`), `SetFile -a c` to clear the volume's custom-icon FinderInfo bit (`:39`), defensively remove `.fseventsd`/`.background`/`.Trashes`/`.Spotlight-V100` (`:41-43`), detach (`:45`), then `hdiutil convert` back UDRW→UDZO `zlib-level=9` overwriting the shipped DMG (`:49-50`). This step is invoked by CI after the build (`.github/workflows/build-dmg.yml:47-51`).

### CI

`.github/workflows/build-dmg.yml` builds on `macos-14` for `aarch64`: `cargo tauri build --target aarch64-apple-darwin` (`:42`) with env `TAURI_SIGNING_PRIVATE_KEY: ''` (`:44`) — that is the Tauri **updater** signing key (empty = updater signing disabled), **not** Apple codesign. It runs `hide-dmg-volicon.sh` (`:47-51`), uploads the artifact (`:56`), and on `v*` tags publishes a GitHub Release via `softprops/action-gh-release` (`:59-63`). No Apple signing/notarization steps. **Known bug:** the workflow still references the old `aisync-app` directory at `working-directory` (`:34,:41`) and in the script path (`:49-50`), but the directory was renamed to `codebaton-app`, so CI would currently fail. The `clean-deploy` skill has the same stale path (`.claude/skills/clean-deploy/SKILL.md:19-20` says `bash aisync-app/scripts/...`; the script is actually at `codebaton-app/scripts/hide-dmg-volicon.sh`).

---

## Code Signing & Gatekeeper

### Ad-hoc signing only

The sole macOS signing config is `bundle.macOS.signingIdentity = "-"` (`tauri.conf.json:42-44`). `"-"` is the literal `codesign` argument for **ad-hoc** (self-signed, no certificate) signing: it produces a valid `_CodeSignature/CodeResources` keyed by cdhash, but no Developer ID trust chain. There is no `providerShortName`, no `entitlements`, no `hardenedRuntime`, no notarization config — `bundle.macOS` has only the one key.

### No notarization, no entitlements, no hardened runtime

There is no Apple Developer certificate, no notarization, no `*.entitlements` file, and no `Info.plist` override anywhere in the project (a `find` for `*.entitlements` / `Info.plist` under `codebaton-app` excluding `target` returns nothing). No `hardenedRuntime` key is present (notarization would require it), and no App Sandbox entitlement, so the installed app is **unsandboxed**. Absence of an entitlements file is the only evidence that sandbox/hardened-runtime are off — neither is explicitly declared "disabled".

### Gatekeeper / quarantine behavior

An ad-hoc signature is not from a recognized authority, so `spctl` Gatekeeper assessment fails for a downloaded app. A DMG fetched via browser / AirDrop / Mail gets the `com.apple.quarantine` extended attribute applied by the downloading agent; on first launch macOS shows the "unidentified developer" (or "damaged") block, and the user must **right-click → Open** (or run `xattr -dr com.apple.quarantine` / `spctl` override) to run it. There is **no quarantine-stripping step anywhere in the repo** — no `xattr -dr com.apple.quarantine`, no `spctl` call — so this first-launch UX is inferred from the ad-hoc-only signing state plus standard macOS behavior, not documented in-repo.

### TCC implications of ad-hoc signing

CodeBaton calls **no** TCC-gated API: no `AXIsProcessTrusted`, no accessibility / screen-capture / automation / full-disk code in the Rust backend (grep for `AXIsProcessTrusted` / `kTCC` / `TCCService` / `NSAppleEvents` / `ScreenCapture` / `requestAccess` in `codebaton-app/src/` returns nothing). Its only OS-permission surface is the notification + dialog plugins (`capabilities/default.json:12-14`). So ad-hoc signing's main TCC consequence is **identity stability, not a permission wall**: macOS keys TCC grants by code-signing identity (cdhash for ad-hoc), so a notification-permission grant does not persist across reinstalls of a freshly-rebuilt binary whose cdhash changed. (This persistence behavior is inferred from how macOS TCC keys grants, not stated in the codebase.) The binary rename to `codebaton` was done to fix the **display name** shown in TCC dialogs (`.claude/skills/clean-deploy/SKILL.md:15-19`).

---

## Install-Mode Behavioral Differences

CodeBaton has **zero** code that branches on its own binary path (no `current_exe` / `resource_dir`). All install-mode differences are therefore environmental — the value of `HOME`, the `AISYNC_*` overrides, and frontend asset source — never a hardcoded `/Applications` path.

| Aspect | DMG-installed (`/Applications/CodeBaton.app`) | `cargo run` (release/debug from `target/`) | `cargo tauri dev` |
|--------|----------------------------------------------|--------------------------------------------|-------------------|
| App wrapper | `.app` bundle with `_CodeSignature/CodeResources` (ad-hoc) | bare binary, no `.app`, no `CodeResources` | bare debug binary, no `.app` |
| Signing | ad-hoc (`signingIdentity "-"`, `tauri.conf.json:42-44`) | unsigned (no codesign step) | unsigned |
| Gatekeeper assessment | runs (`spctl`); fails for downloaded app → right-click Open | none (run directly) | none |
| Quarantine xattr | applied if DMG was browser-downloaded; **not** applied by `clean-deploy`'s `cp -R` from a locally-mounted DMG (`.claude/skills/clean-deploy/SKILL.md:42-46`) | n/a | n/a |
| Frontend assets | `frontendDist = "dist"` baked into bundle (`tauri.conf.json:7`) | `dist/` on disk | `devUrl = http://localhost:1420` (`tauri.conf.json:8`), Vite dev server |
| Config/data paths | `$HOME/.aisync/...` — identical (HOME-anchored) | identical | identical |
| TCC identity | stable per cdhash; grant survives until rebuild | changes every rebuild → grants don't persist | changes every rebuild |
| Sandbox | unsandboxed (no entitlements) | unsandboxed | unsandboxed |

The `clean-deploy` flow installs via `hdiutil attach` + `cp -R` from a locally-mounted DMG into `/Applications` (`.claude/skills/clean-deploy/SKILL.md:42-46`); because the source is a mounted volume rather than a browser download, the quarantine xattr is generally not set, which sidesteps the first-launch Gatekeeper block on the dev/test Macs. An end user who downloads the same DMG **will** hit the quarantine prompt. (The cp-avoids-quarantine claim is standard macOS behavior, not an explicit repo artifact.)

---

## First-Launch Requirements

Checklist for the app to come up fully functional, in dependency order:

1. **`HOME` (or `USERPROFILE`) is set.** Everything is anchored here (`backend/mod.rs:819`). Unset → config loads from cwd-relative `.aisync/config.toml`, logs disabled, most features degrade.
2. **`~/.aisync/` is writable.** `Backend::new()` creates config/state lazily; `start_serve_daemon` `create_dir_all`s `~/.aisync/received/` and writes `receiver.der` (`backend/serve.rs:60`). A write failure aborts the serve daemon (returns `None`, app still runs but cannot receive — `backend/serve.rs:60`).
3. **Gatekeeper clearance (DMG install only).** Ad-hoc signature means a browser-downloaded DMG needs right-click → Open or quarantine removal before first launch (`tauri.conf.json:42-44`). `clean-deploy`'s local `cp -R` install sidesteps this.
4. **TCP port 52000 bindable on `0.0.0.0` (for *receiving*).** `default_receive_port() = 52000` (`codebaton-sync/src/config.rs:456-458`); bind failure (e.g. port in use) is **non-fatal** — `start_serve_daemon` returns `None`, the UI runs, but the device cannot receive pushes or be paired-to (`backend/serve.rs:60`, comment at `backend/serve.rs:60`). The actually-bound port is persisted back to `config.receive_port` (`backend/serve.rs:60`) and advertised over mDNS as `_aisync._tcp.local.` (`codebaton-discovery/src/lib.rs:24`; `DiscoveryConfig::new(name, config.receive_port)` at `backend/serve.rs:60`).
5. **macOS Keychain access (for pairing keys).** Keyring entries are stored under service name `"CodeBaton"` (`codebaton-discovery/src/lib.rs:25,690`).
6. **Notification permission (optional).** Granted via TCC on first notification; non-blocking. Grant is keyed by code-signing identity, so it does not survive a rebuild with a new cdhash.
7. **AI-tool source dirs (optional, for sync content).** `~/.claude/projects` and `~/.codex/sessions` are read only if they exist as directories (`backend/claude_paths.rs:24` `local_claude_projects_root`, `backend/mod.rs:2179` `local_codex_sessions_dir`); absent → those session sources are simply empty, no error.

No first-launch step requires network reachability beyond the LAN/Tailscale transport: no HTTP client, no proxy handling, no notarization callout.
