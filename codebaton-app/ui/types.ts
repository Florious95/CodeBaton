// Mirrors codebaton-app/src/dto.rs. Keep field names in camelCase (serde rename).

export type PeerStatus = "online" | "offline";
export type PeerKind = "local" | "paired" | "discovered";

export interface Peer {
  id: string;
  name: string;
  os: string;
  ip: string;
  status: PeerStatus;
  kind: PeerKind;
  pairedAt: string | null;
}

export interface AiTool {
  name: string;
  configDir: string;
  sessionCount: number;
  installed: boolean;
}

// Manual handoff is push-only. (The backend SyncMode enum still has the other
// variants as dead code; legacy configs may carry them, but the UI never
// produces anything but oneWayPush.)
export type SyncMode = "oneWayPush";
export type ProjectSyncStatus =
  | "synced"
  | "syncing"
  | "disabled"
  | "conflict"
  | "error";

export interface SyncHistoryEntry {
  timestamp: string;
  projectId: string;
  workspaceName: string | null;
  childName: string | null;
  direction: string;
  success: boolean;
  files: number;
  bytes: number;
  detail: string | null;
  // 触发来源：手动（GUI 点推送/拉取）或自动（监听变更）。后端暂未区分时为空，
  // 当前 GUI 记录的都是手动，故缺省按手动显示。
  trigger?: "manual" | "auto" | null;
  role?: "sender" | "receiver" | string | null;
  fileType?: "code" | "session" | "mixed" | string | null;
}

export interface Project {
  id: string;
  name: string;
  localDir: string;
  remoteDir: string;
  remoteSessionDir: string;
  localSessionDir: string;
  peerId: string;
  peerName: string;
  mode: SyncMode;
  targetTool: string;
  status: ProjectSyncStatus;
  progress: number | null;
  lastSync: string | null;
  excludeRules: string[];
  enabled: boolean;
  history: SyncHistoryEntry[];
}

export interface LocalInfo {
  deviceId: string;
  deviceName: string;
  os: string;
  osVersion: string;
  user: string;
  ip: string;
}

export interface Settings {
  deviceName: string;
  deviceId: string;
  tools: AiTool[];
  debounceSecs: number;
  refreshIntervalSecs: number;
  port: number;
  globalExcludes: string[];
  sensitivePatterns: string[];
  autoStart: boolean;
  minimizeToTray: boolean;
  notifyOnComplete: boolean;
  logLevel: string;
  logDir: string;
}

export interface Overview {
  local: LocalInfo;
  tools: AiTool[];
  projects: Project[];
}

export interface SyncStage {
  name: string;
  percent: number;
  done: boolean;
  active: boolean;
}

export interface SyncProgress {
  projectId: string;
  projectName: string;
  peerName: string;
  direction: string;
  percent: number;
  phase: string;
  filesDone: number;
  filesTotal: number;
  bytesDone: number;
  bytesTotal: number;
  speedBps: number;
  etaSecs: number;
  currentFile: string | null;
  stages: SyncStage[];
  finished: boolean;
  success: boolean;
  error: string | null;
}

export interface SyncResult {
  projectId: string;
  projectName: string;
  peerName: string;
  direction: string;
  success: boolean;
  files: number;
  bytes: number;
  elapsedSecs: number;
  rewrittenPaths: number;
  skippedPaths: number;
  workspaceName?: string | null;
  childName?: string | null;
  error: string | null;
}

export interface RewriteEntry {
  location: string;
  field: string;
  before: string;
  after: string;
}
export interface SkippedRewrite {
  location: string;
  field: string;
  snippet: string;
  reason: string;
}
export interface RewriteReport {
  projectId: string;
  projectName: string;
  timestamp: string;
  direction: string;
  rewritten: RewriteEntry[];
  skipped: SkippedRewrite[];
}

export interface ConflictFile {
  path: string;
  change: string;
}
export interface ConflictSide {
  deviceName: string;
  changedFiles: number;
  files: ConflictFile[];
  sessionSummary: string;
}
export interface Conflict {
  projectId: string;
  projectName: string;
  local: ConflictSide;
  remote: ConflictSide;
}

export interface BatchItem {
  projectId: string;
  name: string;
  changedFiles: number;
  bytes: number;
  upToDate: boolean;
}
export interface BatchPlan {
  peerName: string;
  direction: string;
  items: BatchItem[];
  sensitiveFiles: string[];
}

export interface Pairing {
  peerId: string;
  peerName: string;
  peerIp: string;
  peerOs: string;
  code: string;
  requestId: string;
  expiresAtUnixSecs: number;
}

export interface ProjectMappingRequest {
  requestId: string;
  projectName: string;
  peerName: string;
  sourceDir: string;
  mode: SyncMode;
}

export interface TextMessage {
  senderName: string;
  content: string;
  timestamp: number;
  peerName?: string | null;
  mine?: boolean | null;
}

export interface FileTransferRequest {
  transferId: string;
  filename: string;
  size: number;
  senderName: string;
  suggestedPath: string;
}

export interface FileTransferHistory {
  timestamp: string;
  transferId: string;
  direction: "in" | "out" | string;
  peer: string;
  filename: string;
  path: string;
  bytes: number;
  status: string;
  detail: string | null;
}

/** 接收端待接收的文件（ISS-018）。core 的 pending_file_transfers 返回。 */
export interface PendingFileTransfer {
  id: string;
  filename: string;
  size: number;
  senderName: string;
}

/** 传输历史一条（ISS-019），区分传入/传出。 */
export interface FileTransferRecord {
  id: string;
  filename: string;
  size: number;
  direction: "in" | "out";
  peerName: string;
  status: string;
  timestamp: number;
  savePath?: string | null;
}

export type GlobalStatus = "idle" | "syncing" | "conflict";
export interface HandoffFile {
  relPath: string;
  size: number;
}

export interface HandoffSessionGroup {
  tool: string;
  fileCount: number;
  bytes: number;
}

export interface HandoffManifest {
  codeFiles: HandoffFile[];
  sessions: HandoffSessionGroup[];
  totalSize: number;
  incremental: boolean;
}

export interface StatusBar {
  primaryPeer: string | null;
  primaryPeerOnline: boolean;
  status: GlobalStatus;
  syncingProject: string | null;
  syncingPercent: number | null;
  conflictProject: string | null;
  lastSync: string | null;
}
