import { useState } from "react";
import { ChevronRight } from "lucide-react";
import { ipc } from "./ipc";
import { useStore, pushToast } from "./store";
import type { Project } from "./types";
import { fmtBytes, fmtTime, modeLabel } from "./util";

export function ProjectCard({ project }: { project: Project }) {
  const { setDialog, setSelectedProjectId, refresh, t } = useStore();
  const [open, setOpen] = useState(false);

  const remove = async () => {
    setOpen(false);
    try {
      await ipc.deleteProject(project.id);
      await refresh();
    } catch (e) {
      pushToast(`${t.deleteFailed}: ${e}`);
    }
  };

  const sync = (direction: "push" | "pull") => {
    setSelectedProjectId(project.id);
    ipc.startSync(project.id, direction).catch(() => {});
    setDialog({ kind: "syncProgress" });
  };

  return (
    <div className="card flush">
      <div
        className="proj-head"
        onClick={() => {
          setSelectedProjectId(project.id);
          setOpen(!open);
        }}
      >
        <div className="proj-title">
          <span className="chev" style={{ transform: open ? "rotate(90deg)" : "none" }}>
            <ChevronRight size={14} />
          </span>
          <span className="name">{project.name}</span>
          {project.status === "synced" && (
            <span className="status-pill synced" style={{ marginLeft: 6 }}>
              <span className="dot online" />
              {t.synced}
            </span>
          )}
        </div>
        <div className="path indent">{project.localDir}</div>
        <div className="path indent">
          ⇄ {project.peerName} : {project.remoteDir}
        </div>
        <div className="proj-meta indent" onClick={(e) => e.stopPropagation()}>
          {project.lastSync && (
            <span className="faint">
              {t.last}: {fmtTime(project.lastSync)}
            </span>
          )}
          <span className="faint">
            {t.mode}: <span style={{ color: "var(--text-2)" }}>{modeLabel(project.mode)}</span>
          </span>
          <span style={{ flex: 1 }} />
          {project.status === "syncing" ? (
            <button onClick={() => ipc.cancelSync(project.id)}>{t.cancel}</button>
          ) : (
            <>
              <button className="primary" onClick={() => sync("push")}>
                {t.push}
              </button>
              <button onClick={() => sync("pull")}>{t.pull}</button>
            </>
          )}
        </div>
      </div>

      {open && (
        <div className="proj-detail" onClick={(e) => e.stopPropagation()}>
          <div className="detail-grid">
            <span className="label">{t.localPath}</span>
            <span className="path">{project.localDir}</span>
            <button className="ghost">{t.modify}</button>
            <span className="label">{t.remotePath}</span>
            <span className="path">{project.remoteDir}</span>
            <button className="ghost">{t.modify}</button>
            <span className="label">{t.sessionDir}</span>
            <span className="path" style={{ gridColumn: "2/4" }}>
              {project.localSessionDir}
            </span>
            <span className="label">{t.syncMode}</span>
            <span style={{ gridColumn: "2/4" }}>
              <span className="chip">{modeLabel(project.mode)} ▾</span>
            </span>
            <span className="label">{t.targetTool}</span>
            <span style={{ gridColumn: "2/4" }}>
              <span className="chip">{project.targetTool} ▾</span>
            </span>
            <span className="label">{t.excludeRules}</span>
            <span className="path">{project.excludeRules.join(", ")}</span>
            <button className="ghost" onClick={() => setDialog({ kind: "excludeRules", projectId: project.id })}>
              {t.edit}
            </button>
          </div>

          <div className="section-title">{t.recentSync}</div>
          {project.history.map((h, i) => (
            <div className="history-row pc-history-row" key={i}>
              <span>{fmtTime(h.timestamp)}</span>
              <span>
                {h.direction === "push" ? "→" : "←"} {project.peerName}
              </span>
              <span className={h.success ? "ok" : "fail"}>{h.success ? t.success : t.failed}</span>
              <span>{h.success ? `${h.files} ${t.files}` : h.detail ?? ""}</span>
              <span>{h.success ? fmtBytes(h.bytes) : ""}</span>
            </div>
          ))}

          <div className="btn-group" style={{ marginTop: 18 }}>
            <button className="cta" onClick={() => sync("push")}>
              {t.pushToHome} {project.peerName}
            </button>
            <button onClick={() => sync("pull")}>
              {t.pullFromHome} {project.peerName}
            </button>
            <span style={{ flex: 1 }} />
            <button
              className="danger"
              onClick={remove}
            >
              {t.deleteMapping}
            </button>
          </div>
        </div>
      )}
    </div>
  );
}
