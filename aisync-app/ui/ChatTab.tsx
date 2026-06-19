import { useEffect, useRef, useState } from "react";
import { Copy } from "lucide-react";
import { pushToast, useStore } from "./store";
import { fmtTime } from "./util";

/**
 * P2「对话」Tab — 消息来自**全局 store**（ISS-021/022），不存组件本地 state，
 * 切 Tab/卸载不丢失。store 是唯一消息消费者；这里只读 + 发送 + 进 Tab 清未读。
 * 模块顶层组件，避免父级 3s 轮询重渲染时被卸载重建。
 */
export function ChatTab({ peerName, online }: { peerName: string; online: boolean }) {
  const { chatByPeer, sendChat, clearUnread } = useStore();
  const messages = chatByPeer[peerName] ?? [];
  const [draft, setDraft] = useState("");
  const [sending, setSending] = useState(false);
  const scrollRef = useRef<HTMLDivElement | null>(null);

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
      pushToast(`发送失败：${String(e)}`);
    } finally {
      setSending(false);
    }
  };

  return (
    <div className="chat" style={{ marginTop: 14 }}>
      <div className="chat-log" ref={scrollRef}>
        {messages.length === 0 ? (
          <p className="faint" style={{ fontSize: 12, textAlign: "center", padding: 20 }}>
            还没有消息。在下方输入框给 {peerName} 发条消息吧。
          </p>
        ) : (
          messages.map((m, i) => (
            <div className={`chat-msg ${m.mine ? "mine" : ""}`} key={i}>
              <div className="bubble">
                <span className="text">{m.content}</span>
                <button
                  className="copy-btn"
                  title="复制"
                  onClick={() => {
                    navigator.clipboard?.writeText(m.content);
                    pushToast("已复制");
                  }}
                >
                  <Copy size={12} />
                </button>
              </div>
              <span className="meta faint">
                {m.mine ? "我" : m.senderName} · {fmtTime(String(m.timestamp))}
              </span>
            </div>
          ))
        )}
      </div>
      <div className="chat-input">
        <textarea
          rows={2}
          value={draft}
          placeholder={online ? "输入消息，Enter 发送（Shift+Enter 换行）" : "对端离线"}
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
          发送
        </button>
      </div>
    </div>
  );
}
