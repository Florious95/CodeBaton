// Bilingual strings, mirrored from the Claude Design prototype
// (frontend-demo/AISync.dc.html D() dictionary). Covers the visible
// surfaces (sidebar / P1 / P2 / P3 / status bar / dialogs headers).

export type Lang = "zh" | "en";

const dictZh = {
    online: "在线", offline: "离线", syncing: "同步中", idle: "空闲",
    paired: "已配对设备", discovered: "发现的设备", settings: "设置",
    pair: "配对", view: "查看", push: "推送", pull: "拉取", cancel: "取消",
    enable: "开启", manage: "管理", edit: "编辑", save: "保存", close: "关闭",
    selfMachine: "本机", aiSessions: "AI 工具会话", syncProjects: "同步项目",
    projSessions: "个项目会话", addProject: "添加项目", addWorkspace: "添加工作区",
    synced: "已同步", last: "上次", minAgo: "分钟前", mode: "模式", biAuto: "双向自动",
    workspace: "工作区", notEnabled: "未开启", newSubFound: "个新发现的子项目", files: "文件",
    localPath: "本机路径", remotePath: "对端路径", sessionDir: "会话目录", syncMode: "同步模式",
    targetTool: "目标工具", excludeRules: "排除规则", recentSync: "最近同步",
    success: "成功", failed: "失败", connLost: "连接中断", modify: "修改",
    pushToHome: "推送到", pullFromHome: "拉取自", deleteMapping: "删除映射",
    pairedTime: "配对时间", claudeMap: "Claude 配置映射", projMap: "项目映射",
    localM: "本机", remoteM: "对端", addProjMap: "添加项目映射", syncHistory: "同步历史",
    pushAll: "全部推送", pullAll: "全部拉取", unpair: "解除配对",
    lastSyncLabel: "上次同步", conflictDetected: "检测到冲突", handle: "处理",
    pausedAuto: "已暂停自动同步",
    settingsTitle: "设置", deviceName: "设备名称", deviceId: "设备 ID",
};

// The key set, values widened to `string` so zh and en share one type.
export type Strings = Record<keyof typeof dictZh, string>;

const dictEn: Strings = {
    online: "Online", offline: "Offline", syncing: "Syncing", idle: "Idle",
    paired: "Paired Devices", discovered: "Discovered", settings: "Settings",
    pair: "Pair", view: "View", push: "Push", pull: "Pull", cancel: "Cancel",
    enable: "Enable", manage: "Manage", edit: "Edit", save: "Save", close: "Close",
    selfMachine: "This Device", aiSessions: "AI Tool Sessions", syncProjects: "Synced Projects",
    projSessions: "project sessions", addProject: "Add Project", addWorkspace: "Add Workspace",
    synced: "Synced", last: "Last", minAgo: "min ago", mode: "Mode", biAuto: "Two-way auto",
    workspace: "Workspace", notEnabled: "Off", newSubFound: "new sub-project(s) found", files: "files",
    localPath: "Local path", remotePath: "Remote path", sessionDir: "Session dir", syncMode: "Sync mode",
    targetTool: "Target tool", excludeRules: "Exclude rules", recentSync: "Recent syncs",
    success: "OK", failed: "Failed", connLost: "Connection lost", modify: "Edit",
    pushToHome: "Push to", pullFromHome: "Pull from", deleteMapping: "Delete mapping",
    pairedTime: "Paired", claudeMap: "Claude Config Mapping", projMap: "Project Mappings",
    localM: "Local", remoteM: "Remote", addProjMap: "Add Project Mapping", syncHistory: "Sync History",
    pushAll: "Push All", pullAll: "Pull All", unpair: "Unpair",
    lastSyncLabel: "Last sync", conflictDetected: "conflict detected", handle: "Resolve",
    pausedAuto: "Auto-sync paused",
    settingsTitle: "Settings", deviceName: "Device name", deviceId: "Device ID",
};

export const dict: Record<Lang, Strings> = { zh: dictZh, en: dictEn };
