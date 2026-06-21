import { useState } from "react";
import { ChevronDown, ChevronRight, Plus } from "lucide-react";
import { ipc } from "../ipc";
import { useStore } from "../store";
import { ProjectCard } from "../ProjectCard";

export function OverviewPage() {
  const { overview, setDialog, setSelectedProjectId, t } = useStore();
  const [wsOpen, setWsOpen] = useState<Record<string, boolean>>({ "ws-projects": true });

  if (!overview) return <div className="empty">…</div>;
  const { local, tools, projects, workspaces } = overview;

  return (
    <div>
      <div className="page-head">
        <h1>
          {t.selfMachine}: {local.deviceName}
        </h1>
        <span className="sub">
          {local.osVersion}&nbsp;&nbsp;·&nbsp;&nbsp;{local.ip}
        </span>
      </div>

      <div className="section-title">{t.aiSessions}</div>
      <div className="card flush">
        {tools
          .filter((tl) => tl.installed)
          .map((tl) => (
            <div key={tl.name}>
              <div className="tool-row">
                <strong>{tl.name}</strong>
                <span className="path">{tl.configDir}</span>
                <span className="muted">
                  {tl.sessionCount} {t.projSessions}
                </span>
                <button
                  title="在 Finder 中打开该工具的会话目录"
                  onClick={async () => {
                    ipc.uiLog(`ai_tool_view_clicked tool=${tl.name} dir=${tl.configDir}`);
                    try {
                      await ipc.openPath(tl.configDir);
                    } catch (e) {
                      console.error("open tool dir failed", e);
                    }
                  }}
                >
                  {t.view}
                </button>
              </div>
            </div>
          ))}
      </div>

      <div className="section-title">{t.syncProjects}</div>
      {projects.map((p) => (
        <ProjectCard key={p.id} project={p} />
      ))}

      {workspaces.map((ws) => {
        const open = wsOpen[ws.id];
        return (
          <div className="card flush" key={ws.id}>
            <div
              className="ws-head"
              onClick={() => setWsOpen({ ...wsOpen, [ws.id]: !open })}
            >
              <span className="chev">{open ? <ChevronDown size={14} /> : <ChevronRight size={14} />}</span>
              <strong>{t.workspace}: </strong>
              <span className="path">
                {ws.localRoot}&nbsp;&nbsp;⇄&nbsp;&nbsp;{ws.remoteRoot}
                {ws.peerName ? ` (${ws.peerName})` : ""}
              </span>
            </div>
            {open && (
              <div className="ws-body">
                {ws.children
                  .filter((c) => !c.newlyDiscovered)
                  .map((c) => (
                    <div className="ws-child" key={c.name}>
                      <span className={`name ${c.status === "disabled" ? "muted" : ""}`}>{c.name}</span>
                      {c.status === "syncing" ? (
                        <>
                          <span className="status-pill syncing" style={{ width: 148 }}>
                            <span className="spinner" />
                            {t.syncing} {c.progress}%
                          </span>
                          <span className="bar" style={{ flex: 1 }}>
                            <div style={{ width: `${c.progress}%` }} />
                          </span>
                          <button>{t.cancel}</button>
                        </>
                      ) : c.status === "synced" ? (
                        <>
                          <span className="status-pill synced" style={{ width: 88 }}>
                            <span className="dot online" />
                            {t.synced}
                          </span>
                          <span className="faint" style={{ flex: 1 }}>
                            ⇄ {c.peerName}
                          </span>
                          <button
                            onClick={() => {
                              setSelectedProjectId(c.name);
                              ipc.startSync(c.name, "push").catch(() => {});
                              setDialog({ kind: "syncProgress" });
                            }}
                          >
                            {t.push}
                          </button>
                        </>
                      ) : (
                        <>
                          <span className="status-pill disabled" style={{ width: 88 }}>
                            <span className="dot offline" />
                            {t.notEnabled}
                          </span>
                          <span style={{ flex: 1 }} />
                          <button
                            className="primary tiny"
                            onClick={() => setDialog({ kind: "enableChild", workspaceId: ws.id, child: c.name })}
                          >
                            {t.enable}
                          </button>
                        </>
                      )}
                    </div>
                  ))}
                {ws.children.some((c) => c.newlyDiscovered) && (
                  <div className="ws-child">
                    <span className="faint" style={{ flex: 1 }}>
                      + {ws.children.filter((c) => c.newlyDiscovered).length} {t.newSubFound}
                    </span>
                    <button onClick={() => setDialog({ kind: "discovered", workspaceId: ws.id })}>
                      {t.view}
                    </button>
                  </div>
                )}
              </div>
            )}
          </div>
        );
      })}

      <div className="btn-group" style={{ justifyContent: "flex-end", marginTop: 4 }}>
        <button className="cta" onClick={() => setDialog({ kind: "addProject" })}>
          <Plus size={14} style={{ verticalAlign: "-2px" }} /> {t.addProject}
        </button>
        <button onClick={() => setDialog({ kind: "addWorkspace" })}>
          <Plus size={14} style={{ verticalAlign: "-2px" }} /> {t.addWorkspace}
        </button>
      </div>
      <div style={{ height: 20 }} />
    </div>
  );
}
