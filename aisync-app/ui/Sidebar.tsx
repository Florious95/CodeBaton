import { useEffect, useState, type MouseEvent } from "react";
import { HelpCircle, Laptop, Monitor, Server, Settings as Cog } from "lucide-react";
import { ipc } from "./ipc";
import { useStore } from "./store";
import type { Peer } from "./types";

function PeerIcon({ os, kind }: { os: string; kind: string }) {
  if (kind === "discovered") return <HelpCircle size={18} className="icon" />;
  if (os === "darwin") return <Laptop size={18} className="icon" />;
  if (os === "linux") return <Server size={18} className="icon" />;
  return <Monitor size={18} className="icon" />;
}

/**
 * A single sidebar peer row.
 *
 * BUG-UI-002 (2nd pass): this component is defined at MODULE scope, NOT inside
 * Sidebar. Previously it was an inline closure recreated on every render — and
 * Sidebar re-renders every 3s when the discovery poll calls setPeers. React
 * treats a new function identity as a different component type, so it unmounted
 * and remounted the whole list each poll. A click landing in that window hit a
 * button that was torn down before its onClick could run → handler never fired,
 * no pair_dialog_opened log. Hoisting it (+ stable key={p.id}) keeps the DOM
 * node alive across polls so the click always registers.
 */
function PeerItem({ p, onRowClick }: { p: Peer; onRowClick: () => void }) {
  const { view, setDialog, t } = useStore();
  const active =
    (p.kind === "local" && view.page === "overview") ||
    (view.page === "peer" && view.peerId === p.id);

  const startPairing = (e: MouseEvent) => {
    // First line: prove the handler ran — written to the backend log file so
    // qa can see it in a release DMG (console is invisible there).
    ipc.uiLog(`pair_dialog_opened peerId=${p.id} name=${p.name}`);
    e.preventDefault();
    e.stopPropagation();
    // Capture peer id by value into the dialog state immediately — does not
    // depend on the list still holding this row after a refresh.
    setDialog({ kind: "pairing", peerId: p.id });
  };

  return (
    <div className={`sb-item ${active ? "active" : ""}`} onClick={onRowClick}>
      <PeerIcon os={p.os} kind={p.kind} />
      <div className="meta">
        <span className="name">{p.name}</span>
        {p.kind === "discovered" ? (
          <button
            className="primary tiny"
            style={{ marginTop: 4, width: "fit-content", padding: "2px 9px" }}
            onPointerDown={(e) => e.stopPropagation()}
            onClick={startPairing}
          >
            {t.pair}
          </button>
        ) : (
          <span className="status">
            <span className={`dot ${p.status}`} />
            {p.status === "online" ? t.online : t.offline}
          </span>
        )}
      </div>
    </div>
  );
}

/** True when two peer lists are identical for render purposes. */
function samePeers(a: Peer[], b: Peer[]): boolean {
  if (a.length !== b.length) return false;
  return a.every((p, i) => {
    const q = b[i];
    return (
      p.id === q.id &&
      p.kind === q.kind &&
      p.status === q.status &&
      p.name === q.name &&
      p.ip === q.ip &&
      p.pairedAt === q.pairedAt
    );
  });
}

export function Sidebar() {
  const { view, setView, t } = useStore();
  const [peers, setPeers] = useState<Peer[]>([]);

  useEffect(() => {
    // Only replace state when the list actually changed, so an unchanged poll
    // does not trigger a re-render (and a render mid-click can't drop the row).
    let lastSig = "";
    const load = () =>
      ipc
        .getPeers()
        .then((next) => {
          // BUG-UI2-R1: log what the poll actually returns so qa can see whether
          // discovered peers reach the frontend. Only log on change to avoid
          // flooding the file every 3s.
          const nd = next.filter((p) => p.kind === "discovered").length;
          const np = next.filter((p) => p.kind === "paired").length;
          const names = next.map((p) => `${p.name}:${p.kind}`).join(",");
          const sig = `${next.length}|${nd}|${np}|${names}`;
          if (sig !== lastSig) {
            lastSig = sig;
            ipc.uiLog(`getPeers returned total=${next.length} discovered=${nd} paired=${np} [${names}]`);
          }
          setPeers((prev) => (samePeers(prev, next) ? prev : next));
        })
        .catch((e) => ipc.uiLog(`getPeers failed error=${String(e)}`));
    load();
    const timer = setInterval(load, 3000);
    return () => clearInterval(timer);
  }, []);

  const local = peers.find((p) => p.kind === "local");
  const paired = peers.filter((p) => p.kind === "paired");
  const discovered = peers.filter((p) => p.kind === "discovered");

  return (
    <div className="sidebar">
      {local && (
        <PeerItem key={local.id} p={local} onRowClick={() => setView({ page: "overview" })} />
      )}

      {paired.length > 0 && <div className="sb-section">{t.paired}</div>}
      {paired.map((p) => (
        <PeerItem key={p.id} p={p} onRowClick={() => setView({ page: "peer", peerId: p.id })} />
      ))}

      {discovered.length > 0 && <div className="sb-section">{t.discovered}</div>}
      {discovered.map((p) => (
        <PeerItem key={p.id} p={p} onRowClick={() => {}} />
      ))}

      <div className="sb-spacer" />
      <div className="sb-divider" />
      <div
        className={`sb-item ${view.page === "settings" ? "active" : ""}`}
        onClick={() => setView({ page: "settings" })}
      >
        <Cog size={18} className="icon" />
        <span className="name">{t.settings}</span>
      </div>
    </div>
  );
}
