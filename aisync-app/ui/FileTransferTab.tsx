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
  const { clearUnread, bumpUnreadFiles } = useStore();
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
      pushToast(`已发送 ${filename}`);
    } catch (e) {
      const msg = String(e);
      if (msg.includes("sensitive-file:")) {
        const rel = msg.split("sensitive-file:")[1]?.trim() || filename;
        if (window.confirm(`「${rel}」疑似敏感文件，确认仍要发送给 ${peerName}？`)) {
          try {
            await doSend([rel]);
            mark("sent");
            pushToast(`已发送 ${filename}`);
          } catch (e2) {
            mark("failed", String(e2));
            pushToast(`发送失败：${String(e2)}`);
          }
        } else {
          mark("failed", "用户取消（敏感文件）");
        }
      } else {
        mark("failed", msg);
        pushToast(`发送失败：${msg}`);
      }
    }
  };

  // 点击拖拽区 → 访达多选文件并直接发送（ISS-017）。core 的 pick_files_for_transfer
  // 选+发一步完成，返回 transfer_id 数组；历史由后端 file_transfer_history 反映。
  const pickAndSend = async () => {
    if (!online) {
      pushToast("对端离线，无法发送");
      return;
    }
    ipc.uiLog(`file_pick_send peer=${peerName}`);
    try {
      const ids = await ipc.pickFilesForTransfer(peerName);
      if (ids && ids.length) pushToast(`已发送 ${ids.length} 个文件`);
    } catch (e) {
      pushToast(`发送失败：${String(e)}`);
    }
  };

  // 原生拖拽（携带真实路径）。
  useEffect(() => {
    const uns: Array<() => void> = [];
    listen<{ paths: string[] }>("tauri://drag-drop", (p) => {
      setDragOver(false);
      const list: string[] = (p as any)?.paths || (Array.isArray(p) ? (p as any) : []);
      if (!online) {
        pushToast("对端离线，无法发送");
        return;
      }
      list.forEach((path) => void sendPath(path));
    }).then((u) => uns.push(u));
    listen("tauri://drag-enter", () => setDragOver(true)).then((u) => uns.push(u));
    listen("tauri://drag-leave", () => setDragOver(false)).then((u) => uns.push(u));
    return () => uns.forEach((u) => u());
  }, [peerName, online]);

  // 接收：选目录 → 询问设默认 → accept（ISS-018）。
  const accept = async (p: PendingFileTransfer) => {
    const dir = await ipc.pickDirectory().catch(() => null);
    if (!dir) return;
    // 仅当尚无默认目录时才询问“是否设为默认”；用户取消后不再纠缠。
    if (!defaultDir && window.confirm(`是否将「${dir}」设为默认接收目录？\n以后接收文件不再每次询问。`)) {
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
      pushToast(`已接收 ${p.filename}`);
      setPending((prev) => prev.filter((x) => x.id !== p.id));
    } catch (e) {
      pushToast(`接收失败：${String(e)}`);
    }
  };

  const changeDefaultDir = async () => {
    const dir = await ipc.pickDirectory().catch(() => null);
    if (!dir) return;
    try {
      await ipc.setDefaultReceiveDir(dir);
      setDefaultDir(dir);
      pushToast("已更新默认接收目录");
    } catch (e) {
      pushToast(`设置失败：${String(e)}`);
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
          pushToast("对端离线，无法发送");
          return;
        }
        e.preventDefault();
        ipc.uiLog(`file_paste_send peer=${peerName}`);
        ipc
          .pasteFilesForTransfer(peerName)
          .then((ids) => {
            if (ids && ids.length) pushToast(`已粘贴并发送 ${ids.length} 个文件`);
            else pushToast("剪贴板没有文件");
          })
          .catch((err) => pushToast(`粘贴发送失败：${String(err)}`));
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [peerName, online]);

  return (
    <div style={{ marginTop: 14 }}>
      {/* 默认接收目录设置项 */}
      <div className="detail-grid sync" style={{ marginTop: 0 }}>
        <span className="label">默认接收目录</span>
        <span className="path">{defaultDir || "（未设置，接收时每次选择）"}</span>
        <button className="tiny" onClick={changeDefaultDir}>
          <FolderOpen size={12} style={{ verticalAlign: "-2px" }} /> 修改
        </button>
      </div>

      {/* ISS-028/030: 有默认接收目录时，core 自动接收到默认目录，不展示「待接收」
          确认列表、不弹确认。仅当**未设默认目录**时才显示待接收列表让用户选目录。
          用户想改目录直接点上面的「修改」。 */}
      {!defaultDir && pending.length > 0 && (
        <>
          <div className="section-title">待接收文件（{pending.length}）</div>
          <p className="faint" style={{ fontSize: 11, margin: "0 0 6px" }}>
            未设默认接收目录，请为每个文件选择保存位置（设默认后将自动接收）。
          </p>
          <div className="card flush">
            {pending.map((p) => (
              <div className="tool-row" key={p.id} style={{ gridTemplateColumns: "1fr auto" }}>
                <span className="path">
                  {p.filename} <span className="faint">· {fmtBytes(p.size)} · 来自 {p.senderName}</span>
                </span>
                <button className="primary tiny" onClick={() => void accept(p)}>
                  选目录接收
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
        title={online ? "拖拽文件到此发送，或点击选择" : "对端离线"}
        style={{ marginTop: 12 }}
      >
        <Upload size={22} />
        <div>{dragOver ? "松开发送" : `拖拽文件到此发送到 ${peerName}`}</div>
        <div className="faint" style={{ fontSize: 11 }}>
          {online
            ? "或点击选择文件；Cmd+V 粘贴发送已复制的文件"
            : "对端离线，暂不可发送"}
        </div>
      </div>

      {/* 传输历史（含传入/传出） */}
      <div className="section-title">传输历史</div>
      {outgoing.length === 0 && history.length === 0 ? (
        <p className="faint" style={{ fontSize: 12 }}>
          暂无传输记录
        </p>
      ) : (
        <div className="card flush">
          {history.map((h, i) => (
            <div className="tool-row" key={`h${i}`} style={{ gridTemplateColumns: "20px 1fr auto auto" }}>
              <span title={h.direction === "out" ? "发出" : "收到"}>
                {h.direction === "out" ? <ArrowUp size={14} /> : <ArrowDown size={14} />}
              </span>
              <span className="path">
                {h.filename} <span className="faint">· {fmtBytes(h.size)}</span>
              </span>
              <span className="faint" style={{ fontSize: 11 }}>{fmtTime(String(h.timestamp))}</span>
              <span className={h.status === "failed" ? "status-pill error" : "status-pill synced"}>
                {h.direction === "out" ? "已发送" : "已接收"}
              </span>
            </div>
          ))}
          {/* ISS-027: 避免重复记录——core 的 history 是去重后的真实历史；本地
              outgoing 只展示「发送中」这类尚未进 core 历史的瞬时状态，"已发送"
              成功记录交给 core history 显示，不在前端重复写一条。 */}
          {outgoing
            .filter((t) => t.status !== "sent")
            .map((t, i) => (
              <div className="tool-row" key={`o${i}`} style={{ gridTemplateColumns: "20px 1fr auto auto" }}>
                <span title="发出"><ArrowUp size={14} /></span>
                <span className="path">{t.filename}</span>
                <span className="faint" style={{ fontSize: 11 }}>{fmtTime(String(t.ts))}</span>
                <span
                  className={t.status === "failed" ? "status-pill error" : "status-pill syncing"}
                  title={t.detail ?? ""}
                >
                  {t.status === "failed" ? "失败" : "发送中"}
                </span>
              </div>
            ))}
        </div>
      )}
    </div>
  );
}
