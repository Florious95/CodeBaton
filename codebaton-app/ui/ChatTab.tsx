import { useEffect, useRef, useState } from "react";
import { Copy } from "lucide-react";
import { pushToast, useStore } from "./store";
import { ipc } from "./ipc";
import { fmtTime } from "./util";

// ISS-031 第四轮兜底：即使外部 CSS 因某种原因没生效，也用 inline style
// 强制约束气泡宽度与换行，杜绝长文本撑破布局。inline style 优先级高于
// 样式表（非 !important 规则），是最后一道保险。
const MSG_STYLE: React.CSSProperties = { maxWidth: "70%", minWidth: 0 };
const BUBBLE_STYLE: React.CSSProperties = {
  wordBreak: "break-all",
  overflowWrap: "anywhere",
  overflow: "hidden",
  maxWidth: "100%",
  minWidth: 0,
  whiteSpace: "pre-wrap",
};

/**
 * P2「对话」Tab — 消息来自**全局 store**（ISS-021/022），不存组件本地 state，
 * 切 Tab/卸载不丢失。store 是唯一消息消费者；这里只读 + 发送 + 进 Tab 清未读。
 * 模块顶层组件，避免父级 3s 轮询重渲染时被卸载重建。
 */
export function ChatTab({ peerName, online }: { peerName: string; online: boolean }) {
  const { chatByPeer, sendChat, clearUnread, t } = useStore();
  const messages = chatByPeer[peerName] ?? [];
  const [draft, setDraft] = useState("");
  const [sending, setSending] = useState(false);
  const scrollRef = useRef<HTMLDivElement | null>(null);
  const lastBubbleRef = useRef<HTMLDivElement | null>(null);

  // ISS-031 运行时诊断：长消息渲染后，对比气泡 scrollWidth vs clientWidth。
  // scrollWidth > clientWidth 说明内容超出了气泡宽度（即"撑破"），日志带出
  // 实际像素，方便下次定位是哪一层没约束住。
  useEffect(() => {
    const el = lastBubbleRef.current;
    if (!el || messages.length === 0) return;
    const overflow = el.scrollWidth - el.clientWidth;
    const line =
      `[ISS-031] bubble scrollWidth=${el.scrollWidth} clientWidth=${el.clientWidth} ` +
      `overflow=${overflow}px logEl=${el.offsetWidth} ${overflow > 1 ? "⚠️撑破" : "OK"}`;
    console.log(line);
    // 通过 IPC 落到 ~/.aisync/logs/aisync.log，安装后无 devtools 也能取证（QA round4 要求）。
    ipc.uiLog(line);
  }, [messages.length]);

  // 进入该会话的对话 Tab → 清未读角标（ISS-020）。
  useEffect(() => {
    clearUnread(peerName, "chat");
  }, [peerName, messages.length, clearUnread]);

  // 新消息自动滚到底。
  useEffect(() => {
    const el = scrollRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [messages.length]);

  const send = async () => {
    const content = draft.trim();
    if (!content || sending) return;
    setSending(true);
    try {
      await sendChat(peerName, content);
      setDraft("");
    } catch (e) {
      pushToast(t.sendFailed(String(e)));
    } finally {
      setSending(false);
    }
  };

  return (
    <div className="chat" style={{ marginTop: 14 }}>
      <div className="chat-log" ref={scrollRef}>
        {messages.length === 0 ? (
          <p className="faint" style={{ fontSize: 12, textAlign: "center", padding: 20 }}>
            {t.chatEmpty(peerName)}
          </p>
        ) : (
          messages.map((m, i) => (
            <div className={`chat-msg ${m.mine ? "mine" : ""}`} key={i} style={MSG_STYLE}>
              <div className="bubble" style={BUBBLE_STYLE} ref={i === messages.length - 1 ? lastBubbleRef : undefined}>
                <span className="text" style={{ display: "block", maxWidth: "100%", wordBreak: "break-all", overflowWrap: "anywhere" }}>{m.content}</span>
                <button
                  className="copy-btn"
                  title={t.copy}
                  onClick={() => {
                    navigator.clipboard?.writeText(m.content);
                    pushToast(t.copied);
                  }}
                >
                  <Copy size={12} />
                </button>
              </div>
              <span className="meta faint">
                {m.mine ? t.me : m.senderName} · {fmtTime(String(m.timestamp))}
              </span>
            </div>
          ))
        )}
      </div>
      <div className="chat-input">
        <textarea
          rows={2}
          value={draft}
          placeholder={online ? t.chatPlaceholder : t.peerOffline}
          disabled={!online}
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              send();
            }
          }}
        />
        <button className="primary" disabled={!online || !draft.trim() || sending} onClick={send}>
          {t.send}
        </button>
      </div>
    </div>
  );
}
