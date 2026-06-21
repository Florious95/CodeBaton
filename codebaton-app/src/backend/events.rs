//! Event counters / recorded-event ring buffer + log sink.
//! Extracted from backend.rs (refactor phase 1, step 2).
//!
//! `app_log` stays in mod.rs (150+ call sites); it calls `record_event` +
//! `log_line` from here. Event storage is a LIVE production feature (app_log is
//! the app's structured logger); tests only read it — nothing here is gated to
//! cfg(test).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use super::home_dir;

/// 测试可观测事件计数：按 event 名累计。让黑盒测试无需解析日志、无需等 90s
/// cooldown 即可断言「自动同步是否触发 / 是否因 incoming 抑制 / 指纹门是否命中」。
/// 生产开销 = 一次哈希 + 计数，可忽略。
static EVENT_COUNTERS: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();

/// 测试可观测事件**记录**：捕获每条 app_log 事件的名称 + 全部字段键值，供结构化日志断言
/// （AUTO-100/101/102：历史角色、TLS 阶段定位、备份/回收站审计）。环形缓冲上限 2000 条，
/// 内存有界。生产开销 = 一次 clone + push（仅在已有测试触发 get_or_init 后才分配）。
#[derive(Clone)]
pub struct RecordedEvent {
    pub event: String,
    pub fields: Vec<(String, String)>,
}

impl RecordedEvent {
    /// 取某字段值。
    pub fn field(&self, key: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

static EVENT_LOG: OnceLock<Mutex<Vec<RecordedEvent>>> = OnceLock::new();

pub(crate) fn record_event(event: &str, fields: &[(&str, String)]) {
    let counters = EVENT_COUNTERS.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(mut map) = counters.lock() {
        *map.entry(event.to_string()).or_insert(0) += 1;
        // 同时按 event:<project|name> 复合键计数，让并行测试用唯一项目名做隔离断言。
        if let Some((_, scope)) = fields
            .iter()
            .find(|(k, _)| *k == "project" || *k == "name")
        {
            *map.entry(format!("{event}:{scope}")).or_insert(0) += 1;
        }
    }
    let log = EVENT_LOG.get_or_init(|| Mutex::new(Vec::new()));
    if let Ok(mut buf) = log.lock() {
        if buf.len() >= 2000 {
            buf.drain(0..1000); // 环形：超限丢最旧一半
        }
        buf.push(RecordedEvent {
            event: event.to_string(),
            fields: fields.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(),
        });
    }
}

/// 采样当前进程 RSS（常驻内存，字节）。测试用于内存峰值断言（070/071/072）。
/// macOS 经 mach task_info；其他平台返回 0（调用方需 cfg 跳过断言）。
#[cfg(target_os = "macos")]
pub fn current_rss_bytes() -> u64 {
    use std::mem;
    unsafe {
        let mut info: libc::mach_task_basic_info = mem::zeroed();
        let mut count =
            (mem::size_of::<libc::mach_task_basic_info>() / mem::size_of::<libc::natural_t>())
                as libc::mach_msg_type_number_t;
        let r = libc::task_info(
            libc::mach_task_self(),
            libc::MACH_TASK_BASIC_INFO as libc::task_flavor_t,
            &mut info as *mut _ as libc::task_info_t,
            &mut count,
        );
        if r == libc::KERN_SUCCESS {
            info.resident_size
        } else {
            0
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub fn current_rss_bytes() -> u64 {
    0
}

/// 查询已记录事件：返回所有 `event` 名匹配、且（若给 `project`）含 project/name=project
/// 字段的事件（最新在后）。供测试做结构化日志断言。并行隔离：用唯一项目名过滤。
pub fn events_for(event: &str, project: Option<&str>) -> Vec<RecordedEvent> {
    EVENT_LOG
        .get()
        .and_then(|m| m.lock().ok())
        .map(|buf| {
            buf.iter()
                .filter(|e| e.event == event)
                .filter(|e| match project {
                    None => true,
                    Some(p) => e.field("project") == Some(p) || e.field("name") == Some(p),
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// 读取某事件迄今累计次数（测试用；进程级全局）。
/// 传 `"event"` 取全局总数；传 `"event:project-name"` 取该项目的计数（并行隔离）。
pub fn event_count(event: &str) -> u64 {
    EVENT_COUNTERS
        .get()
        .and_then(|m| m.lock().ok())
        .and_then(|map| map.get(event).copied())
        .unwrap_or(0)
}

/// 重置事件计数（测试隔离用；进程级全局——并行测试应针对**特定项目名**断言增量，
/// 不依赖绝对值，故一般无需 reset）。
pub fn reset_event_counts() {
    if let Some(m) = EVENT_COUNTERS.get() {
        if let Ok(mut map) = m.lock() {
            map.clear();
        }
    }
}

/// Tee a log line to stderr AND `~/.aisync/logs/aisync.log`.
///
/// When the DMG is launched via `open -a`, stderr is redirected to /dev/null,
/// so stderr-only logs are invisible to qa. The file sink makes
/// `cat ~/.aisync/logs/aisync.log` work for field diagnostics.
pub fn log_line(line: &str) {
    eprintln!("{line}");
    if let Some(path) = log_file_path() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            use std::io::Write;
            let _ = writeln!(f, "{} {}", now_stamp(), line);
        }
    }
}

fn log_file_path() -> Option<PathBuf> {
    std::env::var_os("AISYNC_LOG_FILE")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join(".aisync").join("logs").join("aisync.log")))
}

/// Coarse wall-clock stamp for log lines (no extra deps): seconds since epoch.
fn now_stamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => format!("[t={}]", d.as_secs()),
        Err(_) => "[t=?]".to_string(),
    }
}
