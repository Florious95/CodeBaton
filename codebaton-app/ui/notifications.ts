import { useEffect } from "react";
import { ipc, listen } from "./ipc";
import type { SyncResult } from "./types";
import { fmtBytes } from "./util";

// System notifications — docs/ui-design.md §9.
// Only sent when the window is hidden / not focused.
export function useNotifications() {
  useEffect(() => {
    if (!ipc.inTauri()) return;
    let unsub: (() => void) | undefined;

    (async () => {
      const notif = await import("@tauri-apps/plugin-notification");
      let granted = await notif.isPermissionGranted();
      if (!granted) granted = (await notif.requestPermission()) === "granted";

      const send = (title: string, body: string) => {
        if (granted && !document.hasFocus()) notif.sendNotification({ title, body });
      };

      unsub = await listen<SyncResult>("sync-result", (r) => {
        const scope = r.workspaceName
          ? r.childName
            ? `${r.workspaceName}/${r.childName}`
            : r.workspaceName
          : r.projectName;
        if (r.success) {
          // "receive" = this device got an inbound handoff; otherwise it's our push.
          const route =
            r.direction === "receive"
              ? `${r.peerName} → 本机`
              : `本机 → ${r.peerName}`;
          send("CodeBaton: 同步完成", `${scope} ${route}, ${r.files} 文件, ${fmtBytes(r.bytes)}`);
        } else {
          send("CodeBaton: 同步失败", `${scope}: ${r.error ?? "未知错误"}`);
        }
      });
    })();

    return () => unsub?.();
  }, []);
}
