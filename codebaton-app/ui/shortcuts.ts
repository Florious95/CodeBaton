import { useEffect } from "react";
import { ipc } from "./ipc";
import { useStore } from "./store";

// Keyboard shortcuts — docs/ui-design.md §10.
export function useShortcuts() {
  const { view, setView, setDialog, dialog, selectedProjectId } = useStore();

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const mod = e.metaKey || e.ctrlKey;
      const onPage = view.page === "overview" || view.page === "peer";

      // Cmd/Ctrl + ,  → settings
      if (mod && e.key === ",") {
        e.preventDefault();
        setView({ page: "settings" });
        return;
      }
      // Cmd/Ctrl + Shift + P → push current project
      if (mod && e.shiftKey && (e.key === "P" || e.key === "p")) {
        e.preventDefault();
        if (onPage && selectedProjectId) {
          ipc.startSync(selectedProjectId, "push").catch(() => {});
          setDialog({ kind: "syncProgress" });
        }
        return;
      }
      // Cmd/Ctrl + Shift + L → pull current project
      if (mod && e.shiftKey && (e.key === "L" || e.key === "l")) {
        e.preventDefault();
        if (onPage && selectedProjectId) {
          ipc.startSync(selectedProjectId, "pull").catch(() => {});
          setDialog({ kind: "syncProgress" });
        }
        return;
      }
      // Cmd/Ctrl + N → add project
      if (mod && !e.shiftKey && (e.key === "n" || e.key === "N")) {
        e.preventDefault();
        if (view.page === "overview") setDialog({ kind: "addProject" });
        return;
      }
      // Esc → close dialog (handled in Dialog too, but covers no-overlay cases)
      if (e.key === "Escape" && dialog?.kind !== "wizard" && dialog?.kind !== "conflict") {
        setDialog(null);
        return;
      }
      // Cmd/Ctrl + W → minimize to tray (hide window)
      if (mod && (e.key === "w" || e.key === "W")) {
        e.preventDefault();
        if (ipc.inTauri()) {
          import("@tauri-apps/api/window").then(({ getCurrentWindow }) => {
            getCurrentWindow().hide();
          });
        }
        return;
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [view, dialog, selectedProjectId, setView, setDialog]);
}
