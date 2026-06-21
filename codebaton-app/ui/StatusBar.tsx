import { AlertTriangle } from "lucide-react";
import { useStore } from "./store";
import { fmtBytes } from "./util";

export function StatusBar() {
  const { statusBar, setDialog, syncProgress, t } = useStore();
  if (!statusBar) return <div className="statusbar" />;

  return (
    <div className="statusbar">
      {statusBar.primaryPeer && (
        <>
          <span className="row" style={{ gap: 6 }}>
            <span className={`dot ${statusBar.primaryPeerOnline ? "online" : "offline"}`} />
            {statusBar.primaryPeer} {statusBar.primaryPeerOnline ? t.online : t.offline}
          </span>
          <span className="sep" />
        </>
      )}

      {statusBar.status === "idle" && (
        <span className="row" style={{ gap: 14 }}>
          <span>{statusBar.autoSyncPaused ? t.pausedAuto : t.idle}</span>
          {statusBar.lastSync && (
            <span className="faint">
              {t.lastSyncLabel}: {statusBar.lastSync}
            </span>
          )}
        </span>
      )}

      {statusBar.status === "syncing" && (
        <span className="row" style={{ gap: 8, color: "var(--blue)" }}>
          <span className="spinner" style={{ width: 9, height: 9, borderWidth: "1.6px" }} />
          {t.syncing}: {statusBar.syncingProject} {statusBar.syncingPercent ?? syncProgress?.percent ?? 0}%
          {syncProgress && syncProgress.bytesTotal > 0 && (
            <span className="faint">
              ({fmtBytes(syncProgress.bytesDone)}/{fmtBytes(syncProgress.bytesTotal)})
            </span>
          )}
        </span>
      )}

      {statusBar.status === "conflict" && (
        <span className="row conflict" style={{ gap: 10 }}>
          <span className="row" style={{ gap: 6 }}>
            <AlertTriangle size={13} />
            {statusBar.conflictProject} {t.conflictDetected}
          </span>
          <button
            className="conflict-handle"
            onClick={() =>
              statusBar.conflictProject &&
              setDialog({ kind: "conflict", projectId: statusBar.conflictProject })
            }
          >
            {t.handle}
          </button>
        </span>
      )}
    </div>
  );
}
