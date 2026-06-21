// Thin wrapper over Tauri IPC. All backend calls go through `call()`.
//
// When running outside a Tauri webview (e.g. `vite` in a plain browser for
// quick UI iteration), the Tauri globals are absent; `call()` then throws a
// clear error and `listen()` becomes a no-op. Inside the app, these proxy to
// the Rust commands in codebaton-app/src/commands.rs.

import type {
  BatchPlan,
  Conflict,
  FileTransferRequest,
  FileTransferRecord,
  PendingFileTransfer,
  LocalInfo,
  Overview,
  Pairing,
  Peer,
  Project,
  ProjectMappingRequest,
  RewriteReport,
  ScannedChild,
  Settings,
  StatusBar,
  SyncHistoryEntry,
  TextMessage,
  Workspace,
  WorkspaceMappingRequest,
} from "./types";

function inTauri(): boolean {
  return typeof (window as any).__TAURI_INTERNALS__ !== "undefined";
}

async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  if (!inTauri()) {
    throw new Error(`IPC '${cmd}' unavailable outside the Tauri app`);
  }
  const { invoke } = await import("@tauri-apps/api/core");
  return invoke<T>(cmd, args);
}

export async function listen<T>(
  event: string,
  handler: (payload: T) => void,
): Promise<() => void> {
  if (!inTauri()) return () => {};
  const { listen: tauriListen } = await import("@tauri-apps/api/event");
  const un = await tauriListen<T>(event, (e) => handler(e.payload));
  return un;
}

/**
 * Write a UI breadcrumb to the backend log file (~/.aisync/logs/aisync.log).
 *
 * Fire-and-forget: also mirrors to the webview console. Used to make the
 * frontend side of flows (e.g. pairing) visible in the same file as backend
 * logs, since console output is invisible in a release DMG.
 */
export function uiLog(message: string): void {
  console.log("[ui]", message);
  if (inTauri()) {
    // Don't await — logging must never block or throw into the UI path.
    void call<void>("log_event", { message }).catch(() => {});
  }
}

export const ipc = {
  inTauri,
  uiLog: (message: string) => uiLog(message),
  openPath: (path: string) => call<void>("open_path", { path }),
  getOverview: () => call<Overview>("get_overview"),
  getPeers: () => call<Peer[]>("get_peers"),
  getPeerDetail: (peerId: string) =>
    call<[Peer, Project[], SyncHistoryEntry[], Workspace[]]>("get_peer_detail", { peerId }),
  getSettings: () => call<Settings>("get_settings"),
  saveSettings: (settings: Settings) => call<void>("save_settings", { settings }),
  getStatusBar: () => call<StatusBar>("get_status_bar"),
  isOnboarded: () => call<boolean>("is_onboarded"),
  getLocalInfo: () => call<LocalInfo>("get_local_info"),
  completeOnboarding: (deviceName: string) =>
    call<void>("complete_onboarding", { deviceName }),

  beginPairing: (peerId: string) => call<Pairing>("begin_pairing", { peerId }),
  pendingPairingRequest: () => call<Pairing | null>("pending_pairing_request"),
  pendingProjectMappingRequest: () =>
    call<ProjectMappingRequest | null>("pending_project_mapping_request"),
  confirmProjectMappingRequest: (requestId: string, localDir: string) =>
    call<void>("confirm_project_mapping_request", { requestId, localDir }),
  pollProjectMappingAcks: () => call<number>("poll_project_mapping_acks"),
  pendingWorkspaceMappingRequest: () =>
    call<WorkspaceMappingRequest | null>("pending_workspace_mapping_request"),
  confirmWorkspaceMappingRequest: (requestId: string, localRoot: string) =>
    call<void>("confirm_workspace_mapping_request", { requestId, localRoot }),
  pollWorkspaceMappingAcks: () => call<number>("poll_workspace_mapping_acks"),
  pendingTextMessage: () => call<TextMessage | null>("pending_text_message"),
  sendTextMessage: (peerName: string, content: string) =>
    call<void>("send_text_message", { peerName, content }),
  textMessages: (peerName: string) =>
    call<{ senderName: string; content: string; timestamp: number; peerName?: string; mine?: boolean }[]>(
      "text_messages",
      { peerName },
    ),
  requestFileTransfer: (peerName: string, path: string, confirmedSensitive?: string[]) =>
    call<string>("request_file_transfer", {
      peerName,
      path,
      confirmedSensitive: confirmedSensitive ?? [],
    }),
  pendingFileTransferRequest: () =>
    call<FileTransferRequest | null>("pending_file_transfer_request"),
  confirmFileTransferRequest: (transferId: string, targetPath: string) =>
    call<void>("confirm_file_transfer_request", { transferId, targetPath }),
  pollFileTransferAcks: () => call<number>("poll_file_transfer_acks"),
  getDefaultFileReceiveDir: () => call<string>("get_default_file_receive_dir"),
  setDefaultFileReceiveDir: (path: string) =>
    call<void>("set_default_file_receive_dir", { path }),
  // ── ISS-017/018/019：core 的文件传输 IPC（已与 core 对齐契约）──
  // 访达多选文件并**直接发送**给 peerName，返回 transfer_id 数组（core 定稿）。
  pickFilesForTransfer: (peerName: string) =>
    call<string[]>("pick_files_for_transfer", { peerName }),
  // ISS-032: Cmd+V 粘贴已复制的文件并发送给 peerName，返回 transfer_id 数组。
  pasteFilesForTransfer: (peerName: string) =>
    call<string[]>("paste_files_for_transfer", { peerName }),
  // 接收端待接收文件列表（非破坏性，可重复读）。
  pendingFileTransfers: () => call<PendingFileTransfer[]>("pending_file_transfers"),
  // 接收端确认接收，落到 saveDir。
  acceptFileTransfer: (id: string, saveDir: string) =>
    call<void>("accept_file_transfer", { id, saveDir }),
  // 默认接收目录（leader 约定名；与上面 net 的别名指向同一后端能力，二选一对接）。
  getDefaultReceiveDir: () => call<string>("get_default_receive_dir"),
  setDefaultReceiveDir: (path: string) =>
    call<void>("set_default_receive_dir", { path }),
  // 传输历史（含接收端下载记录，ISS-019）。
  fileTransferHistory: (peerName: string) =>
    call<FileTransferRecord[]>("file_transfer_history", { peerName }),
  confirmPairing: (peerId: string) => call<void>("confirm_pairing", { peerId }),
  cancelPairing: (peerId: string) => call<void>("cancel_pairing", { peerId }),
  unpair: (peerId: string) => call<void>("unpair", { peerId }),

  scanWorkspace: (localRoot: string, remoteRoot: string) =>
    call<ScannedChild[]>("scan_workspace", { localRoot, remoteRoot }),

  getBatchPlan: (peerId: string, direction: string) =>
    call<BatchPlan>("get_batch_plan", { peerId, direction }),

  getConflict: (projectId: string) => call<Conflict>("get_conflict", { projectId }),
  resolveConflict: (projectId: string, resolution: string) =>
    call<void>("resolve_conflict", { projectId, resolution }),

  getRewriteReport: (projectId: string) =>
    call<RewriteReport>("get_rewrite_report", { projectId }),

  setAutoSyncPaused: (paused: boolean) =>
    call<void>("set_auto_sync_paused", { paused }),
  getAutoSyncPaused: () => call<boolean>("get_auto_sync_paused"),

  checkTargetNotEmpty: (projectId: string, peerName: string) =>
    call<boolean>("check_target_not_empty", { projectId, peerName }),

  // 推送前脑裂检测。split_brain=true → 前端弹「以哪端为准」；否则若 peerNotEmpty
  // 且无快照 → 走覆盖确认；reachable=false → 对端离线提示。
  checkSplitBrain: (projectId: string, peerName: string) =>
    call<{
      reachable: boolean;
      hasSnapshot: boolean;
      peerNotEmpty: boolean;
      splitBrain: boolean;
    }>("check_split_brain", { projectId, peerName }),

  startSync: (
    projectId: string,
    direction: string,
    confirmedSensitive?: string[],
    confirmOverwrite?: boolean,
  ) =>
    call<void>("start_sync", {
      projectId,
      direction,
      confirmedSensitive: confirmedSensitive ?? [],
      confirmOverwrite: confirmOverwrite ?? false,
    }),
  cancelSync: (projectId: string) => call<void>("cancel_sync", { projectId }),

  addProject: (project: unknown) => call<void>("add_project", { project }),
  addWorkspace: (workspace: unknown) => call<void>("add_workspace", { workspace }),
  enableChild: (workspaceId: string, child: string, config: unknown) =>
    call<void>("enable_child", { workspaceId, child, config }),
  setProjectMode: (projectId: string, mode: string) =>
    call<void>("set_project_mode", { projectId, mode }),
  saveExcludeRules: (projectId: string, rules: string[]) =>
    call<void>("save_exclude_rules", { projectId, rules }),
  deleteProject: (projectId: string) => call<void>("delete_project", { projectId }),

  getServeInfo: () => call<ServeInfo | null>("get_serve_info"),
  addPeerEndpoint: (args: {
    name: string;
    peerId?: string;
    endpoint: string;
    certPath?: string;
    serverName?: string;
  }) => call<void>("add_peer_endpoint", args),
  pickDirectory: () => call<string | null>("pick_directory"),
};

export interface ServeInfo {
  port: number;
  certPath: string;
  receiveDir: string;
}
