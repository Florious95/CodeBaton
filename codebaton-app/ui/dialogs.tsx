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
  HandoffManifest,
  Pairing,
  ProjectMappingRequest,
  RewriteReport,
  ScannedChild,
  WorkspaceMappingRequest,
} from "./types";
import { fmtBytes, osLabel } from "./util";

// ── D1: Add single-project mapping ───────────────────────────────────
function AddProjectDialog() {
  const { setDialog, refresh, t } = useStore();
  // Real paired peers from getPeers() — not reverse-derived from existing
  // projects (which is empty on the first add). Peer NAME is the config key the
  // backend maps projects under, so we use it as the option value.
  const [peers, setPeers] = useState<{ id: string; name: string }[]>([]);
  const [name, setName] = useState("");
  const [localDir, setLocalDir] = useState("");
  const [peer, setPeer] = useState("");
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
      title={t.addProjTitle}
      width={520}
      onClose={() => setDialog(null)}
      footer={
        <>
          <button onClick={() => setDialog(null)}>{t.cancel}</button>
          <button
            className="primary"
            disabled={!valid}
            onClick={async () => {
              const submit = async (createLocalDir: boolean) => {
                ipc.uiLog(
                  `add_project_submit peer=${peer} localDir=${localDir} createLocalDir=${createLocalDir}`,
                );
                // Manual handoff is push-only; the mode selector was removed.
                // Send oneWayPush explicitly — the backend defaults unknown
                // labels to twoWayAuto, which would be wrong here.
                await ipc.addProject({
                  name,
                  localDir,
                  peer,
                  mode: "oneWayPush",
                  tool,
                  createLocalDir,
                });
              };
              try {
                await submit(false);
                ipc.uiLog("add_project_request_sent");
                pushToast(t.projReqSent);
                setDialog(null);
              } catch (e) {
                const msg = String(e);
                // 本机目录不存在 → 提示，点确定才新建后重试。
                if (msg.includes("local-dir-missing:")) {
                  ipc.uiLog("add_project_local_dir_missing");
                  if (window.confirm(t.localDirMissing(localDir))) {
                    try {
                      await submit(true);
                      ipc.uiLog("add_project_request_sent_after_mkdir");
                      await refresh();
                      pushToast(t.mkdirAndSent);
                      setDialog(null);
                    } catch (e2) {
                      const m2 = String(e2);
                      ipc.uiLog(`add_project_failed error=${m2}`);
                      pushToast(t.addFailed(m2));
                    }
                  }
                } else {
                  ipc.uiLog(`add_project_failed error=${msg}`);
                  pushToast(t.addFailed(msg));
                }
              }
            }}
          >
            {t.addProject}
          </button>
        </>
      }
    >
      <div className="field">
        <label>{t.projName}</label>
        <input value={name} onChange={(e) => setName(e.target.value)} placeholder={t.projNamePlaceholder} />
      </div>
      <div className="field">
        <label>{t.localDir}</label>
        <div className="row">
          <input value={localDir} onChange={(e) => setLocalDir(e.target.value)} placeholder={t.localDirPlaceholder} />
          <button
            onClick={async () => {
              ipc.uiLog("browse_clicked dialog=add_project");
              const dir = await ipc.pickDirectory().catch(() => null);
              if (dir) {
                setLocalDir(dir);
                ipc.uiLog(`path_selected dir=${dir}`);
              }
            }}
          >{t.browse}</button>
        </div>
      </div>
      <div className="field">
        <label>{t.targetDevice}</label>
        <select value={peer} onChange={(e) => setPeer(e.target.value)}>
          {peers.length === 0 && <option value="">{t.noPairedPeerPick}</option>}
          {peers.map((p) => (
            <option key={p.id} value={p.name}>
              {p.name}
            </option>
          ))}
        </select>
      </div>
      <div className="field">
        <label>{t.targetAiTool}</label>
        <select value={tool} onChange={(e) => setTool(e.target.value)}>
          <option value="same">{t.sameToolOpt}</option>
          <option value="codex">{t.toCodex}</option>
          <option value="gemini">{t.toGemini}</option>
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
  const { setDialog, refresh, t } = useStore();
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
      title={t.addWsTitle}
      width={560}
      onClose={() => setDialog(null)}
      footer={
        <>
          <button onClick={() => setDialog(null)}>{t.cancel}</button>
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
                pushToast(t.addedWorkspace(sel));
                setDialog(null);
              } catch (e) {
                const msg = String(e);
                ipc.uiLog(`add_workspace_failed error=${msg}`);
                pushToast(t.addWsFailed(msg));
              }
            }}
          >
            {t.addProject}
          </button>
        </>
      }
    >
      <div className="field">
        <label>{t.wsName}</label>
        <input value={name} onChange={(e) => setName(e.target.value)} />
      </div>
      <div className="field">
        <label>{t.localRoot}</label>
        <div className="row">
          <input value={localRoot} onChange={(e) => setLocalRoot(e.target.value)} placeholder="~/projects" />
          <button onClick={scan}>{t.browse}</button>
        </div>
      </div>
      <div className="field">
        <label>{t.targetDevice}</label>
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
          {pairedPeers.length === 0 && <option value="">{t.noPairedPeer}</option>}
          {pairedPeers.map((p) => (
            <option key={p.id} value={p.name}>
              {p.name}
            </option>
          ))}
        </select>
      </div>
      {/* ISS-004: 「对端根目录」输入框已移除——有对端确认流程后，对端目录由对端
          自己在确认弹窗里选；主方不需要填。 */}
      <div className="section-title">{t.scannedChildren}{children.length > 0 ? `（${children.length}）` : ""}</div>
      {children.length === 0 ? (
        <p className="faint" style={{ fontSize: 12 }}>
          {t.scanHint}
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
        <label>{t.defaultSyncMode}</label>
        <select value={mode} onChange={(e) => setMode(e.target.value)}>
          <option value="twoWayAuto">{t.twoWayAuto}</option>
          <option value="oneWayPush">{t.oneWayPush}</option>
        </select>
      </div>
      <div className="field">
        <label>{t.newChildren}</label>
        <label className="radio">
          <input type="radio" checked={autoEnable} onChange={() => setAutoEnable(true)} />
          {t.autoEnableSync}
        </label>
        <label className="radio">
          <input type="radio" checked={!autoEnable} onChange={() => setAutoEnable(false)} />
          {t.manualEnable}
        </label>
      </div>
    </Dialog>
  );
}

// ── D3: Enable a workspace child ─────────────────────────────────────
function EnableChildDialog({ workspaceId, child }: { workspaceId: string; child: string }) {
  const { setDialog, overview, t } = useStore();
  const ws = overview?.workspaces.find((w) => w.id === workspaceId);
  const c = ws?.children.find((x) => x.name === child);
  const [peer, setPeer] = useState(ws?.peerName ?? "");
  const [remote, setRemote] = useState(c?.remoteDir ?? "");
  const [mode, setMode] = useState("twoWayAuto");

  return (
    <Dialog
      title={t.enableChildTitle}
      width={440}
      onClose={() => setDialog(null)}
      footer={
        <>
          <button onClick={() => setDialog(null)}>{t.cancel}</button>
          <button
            className="primary"
            onClick={async () => {
              await ipc.enableChild(workspaceId, child, { peer, remote, mode }).catch(() => {});
              pushToast(t.childEnabled(child));
              setDialog(null);
            }}
          >
            {t.enable}
          </button>
        </>
      }
    >
      <div className="detail-grid">
        <span className="label">{t.subProject}</span>
        <span>{child}</span>
        <span />
        <span className="label">{t.localPath}</span>
        <span className="path">{c?.localDir}</span>
        <span />
      </div>
      <div className="field">
        <label>{t.targetDevice}</label>
        <select value={peer} onChange={(e) => setPeer(e.target.value)}>
          {peer ? <option value={peer}>{peer}</option> : <option value="">{t.noPairedPeer}</option>}
        </select>
      </div>
      <div className="field">
        <label>{t.remotePath}</label>
        <input value={remote} onChange={(e) => setRemote(e.target.value)} />
        <div className="hint">{t.remoteDirAutofill}</div>
      </div>
      <div className="field">
        <label>{t.syncMode}</label>
        <select value={mode} onChange={(e) => setMode(e.target.value)}>
          <option value="twoWayAuto">{t.twoWayAuto}</option>
          <option value="oneWayPush">{t.oneWayPush}</option>
        </select>
      </div>
    </Dialog>
  );
}

// ── D4: Pairing confirmation (initiator view) ────────────────────────
function PairingDialog({ peerId }: { peerId: string }) {
  const { setDialog, refresh, t } = useStore();
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
        pushToast(t.pairFailed(msg));
      });
  }, [peerId]);

  return (
    <Dialog
      title={t.pairReqSent}
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
            {t.cancelPair}
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
                pushToast(t.pairedWith(pairing?.peerName ?? ""));
                setDialog(null);
              } catch (e) {
                const msg = String(e);
                ipc.uiLog(`confirmPairing threw peerId=${peerId} error=${msg}`);
                pushToast(t.confirmPairFailed(msg));
              }
            }}
          >
            {t.confirmPair}
          </button>
        </>
      }
    >
      {error ? (
        <p className="muted" style={{ textAlign: "center", marginBottom: 10, color: "var(--red)" }}>
          {t.pairFailedLabel}{error}
        </p>
      ) : (
        <p className="muted" style={{ textAlign: "center", marginBottom: 10 }}>
          {pairing ? t.waitingPeerConfirm : t.fetchingPairCode}
        </p>
      )}
      <div className="detail-grid">
        <span className="label">{t.targetDevice}</span>
        <span>{pairing?.peerName}</span>
        <span />
        <span className="label">{t.ipAddress}</span>
        <span className="path">{pairing?.peerIp}</span>
        <span />
        <span className="label">{t.osLabel}</span>
        <span>{osLabel(pairing?.peerOs ?? "")}</span>
        <span />
      </div>
      <p className="muted" style={{ fontSize: 12 }}>
        {t.confirmPairCodeHint}
      </p>
      <div className="pairing-code">{pairing?.code ?? "····"}</div>
      <p className="faint" style={{ fontSize: 11, textAlign: "center" }}>
        {t.samePairCodeHint}
      </p>
    </Dialog>
  );
}

function ProjectMappingRequestDialog({ request }: { request: ProjectMappingRequest }) {
  const { setDialog, refresh, t } = useStore();
  // 默认填发起端发来的路径（两端目录结构通常一致），用户可改。目录不存在时
  // 点确认会由后端 mkdir -p 自动创建。
  const [localDir, setLocalDir] = useState(request.sourceDir ?? "");
  const [busy, setBusy] = useState(false);
  const valid = localDir.trim().length > 0;

  return (
    <Dialog
      title={t.projMapReqTitle}
      icon={<FolderSearch size={18} />}
      width={520}
      onClose={() => setDialog(null)}
      footer={
        <>
          <button disabled={busy} onClick={() => setDialog(null)}>
            {t.later}
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
                pushToast(t.projMapConfirmed);
                setDialog(null);
              } catch (e) {
                const msg = String(e);
                ipc.uiLog(
                  `confirm_project_mapping failed requestId=${request.requestId} error=${msg}`,
                );
                pushToast(t.confirmFailed(msg));
              } finally {
                setBusy(false);
              }
            }}
          >
            {t.confirmMap}
          </button>
        </>
      }
    >
      <div className="detail-grid">
        <span className="label">{t.initiatorDevice}</span>
        <span>{request.peerName}</span>
        <span />
        <span className="label">{t.projName}</span>
        <span>{request.projectName}</span>
        <span />
        <span className="label">{t.remoteDir}</span>
        <span className="path">{request.sourceDir}</span>
        <span />
      </div>
      <div className="field">
        <label>{t.localPlaceDir}</label>
        <div className="row">
          <input
            value={localDir}
            onChange={(e) => setLocalDir(e.target.value)}
            placeholder={t.pickProjDirPlaceholder}
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
            {t.browse}
          </button>
        </div>
      </div>
    </Dialog>
  );
}

function WorkspaceMappingRequestDialog({ request }: { request: WorkspaceMappingRequest }) {
  const { setDialog, refresh, t } = useStore();
  const [localRoot, setLocalRoot] = useState(request.suggestedRemoteRoot ?? request.sourceRoot ?? "");
  const [busy, setBusy] = useState(false);
  const valid = localRoot.trim().length > 0;

  return (
    <Dialog
      title={t.wsMapReqTitle}
      icon={<FolderSearch size={18} />}
      width={560}
      onClose={() => setDialog(null)}
      footer={
        <>
          <button disabled={busy} onClick={() => setDialog(null)}>
            {t.later}
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
                pushToast(t.wsMapConfirmed);
                setDialog(null);
              } catch (e) {
                const msg = String(e);
                ipc.uiLog(
                  `confirm_workspace_mapping failed requestId=${request.requestId} error=${msg}`,
                );
                pushToast(t.confirmFailed(msg));
              } finally {
                setBusy(false);
              }
            }}
          >
            {t.confirmMap}
          </button>
        </>
      }
    >
      <div className="detail-grid">
        <span className="label">{t.initiatorDevice}</span>
        <span>{request.peerName}</span>
        <span />
        <span className="label">{t.workspace}</span>
        <span>{request.workspaceName}</span>
        <span />
        <span className="label">{t.remoteRoot}</span>
        <span className="path">{request.sourceRoot}</span>
        <span />
        <span className="label">{t.subProject}</span>
        <span>{request.children.length}</span>
        <span />
      </div>
      <div className="field">
        <label>{t.localRoot}</label>
        <div className="row">
          <input
            value={localRoot}
            onChange={(e) => setLocalRoot(e.target.value)}
            placeholder={t.pickWsRootPlaceholder}
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
            {t.browse}
          </button>
        </div>
      </div>
      {request.children.length > 0 && (
        <div className="field">
          <label>{t.subProject}</label>
          <p className="path">{request.children.join(", ")}</p>
        </div>
      )}
    </Dialog>
  );
}

// ── D5: Split-brain conflict ─────────────────────────────────────────
function ConflictDialog({ projectId }: { projectId: string }) {
  const { setDialog, refresh, t } = useStore();
  const [conflict, setConflict] = useState<Conflict | null>(null);
  const [choice, setChoice] = useState<string>("");
  useEffect(() => {
    ipc.getConflict(projectId).then(setConflict).catch(() => {});
  }, [projectId]);
  const destructive = choice === "local" || choice === "remote";

  return (
    <Dialog
      title={t.conflictTitle}
      icon={<AlertTriangle size={18} color="var(--amber)" />}
      width={560}
      closeOnOverlay={false}
      onClose={() => setDialog(null)}
      footer={
        <>
          <button onClick={() => setDialog(null)}>{t.cancel}</button>
          <button
            className={destructive ? "danger" : "primary"}
            disabled={!choice}
            onClick={async () => {
              await ipc.resolveConflict(projectId, choice).catch(() => {});
              await refresh();
              setDialog(null);
            }}
          >
            {destructive ? t.confirmOverwrite : t.execute}
          </button>
        </>
      }
    >
      <p style={{ marginBottom: 8 }}>
        {t.conflictDesc(conflict?.projectName ?? "")}
      </p>
      <div className="conflict-cols">
        {[conflict?.local, conflict?.remote].map((side, i) => (
          <div className="conflict-col" key={i}>
            <h4>{side?.deviceName}</h4>
            <p className="muted" style={{ fontSize: 11, marginBottom: 6 }}>
              {t.changedAfterSync(side?.changedFiles ?? 0)}
            </p>
            {side?.files.map((f) => (
              <div className="file" key={f.path}>
                <span>{f.path}</span>
                <span>{f.change}</span>
              </div>
            ))}
            <p className="faint" style={{ fontSize: 11, marginTop: 6 }}>
              {t.sessionLabel(side?.sessionSummary ?? "")}
            </p>
          </div>
        ))}
      </div>
      <p className="muted" style={{ fontSize: 12, marginTop: 6 }}>
        {t.chooseHow}
      </p>
      {[
        ["local", t.preferLocal],
        ["remote", t.preferRemote],
        ["none", t.preferNone],
      ].map(([v, l]) => (
        <label className="radio" key={v}>
          <input type="radio" checked={choice === v} onChange={() => setChoice(v)} />
          {l}
        </label>
      ))}
      {destructive && (
        <div className="warn-box">{t.overwriteWarn}</div>
      )}
    </Dialog>
  );
}

// ── D6: Batch sync confirmation (G6 sensitive-file opt-in) ───────────
function BatchDialog({ peerId }: { peerId: string }) {
  const { setDialog, t } = useStore();
  const [plan, setPlan] = useState<BatchPlan | null>(null);
  const [selected, setSelected] = useState<Record<string, boolean>>({});
  const [sensitiveOptIn, setSensitiveOptIn] = useState<Record<string, boolean>>({});
  useEffect(() => {
    ipc.getBatchPlan(peerId).then((p) => {
      setPlan(p);
      const sel: Record<string, boolean> = {};
      p.items.forEach((i) => (sel[i.projectId] = !i.upToDate));
      setSelected(sel);
    });
  }, [peerId]);

  // Manual handoff is push-only.
  const verb = t.push;
  const chosen = (plan?.items ?? []).filter((i) => selected[i.projectId] && !i.upToDate);
  const totalFiles = chosen.reduce((s, i) => s + i.changedFiles, 0);
  const totalBytes = chosen.reduce((s, i) => s + i.bytes, 0);

  return (
    <Dialog
      title={t.batchTitle(verb)}
      width={480}
      onClose={() => setDialog(null)}
      footer={
        <>
          <button onClick={() => setDialog(null)}>{t.cancel}</button>
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
                await ipc.startSync(item.projectId, confirmedFor(item.projectId)).catch(() => {});
              }
              setDialog({ kind: "syncProgress" });
            }}
          >
            {t.start}{verb}
          </button>
        </>
      }
    >
      <p className="muted" style={{ marginBottom: 10 }}>
        {t.batchIntro(t.toPeer(plan?.peerName ?? ""), verb)}
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
            {i.upToDate ? t.upToDate : t.changedFilesApprox(i.changedFiles, fmtBytes(i.bytes))}
          </span>
        </label>
      ))}
      <div className="section-title">{t.total}</div>
      <p className="muted" style={{ fontSize: 12 }}>
        {t.totalSummary(totalFiles, fmtBytes(totalBytes))}
      </p>

      {plan && plan.sensitiveFiles.length > 0 && (
        <div className="warn-box" style={{ marginTop: 12 }}>
          <div className="row" style={{ gap: 6 }}>
            <ShieldAlert size={14} />
            <strong>{t.sensitiveMatched}</strong>
          </div>
          {plan.sensitiveFiles.map((f) => (
            <label className="check" key={f}>
              <input
                type="checkbox"
                checked={!!sensitiveOptIn[f]}
                onChange={() => setSensitiveOptIn({ ...sensitiveOptIn, [f]: !sensitiveOptIn[f] })}
              />
              <span className="path" style={{ color: "var(--amber)" }}>
                {t.includeThisFile(f)}
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
  const { setDialog, overview, t } = useStore();
  const project = overview?.projects.find((p) => p.id === projectId);
  const [rules, setRules] = useState((project?.excludeRules ?? []).join("\n"));

  return (
    <Dialog
      title={t.excludeRulesTitle(project?.name ?? "")}
      width={480}
      onClose={() => setDialog(null)}
      footer={
        <>
          <button onClick={() => setDialog(null)}>{t.cancel}</button>
          <button
            className="primary"
            onClick={async () => {
              await ipc.saveExcludeRules(projectId, rules.split("\n").filter(Boolean)).catch(() => {});
              pushToast(t.excludeSaved);
              setDialog(null);
            }}
          >
            {t.save}
          </button>
        </>
      }
    >
      <div className="section-title">{t.globalRulesRO}</div>
      <p className="path" style={{ marginBottom: 10 }}>
        node_modules/ .git/objects/ target/ __pycache__/ .next/ dist/ build/ .DS_Store
      </p>
      <div className="section-title">{t.projSpecificRules}</div>
      <textarea rows={6} value={rules} onChange={(e) => setRules(e.target.value)} />
      <div className="hint">{t.globPerLine}</div>
    </Dialog>
  );
}

// ── D8: Unpair confirmation ──────────────────────────────────────────
function UnpairDialog({ peerId }: { peerId: string }) {
  const { setDialog, setView, refresh, overview, t } = useStore();
  const name =
    overview?.projects.find((p) => p.peerId === peerId)?.peerName ?? t.thisDevice;
  return (
    <Dialog
      title={t.unpairTitle}
      width={420}
      closeOnOverlay={false}
      onClose={() => setDialog(null)}
      footer={
        <>
          <button onClick={() => setDialog(null)}>{t.cancel}</button>
          <button
            className="danger"
            onClick={async () => {
              await ipc.unpair(peerId).catch(() => {});
              await refresh();
              setView({ page: "overview" });
              setDialog(null);
            }}
          >
            {t.unpair}
          </button>
        </>
      }
    >
      <p style={{ marginBottom: 10 }}>{t.unpairConfirm(name)}</p>
      <p className="muted" style={{ fontSize: 12, lineHeight: 1.8 }}>
        {t.unpairAfter}
        <br />• {t.unpairBullet1}
        <br />• {t.unpairBullet2}
        <br />• {t.unpairBullet3}
      </p>
    </Dialog>
  );
}

// ── Handoff preview (manifest before a manual push) ──────────────────
function HandoffPreviewDialog({
  projectId,
  peerName,
}: {
  projectId: string;
  peerName: string;
}) {
  const { setDialog, setSelectedProjectId, t } = useStore();
  const [manifest, setManifest] = useState<HandoffManifest | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [force, setForce] = useState(false);

  useEffect(() => {
    ipc
      .previewHandoff(projectId, peerName)
      .then(setManifest)
      .catch((e) => setError(String(e)));
  }, [projectId, peerName]);

  const start = () => {
    setSelectedProjectId(projectId);
    ipc.startSync(projectId, [], force).catch(() => {});
    setDialog({ kind: "syncProgress" });
  };

  return (
    <Dialog
      title={t.handoffTitle}
      icon={<FolderSearch size={18} color="var(--blue)" />}
      width={520}
      onClose={() => setDialog(null)}
      footer={
        <>
          <button onClick={() => setDialog(null)}>{t.cancel}</button>
          <button className="primary" disabled={!manifest} onClick={start}>
            {t.handoffStart}
          </button>
        </>
      }
    >
      {error ? (
        <p style={{ color: "var(--red)" }}>{t.handoffFailed}: {error}</p>
      ) : !manifest ? (
        <p className="muted">{t.handoffLoading}</p>
      ) : (
        <>
          <div className="row" style={{ gap: 14, marginBottom: 10 }}>
            <span>
              {t.handoffCode}: <b>{manifest.codeFiles.length}</b> {t.files}
            </span>
            <span>
              {t.handoffSessions}:{" "}
              <b>{manifest.sessions.reduce((s, g) => s + g.fileCount, 0)}</b> {t.files}
            </span>
            <span>
              {t.handoffTotalSize}: <b>{fmtBytes(manifest.totalSize)}</b>
            </span>
          </div>
          <p className="muted" style={{ marginBottom: 10 }}>
            {manifest.incremental ? t.handoffIncremental : t.handoffFull}
          </p>
          {manifest.sessions.length > 0 && (
            <div className="muted" style={{ marginBottom: 10 }}>
              {manifest.sessions
                .map((g) => `${g.tool}: ${g.fileCount} ${t.files} (${fmtBytes(g.bytes)})`)
                .join("  ·  ")}
            </div>
          )}
          <p className="faint" style={{ marginBottom: 12 }}>{t.handoffExcludedHint}</p>
          <label className="row" style={{ gap: 8, cursor: "pointer" }}>
            <input type="checkbox" checked={force} onChange={(e) => setForce(e.target.checked)} />
            <span>{t.handoffForceOverwrite}</span>
          </label>
        </>
      )}
    </Dialog>
  );
}

// ── D9: Sync progress + result view ──────────────────────────────────
function SyncProgressDialog() {
  const { setDialog, syncProgress, lastResult, clearResult, t } = useStore();

  if (lastResult) {
    const ok = lastResult.success;
    return (
      <Dialog
        title={ok ? t.syncDone : t.syncFailed}
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
            {t.close}
          </button>
        }
      >
        <p style={{ marginBottom: 12 }}>
          {ok ? "✓" : "✗"} {lastResult.direction}
        </p>
        {ok ? (
          <>
            <div className="detail-grid">
              <span className="label">{t.transferredFiles}</span>
              <span>{lastResult.files} {t.count}</span>
              <span />
              <span className="label">{t.transferredData}</span>
              <span>{fmtBytes(lastResult.bytes)}</span>
              <span />
              <span className="label">{t.elapsed}</span>
              <span>{lastResult.elapsedSecs} {t.secs}</span>
              <span />
              <span className="label">{t.pathRewrite}</span>
              <span>{lastResult.rewrittenPaths} {t.places}</span>
              <span />
            </div>
            {lastResult.skippedPaths > 0 && (
              <div className="warn-box">
                {t.skippedRewriteWarn(lastResult.skippedPaths)}{" "}
                <button
                  className="ghost tiny"
                  onClick={() => setDialog({ kind: "rewriteReport", projectId: lastResult.projectId })}
                >
                  {t.viewDetails}
                </button>
              </div>
            )}
          </>
        ) : (
          <div className="warn-box">{lastResult.error ?? t.unknownError}</div>
        )}
      </Dialog>
    );
  }

  const p = syncProgress;
  return (
    <Dialog
      title={t.syncInProgress}
      width={480}
      closeOnOverlay={false}
      onClose={() => setDialog(null)}
      footer={
        <>
          <button onClick={() => setDialog(null)}>{t.minimizeToBackground}</button>
          <button
            className="danger"
            onClick={() => {
              if (p) ipc.cancelSync(p.projectId);
              setDialog(null);
            }}
          >
            {t.cancelSync}
          </button>
        </>
      }
    >
      <p style={{ marginBottom: 10 }}>{p?.direction ?? t.preparing}</p>
      <p className="muted" style={{ fontSize: 12, marginBottom: 6 }}>
        {t.phaseLabel(p?.phase ?? "")}
      </p>
      <div className="bar">
        <div style={{ width: `${p?.percent ?? 0}%` }} />
      </div>
      <div className="detail-grid" style={{ marginTop: 14 }}>
        <span className="label">{t.transferred}</span>
        <span>
          {t.filesProgress(p?.filesDone ?? 0, p?.filesTotal ?? 0)}
        </span>
        <span />
        <span className="label">{t.dataAmount}</span>
        <span>
          {fmtBytes(p?.bytesDone ?? 0)} / {fmtBytes(p?.bytesTotal ?? 0)}
        </span>
        <span />
        <span className="label">{t.speed}</span>
        <span>{fmtBytes(p?.speedBps ?? 0)}/s</span>
        <span />
        <span className="label">{t.etaLabel}</span>
        <span>{t.etaSecs(p?.etaSecs ?? 0)}</span>
        <span />
      </div>
      {p?.currentFile && (
        <p className="path" style={{ marginTop: 8 }}>
          {t.currentFile(p.currentFile)}
        </p>
      )}
      <div className="section-title">{t.stageProgress}</div>
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
  const { setDialog, t } = useStore();
  const [report, setReport] = useState<RewriteReport | null>(null);
  useEffect(() => {
    ipc.getRewriteReport(projectId).then(setReport).catch(() => {});
  }, [projectId]);

  return (
    <Dialog
      title={t.rewriteReportTitle}
      width={640}
      onClose={() => setDialog(null)}
      footer={<button className="primary" onClick={() => setDialog(null)}>{t.close}</button>}
    >
      <p className="muted" style={{ fontSize: 12 }}>
        {report?.projectName}  {report?.timestamp}  {report?.direction}
      </p>
      <div className="section-title">{t.rewrittenCount(report?.rewritten.length ?? 0)}</div>
      {report?.rewritten.map((r, i) => (
        <div className="rewrite-entry" key={i}>
          <div className="loc">
            {r.location}  {r.field}
          </div>
          <div className="before">{r.before}</div>
          <div className="after">→ {r.after}</div>
        </div>
      ))}
      <div className="section-title">{t.skippedCount(report?.skipped.length ?? 0)}</div>
      {report?.skipped.map((s, i) => (
        <div className="rewrite-entry" key={i}>
          <div className="loc">
            {s.location}  {s.field}
          </div>
          <div className="path">"{s.snippet}"</div>
          <div className="reason">{t.reasonLabel(s.reason)}</div>
        </div>
      ))}
    </Dialog>
  );
}

// ── D11: Newly discovered child projects ─────────────────────────────
function DiscoveredDialog({ workspaceId }: { workspaceId: string }) {
  const { setDialog, overview, t } = useStore();
  const ws = overview?.workspaces.find((w) => w.id === workspaceId);
  const discovered = (ws?.children ?? []).filter((c) => c.newlyDiscovered);
  const [sel, setSel] = useState<Record<string, boolean>>({});

  return (
    <Dialog
      title={t.discoveredTitle}
      icon={<FolderSearch size={18} />}
      width={480}
      onClose={() => setDialog(null)}
      footer={
        <>
          <button onClick={() => setDialog(null)}>{t.ignoreAll}</button>
          <button
            className="primary"
            onClick={async () => {
              for (const c of discovered) {
                if (sel[c.name]) await ipc.enableChild(workspaceId, c.name, {}).catch(() => {});
              }
              pushToast(t.selectedEnabled);
              setDialog(null);
            }}
          >
            {t.enableSelected}
          </button>
        </>
      }
    >
      <p className="muted" style={{ marginBottom: 10 }}>
        {t.discoveredIntro(ws?.localRoot ?? "")}
      </p>
      {discovered.map((c) => (
        <label className="check" key={c.name}>
          <input type="checkbox" checked={!!sel[c.name]} onChange={() => setSel({ ...sel, [c.name]: !sel[c.name] })} />
          <div>
            <div>{c.name}/</div>
            <div className="path">
              {t.discoveredAtRemote(c.discoveredAt ?? "", c.remoteDir ?? "")}
            </div>
          </div>
        </label>
      ))}
      <div className="warn-box" style={{ color: "var(--text-dim)", background: "transparent", border: "1px solid var(--border)" }}>
        <span style={{ whiteSpace: "pre-line" }}>{t.discoveredHint(ws?.peerName ?? "")}</span>
      </div>
    </Dialog>
  );
}

// ── D12: First-run wizard (3 steps) ──────────────────────────────────
function WizardDialog() {
  const { setDialog, refresh, overview, t: tr } = useStore();
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
      title={tr.wizardTitle(step)}
      width={560}
      closeOnOverlay={false}
      onClose={() => {}}
      footer={
        <>
          {step > 1 && <button onClick={() => setStep(step - 1)}>{tr.prevStep}</button>}
          {step < 3 ? (
            <button className="primary" onClick={() => setStep(step + 1)}>
              {tr.nextStep}
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
              {tr.done}
            </button>
          )}
        </>
      }
    >
      {step === 1 && (
        <>
          <p className="muted" style={{ marginBottom: 12 }}>
            {tr.nameThisDevice}
          </p>
          <div className="field">
            <label>{tr.deviceName}</label>
            <input value={name} onChange={(e) => setName(e.target.value)} />
            <div className="hint">{tr.nameHint}</div>
          </div>
          <div className="section-title">{tr.detectedInfo}</div>
          <div className="detail-grid">
            <span className="label">{tr.osLabel}</span>
            <span>{local?.osVersion}</span>
            <span />
            <span className="label">{tr.username}</span>
            <span>{local?.user}</span>
            <span />
            <span className="label">{tr.lanIp}</span>
            <span className="path">{local?.ip}</span>
            <span />
          </div>
        </>
      )}
      {step === 2 && (
        <>
          <p className="muted" style={{ marginBottom: 12 }}>
            {tr.detectedTools}
          </p>
          {tools.map((t) => (
            <div className="tool-row" key={t.name}>
              <span>
                {t.installed ? "✓" : "✗"} {t.name}
              </span>
              <span className="path">{t.installed ? t.configDir : tr.notInstalled}</span>
              <span className="muted">{t.installed ? tr.sessionsCount(t.sessionCount) : ""}</span>
              <span />
            </div>
          ))}
          <p className="faint" style={{ fontSize: 12, marginTop: 12 }}>
            {tr.pathsForSync}
          </p>
        </>
      )}
      {step === 3 && (
        <>
          <p style={{ lineHeight: 2 }}>
            ✓ {tr.registeredAs(name)}
            <br />✓ {tr.detectedToolsN(tools.filter((t) => t.installed).length)}
            <br />✓ {tr.mdnsStarted}
          </p>
          <div className="section-title">{tr.nextStepTitle}</div>
          <p className="muted" style={{ fontSize: 12, lineHeight: 1.8 }}>
            {tr.wizardNext}
          </p>
          <div className="section-title">{tr.devicesOnLan}</div>
          <p className="faint" style={{ fontSize: 12 }}>
            {tr.scanning}
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
      return <BatchDialog peerId={dialog.peerId} />;
    case "excludeRules":
      return <ExcludeRulesDialog projectId={dialog.projectId} />;
    case "unpair":
      return <UnpairDialog peerId={dialog.peerId} />;
    case "handoffPreview":
      return <HandoffPreviewDialog projectId={dialog.projectId} peerName={dialog.peerName} />;
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
