import { Plus } from "lucide-react";
import { ipc } from "../ipc";
import { useStore } from "../store";
import { ProjectCard } from "../ProjectCard";

export function OverviewPage() {
  const { overview, setDialog, t } = useStore();

  if (!overview) return <div className="empty">…</div>;
  const { local, tools, projects } = overview;

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

      <div className="btn-group" style={{ justifyContent: "flex-end", marginTop: 4 }}>
        <button className="cta" onClick={() => setDialog({ kind: "addProject" })}>
          <Plus size={14} style={{ verticalAlign: "-2px" }} /> {t.addProject}
        </button>
      </div>
      <div style={{ height: 20 }} />
    </div>
  );
}
