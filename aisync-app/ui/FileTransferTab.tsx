import { useEffect, useRef, useState } from "react";
import { Upload, ArrowDown, ArrowUp } from "lucide-react";
import { ipc, listen } from "./ipc";
import { pushToast } from "./store";
import { fmtTime } from "./util";

/**
 * P2「文件传输」Tab — 拖拽文件发送到对端（net 的 FileTransferRequest/Data/Ack 帧）。
 * 对端收到后选接收路径（由 App.tsx 的 pendingFileTransferRequest 弹窗处理）。
 * 这里负责：发起端拖拽发送 + 敏感文件确认 + 本地传输历史（区分传入/传出）。
 *
 * 模块顶层组件，避免父级 3s 轮询重渲染时卸载重建（拖拽监听才不会丢）。
 */
type Transfer = {
  dir: "out" | "in";
  filename: string;
  path: string;
  status: "sending" | "sent" | "failed";
  ts: number;
  detail?: string;
};

function baseName(p: string): string {
  return p.split("/").pop() || p;
}

export function FileTransferTab({ peerName, online }: { peerName: string; online: boolean }) {
  const [transfers, setTransfers] = useState<Transfer[]>([]);
  const [dragOver, setDragOver] = useState(false);
  const transfersRef = useRef(transfers);
  transfersRef.current = transfers;

  const sendPath = async (path: string) => {
    const filename = baseName(path);
    const ts = Date.now();
    setTransfers((prev) => [{ dir: "out", filename, path, status: "sending", ts }, ...prev]);
    ipc.uiLog(`file_send_start peer=${peerName} path=${path}`);
    const doSend = async (confirmedSensitive?: string[]) =>
      ipc.requestFileTransfer(peerName, path, confirmedSensitive);
    try {
      await doSend();
      mark(ts, "sent");
      pushToast(`已发送 ${filename}`);
    } catch (e) {
      const msg = String(e);
      // 敏感文件：后端返回 sensitive-file:<相对路径> 时弹确认，确认后带 confirmedSensitive 重试。
      if (msg.includes("sensitive-file:")) {
        const rel = msg.split("sensitive-file:")[1]?.trim() || filename;
        if (window.confirm(`「${rel}」疑似敏感文件，确认仍要发送给 ${peerName}？`)) {
          try {
            await doSend([rel]);
            mark(ts, "sent");
            pushToast(`已发送 ${filename}`);
            return;
          } catch (e2) {
            mark(ts, "failed", String(e2));
            pushToast(`发送失败：${String(e2)}`);
            return;
          }
        }
        mark(ts, "failed", "用户取消（敏感文件）");
      } else {
        mark(ts, "failed", msg);
        pushToast(`发送失败：${msg}`);
      }
    }
  };

  const mark = (ts: number, status: Transfer["status"], detail?: string) =>
    setTransfers((prev) => prev.map((t) => (t.ts === ts ? { ...t, status, detail } : t)));

  // Tauri 原生拖拽事件（携带文件绝对路径）。webview 的 drop 拿不到真实路径，必须用它。
  useEffect(() => {
    let un: (() => void) | undefined;
    listen<{ paths: string[] }>("tauri://drag-drop", (p) => {
      setDragOver(false);
      const paths = (p as any)?.paths || (p as any) || [];
      const list: string[] = Array.isArray(paths) ? paths : [];
      if (!online) {
        pushToast("对端离线，无法发送");
        return;
      }
      list.forEach((path) => void sendPath(path));
    }).then((u) => (un = u));
    let un2: (() => void) | undefined;
    listen("tauri://drag-enter", () => setDragOver(true)).then((u) => (un2 = u));
    let un3: (() => void) | undefined;
    listen("tauri://drag-leave", () => setDragOver(false)).then((u) => (un3 = u));
    return () => {
      un?.();
      un2?.();
      un3?.();
    };
  }, [peerName, online]);

  return (
    <div style={{ marginTop: 14 }}>
      <div
        className={`drop-zone ${dragOver ? "over" : ""}`}
        title={online ? "拖拽文件到此发送" : "对端离线"}
      >
        <Upload size={22} />
        <div>{dragOver ? "松开发送" : `拖拽文件到此发送到 ${peerName}`}</div>
        <div className="faint" style={{ fontSize: 11 }}>
          {online ? "支持多文件；敏感文件会二次确认" : "对端离线，暂不可发送"}
        </div>
      </div>

      <div className="section-title">传输历史</div>
      {transfers.length === 0 ? (
        <p className="faint" style={{ fontSize: 12 }}>
          暂无传输记录
        </p>
      ) : (
        <div className="card flush">
          {transfers.map((t, i) => (
            <div className="tool-row" key={i} style={{ gridTemplateColumns: "20px 1fr auto auto" }}>
              <span title={t.dir === "out" ? "发出" : "收到"}>
                {t.dir === "out" ? <ArrowUp size={14} /> : <ArrowDown size={14} />}
              </span>
              <span className="path">{t.filename}</span>
              <span className="faint" style={{ fontSize: 11 }}>{fmtTime(String(t.ts))}</span>
              <span
                className={
                  t.status === "sent"
                    ? "status-pill synced"
                    : t.status === "failed"
                      ? "status-pill error"
                      : "status-pill syncing"
                }
                title={t.detail ?? ""}
              >
                {t.status === "sent" ? "已发送" : t.status === "failed" ? "失败" : "发送中"}
              </span>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
