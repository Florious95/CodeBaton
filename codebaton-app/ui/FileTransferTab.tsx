import { useEffect, useRef, useState } from "react";
import { Upload, ArrowDown, ArrowUp, FolderOpen } from "lucide-react";
import { ipc, listen } from "./ipc";
import { pushToast, useStore } from "./store";
import type { FileTransferRecord, PendingFileTransfer } from "./types";
import { fmtBytes, fmtTime } from "./util";

/**
 * P2「文件传输」Tab（ISS-017/018/019）。
 * 发送：拖拽 或 点击拖拽区调 pickFilesForTransfer → requestFileTransfer；敏感文件二次确认。
 * 接收：轮询 pendingFileTransfers 显示待接收列表 → 点接收选目录 → 询问是否设默认 → accept_file_transfer。
 * 顶部显示/可改默认接收目录；传输历史含传入/传出（含接收下载记录）。
 * 模块顶层组件，避免父级 3s 轮询重渲染时被卸载（拖拽监听/状态才不丢）。
 */
type LocalXfer = { dir: "out"; filename: string; status: "sending" | "sent" | "failed"; ts: number; detail?: string };

function baseName(p: string): string {
  return p.split("/").pop() || p;
}

export function FileTransferTab({ peerName, online }: { peerName: string; online: boolean }) {
  const { clearUnread, bumpUnreadFiles, t } = useStore();
  const [outgoing, setOutgoing] = useState<LocalXfer[]>([]);
  const [pending, setPending] = useState<PendingFileTransfer[]>([]);
  const [history, setHistory] = useState<FileTransferRecord[]>([]);
  const [defaultDir, setDefaultDir] = useState<string>("");
  const [dragOver, setDragOver] = useState(false);
  const seenPending = useRef<Set<string>>(new Set());

  // 进 Tab 清未读（ISS-020）。
  useEffect(() => {
    clearUnread(peerName, "files");
  }, [peerName, pending.length, clearUnread]);

  // 默认接收目录。
  useEffect(() => {
    ipc.getDefaultReceiveDir().then(setDefaultDir).catch(() => {});
  }, []);

  // 轮询待接收列表 + 传输历史（ISS-018/019）。
  useEffect(() => {
    let cancelled = false;
    const poll = () => {
      ipc
        .pendingFileTransfers()
        .then((list) => {
          if (cancelled) return;
          setPending(list);
          // 新待接收项 → 未读角标 +1。
          const fresh = list.filter((p) => !seenPending.current.has(p.id));
          if (fresh.length) {
            fresh.forEach((p) => seenPending.current.add(p.id));
            bumpUnreadFiles(peerName, fresh.length);
          }
        })
        .catch(() => {});
      ipc.fileTransferHistory(peerName).then((h) => !cancelled && setHistory(h)).catch(() => {});
    };
    poll();
    const timer = window.setInterval(poll, 1500);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [peerName, bumpUnreadFiles]);

  const sendPath = async (path: string) => {
    const filename = baseName(path);
    const ts = Date.now();
    setOutgoing((prev) => [{ dir: "out", filename, status: "sending", ts }, ...prev]);
    const mark = (status: LocalXfer["status"], detail?: string) =>
      setOutgoing((prev) => prev.map((t) => (t.ts === ts ? { ...t, status, detail } : t)));
    ipc.uiLog(`file_send_start peer=${peerName} path=${path}`);
    const doSend = (confirmedSensitive?: string[]) =>
      ipc.requestFileTransfer(peerName, path, confirmedSensitive);
    try {
      await doSend();
      mark("sent");
      pushToast(t.fileSent(filename));
    } catch (e) {
      const msg = String(e);
      if (msg.includes("sensitive-file:")) {
        const rel = msg.split("sensitive-file:")[1]?.trim() || filename;
        if (window.confirm(t.sensitiveConfirm(rel, peerName))) {
          try {
            await doSend([rel]);
            mark("sent");
            pushToast(t.fileSent(filename));
          } catch (e2) {
            mark("failed", String(e2));
            pushToast(t.sendFailed(String(e2)));
          }
        } else {
          mark("failed", t.userCancelSensitive);
        }
      } else {
        mark("failed", msg);
        pushToast(t.sendFailed(msg));
      }
    }
  };

  // 点击拖拽区 → 访达多选文件并直接发送（ISS-017）。core 的 pick_files_for_transfer
  // 选+发一步完成，返回 transfer_id 数组；历史由后端 file_transfer_history 反映。
  const pickAndSend = async () => {
    if (!online) {
      pushToast(t.peerOfflineCantSend);
      return;
    }
    ipc.uiLog(`file_pick_send peer=${peerName}`);
    try {
      const ids = await ipc.pickFilesForTransfer(peerName);
      if (ids && ids.length) pushToast(t.filesSent(ids.length));
    } catch (e) {
      pushToast(t.sendFailed(String(e)));
    }
  };

  // 原生拖拽（携带真实路径）。
  useEffect(() => {
    const uns: Array<() => void> = [];
    listen<{ paths: string[] }>("tauri://drag-drop", (p) => {
      setDragOver(false);
      const list: string[] = (p as any)?.paths || (Array.isArray(p) ? (p as any) : []);
      if (!online) {
        pushToast(t.peerOfflineCantSend);
        return;
      }
      list.forEach((path) => void sendPath(path));
    }).then((u) => uns.push(u));
    listen("tauri://drag-enter", () => setDragOver(true)).then((u) => uns.push(u));
    listen("tauri://drag-leave", () => setDragOver(false)).then((u) => uns.push(u));
    return () => uns.forEach((u) => u());
  }, [peerName, online, t]);

  // 接收：选目录 → 询问设默认 → accept（ISS-018）。
  const accept = async (p: PendingFileTransfer) => {
    const dir = await ipc.pickDirectory().catch(() => null);
    if (!dir) return;
    // 仅当尚无默认目录时才询问“是否设为默认”；用户取消后不再纠缠。
    if (!defaultDir && window.confirm(t.setDefaultReceiveDir(dir))) {
      try {
        await ipc.setDefaultReceiveDir(dir);
        setDefaultDir(dir);
      } catch {
        /* 设默认失败不阻断接收 */
      }
    }
    try {
      await ipc.acceptFileTransfer(p.id, dir);
      ipc.uiLog(`file_accept id=${p.id} dir=${dir}`);
      pushToast(t.fileReceived(p.filename));
      setPending((prev) => prev.filter((x) => x.id !== p.id));
    } catch (e) {
      pushToast(t.receiveFailed(String(e)));
    }
  };

  const changeDefaultDir = async () => {
    const dir = await ipc.pickDirectory().catch(() => null);
    if (!dir) return;
    try {
      await ipc.setDefaultReceiveDir(dir);
      setDefaultDir(dir);
      pushToast(t.defaultDirUpdated);
    } catch (e) {
      pushToast(t.setFailed(String(e)));
    }
  };

  // ISS-032: Cmd+V(mac)/Ctrl+V → 粘贴已复制的文件并发送。
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && (e.key === "v" || e.key === "V")) {
        // 不拦截输入框里的粘贴（对话输入等）。
        const tag = (e.target as HTMLElement)?.tagName;
        if (tag === "INPUT" || tag === "TEXTAREA") return;
        if (!online) {
          pushToast(t.peerOfflineCantSend);
          return;
        }
        e.preventDefault();
        ipc.uiLog(`file_paste_send peer=${peerName}`);
        ipc
          .pasteFilesForTransfer(peerName)
          .then((ids) => {
            if (ids && ids.length) pushToast(t.pastedAndSent(ids.length));
            else pushToast(t.clipboardNoFile);
          })
          .catch((err) => pushToast(t.pasteSendFailed(String(err))));
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [peerName, online, t]);

  return (
    <div style={{ marginTop: 14 }}>
      {/* 默认接收目录设置项 */}
      <div className="detail-grid sync" style={{ marginTop: 0 }}>
        <span className="label">{t.defaultReceiveDir}</span>
        <span className="path">{defaultDir || t.notSetPickEachTime}</span>
        <button className="tiny" onClick={changeDefaultDir}>
          <FolderOpen size={12} style={{ verticalAlign: "-2px" }} /> {t.modify}
        </button>
      </div>

      {/* ISS-028/030: 有默认接收目录时，core 自动接收到默认目录，不展示「待接收」
          确认列表、不弹确认。仅当**未设默认目录**时才显示待接收列表让用户选目录。
          用户想改目录直接点上面的「修改」。 */}
      {!defaultDir && pending.length > 0 && (
        <>
          <div className="section-title">{t.pendingFiles(pending.length)}</div>
          <p className="faint" style={{ fontSize: 11, margin: "0 0 6px" }}>
            {t.pendingHint}
          </p>
          <div className="card flush">
            {pending.map((p) => (
              <div className="tool-row" key={p.id} style={{ gridTemplateColumns: "1fr auto" }}>
                <span className="path">
                  {p.filename} <span className="faint">{t.fromSender(fmtBytes(p.size), p.senderName)}</span>
                </span>
                <button className="primary tiny" onClick={() => void accept(p)}>
                  {t.pickDirReceive}
                </button>
              </div>
            ))}
          </div>
        </>
      )}

      {/* 发送拖拽区 */}
      <div
        className={`drop-zone ${dragOver ? "over" : ""}`}
        onClick={pickAndSend}
        title={online ? t.dropToSend : t.peerOffline}
        style={{ marginTop: 12 }}
      >
        <Upload size={22} />
        <div>{dragOver ? t.releaseToSend : t.dropFilesTo(peerName)}</div>
        <div className="faint" style={{ fontSize: 11 }}>
          {online ? t.clickOrPaste : t.offlineCantSend}
        </div>
      </div>

      {/* 传输历史（含传入/传出） */}
      <div className="section-title">{t.transferHistory}</div>
      {outgoing.length === 0 && history.length === 0 ? (
        <p className="faint" style={{ fontSize: 12 }}>
          {t.noTransferRecord}
        </p>
      ) : (
        <div className="card flush">
          {history.map((h, i) => (
            <div className="tool-row" key={`h${i}`} style={{ gridTemplateColumns: "20px 1fr auto auto" }}>
              <span title={h.direction === "out" ? t.outgoing : t.incoming}>
                {h.direction === "out" ? <ArrowUp size={14} /> : <ArrowDown size={14} />}
              </span>
              <span className="path">
                {h.filename} <span className="faint">· {fmtBytes(h.size)}</span>
              </span>
              <span className="faint" style={{ fontSize: 11 }}>{fmtTime(String(h.timestamp))}</span>
              <span className={h.status === "failed" ? "status-pill error" : "status-pill synced"}>
                {h.direction === "out" ? t.sent : t.received}
              </span>
            </div>
          ))}
          {/* ISS-027: 避免重复记录——core 的 history 是去重后的真实历史；本地
              outgoing 只展示「发送中」这类尚未进 core 历史的瞬时状态，"已发送"
              成功记录交给 core history 显示，不在前端重复写一条。 */}
          {outgoing
            .filter((xfer) => xfer.status !== "sent")
            .map((xfer, i) => (
              <div className="tool-row" key={`o${i}`} style={{ gridTemplateColumns: "20px 1fr auto auto" }}>
                <span title={t.outgoing}><ArrowUp size={14} /></span>
                <span className="path">{xfer.filename}</span>
                <span className="faint" style={{ fontSize: 11 }}>{fmtTime(String(xfer.ts))}</span>
                <span
                  className={xfer.status === "failed" ? "status-pill error" : "status-pill syncing"}
                  title={xfer.detail ?? ""}
                >
                  {xfer.status === "failed" ? t.failed : t.sending}
                </span>
              </div>
            ))}
        </div>
      )}
    </div>
  );
}
