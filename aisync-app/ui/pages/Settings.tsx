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
  const { setDialog, theme, setTheme } = useStore();
  const [s, setS] = useState<Settings | null>(null);
  useEffect(() => {
    ipc.getSettings().then(setS).catch(() => {});
  }, []);
  if (!s) return <div className="empty">加载中...</div>;

  const update = (patch: Partial<Settings>) => {
    const next = { ...s, ...patch };
    setS(next);
    ipc.saveSettings(next).catch(() => {});
  };

  return (
    <div>
      <div className="page-head">
        <h1>设置</h1>
      </div>

      <div className="section-title">本机</div>
      <div className="detail-grid">
        <span className="label">设备名称</span>
        <input value={s.deviceName} onChange={(e) => update({ deviceName: e.target.value })} />
        <span />
        <span className="label">设备 ID</span>
        <span className="path">{s.deviceId}</span>
        <button
          className="tiny"
          onClick={() => {
            navigator.clipboard?.writeText(s.deviceId);
            pushToast("已复制设备 ID");
          }}
        >
          <Copy size={13} />
        </button>
      </div>

      <div className="section-title">外观</div>
      <div className="detail-grid sync">
        <span className="label">主题</span>
        <span>
          <select
            className="pill"
            value={theme}
            onChange={(e) => setTheme(e.target.value as "dark" | "light" | "system")}
          >
            <option value="system">跟随系统</option>
            <option value="dark">暗色</option>
            <option value="light">亮色</option>
          </select>
        </span>
        <span className="faint">亮/暗主题，默认跟随系统</span>
      </div>

      <div className="section-title">AI 工具配置目录</div>
      <div className="card flush">
        {s.tools.map((t) => (
          <div className="tool-row" key={t.name}>
            <strong>{t.name}</strong>
            <span className="path">{t.installed ? t.configDir : "未检测到"}</span>
            <button
              className="tiny"
              onClick={() => {
                ipc.uiLog(`settings_tool_autodetect_clicked tool=${t.name}`);
                ipc
                  .getSettings()
                  .then((fresh) => {
                    setS(fresh);
                    pushToast(`已重新检测 ${t.name}`);
                  })
                  .catch(() => {});
              }}
            >
              自动检测
            </button>
            <button
              className="tiny"
              disabled={!t.installed}
              title={t.installed ? "在 Finder 中打开" : "未检测到该工具"}
              onClick={async () => {
                ipc.uiLog(`settings_tool_path_modify_clicked tool=${t.name} dir=${t.configDir}`);
                try {
                  await ipc.openPath(t.configDir);
                } catch (e) {
                  pushToast(`打开失败：${String(e)}`);
                }
              }}
            >
              打开目录
            </button>
          </div>
        ))}
      </div>

      <div className="section-title">同步</div>
      <div className="detail-grid sync">
        <span className="label">去抖动时间</span>
        <input
          type="number"
          style={{ width: 80 }}
          value={s.debounceSecs}
          onChange={(e) => update({ debounceSecs: +e.target.value })}
        />
        <span className="faint">秒 — 文件变更后等待多久触发同步</span>
        <span className="label">刷新周期</span>
        <input
          type="number"
          min={1}
          style={{ width: 80 }}
          value={s.refreshIntervalSecs}
          onChange={(e) => update({ refreshIntervalSecs: +e.target.value })}
        />
        <span className="faint">秒 — 会话目录 mtime 增量扫描</span>
        <span className="label">传输端口</span>
        <input
          type="number"
          style={{ width: 100 }}
          value={s.port}
          onChange={(e) => update({ port: +e.target.value })}
        />
        <span className="faint">TCP 通信端口</span>
      </div>

      <div className="section-title">全局排除规则</div>
      <textarea
        rows={8}
        value={s.globalExcludes.join("\n")}
        onChange={(e) => update({ globalExcludes: e.target.value.split("\n") })}
      />
      <div className="spread" style={{ marginTop: 7 }}>
        <span className="hint">每行一条 glob 模式</span>
        <button className="ghost tiny">恢复默认</button>
      </div>

      <div className="section-title">敏感文件模式</div>
      <textarea
        rows={5}
        value={s.sensitivePatterns.join("\n")}
        onChange={(e) => update({ sensitivePatterns: e.target.value.split("\n") })}
      />
      <div className="hint" style={{ marginTop: 7 }}>
        匹配这些模式的文件同步前需要额外确认
      </div>

      <div className="section-title">行为</div>
      <div className="behavior-list">
        <BehaviorRow
          label="开机自启动"
          checked={s.autoStart}
          onChange={(v) => update({ autoStart: v })}
        />
        <BehaviorRow
          label="最小化到托盘"
          checked={s.minimizeToTray}
          onChange={(v) => update({ minimizeToTray: v })}
        />
        <BehaviorRow
          label="同步完成通知"
          checked={s.notifyOnComplete}
          onChange={(v) => update({ notifyOnComplete: v })}
        />
      </div>

      <div className="section-title">日志</div>
      <div className="detail-grid">
        <span className="label">日志级别</span>
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
        <span className="label">日志目录</span>
        <span className="path">{s.logDir}</span>
        <button
          className="tiny"
          onClick={async () => {
            ipc.uiLog(`settings_open_log_dir_clicked dir=${s.logDir}`);
            try {
              await ipc.openPath(s.logDir);
            } catch (e) {
              pushToast(`打开失败：${String(e)}`);
            }
          }}
        >
          打开
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
          重新运行首次向导
        </button>
      </div>
      <div style={{ height: 20 }} />
    </div>
  );
}
