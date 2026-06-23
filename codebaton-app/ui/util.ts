export function fmtBytes(b: number): string {
  if (b === 0) return "0B";
  const u = ["B", "KB", "MB", "GB"];
  const i = Math.min(Math.floor(Math.log(b) / Math.log(1024)), u.length - 1);
  const v = b / Math.pow(1024, i);
  return `${v >= 10 || i === 0 ? Math.round(v) : v.toFixed(1)}${u[i]}`;
}

/** Format a backend timestamp (epoch milliseconds as string) to local date/time. */
export function fmtTime(ts: string): string {
  if (!ts) return "";
  let ms = Number(ts);
  if (!Number.isFinite(ms)) return ts; // already human-readable
  // ISS-025: 后端应传毫秒级。但防御性处理——10 位左右是秒级（否则当毫秒会显示
  // 1970 年），<1e12 视为秒，乘以 1000 还原。
  if (ms > 0 && ms < 1e12) ms = ms * 1000;
  const d = new Date(ms);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

export function modeLabel(mode: string): string {
  // Manual handoff is push-only; other modes are legacy/dead.
  return mode === "oneWayPush" ? "单向推送" : mode;
}

export function osLabel(os: string): string {
  switch (os) {
    case "darwin":
      return "macOS";
    case "windows":
      return "Windows";
    case "linux":
      return "Linux";
    default:
      return os;
  }
}
