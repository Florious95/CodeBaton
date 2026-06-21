import { useEffect, useState } from "react";
import { Copy } from "lucide-react";
import { ipc } from "../ipc";
import { pushToast, useStore } from "../store";
import type { Settings } from "../types";

function Toggle({ checked, onChange }: { checked: boolean; onChange: (v: boolean) => void }) {
  return (
    <span
      className={`switch ${checked ? "on" : ""}`}
      role="switch"
      aria-checked={checked}
      onClick={() => onChange(!checked)}
    >
      <span className="knob" />
    </span>
  );
}

/** Behavior row: label left, toggle right (prototype gap 13px). */
function BehaviorRow({
  label,
  checked,
  onChange,
}: {
  label: string;
  checked: boolean;
  onChange: (v: boolean) => void;
}) {
  return (
    <div className="behavior-row">
      <span>{label}</span>
      <Toggle checked={checked} onChange={onChange} />
    </div>
  );
}

export function SettingsPage() {
  const { setDialog, theme, setTheme, t: tr } = useStore();
  const [s, setS] = useState<Settings | null>(null);
  useEffect(() => {
    ipc.getSettings().then(setS).catch(() => {});
  }, []);
  if (!s) return <div className="empty">{tr.loading}</div>;

  const update = (patch: Partial<Settings>) => {
    const next = { ...s, ...patch };
    setS(next);
    ipc.saveSettings(next).catch(() => {});
  };

  return (
    <div>
      <div className="page-head">
        <h1>{tr.settingsTitle}</h1>
      </div>

      <div className="section-title">{tr.selfMachine}</div>
      <div className="detail-grid">
        <span className="label">{tr.deviceName}</span>
        <input value={s.deviceName} onChange={(e) => update({ deviceName: e.target.value })} />
        <span />
        <span className="label">{tr.deviceId}</span>
        <span className="path">{s.deviceId}</span>
        <button
          className="tiny"
          onClick={() => {
            navigator.clipboard?.writeText(s.deviceId);
            pushToast(tr.copiedDeviceId);
          }}
        >
          <Copy size={13} />
        </button>
      </div>

      <div className="section-title">{tr.appearance}</div>
      <div className="detail-grid sync">
        <span className="label">{tr.theme}</span>
        <span>
          <select
            className="pill"
            value={theme}
            onChange={(e) => setTheme(e.target.value as "dark" | "light" | "system")}
          >
            <option value="system">{tr.themeSystem}</option>
            <option value="dark">{tr.themeDark}</option>
            <option value="light">{tr.themeLight}</option>
          </select>
        </span>
        <span className="faint">{tr.themeFollowsSystem}</span>
      </div>

      <div className="section-title">{tr.aiToolConfigDir}</div>
      <div className="card flush">
        {s.tools.map((t) => (
          <div className="tool-row" key={t.name}>
            <strong>{t.name}</strong>
            <span className="path">{t.installed ? t.configDir : tr.notDetected}</span>
            <button
              className="tiny"
              onClick={() => {
                ipc.uiLog(`settings_tool_autodetect_clicked tool=${t.name}`);
                ipc
                  .getSettings()
                  .then((fresh) => {
                    setS(fresh);
                    pushToast(tr.redetected(t.name));
                  })
                  .catch(() => {});
              }}
            >
              {tr.autoDetect}
            </button>
            <button
              className="tiny"
              disabled={!t.installed}
              title={t.installed ? tr.openInFinder : tr.toolNotDetected}
              onClick={async () => {
                ipc.uiLog(`settings_tool_path_modify_clicked tool=${t.name} dir=${t.configDir}`);
                try {
                  await ipc.openPath(t.configDir);
                } catch (e) {
                  pushToast(tr.openFailed(String(e)));
                }
              }}
            >
              {tr.openDir}
            </button>
          </div>
        ))}
      </div>

      <div className="section-title">{tr.syncSection}</div>
      <div className="detail-grid sync">
        <span className="label">{tr.debounceTime}</span>
        <input
          type="number"
          style={{ width: 80 }}
          value={s.debounceSecs}
          onChange={(e) => update({ debounceSecs: +e.target.value })}
        />
        <span className="faint">{tr.debounceHint}</span>
        <span className="label">{tr.refreshInterval}</span>
        <input
          type="number"
          min={1}
          style={{ width: 80 }}
          value={s.refreshIntervalSecs}
          onChange={(e) => update({ refreshIntervalSecs: +e.target.value })}
        />
        <span className="faint">{tr.refreshHint}</span>
        <span className="label">{tr.transferPort}</span>
        <input
          type="number"
          style={{ width: 100 }}
          value={s.port}
          onChange={(e) => update({ port: +e.target.value })}
        />
        <span className="faint">{tr.portHint}</span>
      </div>

      <div className="section-title">{tr.globalExcludeRules}</div>
      <textarea
        rows={8}
        value={s.globalExcludes.join("\n")}
        onChange={(e) => update({ globalExcludes: e.target.value.split("\n") })}
      />
      <div className="spread" style={{ marginTop: 7 }}>
        <span className="hint">{tr.globPerLine}</span>
        <button className="ghost tiny">{tr.restoreDefault}</button>
      </div>

      <div className="section-title">{tr.sensitivePatterns}</div>
      <textarea
        rows={5}
        value={s.sensitivePatterns.join("\n")}
        onChange={(e) => update({ sensitivePatterns: e.target.value.split("\n") })}
      />
      <div className="hint" style={{ marginTop: 7 }}>
        {tr.sensitiveHint}
      </div>

      <div className="section-title">{tr.behavior}</div>
      <div className="behavior-list">
        <BehaviorRow
          label={tr.autoStart}
          checked={s.autoStart}
          onChange={(v) => update({ autoStart: v })}
        />
        <BehaviorRow
          label={tr.minimizeToTray}
          checked={s.minimizeToTray}
          onChange={(v) => update({ minimizeToTray: v })}
        />
        <BehaviorRow
          label={tr.notifyOnComplete}
          checked={s.notifyOnComplete}
          onChange={(v) => update({ notifyOnComplete: v })}
        />
      </div>

      <div className="section-title">{tr.logs}</div>
      <div className="detail-grid">
        <span className="label">{tr.logLevel}</span>
        <span>
          <select
            className="pill"
            value={s.logLevel}
            onChange={(e) => update({ logLevel: e.target.value })}
          >
            <option>Error</option>
            <option>Warn</option>
            <option>Info</option>
            <option>Debug</option>
            <option>Trace</option>
          </select>
        </span>
        <span />
        <span className="label">{tr.logDir}</span>
        <span className="path">{s.logDir}</span>
        <button
          className="tiny"
          onClick={async () => {
            ipc.uiLog(`settings_open_log_dir_clicked dir=${s.logDir}`);
            try {
              await ipc.openPath(s.logDir);
            } catch (e) {
              pushToast(tr.openFailed(String(e)));
            }
          }}
        >
          {tr.open}
        </button>
      </div>

      <div
        style={{
          marginTop: 26,
          paddingTop: 18,
          borderTop: "1px solid var(--border-line)",
        }}
      >
        <button
          className="tiny"
          onClick={() => {
            ipc.uiLog("rerun_wizard_clicked");
            setDialog({ kind: "wizard" });
          }}
        >
          {tr.rerunWizard}
        </button>
      </div>
      <div style={{ height: 20 }} />
    </div>
  );
}
