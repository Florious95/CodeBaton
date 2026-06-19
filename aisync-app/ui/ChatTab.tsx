import { useEffect, useRef, useState } from "react";
import { Copy } from "lucide-react";
import { ipc } from "./ipc";
import { pushToast } from "./store";
import type { TextMessage } from "./types";
import { fmtTime } from "./util";

/**
 * P2「对话」Tab — 通过现有 TLS 通道与对端发纯文本消息（net 的 TextMessage 帧）。
 * 上方对话历史（新消息在下、自动滚到底），下方输入框；悬停消息显示复制图标。
 *
 * 模块顶层组件（非内联），避免父组件每 3s 轮询重渲染时被卸载重建。
 */
type ChatEntry = TextMessage & { mine: boolean };

export function ChatTab({ peerName, online }: { peerName: string; online: boolean }) {
  const [messages, setMessages] = useState<ChatEntry[]>([]);
  const [draft, setDraft] = useState("");
  const [sending, setSending] = useState(false);
  const scrollRef = useRef<HTMLDivElement | null>(null);

  // 轮询入站消息（pending_text_message 是单条队列）。只收发给本机、来自该 peer 的。
  useEffect(() => {
    let cancelled = false;
    const poll = () =>
      ipc
        .pendingTextMessage()
        .then((m) => {
          if (cancelled || !m) return;
          if (m.senderName !== peerName) return; // 别的设备的消息不进这个会话
          setMessages((prev) => [...prev, { ...m, mine: false }]);
        })
        .catch(() => {});
    poll();
    const timer = window.setInterval(poll, 1000);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [peerName]);

  // 新消息到达自动滚到底。
  useEffect(() => {
    const el = scrollRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [messages.length]);

  const send = async () => {
    const content = draft.trim();
    if (!content || sending) return;
    setSending(true);
    ipc.uiLog(`chat_send peer=${peerName} bytes=${content.length}`);
    try {
      await ipc.sendTextMessage(peerName, content);
      setMessages((prev) => [
        ...prev,
        { senderName: "我", content, timestamp: Date.now(), mine: true },
      ]);
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
