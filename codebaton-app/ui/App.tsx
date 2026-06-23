import { useEffect } from "react";
import { AlertCircle, Moon, Sun, Monitor } from "lucide-react";
import { Sidebar } from "./Sidebar";
import { StatusBar } from "./StatusBar";
import { DialogHost } from "./dialogs";
import { OverviewPage } from "./pages/Overview";
import { PeerDetailPage } from "./pages/PeerDetail";
import { SettingsPage } from "./pages/Settings";
import { ipc, listen } from "./ipc";
import { useNotifications } from "./notifications";
import { useShortcuts } from "./shortcuts";
import { pushToast, useStore } from "./store";

export function App() {
  const { view, dialog, setDialog, toast, syncProgress, lang, setLang, refresh, theme, setTheme, t } =
    useStore();
  useShortcuts();
  useNotifications();

  // When a sync finishes while the progress dialog isn't open, still surface
  // the result (e.g. background / auto-sync) by opening D9's result view.
  useEffect(() => {
    let unsub: (() => void) | undefined;
    listen("sync-result", () => setDialog({ kind: "syncProgress" })).then((u) => (unsub = u));
    return () => unsub?.();
  }, [setDialog]);

  useEffect(() => {
    if (!ipc.inTauri() || dialog) return;
    let cancelled = false;
    const poll = async () => {
      try {
        const pairing = await ipc.pendingPairingRequest();
        if (cancelled) return;
        if (pairing) {
          ipc.uiLog(`pending_pairing_request received peerId=${pairing.peerId} code=${pairing.code}`);
          setDialog({ kind: "pairing", peerId: pairing.peerId });
          return;
        }
        const request = await ipc.pendingProjectMappingRequest();
        if (cancelled) return;
        if (request) {
          ipc.uiLog(
            `pending_project_mapping_request received requestId=${request.requestId} project=${request.projectName}`,
          );
          setDialog({ kind: "projectMappingRequest", request });
          return;
        }
        const fileRequest = await ipc.pendingFileTransferRequest();
        if (cancelled || !fileRequest) return;
        ipc.uiLog(
          `pending_file_transfer_request received transferId=${fileRequest.transferId} filename=${fileRequest.filename}`,
        );
        if (
          window.confirm(
            t.fileReceiveConfirm(
              fileRequest.senderName,
              fileRequest.filename,
              fileRequest.suggestedPath,
            ),
          )
        ) {
          await ipc.confirmFileTransferRequest(fileRequest.transferId, fileRequest.suggestedPath);
          pushToast(t.fileReceiveConfirmed);
        }
      } catch {
        // Polling is best-effort; receiver logs carry detailed backend errors.
      }
    };
    poll();
    const timer = window.setInterval(poll, 1000);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [dialog, setDialog, t]);

  useEffect(() => {
    if (!ipc.inTauri()) return;
    let cancelled = false;
    const poll = () => {
      ipc
        .pollProjectMappingAcks()
        .then(async (count) => {
          if (cancelled || count <= 0) return;
          ipc.uiLog(`project_mapping_acks_applied count=${count}`);
          await refresh();
          pushToast(t.projMapAckd);
        })
        .catch((e) => ipc.uiLog(`project_mapping_ack_poll_failed error=${String(e)}`));
      ipc
        .pollFileTransferAcks()
        .then((count) => {
          if (cancelled || count <= 0) return;
          ipc.uiLog(`file_transfer_acks_applied count=${count}`);
          pushToast(t.fileSentToast);
        })
        .catch((e) => ipc.uiLog(`file_transfer_ack_poll_failed error=${String(e)}`));
      // ISS-022: 文本消息的唯一消费者已移到全局 store（store.tsx 轮询
      // pendingTextMessage→写入 chatByPeer + 派生 toast）。这里不再消费，
      // 否则会和 store 抢同一队列导致消息丢进 toast 却不进对话列表。
    };
    poll();
    const timer = window.setInterval(poll, 1000);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [refresh, t]);

  return (
    <>
      <div className="app">
        {/* Title bar: centered CodeBaton mark + name, lang toggle on the right. */}
        <div className="titlebar">
          <div className="brand">
            <span className="logo">
              <svg viewBox="0 0 1024 1024" width="15" height="15" aria-label="CodeBaton">
                <path
                  d="M 675.77 302.39 L 652.96 286.42 L 628.61 272.92 L 602.98 262.04 L 576.35 253.90 L 549.02 248.59 L 521.28 246.16 L 493.44 246.65 L 465.81 250.04 L 438.68 256.30 L 412.35 265.37 L 387.12 277.14 L 363.25 291.48 L 341.02 308.23 L 320.66 327.22 L 302.39 348.23 L 286.42 371.04 L 272.92 395.39 L 262.04 421.02 L 253.90 447.65 L 248.59 474.98 L 246.16 502.72 L 246.65 530.56 L 250.04 558.19 L 256.30 585.32 L 265.37 611.65 L 277.14 636.88 L 291.48 660.75 L 308.23 682.98 L 327.22 703.34 L 348.23 721.61 L 371.04 737.58 L 395.39 751.08 L 421.02 761.96 L 447.65 770.10 L 474.98 775.41 L 502.72 777.84 L 530.56 777.35 L 558.19 773.96 L 585.32 767.70 L 611.65 758.63 L 636.88 746.86 L 660.75 732.52 L 672.08 724.44"
                  stroke="#0B1020"
                  strokeWidth="108"
                  strokeLinecap="round"
                  strokeLinejoin="round"
                  fill="none"
                />
                <g transform="translate(512 512) rotate(-34)">
                  <rect x="-252" y="-44" width="504" height="88" rx="44" fill="#38D9C8" />
                </g>
              </svg>
            </span>
            <span className="title">CodeBaton</span>
          </div>
          {/* ISS-033: 标题栏右上角 主题切换（暗/亮/跟随系统）+ 语言切换 */}
          <div className="titlebar-right">
            <div className="seg-toggle theme-toggle" title={t.theme}>
              <button
                className={theme === "dark" ? "on" : ""}
                title={t.themeDark}
                onClick={() => setTheme("dark")}
              >
                <Moon size={13} />
              </button>
              <button
                className={theme === "light" ? "on" : ""}
                title={t.themeLight}
                onClick={() => setTheme("light")}
              >
                <Sun size={13} />
              </button>
              <button
                className={theme === "system" ? "on" : ""}
                title={t.themeSystem}
                onClick={() => setTheme("system")}
              >
                <Monitor size={13} />
              </button>
            </div>
            <div className="lang-toggle">
              <button className={lang === "zh" ? "on" : ""} onClick={() => setLang("zh")}>
                中
              </button>
              <button className={lang === "en" ? "on" : ""} onClick={() => setLang("en")}>
                EN
              </button>
            </div>
          </div>
        </div>
        <Sidebar />
        <div className="main">
          {!ipc.inTauri() && (
            <div className="banner">
              <AlertCircle size={14} />
              {t.browserPreview}
            </div>
          )}
          {view.page === "overview" && <OverviewPage />}
          {view.page === "peer" && <PeerDetailPage peerId={view.peerId} />}
          {view.page === "settings" && <SettingsPage />}
        </div>
        <StatusBar />
      </div>

      {dialog && <DialogHost />}
      {/* keep syncProgress referenced so status bar updates re-render App tree */}
      <span style={{ display: "none" }}>{syncProgress?.percent}</span>

      {toast && (
        <div
          style={{
            position: "fixed",
            bottom: 44,
            left: "50%",
            transform: "translateX(-50%)",
            background: "var(--bg-active)",
            border: "1px solid var(--border-strong)",
            borderRadius: 8,
            padding: "8px 16px",
            fontSize: 12,
            zIndex: 200,
            boxShadow: "var(--shadow)",
            maxWidth: "min(420px, 90vw)",
            overflow: "hidden",
            wordBreak: "break-all" as const,
          }}
        >
          {toast}
        </div>
      )}
    </>
  );
}
