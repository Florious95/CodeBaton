import { useEffect, useState } from "react";
import { ipc } from "../ipc";
import { useStore } from "../store";
import type { Peer, Project, SyncHistoryEntry } from "../types";
import { fmtBytes, fmtTime, osLabel } from "../util";
import { ProjectCard } from "../ProjectCard";
import { ChatTab } from "../ChatTab";
import { FileTransferTab } from "../FileTransferTab";

export function PeerDetailPage({ peerId }: { peerId: string }) {
  const { setDialog, unreadChat, unreadFiles, t } = useStore();
  const [peer, setPeer] = useState<Peer | null>(null);
  const [projects, setProjects] = useState<Project[]>([]);
  const [history, setHistory] = useState<SyncHistoryEntry[]>([]);
  const [managed, setManaged] = useState<string | null>(null);
  const [tab, setTab] = useState<"mappings" | "history" | "chat" | "files">("mappings");

  useEffect(() => {
    const load = () =>
      ipc
        .getPeerDetail(peerId)
        // Workspace UI is hidden; ignore the 4th tuple item (backend unchanged).
        .then(([p, pr, h]) => {
          setPeer(p);
          setProjects(pr);
          setHistory(h);
        })
        .catch(() => {});
    load();
    const timer = setInterval(load, 3000);
    return () => clearInterval(timer);
  }, [peerId]);

  if (!peer) return <div className="empty">{t.peerNotFound}</div>;
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
        <span>{t.statusLabel}</span>
        {/* ISS-373: 在线点用品牌绿 #3ecf8e，离线用空心灰点 */}
        <span className="status">
          <span className={`dot ${online ? "online" : "offline"}`} />
          {online ? t.online : t.offline}
        </span>
        {pairedAt && <span className="faint">{t.pairedAt(fmtTime(pairedAt))}</span>}
      </p>

      {/* ISS-013: Tab 页签 —「映射关系」｜「同步历史」 */}
      <div className="tabs">
        <button
          className={`tab ${tab === "mappings" ? "active" : ""}`}
          onClick={() => setTab("mappings")}
        >
          {t.tabMappings}
        </button>
        <button
          className={`tab ${tab === "history" ? "active" : ""}`}
          onClick={() => setTab("history")}
        >
          {t.tabHistory}
        </button>
        <button
          className={`tab ${tab === "chat" ? "active" : ""}`}
          onClick={() => setTab("chat")}
        >
          {t.tabChat}
          {(unreadChat[peer.name] ?? 0) > 0 && (
            <span className="badge">{unreadChat[peer.name]}</span>
          )}
        </button>
        <button
          className={`tab ${tab === "files" ? "active" : ""}`}
          onClick={() => setTab("files")}
        >
          {t.tabFiles}
          {(unreadFiles[peer.name] ?? 0) > 0 && (
            <span className="badge">{unreadFiles[peer.name]}</span>
          )}
        </button>
      </div>

      {tab === "chat" && <ChatTab peerName={peer.name} online={online} />}
      {tab === "files" && <FileTransferTab peerName={peer.name} online={online} />}

      {tab === "mappings" && (
      <>
      <div className="section-title">{t.claudeMap}</div>
      <div className="card">
        <div className="detail-grid">
          <span className="label">{t.localM}</span>
          <span className="path">~/.claude/</span>
          <span />
          <span className="label">{t.remoteM}</span>
          <span className="path">{remoteSessionDir}</span>
          <span />
        </div>
      </div>

      <div className="section-title">{t.projMap}</div>
      {projects.length === 0 ? (
        <p className="faint" style={{ fontSize: 12 }}>
          {t.noProjMap}
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
                      {p.status === "synced" ? t.pillSynced : p.status === "syncing" ? t.pillSyncing(p.progress ?? 0) : t.pillIdle}
                    </span>
                  </div>
                </div>
                <button
                  className="tiny"
                  style={{ flex: "none", whiteSpace: "nowrap" }}
                  onClick={() => setManaged(p.id)}
                >
                  {t.manage}
                </button>
              </div>
            </div>
          ),
        )
      )}
      <div className="btn-group" style={{ justifyContent: "flex-end" }}>
        <button onClick={() => setDialog({ kind: "addProject" })}>{"+ "}{t.addProjMap}</button>
      </div>
      </>
      )}

      {tab === "history" && (
        <div className="card" style={{ marginTop: 14 }}>
          {history.length === 0 ? (
            <p className="faint" style={{ fontSize: 12 }}>
              {t.noSyncHistory}
            </p>
          ) : (
            history.slice(0, 20).map((h, i) => (
              <div className="history-row" key={i}>
                <span>{fmtTime(h.timestamp)}</span>
                <span>{h.childName ? `${h.workspaceName}/${h.childName}` : h.projectId}</span>
                <span>{h.direction === "push" ? t.histPush : t.histPull}</span>
                {/* ISS-013: 标注手动/自动触发 */}
                <span className="faint">{h.trigger === "auto" ? t.trigAuto : t.trigManual}</span>
                <span className={h.success ? "ok" : "fail"}>{h.success ? t.success : t.failed}</span>
                <span>{h.success ? t.histFiles(h.files, fmtBytes(h.bytes)) : (h.detail ?? "")}</span>
              </div>
            ))
          )}
        </div>
      )}

      {/* ISS-016: 全部推送/拉取/解除配对 仅在 映射关系 / 同步历史 Tab 显示，
          对话/文件传输 Tab 不显示。 */}
      {(tab === "mappings" || tab === "history") && (
        <div className="spread" style={{ marginTop: 16 }}>
          <div className="btn-group">
            <button
              disabled={!online}
              title={online ? "" : t.deviceOffline}
              onClick={() => setDialog({ kind: "batch", peerId })}
            >
              {t.pushAll}
            </button>
          </div>
          <button className="danger" onClick={() => setDialog({ kind: "unpair", peerId })}>
            {t.unpair}
          </button>
        </div>
      )}
      <div style={{ height: 20 }} />
    </div>
  );
}
