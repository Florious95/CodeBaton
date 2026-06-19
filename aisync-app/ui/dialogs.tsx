import { useEffect, useState } from "react";
import {
  AlertTriangle,
  CheckCircle2,
  FolderSearch,
  Link2,
  ShieldAlert,
  XCircle,
} from "lucide-react";
import { Dialog } from "./Dialog";
import { ipc } from "./ipc";
import { pushToast, useStore } from "./store";
import type {
  BatchPlan,
  Conflict,
  Pairing,
  ProjectMappingRequest,
  RewriteReport,
  ScannedChild,
  WorkspaceMappingRequest,
} from "./types";
import { fmtBytes, osLabel } from "./util";

// ── D1: Add single-project mapping ───────────────────────────────────
function AddProjectDialog() {
  const { setDialog, refresh } = useStore();
  // Real paired peers from getPeers() — not reverse-derived from existing
  // projects (which is empty on the first add). Peer NAME is the config key the
  // backend maps projects under, so we use it as the option value.
  const [peers, setPeers] = useState<{ id: string; name: string }[]>([]);
  const [name, setName] = useState("");
  const [localDir, setLocalDir] = useState("");
  const [peer, setPeer] = useState("");
  const [mode, setMode] = useState("twoWayAuto");
  const [tool, setTool] = useState("same");
  const valid = localDir.trim() && peer.trim();

  useEffect(() => {
    ipc.uiLog("add_project_dialog_opened");
    ipc
      .getPeers()
      .then((ps) => {
        const real = ps
          .filter((p) => p.kind !== "local")
          .map((p) => ({ id: p.id, name: p.name }));
        setPeers(real);
        if (real[0]) setPeer(real[0].name);
        ipc.uiLog(`add_project_peers_loaded count=${real.length}`);
      })
      .catch((e) => ipc.uiLog(`add_project_peers_load_failed error=${String(e)}`));
  }, []);

  return (
    <Dialog
      title="添加项目映射"
      width={520}
      onClose={() => setDialog(null)}
      footer={
        <>
          <button onClick={() => setDialog(null)}>取消</button>
          <button
            className="primary"
            disabled={!valid}
            onClick={async () => {
              const submit = async (createLocalDir: boolean) => {
                ipc.uiLog(
                  `add_project_submit peer=${peer} localDir=${localDir} createLocalDir=${createLocalDir}`,
                );
                await ipc.addProject({ name, localDir, peer, mode, tool, createLocalDir });
              };
              try {
                await submit(false);
                ipc.uiLog("add_project_request_sent");
                pushToast("已发送项目映射请求，等待对端选择目录");
                setDialog(null);
              } catch (e) {
                const msg = String(e);
                // 本机目录不存在 → 提示，点确定才新建后重试。
                if (msg.includes("local-dir-missing:")) {
                  ipc.uiLog("add_project_local_dir_missing");
                  if (window.confirm(`本机目录不存在：\n${localDir}\n\n是否创建该目录并继续添加？`)) {
                    try {
                      await submit(true);
                      ipc.uiLog("add_project_request_sent_after_mkdir");
                      await refresh();
                      pushToast("已创建目录并发送项目映射请求");
                      setDialog(null);
                    } catch (e2) {
                      const m2 = String(e2);
                      ipc.uiLog(`add_project_failed error=${m2}`);
                      pushToast(`添加失败：${m2}`);
                    }
                  }
                } else {
                  ipc.uiLog(`add_project_failed error=${msg}`);
                  pushToast(`添加失败：${msg}`);
                }
              }
            }}
          >
            添加
          </button>
        </>
      }
    >
      <div className="field">
        <label>项目名称</label>
        <input value={name} onChange={(e) => setName(e.target.value)} placeholder="可选，留空则使用目录名" />
      </div>
      <div className="field">
        <label>本机目录</label>
        <div className="row">
          <input value={localDir} onChange={(e) => setLocalDir(e.target.value)} placeholder="点「浏览」选择本机项目目录" />
          <button
            onClick={async () => {
              ipc.uiLog("browse_clicked dialog=add_project");
              const dir = await ipc.pickDirectory().catch(() => null);
              if (dir) {
                setLocalDir(dir);
                ipc.uiLog(`path_selected dir=${dir}`);
              }
            }}
          >浏览</button>
        </div>
      </div>
      <div className="field">
        <label>目标设备</label>
        <select value={peer} onChange={(e) => setPeer(e.target.value)}>
          {peers.length === 0 && <option value="">（无配对设备 — 请先配对）</option>}
          {peers.map((p) => (
            <option key={p.id} value={p.name}>
              {p.name}
            </option>
          ))}
        </select>
      </div>
      <div className="field">
        <label>同步模式</label>
        {[
          ["twoWayAuto", "双向自动同步"],
          ["oneWayPush", "单向推送（本机 → 对端）"],
          ["oneWayPull", "单向推送（对端 → 本机）"],
        ].map(([v, l]) => (
          <label className="radio" key={v}>
            <input type="radio" checked={mode === v} onChange={() => setMode(v)} />
            {l}
          </label>
        ))}
      </div>
      <div className="field">
        <label>目标 AI 工具</label>
        <select value={tool} onChange={(e) => setTool(e.target.value)}>
          <option value="same">Claude Code (同工具)</option>
          <option value="codex">转换为 Codex</option>
          <option value="gemini">转换为 Gemini CLI</option>
        </select>
      </div>
    </Dialog>
  );
}

// ── D2: Add workspace mapping ────────────────────────────────────────
// Suggested remote-root default for a peer based on its OS (item 5).
function defaultRemoteRoot(os: string): string {
  return os === "windows" ? "D:\\projects" : "~/projects";
}

function AddWorkspaceDialog() {
  const { setDialog, refresh } = useStore();
  const [name, setName] = useState("");
  const [localRoot, setLocalRoot] = useState("");
  // Real paired peers (excludes the local machine); empty on first run. Track
  // os so the remote-root default matches the peer platform.
  const [pairedPeers, setPairedPeers] = useState<{ id: string; name: string; os: string }[]>([]);
  const [peer, setPeer] = useState("");
  const [remoteRoot, setRemoteRoot] = useState("");
  const [children, setChildren] = useState<ScannedChild[]>([]);
  const [mode, setMode] = useState("twoWayAuto");
  const [autoEnable, setAutoEnable] = useState(false);

  useEffect(() => {
    ipc
      .getPeers()
      .then((ps) => {
        const real = ps
          .filter((p) => p.kind !== "local")
          .map((p) => ({ id: p.id, name: p.name, os: p.os }));
        setPairedPeers(real);
        if (real[0]) {
          setPeer(real[0].name);
          // Seed the remote root from the first peer's OS unless user typed one.
          setRemoteRoot((cur) => cur || defaultRemoteRoot(real[0].os));
        }
      })
      .catch(() => {});
  }, []);

  const scan = async () => {
    // Open the native picker, then scan the chosen LOCAL directory for its
    // first-level subdirectories. Does not require a peer — scanning is purely
    // a local filesystem listing (remoteRoot only annotates matched hints).
    const dir = await ipc.pickDirectory().catch(() => null);
    const root = dir ?? localRoot;
    if (dir) {
      setLocalRoot(dir);
      // 对端根目录默认 = 本机根目录（参考值，可改）。
      if (!remoteRoot.trim()) setRemoteRoot(dir);
    }
    if (!root.trim()) return;
    ipc.uiLog(`workspace_browse_scan root=${root}`);
    const r = await ipc.scanWorkspace(root, remoteRoot.trim() || root).catch(() => []);
    ipc.uiLog(`workspace_scan_returned count=${r.length}`);
    // Default every scanned child to selected so the list is immediately
    // actionable (otherwise all-unchecked disables the 添加 button).
    setChildren(r.map((c) => ({ ...c, selected: true })));
  };

  return (
    <Dialog
      title="添加工作区映射"
      width={560}
      onClose={() => setDialog(null)}
      footer={
        <>
          <button onClick={() => setDialog(null)}>取消</button>
          <button
            className="primary"
            disabled={!localRoot.trim() || !peer.trim() || children.filter((c) => c.selected).length === 0}
            onClick={async () => {
              const sel = children.filter((c) => c.selected).length;
              ipc.uiLog(`add_workspace_submit localRoot=${localRoot} peer=${peer} selected=${sel}`);
              try {
                await ipc.addWorkspace({ name, localRoot, peer, remoteRoot, mode, autoEnable, children });
                ipc.uiLog("add_workspace_saved");
                await refresh();
                pushToast(`已添加工作区（${sel} 个子项目）`);
                setDialog(null);
              } catch (e) {
                const msg = String(e);
                ipc.uiLog(`add_workspace_failed error=${msg}`);
                pushToast(`添加工作区失败：${msg}`);
              }
            }}
          >
            添加
          </button>
        </>
      }
    >
      <div className="field">
        <label>工作区名称</label>
        <input value={name} onChange={(e) => setName(e.target.value)} />
      </div>
      <div className="field">
        <label>本机根目录</label>
        <div className="row">
          <input value={localRoot} onChange={(e) => setLocalRoot(e.target.value)} placeholder="~/projects" />
          <button onClick={scan}>浏览</button>
        </div>
      </div>
      <div className="field">
        <label>目标设备</label>
        <select
          value={peer}
          onChange={(e) => {
            const name = e.target.value;
            setPeer(name);
            // Reset the remote-root suggestion to match the newly selected
            // peer's OS (macOS → ~/projects, Windows → D:\projects).
            const os = pairedPeers.find((p) => p.name === name)?.os ?? "";
            setRemoteRoot(defaultRemoteRoot(os));
          }}
        >
          {pairedPeers.length === 0 && <option value="">（无配对设备）</option>}
          {pairedPeers.map((p) => (
            <option key={p.id} value={p.name}>
              {p.name}
            </option>
          ))}
        </select>
      </div>
      {/* ISS-004: 「对端根目录」输入框已移除——有对端确认流程后，对端目录由对端
          自己在确认弹窗里选；主方不需要填。 */}
      <div className="section-title">扫描到的子项目{children.length > 0 ? `（${children.length}）` : ""}</div>
      {children.length === 0 ? (
        <p className="faint" style={{ fontSize: 12 }}>
          选择本机根目录后点「浏览」自动扫描
        </p>
      ) : (
        children.map((c, i) => (
          <label className="check" key={c.localName}>
            <input
              type="checkbox"
              checked={c.selected}
              onChange={() => {
                const next = [...children];
                next[i] = { ...c, selected: !c.selected };
                setChildren(next);
              }}
            />
            <span className="path">
              {c.localName}/ ↔ {c.remoteName}/
            </span>
          </label>
        ))
      )}
      <div className="field" style={{ marginTop: 14 }}>
        <label>默认同步模式</label>
        <select value={mode} onChange={(e) => setMode(e.target.value)}>
          <option value="twoWayAuto">双向自动</option>
          <option value="oneWayPush">单向推送</option>
        </select>
      </div>
      <div className="field">
        <label>新子项目</label>
        <label className="radio">
          <input type="radio" checked={autoEnable} onChange={() => setAutoEnable(true)} />
          自动开启同步
        </label>
        <label className="radio">
          <input type="radio" checked={!autoEnable} onChange={() => setAutoEnable(false)} />
          手动确认后开启
        </label>
      </div>
    </Dialog>
  );
}

// ── D3: Enable a workspace child ─────────────────────────────────────
function EnableChildDialog({ workspaceId, child }: { workspaceId: string; child: string }) {
  const { setDialog, overview } = useStore();
  const ws = overview?.workspaces.find((w) => w.id === workspaceId);
  const c = ws?.children.find((x) => x.name === child);
  const [peer, setPeer] = useState(ws?.peerName ?? "");
  const [remote, setRemote] = useState(c?.remoteDir ?? "");
  const [mode, setMode] = useState("twoWayAuto");

  return (
    <Dialog
      title="开启子项目同步"
      width={440}
      onClose={() => setDialog(null)}
      footer={
        <>
          <button onClick={() => setDialog(null)}>取消</button>
          <button
            className="primary"
            onClick={async () => {
              await ipc.enableChild(workspaceId, child, { peer, remote, mode }).catch(() => {});
              pushToast(`已开启 ${child} 同步`);
              setDialog(null);
            }}
          >
            开启
          </button>
        </>
      }
    >
      <div className="detail-grid">
        <span className="label">子项目</span>
        <span>{child}</span>
        <span />
        <span className="label">本机路径</span>
        <span className="path">{c?.localDir}</span>
        <span />
      </div>
      <div className="field">
        <label>目标设备</label>
        <select value={peer} onChange={(e) => setPeer(e.target.value)}>
          {peer ? <option value={peer}>{peer}</option> : <option value="">（无配对设备）</option>}
        </select>
      </div>
      <div className="field">
        <label>对端路径</label>
        <input value={remote} onChange={(e) => setRemote(e.target.value)} />
        <div className="hint">基于工作区映射自动填充，可修改</div>
      </div>
      <div className="field">
        <label>同步模式</label>
        <select value={mode} onChange={(e) => setMode(e.target.value)}>
          <option value="twoWayAuto">双向自动</option>
          <option value="oneWayPush">单向推送</option>
        </select>
      </div>
    </Dialog>
  );
}

// ── D4: Pairing confirmation (initiator view) ────────────────────────
function PairingDialog({ peerId }: { peerId: string }) {
  const { setDialog, refresh } = useStore();
  const [pairing, setPairing] = useState<Pairing | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    // Logged via the backend so the IPC call site is visible in aisync.log.
    ipc.uiLog(`pairing_dialog_mounted, calling beginPairing peerId=${peerId}`);
    ipc
      .beginPairing(peerId)
      .then((p) => {
        ipc.uiLog(`beginPairing resolved peerId=${peerId} code=${p.code}`);
        setPairing(p);
        setError(null);
      })
      .catch((e) => {
        const msg = String(e);
        ipc.uiLog(`beginPairing threw peerId=${peerId} error=${msg}`);
        setError(msg);
        pushToast(`配对失败：${msg}`);
      });
  }, [peerId]);

  return (
    <Dialog
      title="配对请求已发送"
      icon={<Link2 size={18} />}
      width={400}
      onClose={() => {
        ipc.cancelPairing(peerId);
        setDialog(null);
      }}
      footer={
        <>
          <button
            onClick={() => {
              ipc.cancelPairing(peerId);
              setDialog(null);
            }}
          >
            取消配对
          </button>
          <button
            className="primary"
            disabled={!pairing}
            onClick={async () => {
              ipc.uiLog(`confirm_pairing clicked peerId=${peerId}`);
              try {
                await ipc.confirmPairing(peerId);
                ipc.uiLog(`confirmPairing resolved peerId=${peerId}`);
                await refresh();
                pushToast(`已与 ${pairing?.peerName} 配对`);
                setDialog(null);
              } catch (e) {
                const msg = String(e);
                ipc.uiLog(`confirmPairing threw peerId=${peerId} error=${msg}`);
                pushToast(`确认配对失败：${msg}`);
              }
            }}
          >
            确认配对
          </button>
        </>
      }
    >
      {error ? (
        <p className="muted" style={{ textAlign: "center", marginBottom: 10, color: "var(--red)" }}>
          配对失败：{error}
        </p>
      ) : (
        <p className="muted" style={{ textAlign: "center", marginBottom: 10 }}>
          {pairing ? "正在等待对方确认..." : "正在获取配对码..."}
        </p>
      )}
      <div className="detail-grid">
        <span className="label">目标设备</span>
        <span>{pairing?.peerName}</span>
        <span />
        <span className="label">IP 地址</span>
        <span className="path">{pairing?.peerIp}</span>
        <span />
        <span className="label">操作系统</span>
        <span>{osLabel(pairing?.peerOs ?? "")}</span>
        <span />
      </div>
      <p className="muted" style={{ fontSize: 12 }}>
        请确认对方设备上显示的配对码：
      </p>
      <div className="pairing-code">{pairing?.code ?? "····"}</div>
      <p className="faint" style={{ fontSize: 11, textAlign: "center" }}>
        两端必须显示相同的配对码才能配对
      </p>
    </Dialog>
  );
}

function ProjectMappingRequestDialog({ request }: { request: ProjectMappingRequest }) {
  const { setDialog, refresh } = useStore();
  // 默认填发起端发来的路径（两端目录结构通常一致），用户可改。目录不存在时
  // 点确认会由后端 mkdir -p 自动创建。
  const [localDir, setLocalDir] = useState(request.sourceDir ?? "");
  const [busy, setBusy] = useState(false);
  const valid = localDir.trim().length > 0;

  return (
    <Dialog
      title="项目映射请求"
      icon={<FolderSearch size={18} />}
      width={520}
      onClose={() => setDialog(null)}
      footer={
        <>
          <button disabled={busy} onClick={() => setDialog(null)}>
            稍后处理
          </button>
          <button
            className="primary"
            disabled={!valid || busy}
            onClick={async () => {
              setBusy(true);
              try {
                ipc.uiLog(
                  `confirm_project_mapping clicked requestId=${request.requestId} localDir=${localDir}`,
                );
                await ipc.confirmProjectMappingRequest(request.requestId, localDir);
                await refresh();
                pushToast("已确认项目映射");
                setDialog(null);
              } catch (e) {
                const msg = String(e);
                ipc.uiLog(
                  `confirm_project_mapping failed requestId=${request.requestId} error=${msg}`,
                );
                pushToast(`确认失败：${msg}`);
              } finally {
                setBusy(false);
              }
            }}
          >
            确认映射
          </button>
        </>
      }
    >
      <div className="detail-grid">
        <span className="label">发起设备</span>
        <span>{request.peerName}</span>
        <span />
        <span className="label">项目名称</span>
        <span>{request.projectName}</span>
        <span />
        <span className="label">对端目录</span>
        <span className="path">{request.sourceDir}</span>
        <span />
      </div>
      <div className="field">
        <label>本机安放目录</label>
        <div className="row">
          <input
            value={localDir}
            onChange={(e) => setLocalDir(e.target.value)}
            placeholder="选择本机用于同步该项目的目录"
          />
          <button
            disabled={busy}
            onClick={async () => {
              ipc.uiLog("browse_clicked dialog=project_mapping_request");
              const dir = await ipc.pickDirectory().catch(() => null);
              if (dir) {
                setLocalDir(dir);
                ipc.uiLog(`project_mapping_path_selected dir=${dir}`);
              }
            }}
          >
            浏览
          </button>
        </div>
      </div>
    </Dialog>
  );
}

function WorkspaceMappingRequestDialog({ request }: { request: WorkspaceMappingRequest }) {
  const { setDialog, refresh } = useStore();
  const [localRoot, setLocalRoot] = useState(request.suggestedRemoteRoot ?? request.sourceRoot ?? "");
  const [busy, setBusy] = useState(false);
  const valid = localRoot.trim().length > 0;

  return (
    <Dialog
      title="工作区映射请求"
      icon={<FolderSearch size={18} />}
      width={560}
      onClose={() => setDialog(null)}
      footer={
        <>
          <button disabled={busy} onClick={() => setDialog(null)}>
            稍后处理
          </button>
          <button
            className="primary"
            disabled={!valid || busy}
            onClick={async () => {
              setBusy(true);
              try {
                ipc.uiLog(
                  `confirm_workspace_mapping clicked requestId=${request.requestId} localRoot=${localRoot}`,
                );
                await ipc.confirmWorkspaceMappingRequest(request.requestId, localRoot);
                await refresh();
                pushToast("已确认工作区映射");
                setDialog(null);
              } catch (e) {
                const msg = String(e);
                ipc.uiLog(
                  `confirm_workspace_mapping failed requestId=${request.requestId} error=${msg}`,
                );
                pushToast(`确认失败：${msg}`);
              } finally {
                setBusy(false);
              }
            }}
          >
            确认映射
          </button>
        </>
      }
    >
      <div className="detail-grid">
        <span className="label">发起设备</span>
        <span>{request.peerName}</span>
        <span />
        <span className="label">工作区</span>
        <span>{request.workspaceName}</span>
        <span />
        <span className="label">对端根目录</span>
        <span className="path">{request.sourceRoot}</span>
        <span />
        <span className="label">子项目</span>
        <span>{request.children.length}</span>
        <span />
      </div>
      <div className="field">
        <label>本机根目录</label>
        <div className="row">
          <input
            value={localRoot}
            onChange={(e) => setLocalRoot(e.target.value)}
            placeholder="选择本机用于同步该工作区的根目录"
          />
          <button
            disabled={busy}
            onClick={async () => {
              ipc.uiLog("browse_clicked dialog=workspace_mapping_request");
              const dir = await ipc.pickDirectory().catch(() => null);
              if (dir) {
                setLocalRoot(dir);
                ipc.uiLog(`workspace_mapping_path_selected dir=${dir}`);
              }
            }}
          >
            浏览
          </button>
        </div>
      </div>
      {request.children.length > 0 && (
        <div className="field">
          <label>子项目</label>
          <p className="path">{request.children.join(", ")}</p>
        </div>
      )}
    </Dialog>
  );
}

// ── D5: Split-brain conflict ─────────────────────────────────────────
function ConflictDialog({ projectId }: { projectId: string }) {
  const { setDialog, refresh } = useStore();
  const [conflict, setConflict] = useState<Conflict | null>(null);
  const [choice, setChoice] = useState<string>("");
  useEffect(() => {
    ipc.getConflict(projectId).then(setConflict).catch(() => {});
  }, [projectId]);
  const destructive = choice === "local" || choice === "remote";

  return (
    <Dialog
      title="检测到冲突"
      icon={<AlertTriangle size={18} color="var(--amber)" />}
      width={560}
      closeOnOverlay={false}
      onClose={() => setDialog(null)}
      footer={
        <>
          <button onClick={() => setDialog(null)}>取消</button>
          <button
            className={destructive ? "danger" : "primary"}
            disabled={!choice}
            onClick={async () => {
              await ipc.resolveConflict(projectId, choice).catch(() => {});
              await refresh();
              setDialog(null);
            }}
          >
            {destructive ? "确认覆盖" : "执行"}
          </button>
        </>
      }
    >
      <p style={{ marginBottom: 8 }}>
        项目 “{conflict?.projectName}” 的两端都有未同步的变更，无法自动同步。
      </p>
      <div className="conflict-cols">
        {[conflict?.local, conflict?.remote].map((side, i) => (
          <div className="conflict-col" key={i}>
            <h4>{side?.deviceName}</h4>
            <p className="muted" style={{ fontSize: 11, marginBottom: 6 }}>
              上次同步后修改: {side?.changedFiles} 个文件
            </p>
            {side?.files.map((f) => (
              <div className="file" key={f.path}>
                <span>{f.path}</span>
                <span>{f.change}</span>
              </div>
            ))}
            <p className="faint" style={{ fontSize: 11, marginTop: 6 }}>
              会话: {side?.sessionSummary}
            </p>
          </div>
        ))}
      </div>
      <p className="muted" style={{ fontSize: 12, marginTop: 6 }}>
        请选择如何处理：
      </p>
      {[
        ["local", "以本机为准（对端变更将被覆盖）"],
        ["remote", "以对端为准（本机变更将被覆盖）"],
        ["none", "暂不处理（保持两端各自的状态）"],
      ].map(([v, l]) => (
        <label className="radio" key={v}>
          <input type="radio" checked={choice === v} onChange={() => setChoice(v)} />
          {l}
        </label>
      ))}
      {destructive && (
        <div className="warn-box">⚠ 被覆盖的一方变更将不可恢复，建议先手动备份</div>
      )}
    </Dialog>
  );
}

// ── D6: Batch sync confirmation (G6 sensitive-file opt-in) ───────────
function BatchDialog({ peerId, direction }: { peerId: string; direction: "push" | "pull" }) {
  const { setDialog } = useStore();
  const [plan, setPlan] = useState<BatchPlan | null>(null);
  const [selected, setSelected] = useState<Record<string, boolean>>({});
  const [sensitiveOptIn, setSensitiveOptIn] = useState<Record<string, boolean>>({});
  useEffect(() => {
    ipc.getBatchPlan(peerId, direction).then((p) => {
      setPlan(p);
      const sel: Record<string, boolean> = {};
      p.items.forEach((i) => (sel[i.projectId] = !i.upToDate));
      setSelected(sel);
    });
  }, [peerId, direction]);

  const verb = direction === "pull" ? "拉取" : "推送";
  const chosen = (plan?.items ?? []).filter((i) => selected[i.projectId] && !i.upToDate);
  const totalFiles = chosen.reduce((s, i) => s + i.changedFiles, 0);
  const totalBytes = chosen.reduce((s, i) => s + i.bytes, 0);

  return (
    <Dialog
      title={`批量${verb}确认`}
      width={480}
      onClose={() => setDialog(null)}
      footer={
        <>
          <button onClick={() => setDialog(null)}>取消</button>
          <button
            className="primary"
            onClick={async () => {
              // G6: collect confirmed sensitive files per project, stripping the
              // "{projectName}/" prefix to get the relative path the backend
              // matches against. Unconfirmed files stay excluded.
              const confirmedFor = (projectId: string) =>
                Object.keys(sensitiveOptIn)
                  .filter((k) => sensitiveOptIn[k] && k.startsWith(`${projectId}/`))
                  .map((k) => k.slice(projectId.length + 1));
              for (const item of chosen) {
                await ipc.startSync(item.projectId, direction, confirmedFor(item.projectId)).catch(() => {});
              }
              setDialog({ kind: "syncProgress" });
            }}
          >
            开始{verb}
          </button>
        </>
      }
    >
      <p className="muted" style={{ marginBottom: 10 }}>
        即将{direction === "pull" ? `从 ${plan?.peerName}` : `向 ${plan?.peerName}`}{verb}以下项目：
      </p>
      {plan?.items.map((i) => (
        <label className="check" key={i.projectId}>
          <input
            type="checkbox"
            disabled={i.upToDate}
            checked={!!selected[i.projectId] && !i.upToDate}
            onChange={() => setSelected({ ...selected, [i.projectId]: !selected[i.projectId] })}
          />
          <span style={{ flex: 1 }}>{i.name}</span>
          <span className="muted" style={{ fontSize: 11 }}>
            {i.upToDate ? "(已是最新)" : `${i.changedFiles} 个文件变更  ~${fmtBytes(i.bytes)}`}
          </span>
        </label>
      ))}
      <div className="section-title">总计</div>
      <p className="muted" style={{ fontSize: 12 }}>
        {totalFiles} 个文件  约 {fmtBytes(totalBytes)}
      </p>

      {plan && plan.sensitiveFiles.length > 0 && (
        <div className="warn-box" style={{ marginTop: 12 }}>
          <div className="row" style={{ gap: 6 }}>
            <ShieldAlert size={14} />
            <strong>以下文件匹配敏感文件模式：</strong>
          </div>
          {plan.sensitiveFiles.map((f) => (
            <label className="check" key={f}>
              <input
                type="checkbox"
                checked={!!sensitiveOptIn[f]}
                onChange={() => setSensitiveOptIn({ ...sensitiveOptIn, [f]: !sensitiveOptIn[f] })}
              />
              <span className="path" style={{ color: "var(--amber)" }}>
                包含此文件 — {f}
              </span>
            </label>
          ))}
        </div>
      )}
    </Dialog>
  );
}

// ── D7: Edit exclude rules ───────────────────────────────────────────
function ExcludeRulesDialog({ projectId }: { projectId: string }) {
  const { setDialog, overview } = useStore();
  const project = overview?.projects.find((p) => p.id === projectId);
  const [rules, setRules] = useState((project?.excludeRules ?? []).join("\n"));

  return (
    <Dialog
      title={`排除规则 — ${project?.name ?? ""}`}
      width={480}
      onClose={() => setDialog(null)}
      footer={
        <>
          <button onClick={() => setDialog(null)}>取消</button>
          <button
            className="primary"
            onClick={async () => {
              await ipc.saveExcludeRules(projectId, rules.split("\n").filter(Boolean)).catch(() => {});
              pushToast("已保存排除规则");
              setDialog(null);
            }}
          >
            保存
          </button>
        </>
      }
    >
      <div className="section-title">全局规则（从设置继承，只读）</div>
      <p className="path" style={{ marginBottom: 10 }}>
        node_modules/ .git/objects/ target/ __pycache__/ .next/ dist/ build/ .DS_Store
      </p>
      <div className="section-title">项目专属规则</div>
      <textarea rows={6} value={rules} onChange={(e) => setRules(e.target.value)} />
      <div className="hint">每行一条 glob 模式</div>
    </Dialog>
  );
}

// ── D8: Unpair confirmation ──────────────────────────────────────────
function UnpairDialog({ peerId }: { peerId: string }) {
  const { setDialog, setView, refresh, overview } = useStore();
  const name =
    overview?.projects.find((p) => p.peerId === peerId)?.peerName ?? "该设备";
  return (
    <Dialog
      title="解除配对"
      width={420}
      closeOnOverlay={false}
      onClose={() => setDialog(null)}
      footer={
        <>
          <button onClick={() => setDialog(null)}>取消</button>
          <button
            className="danger"
            onClick={async () => {
              await ipc.unpair(peerId).catch(() => {});
              await refresh();
              setView({ page: "overview" });
              setDialog(null);
            }}
          >
            解除配对
          </button>
        </>
      }
    >
      <p style={{ marginBottom: 10 }}>确定要解除与 {name} 的配对吗？</p>
      <p className="muted" style={{ fontSize: 12, lineHeight: 1.8 }}>
        解除后：
        <br />• 该设备的所有项目映射将被删除
        <br />• 已同步到对端的文件不会被删除
        <br />• 如需重新配对，需要再次确认配对码
      </p>
    </Dialog>
  );
}

// ── D9: Sync progress + result view ──────────────────────────────────
function SyncProgressDialog() {
  const { setDialog, syncProgress, lastResult, clearResult } = useStore();

  if (lastResult) {
    const ok = lastResult.success;
    return (
      <Dialog
        title={ok ? "同步完成" : "同步失败"}
        icon={ok ? <CheckCircle2 size={18} color="var(--accent)" /> : <XCircle size={18} color="var(--red)" />}
        width={480}
        onClose={() => {
          clearResult();
          setDialog(null);
        }}
        footer={
          <button
            className="primary"
            onClick={() => {
              clearResult();
              setDialog(null);
            }}
          >
            关闭
          </button>
        }
      >
        <p style={{ marginBottom: 12 }}>
          {ok ? "✓" : "✗"} {lastResult.direction}
        </p>
        {ok ? (
          <>
            <div className="detail-grid">
              <span className="label">传输文件</span>
              <span>{lastResult.files} 个</span>
              <span />
              <span className="label">传输数据</span>
              <span>{fmtBytes(lastResult.bytes)}</span>
              <span />
              <span className="label">耗时</span>
              <span>{lastResult.elapsedSecs} 秒</span>
              <span />
              <span className="label">路径重写</span>
              <span>{lastResult.rewrittenPaths} 处</span>
              <span />
            </div>
            {lastResult.skippedPaths > 0 && (
              <div className="warn-box">
                ⚠ {lastResult.skippedPaths} 处路径未能确定是否需要重写（低置信度），已跳过{" "}
                <button
                  className="ghost tiny"
                  onClick={() => setDialog({ kind: "rewriteReport", projectId: lastResult.projectId })}
                >
                  查看详情 →
                </button>
              </div>
            )}
          </>
        ) : (
          <div className="warn-box">{lastResult.error ?? "未知错误"}</div>
        )}
      </Dialog>
    );
  }

  const p = syncProgress;
  return (
    <Dialog
      title="同步进行中"
      width={480}
      closeOnOverlay={false}
      onClose={() => setDialog(null)}
      footer={
        <>
          <button onClick={() => setDialog(null)}>最小化到后台</button>
          <button
            className="danger"
            onClick={() => {
              if (p) ipc.cancelSync(p.projectId);
              setDialog(null);
            }}
          >
            取消同步
          </button>
        </>
      }
    >
      <p style={{ marginBottom: 10 }}>{p?.direction ?? "准备中..."}</p>
      <p className="muted" style={{ fontSize: 12, marginBottom: 6 }}>
        阶段: {p?.phase}
      </p>
      <div className="bar">
        <div style={{ width: `${p?.percent ?? 0}%` }} />
      </div>
      <div className="detail-grid" style={{ marginTop: 14 }}>
        <span className="label">已传输</span>
        <span>
          {p?.filesDone ?? 0} / {p?.filesTotal ?? 0} 个文件
        </span>
        <span />
        <span className="label">数据量</span>
        <span>
          {fmtBytes(p?.bytesDone ?? 0)} / {fmtBytes(p?.bytesTotal ?? 0)}
        </span>
        <span />
        <span className="label">速度</span>
        <span>{fmtBytes(p?.speedBps ?? 0)}/s</span>
        <span />
        <span className="label">预计剩余</span>
        <span>~{p?.etaSecs ?? 0} 秒</span>
        <span />
      </div>
      {p?.currentFile && (
        <p className="path" style={{ marginTop: 8 }}>
          当前: {p.currentFile}
        </p>
      )}
      <div className="section-title">阶段进度</div>
      {p?.stages.map((s) => (
        <div className="row" key={s.name} style={{ padding: "3px 0" }}>
          <span style={{ width: 18 }}>{s.done ? "✓" : s.active ? "◐" : "○"}</span>
          <span style={{ flex: 1, color: s.active ? "var(--text)" : "var(--text-dim)" }}>{s.name}</span>
          {s.active && <span className="muted">{s.percent}%</span>}
        </div>
      ))}
    </Dialog>
  );
}

// ── D10: Path-rewrite report (G7) ────────────────────────────────────
function RewriteReportDialog({ projectId }: { projectId: string }) {
  const { setDialog } = useStore();
  const [report, setReport] = useState<RewriteReport | null>(null);
  useEffect(() => {
    ipc.getRewriteReport(projectId).then(setReport).catch(() => {});
  }, [projectId]);

  return (
    <Dialog
      title="路径重写报告"
      width={640}
      onClose={() => setDialog(null)}
      footer={<button className="primary" onClick={() => setDialog(null)}>关闭</button>}
    >
      <p className="muted" style={{ fontSize: 12 }}>
        {report?.projectName}  {report?.timestamp}  {report?.direction}
      </p>
      <div className="section-title">已重写 ({report?.rewritten.length ?? 0} 处)</div>
      {report?.rewritten.map((r, i) => (
        <div className="rewrite-entry" key={i}>
          <div className="loc">
            {r.location}  {r.field}
          </div>
          <div className="before">{r.before}</div>
          <div className="after">→ {r.after}</div>
        </div>
      ))}
      <div className="section-title">已跳过 ({report?.skipped.length ?? 0} 处，低置信度)</div>
      {report?.skipped.map((s, i) => (
        <div className="rewrite-entry" key={i}>
          <div className="loc">
            {s.location}  {s.field}
          </div>
          <div className="path">"{s.snippet}"</div>
          <div className="reason">原因: {s.reason}</div>
        </div>
      ))}
    </Dialog>
  );
}

// ── D11: Newly discovered child projects ─────────────────────────────
function DiscoveredDialog({ workspaceId }: { workspaceId: string }) {
  const { setDialog, overview } = useStore();
  const ws = overview?.workspaces.find((w) => w.id === workspaceId);
  const discovered = (ws?.children ?? []).filter((c) => c.newlyDiscovered);
  const [sel, setSel] = useState<Record<string, boolean>>({});

  return (
    <Dialog
      title="新发现的子项目"
      icon={<FolderSearch size={18} />}
      width={480}
      onClose={() => setDialog(null)}
      footer={
        <>
          <button onClick={() => setDialog(null)}>忽略全部</button>
          <button
            className="primary"
            onClick={async () => {
              for (const c of discovered) {
                if (sel[c.name]) await ipc.enableChild(workspaceId, c.name, {}).catch(() => {});
              }
              pushToast("已开启选中的子项目");
              setDialog(null);
            }}
          >
            开启选中项
          </button>
        </>
      }
    >
      <p className="muted" style={{ marginBottom: 10 }}>
        工作区 {ws?.localRoot} 中发现了以下新的子目录：
      </p>
      {discovered.map((c) => (
        <label className="check" key={c.name}>
          <input type="checkbox" checked={!!sel[c.name]} onChange={() => setSel({ ...sel, [c.name]: !sel[c.name] })} />
          <div>
            <div>{c.name}/</div>
            <div className="path">
              创建于 {c.discoveredAt} · 对端可能的路径: {c.remoteDir}
            </div>
          </div>
        </label>
      ))}
      <div className="warn-box" style={{ color: "var(--text-dim)", background: "transparent", border: "1px solid var(--border)" }}>
        选中的子项目将使用工作区默认设置开启同步。
        <br />
        同步模式: 双向自动 · 目标设备: {ws?.peerName}
      </div>
    </Dialog>
  );
}

// ── D12: First-run wizard (3 steps) ──────────────────────────────────
function WizardDialog() {
  const { setDialog, refresh, overview } = useStore();
  const [step, setStep] = useState(1);
  const [name, setName] = useState(overview?.local.deviceName ?? "");
  const local = overview?.local;
  const tools = overview?.tools ?? [];

  useEffect(() => {
    if (!name && overview?.local.deviceName) {
      setName(overview.local.deviceName);
    }
  }, [name, overview?.local.deviceName]);

  return (
    <Dialog
      title={`欢迎使用 CodeBaton — Step ${step}/3`}
      width={560}
      closeOnOverlay={false}
      onClose={() => {}}
      footer={
        <>
          {step > 1 && <button onClick={() => setStep(step - 1)}>上一步</button>}
          {step < 3 ? (
            <button className="primary" onClick={() => setStep(step + 1)}>
              下一步
            </button>
          ) : (
            <button
              className="primary"
              onClick={async () => {
                await ipc.completeOnboarding(name).catch(() => {});
                await refresh();
                setDialog(null);
              }}
            >
              完成
            </button>
          )}
        </>
      }
    >
      {step === 1 && (
        <>
          <p className="muted" style={{ marginBottom: 12 }}>
            为这台设备设置一个名称，方便在其他设备上识别它：
          </p>
          <div className="field">
            <label>设备名称</label>
            <input value={name} onChange={(e) => setName(e.target.value)} />
            <div className="hint">建议使用容易辨别的名称</div>
          </div>
          <div className="section-title">检测到的信息</div>
          <div className="detail-grid">
            <span className="label">操作系统</span>
            <span>{local?.osVersion}</span>
            <span />
            <span className="label">用户名</span>
            <span>{local?.user}</span>
            <span />
            <span className="label">局域网 IP</span>
            <span className="path">{local?.ip}</span>
            <span />
          </div>
        </>
      )}
      {step === 2 && (
        <>
          <p className="muted" style={{ marginBottom: 12 }}>
            检测到以下 AI 编程工具：
          </p>
          {tools.map((t) => (
            <div className="tool-row" key={t.name}>
              <span>
                {t.installed ? "✓" : "✗"} {t.name}
              </span>
              <span className="path">{t.installed ? t.configDir : "未安装"}</span>
              <span className="muted">{t.installed ? `${t.sessionCount} 个项目会话` : ""}</span>
              <span />
            </div>
          ))}
          <p className="faint" style={{ fontSize: 12, marginTop: 12 }}>
            这些路径将用于同步会话数据。如果路径不正确，可以在设置中修改。
          </p>
        </>
      )}
      {step === 3 && (
        <>
          <p style={{ lineHeight: 2 }}>
            ✓ 设备已注册为 “{name}”
            <br />✓ 已检测到 {tools.filter((t) => t.installed).length} 个 AI 工具
            <br />✓ mDNS 服务已启动，正在局域网中广播
          </p>
          <div className="section-title">下一步</div>
          <p className="muted" style={{ fontSize: 12, lineHeight: 1.8 }}>
            在另一台设备上安装并运行 CodeBaton，两台设备将自动发现彼此。然后在侧边栏中点击发现的设备进行配对。
          </p>
          <div className="section-title">当前局域网中的设备</div>
          <p className="faint" style={{ fontSize: 12 }}>
            正在扫描...
          </p>
        </>
      )}
    </Dialog>
  );
}

// ── Router ───────────────────────────────────────────────────────────
export function DialogHost() {
  const { dialog } = useStore();
  if (!dialog) return null;
  switch (dialog.kind) {
    case "addProject":
      return <AddProjectDialog />;
    case "addWorkspace":
      return <AddWorkspaceDialog />;
    case "enableChild":
      return <EnableChildDialog workspaceId={dialog.workspaceId} child={dialog.child} />;
    case "pairing":
      return <PairingDialog peerId={dialog.peerId} />;
    case "projectMappingRequest":
      return <ProjectMappingRequestDialog request={dialog.request} />;
    case "workspaceMappingRequest":
      return <WorkspaceMappingRequestDialog request={dialog.request} />;
    case "conflict":
      return <ConflictDialog projectId={dialog.projectId} />;
    case "batch":
      return <BatchDialog peerId={dialog.peerId} direction={dialog.direction} />;
    case "excludeRules":
      return <ExcludeRulesDialog projectId={dialog.projectId} />;
    case "unpair":
      return <UnpairDialog peerId={dialog.peerId} />;
    case "syncProgress":
      return <SyncProgressDialog />;
    case "rewriteReport":
      return <RewriteReportDialog projectId={dialog.projectId} />;
    case "discovered":
      return <DiscoveredDialog workspaceId={dialog.workspaceId} />;
    case "wizard":
      return <WizardDialog />;
  }
}
