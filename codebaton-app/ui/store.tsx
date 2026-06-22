import {
  createContext,
  ReactNode,
  useCallback,
  useContext,
  useEffect,
  useState,
} from "react";
import { ipc, listen } from "./ipc";
import { dict, type Lang, type Strings } from "./i18n";
import type {
  Overview,
  ProjectMappingRequest,
  StatusBar,
  SyncProgress,
  SyncResult,
  WorkspaceMappingRequest,
} from "./types";

// Active dialog descriptor (D1-D12). `null` = no dialog.
export type DialogState =
  | null
  | { kind: "addProject" }
  | { kind: "addWorkspace" }
  | { kind: "enableChild"; workspaceId: string; child: string }
  | { kind: "pairing"; peerId: string }
  | { kind: "projectMappingRequest"; request: ProjectMappingRequest }
  | { kind: "workspaceMappingRequest"; request: WorkspaceMappingRequest }
  | { kind: "conflict"; projectId: string }
  | { kind: "batch"; peerId: string; direction: "push" | "pull" }
  | { kind: "excludeRules"; projectId: string }
  | { kind: "unpair"; peerId: string }
  | { kind: "syncProgress" }
  | { kind: "overwriteConfirm"; projectId: string; peerName: string }
  | { kind: "rewriteReport"; projectId: string }
  | { kind: "discovered"; workspaceId: string }
  | { kind: "wizard" };

export type View =
  | { page: "overview" }
  | { page: "peer"; peerId: string }
  | { page: "settings" };

interface Store {
  overview: Overview | null;
  statusBar: StatusBar | null;
  view: View;
  setView: (v: View) => void;
  dialog: DialogState;
  setDialog: (d: DialogState) => void;
  selectedProjectId: string | null;
  setSelectedProjectId: (id: string | null) => void;
  syncProgress: SyncProgress | null;
  lastResult: SyncResult | null;
  clearResult: () => void;
  refresh: () => Promise<void>;
  toast: string | null;
  lang: Lang;
  setLang: (l: Lang) => void;
  t: Strings;
  theme: ThemePref;
  setTheme: (tm: ThemePref) => void;
  // ── 对话/文件 全局 store (ISS-020/021/022) ──
  chatByPeer: Record<string, ChatEntry[]>;
  sendChat: (peerName: string, content: string) => Promise<void>;
  unreadChat: Record<string, number>;
  unreadFiles: Record<string, number>;
  bumpUnreadFiles: (peerName: string, n: number) => void;
  clearUnread: (peerName: string, which: "chat" | "files") => void;
}

export type ThemePref = "dark" | "light" | "system";

/** A chat message in the global store. mine=true 表示本机发出的。 */
export type ChatEntry = {
  senderName: string;
  content: string;
  timestamp: number;
  mine: boolean;
};

/** Resolve a theme preference to the actual theme + apply it to <html>. */
function applyTheme(pref: ThemePref) {
  const sys =
    typeof window !== "undefined" &&
    window.matchMedia &&
    window.matchMedia("(prefers-color-scheme: light)").matches
      ? "light"
      : "dark";
  const resolved = pref === "system" ? sys : pref;
  document.documentElement.dataset.theme = resolved;
}

const Ctx = createContext<Store>(null as any);
export const useStore = () => useContext(Ctx);

export function StoreProvider({ children }: { children: ReactNode }) {
  const [overview, setOverview] = useState<Overview | null>(null);
  const [statusBar, setStatusBar] = useState<StatusBar | null>(null);
  const [view, setView] = useState<View>({ page: "overview" });
  const [dialog, setDialog] = useState<DialogState>(null);
  const [selectedProjectId, setSelectedProjectId] = useState<string | null>(null);
  const [syncProgress, setSyncProgress] = useState<SyncProgress | null>(null);
  const [lastResult, setLastResult] = useState<SyncResult | null>(null);
  const [toast, setToast] = useState<string | null>(null);
  const [lang, setLang] = useState<Lang>("zh");
  // 主题偏好（item 9）：默认跟随系统，持久化到 localStorage。
  const [theme, setThemeState] = useState<ThemePref>(
    () => (localStorage.getItem("cb-theme") as ThemePref) || "system",
  );
  const setTheme = useCallback((tm: ThemePref) => {
    setThemeState(tm);
    localStorage.setItem("cb-theme", tm);
    applyTheme(tm);
  }, []);
  // Apply on mount + follow OS changes when in "system" mode.
  useEffect(() => {
    applyTheme(theme);
    if (theme !== "system" || !window.matchMedia) return;
    const mq = window.matchMedia("(prefers-color-scheme: light)");
    const onChange = () => applyTheme("system");
    mq.addEventListener("change", onChange);
    return () => mq.removeEventListener("change", onChange);
  }, [theme]);

  // ── 对话全局 store（ISS-021/022）：消息存这里，不随 ChatTab 卸载丢失。 ──
  const [chatByPeer, setChatByPeer] = useState<Record<string, ChatEntry[]>>({});
  const [unreadChat, setUnreadChat] = useState<Record<string, number>>({});
  const [unreadFiles, setUnreadFiles] = useState<Record<string, number>>({});

  const appendChat = useCallback((peerName: string, e: ChatEntry) => {
    setChatByPeer((prev) => {
      const list = prev[peerName] ?? [];
      const dup = list.some(
        (x) => x.timestamp === e.timestamp && x.content === e.content && x.mine === e.mine,
      );
      if (dup) return prev;
      return { ...prev, [peerName]: [...list, e] };
    });
  }, []);

  const clearUnread = useCallback((peerName: string, which: "chat" | "files") => {
    const setter = which === "chat" ? setUnreadChat : setUnreadFiles;
    setter((prev) => (prev[peerName] ? { ...prev, [peerName]: 0 } : prev));
  }, []);

  const bumpUnreadFiles = useCallback((peerName: string, n: number) => {
    if (n <= 0) return;
    setUnreadFiles((prev) => ({ ...prev, [peerName]: (prev[peerName] ?? 0) + n }));
  }, []);

  const sendChat = useCallback(
    async (peerName: string, content: string) => {
      await ipc.sendTextMessage(peerName, content);
      appendChat(peerName, { senderName: "我", content, timestamp: Date.now(), mine: true });
    },
    [appendChat],
  );

  // 单一消息消费者（ISS-022）：全局轮询 pendingTextMessage → 写入 store；
  // toast 只是写入的副效果，不再是独立消费者。收到非当前会话的消息记未读。
  useEffect(() => {
    if (!ipc.inTauri()) return;
    let cancelled = false;
    const poll = () =>
      ipc
        .pendingTextMessage()
        .then((m) => {
          if (cancelled || !m) return;
          appendChat(m.senderName, {
            senderName: m.senderName,
            content: m.content,
            timestamp: m.timestamp || Date.now(),
            mine: false,
          });
          // 未读 +1；ChatTab 打开时会立即 clearUnread 清掉，所以不在会话里才会留。
          setUnreadChat((prev) => ({ ...prev, [m.senderName]: (prev[m.senderName] ?? 0) + 1 }));
          const preview = m.content.length > 80 ? m.content.slice(0, 80) + "…" : m.content;
          setToast(`${m.senderName}: ${preview}`);
          setTimeout(() => setToast(null), 2600);
        })
        .catch(() => {});
    poll();
    const timer = window.setInterval(poll, 1000);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [appendChat]);

  const refresh = useCallback(async () => {
    if (!ipc.inTauri()) return;
    try {
      setOverview(await ipc.getOverview());
      setStatusBar(await ipc.getStatusBar());
    } catch (e) {
      console.error("refresh failed", e);
    }
  }, []);

  // First-run wizard gating + initial load.
  useEffect(() => {
    (async () => {
      if (!ipc.inTauri()) return;
      const onboarded = await ipc.isOnboarded().catch(() => true);
      await refresh();
      if (!onboarded) setDialog({ kind: "wizard" });
    })();
  }, [refresh]);

  // 对话历史持久化：从后端拉已存储的 text_messages，灌进 chatByPeer 做初始化。
  // 只在 overview 变化时（含首次加载）执行，对每个已配对 peer 拉一次。
  useEffect(() => {
    if (!ipc.inTauri() || !overview) return;
    const peerNames = [...new Set(overview.projects.map((p) => p.peerName).filter(Boolean))];
    peerNames.forEach((name) => {
      ipc
        .textMessages(name)
        .then((msgs) => {
          if (!msgs || msgs.length === 0) return;
          const entries: ChatEntry[] = msgs.map((m) => ({
            senderName: m.mine ? "我" : m.senderName,
            content: m.content,
            timestamp: m.timestamp,
            mine: !!m.mine,
          }));
          setChatByPeer((prev) => {
            if ((prev[name]?.length ?? 0) >= entries.length) return prev;
            return { ...prev, [name]: entries };
          });
        })
        .catch(() => {});
    });
  }, [overview]);

  // Sync progress / result event stream (D9).
  useEffect(() => {
    let unsubP: (() => void) | undefined;
    let unsubR: (() => void) | undefined;
    listen<SyncProgress>("sync-progress", (p) => {
      setSyncProgress(p);
    }).then((u) => (unsubP = u));
    listen<SyncResult>("sync-result", (r) => {
      setLastResult(r);
      refresh();
    }).then((u) => (unsubR = u));
    return () => {
      unsubP?.();
      unsubR?.();
    };
  }, [refresh]);

  // Tray push/pull-all actions land here.
  useEffect(() => {
    let unsub: (() => void) | undefined;
    listen<string>("tray-action", (action) => {
      const peer = overview?.projects[0]?.peerId;
      if (peer)
        setDialog({
          kind: "batch",
          peerId: peer,
          direction: action === "pull_all" ? "pull" : "push",
        });
    }).then((u) => (unsub = u));
    return () => unsub?.();
  }, [overview]);

  const clearResult = useCallback(() => setLastResult(null), []);

  // Poll status bar periodically so syncing % stays fresh during a transfer.
  useEffect(() => {
    if (!ipc.inTauri()) return;
    const t = setInterval(() => ipc.getStatusBar().then(setStatusBar).catch(() => {}), 1500);
    return () => clearInterval(t);
  }, []);

  // ISS-036: 定时重新拉 overview，让新发现的工作区子项目/项目自动出现在首页，
  // 不必手动刷新。5s 一次，避免太频繁。
  useEffect(() => {
    if (!ipc.inTauri()) return;
    const t = setInterval(() => {
      ipc.getOverview().then(setOverview).catch(() => {});
    }, 5000);
    return () => clearInterval(t);
  }, []);

  return (
    <Ctx.Provider
      value={{
        overview,
        statusBar,
        view,
        setView,
        dialog,
        setDialog,
        selectedProjectId,
        setSelectedProjectId,
        syncProgress,
        lastResult,
        clearResult,
        refresh,
        toast,
        lang,
        setLang,
        t: dict[lang],
        theme,
        setTheme,
        chatByPeer,
        sendChat,
        unreadChat,
        unreadFiles,
        bumpUnreadFiles,
        clearUnread,
      }}
    >
      <ToastSetter setToast={setToast} />
      {children}
    </Ctx.Provider>
  );
}

// Hidden helper so child components can push toasts without prop drilling.
let pushToastFn: (m: string) => void = () => {};
export function pushToast(m: string) {
  pushToastFn(m);
}
function ToastSetter({ setToast }: { setToast: (m: string | null) => void }) {
  useEffect(() => {
    pushToastFn = (m: string) => {
      setToast(m);
      setTimeout(() => setToast(null), 2600);
    };
  }, [setToast]);
  return null;
}
