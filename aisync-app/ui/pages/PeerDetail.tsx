import { useEffect, useState } from "react";
import { ChevronDown, ChevronRight } from "lucide-react";
import { ipc } from "../ipc";
import { useStore } from "../store";
import type { Peer, Project, SyncHistoryEntry, Workspace } from "../types";
import { fmtBytes, fmtTime, modeLabel, osLabel } from "../util";
import { ProjectCard } from "../ProjectCard";
import { ChatTab } from "../ChatTab";
import { FileTransferTab } from "../FileTransferTab";

export function PeerDetailPage({ peerId }: { peerId: string }) {
  const { setDialog } = useStore();
  const [peer, setPeer] = useState<Peer | null>(null);
  const [projects, setProjects] = useState<Project[]>([]);
  const [history, setHistory] = useState<SyncHistoryEntry[]>([]);
  const [workspaces, setWorkspaces] = useState<Workspace[]>([]);
  const [managed, setManaged] = useState<string | null>(null);
  const [wsOpen, setWsOpen] = useState<Record<string, boolean>>({});
  const [tab, setTab] = useState<"mappings" | "history" | "chat" | "files">("mappings");

  useEffect(() => {
    const load = () =>
      ipc
        .getPeerDetail(peerId)
        .then(([p, pr, h, ws]) => {
          setPeer(p);
          setProjects(pr);
          setHistory(h);
          setWorkspaces(ws ?? []);
        })
        .catch(() => {});
    load();
    const timer = setInterval(load, 3000);
    return () => clearInterval(timer);
  }, [peerId]);

  if (!peer) return <div className="empty">设备不存在</div>;
  const online = peer.status === "online";
  // ISS-009: 对端 Claude 目录优先用项目映射推导出的真实路径；否则按惯例显示
  // ~/.claude/（两端同为 macOS，默认目录一致），不再显示占位说明。
  const remoteSessionDir =
    projects.map((p) => p.remoteSessionDir).find((d) => d && d.length > 0) ?? "~/.claude/";
  // ISS-008: 配对时间可能为空——为空时不显示这行，避免「配对时间:  · 状态:」。
  const pairedAt = peer.pairedAt && peer.pairedAt.trim().length > 0 ? peer.pairedAt : "";

  return (
    <div>
      <div className="page-head">
        <h1>{peer.name}</h1>
        <span className="sub">
          {osLabel(peer.os)} │ {peer.ip}
        </span>
      </div>
      <p className="muted" style={{ fontSize: 12, display: "flex", alignItems: "center", gap: 6 }}>
        <span>状态:</span>
        {/* ISS-373: 在线点用品牌绿 #3ecf8e，离线用空心灰点 */}
        <span className="status">
          <span className={`dot ${online ? "online" : "offline"}`} />
          {online ? "在线" : "离线"}
        </span>
        {pairedAt && <span className="faint">· 配对于 {fmtTime(pairedAt)}</span>}
      </p>

      {/* ISS-013: Tab 页签 —「映射关系」｜「同步历史」 */}
      <div className="tabs">
        <button
          className={`tab ${tab === "mappings" ? "active" : ""}`}
          onClick={() => setTab("mappings")}
        >
          映射关系
        </button>
        <button
          className={`tab ${tab === "history" ? "active" : ""}`}
          onClick={() => setTab("history")}
        >
          同步历史
        </button>
        <button
          className={`tab ${tab === "chat" ? "active" : ""}`}
          onClick={() => setTab("chat")}
        >
          对话
        </button>
        <button
          className={`tab ${tab === "files" ? "active" : ""}`}
          onClick={() => setTab("files")}
        >
          文件传输
        </button>
      </div>

      {tab === "chat" && <ChatTab peerName={peer.name} online={online} />}
      {tab === "files" && <FileTransferTab peerName={peer.name} online={online} />}

      {tab === "mappings" && (
      <>
      <div className="section-title">Claude 配置映射</div>
      <div className="card">
        <div className="detail-grid">
          <span className="label">本机</span>
          <span className="path">~/.claude/</span>
          <span />
          <span className="label">对端</span>
          <span className="path">{remoteSessionDir}</span>
          <span />
        </div>
      </div>

      <div className="section-title">项目映射</div>
      {projects.length === 0 ? (
        <p className="faint" style={{ fontSize: 12 }}>
          暂无项目映射
        </p>
      ) : (
        projects.map((p) =>
          managed === p.id ? (
            <ProjectCard key={p.id} project={p} />
          ) : (
            <div className="card" key={p.id}>
              <div className="card-row">
                <div>
                  <strong>{p.name}</strong>
                  <div className="path">
                    {p.localDir} ↔ {p.remoteDir}
                  </div>
                  <div className="row" style={{ marginTop: 6 }}>
                    <span className={`status-pill ${p.status}`}>
                      {p.status === "synced" ? "● 已同步" : p.status === "syncing" ? `◐ 同步中 ${p.progress}%` : "○"}
                    </span>
                    <span className="faint">{modeLabel(p.mode)}</span>
                  </div>
                </div>
                <button
                  className="tiny"
                  style={{ flex: "none", whiteSpace: "nowrap" }}
                  onClick={() => setManaged(p.id)}
                >
                  管理
                </button>
              </div>
            </div>
          ),
        )
      )}
      <div className="btn-group" style={{ justifyContent: "flex-end" }}>
        <button onClick={() => setDialog({ kind: "addProject" })}>+ 添加项目映射</button>
      </div>

      {workspaces.length > 0 && (
        <>
          <div className="section-title">工作区映射</div>
          {workspaces.map((ws) => {
            const open = wsOpen[ws.id] ?? true;
            return (
              <div className="card flush" key={ws.id}>
                <div
                  className="ws-head"
                  onClick={() => setWsOpen({ ...wsOpen, [ws.id]: !open })}
                >
                  <span className="chev">
                    {open ? <ChevronDown size={14} /> : <ChevronRight size={14} />}
                  </span>
                  <strong>工作区: </strong>
                  <span className="path">
                    {ws.localRoot}&nbsp;&nbsp;⇄&nbsp;&nbsp;{ws.remoteRoot}
                  </span>
                </div>
                {open && (
                  <div className="ws-body">
                    {ws.children.map((c) => (
                      <div className="ws-child" key={c.name}>
                        <span className={`name ${c.status === "disabled" ? "muted" : ""}`}>
                          {c.name}
                        </span>
                        {c.status === "synced" ? (
                          <span className="status-pill synced" style={{ width: 88 }}>
                            <span className="dot online" />
                            已同步
                          </span>
                        ) : c.status === "syncing" ? (
                          <span className="status-pill syncing" style={{ width: 120 }}>
                            <span className="spinner" />
                            同步中 {c.progress ?? 0}%
                          </span>
                        ) : c.status === "conflict" ? (
                          <span className="status-pill conflict" style={{ width: 88 }}>
                            ⚠ 冲突
                          </span>
                        ) : (
                          <span className="status-pill disabled" style={{ width: 88 }}>
                            <span className="dot offline" />
                            未开启
                          </span>
                        )}
                        <span className="faint" style={{ flex: 1 }} />
                        {c.newlyDiscovered && <span className="faint">新发现</span>}
                      </div>
                    ))}
                  </div>
                )}
              </div>
            );
          })}
        </>
      )}
      </>
      )}

      {tab === "history" && (
        <div className="card" style={{ marginTop: 14 }}>
          {history.length === 0 ? (
            <p className="faint" style={{ fontSize: 12 }}>
              暂无同步历史
            </p>
          ) : (
            history.slice(0, 20).map((h, i) => (
              <div className="history-row" key={i}>
                <span>{fmtTime(h.timestamp)}</span>
                <span>{h.childName ? `${h.workspaceName}/${h.childName}` : h.projectId}</span>
                <span>{h.direction === "push" ? "→推送" : "←拉取"}</span>
                {/* ISS-013: 标注手动/自动触发 */}
                <span className="faint">{h.trigger === "auto" ? "自动" : "手动"}</span>
                <span className={h.success ? "ok" : "fail"}>{h.success ? "成功" : "失败"}</span>
                <span>{h.success ? `${h.files}文件 ${fmtBytes(h.bytes)}` : (h.detail ?? "")}</span>
              </div>
            ))
          )}
        </div>
      )}

      <div className="spread" style={{ marginTop: 16 }}>
        <div className="btn-group">
          <button
            disabled={!online}
            title={online ? "" : "设备离线"}
            onClick={() => setDialog({ kind: "batch", peerId, direction: "push" })}
          >
            全部推送
          </button>
          <button
            disabled={!online}
            title={online ? "" : "设备离线"}
            onClick={() => setDialog({ kind: "batch", peerId, direction: "pull" })}
          >
            全部拉取
          </button>
        </div>
        <button className="danger" onClick={() => setDialog({ kind: "unpair", peerId })}>
          解除配对
        </button>
      </div>
      <div style={{ height: 20 }} />
    </div>
  );
}
