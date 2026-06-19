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
}

export type ThemePref = "dark" | "light" | "system";

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
