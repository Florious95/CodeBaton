//! T4 Claude Code 会话解析器。
//!
//! 解析 `~/.claude/projects/<encoded-path>/<session-id>.jsonl`。
//! 每行是一条独立 JSON 记录（typed record）。原始项目路径存在记录的 `cwd` 字段里，
//! 而目录名是路径编码后的结果（非 ASCII → 横杠，有损 → 可能冲突，见 G4）。
//!
//! 设计约束：
//! - 写回必须 byte-identical（不丢字段、不重排、不改格式），除非显式做了路径重写（X8）。
//!   实现方式：每条记录同时保留**原始行字节**与解析后的 [`Value`]。未被重写的记录
//!   写回时直接输出原始字节；只有真正改过的记录才重新序列化。这比依赖 serde_json 的
//!   键序保留更可靠（连原始空白也保留），且不引入额外依赖。
//! - 映射 key 用 `cwd`（原始路径），不用编码目录名（G4）。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use codebaton_core::{AisyncError, Result, RewriteDirection};
use serde_json::Value;

use crate::path_rewriter::{Confidence, RewriteRecord, RewriteReport, RuleBasedRewriter};

/// 已知一定包含路径的结构化字段名（顶层或 tool_use input 内），confidence=High。
/// 仅这些字段做精确重写；其余内容保持原样。
const STRUCTURED_PATH_KEYS: &[&str] = &[
    "cwd",
    "file_path",
    "filePath",
    "path",
    "notebook_path",
    "working_directory",
    "workingDirectory",
];

/// 单行记录：原始字节 + 解析值 + 是否被改写。
#[derive(Debug, Clone, PartialEq)]
pub struct RecordLine {
    /// 原始行内容（不含换行符）。空行为 ""。
    raw: String,
    /// 解析后的 JSON 值；空行为 [`Value::Null`]。
    value: Value,
    /// 是否被路径重写改动过（决定写回时用 raw 还是重新序列化 value）。
    dirty: bool,
}

impl RecordLine {
    fn from_raw(raw: String, value: Value) -> Self {
        Self {
            raw,
            value,
            dirty: false,
        }
    }

    /// 从一个 JSON 值构造（无原始字节，按 compact 序列化）。供测试 / 信封还原使用。
    pub fn from_value(value: Value) -> Self {
        let raw = if value.is_null() {
            String::new()
        } else {
            serde_json::to_string(&value).unwrap_or_default()
        };
        Self {
            raw,
            value,
            dirty: false,
        }
    }

    pub fn value(&self) -> &Value {
        &self.value
    }

    /// 写回时使用的行内容。dirty 则重新序列化 value，否则用原始字节。
    fn emit(&self) -> Result<String> {
        if self.value.is_null() {
            return Ok(String::new());
        }
        if self.dirty {
            serde_json::to_string(&self.value)
                .map_err(|e| AisyncError::Session(format!("serialize record: {e}")))
        } else {
            Ok(self.raw.clone())
        }
    }
}

/// 解析后的单个会话。records 逐行保留原始 JSON，不做语义改写。
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedSession {
    pub session_id: String,
    /// 原始项目路径（来自 cwd），不是编码后的目录名。
    pub original_project_path: String,
    /// 磁盘上的编码目录名。
    pub encoded_dir_name: String,
    /// 每行一条记录，保持原始 JSON 结构与顺序。
    pub records: Vec<RecordLine>,
    /// 原始文件是否以换行结尾（写回时保持一致，保证 byte-identical）。
    trailing_newline: bool,
}

impl ParsedSession {
    /// 显式构造（供从信封还原 / 测试使用）。records 以 [`Value`] 给出，按 compact 序列化。
    pub fn from_parts(
        session_id: String,
        original_project_path: String,
        encoded_dir_name: String,
        records: Vec<Value>,
        trailing_newline: bool,
    ) -> Self {
        Self {
            session_id,
            original_project_path,
            encoded_dir_name,
            records: records.into_iter().map(RecordLine::from_value).collect(),
            trailing_newline,
        }
    }

    /// 原始文件是否以换行结尾（影响 byte-identical 写回）。
    pub fn trailing_newline(&self) -> bool {
        self.trailing_newline
    }

    /// 记录值的只读视图（供外部读取，不暴露 dirty 标志）。
    pub fn record_values(&self) -> Vec<Value> {
        self.records.iter().map(|r| r.value.clone()).collect()
    }
}

/// 一处路径引用及其位置与置信度。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathReference {
    pub value: String,
    pub location: PathLocation,
    pub confidence: Confidence,
}

/// 路径引用的位置：第几条记录、JSON 指针路径（如 `/message/content/0/input/file_path`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathLocation {
    pub record_index: usize,
    pub json_pointer: String,
}

/// 编码冲突：多个不同原始路径编码为同一目录名（G4 必须警告）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodingConflict {
    pub encoded_dir_name: String,
    pub original_paths: Vec<String>,
}

/// 会话元数据索引：{ 编码目录名 → 原始路径集合 }，用于检测冲突。
#[derive(Debug, Clone, Default)]
pub struct SessionIndex {
    map: BTreeMap<String, Vec<String>>,
}

impl SessionIndex {
    pub fn from_sessions(sessions: &[ParsedSession]) -> Self {
        let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for s in sessions {
            let entry = map.entry(s.encoded_dir_name.clone()).or_default();
            if !entry.contains(&s.original_project_path) {
                entry.push(s.original_project_path.clone());
            }
        }
        Self { map }
    }

    /// 返回所有编码冲突（同一目录名对应多个不同原始路径）。
    pub fn conflicts(&self) -> Vec<EncodingConflict> {
        self.map
            .iter()
            .filter(|(_, paths)| paths.len() > 1)
            .map(|(name, paths)| EncodingConflict {
                encoded_dir_name: name.clone(),
                original_paths: paths.clone(),
            })
            .collect()
    }

    /// 给定编码目录名查原始路径列表。
    pub fn original_paths(&self, encoded_dir_name: &str) -> Option<&[String]> {
        self.map.get(encoded_dir_name).map(Vec::as_slice)
    }
}

/// Claude Code 解析器。
#[derive(Debug, Default)]
pub struct ClaudeCodeParser;

impl ClaudeCodeParser {
    pub fn new() -> Self {
        Self
    }

    pub fn tool_name(&self) -> &str {
        "claude-code"
    }

    /// 判断 path 是否是 Claude Code 的 projects 配置目录。
    /// 约定：路径以 `projects` 结尾，或其下含 `projects` 子目录（即 `.claude`）。
    pub fn detect(path: &Path) -> bool {
        if !path.is_dir() {
            return false;
        }
        if path.file_name().and_then(|n| n.to_str()) == Some("projects") {
            return true;
        }
        path.join("projects").is_dir()
    }

    /// 解析 config_dir 下所有项目目录的所有会话。
    /// config_dir 可以是 `.claude` 或 `.claude/projects`。
    pub fn parse_sessions(config_dir: &Path) -> Result<Vec<ParsedSession>> {
        Self::parse_sessions_filtered(config_dir, |_| true)
    }

    /// 同 [`parse_sessions`]，但只解析 `dir_filter` 返回 true 的编码目录。
    ///
    /// 用于同步热路径：避免把整棵 `~/.claude/projects`（可能上千文件、GB 级）
    /// 全量读进内存后才按项目过滤。调用方传入只匹配目标项目编码目录名的谓词，
    /// 不匹配的子目录连 `read_dir`/`read_to_string` 都不会发生。
    /// 编码目录名按 `parse_session_file` 的来源约定，与 backend 的
    /// `claude_project_dir_name` 逐字符一致（仅 ASCII 字母数字 `-_.` 保留，其余塌缩成 `-`）。
    pub fn parse_sessions_filtered(
        config_dir: &Path,
        dir_filter: impl Fn(&str) -> bool,
    ) -> Result<Vec<ParsedSession>> {
        let projects_dir = if config_dir.join("projects").is_dir() {
            config_dir.join("projects")
        } else {
            config_dir.to_path_buf()
        };

        let mut sessions = Vec::new();
        let entries = fs::read_dir(&projects_dir)?;
        for entry in entries {
            let entry = entry?;
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let encoded_dir_name = match dir.file_name().and_then(|n| n.to_str()) {
                Some(name) => name.to_string(),
                None => continue,
            };
            if !dir_filter(&encoded_dir_name) {
                continue;
            }
            for file in fs::read_dir(&dir)? {
                let file = file?;
                let fpath = file.path();
                if fpath.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                let session = parse_session_file(&fpath, &encoded_dir_name)?;
                sessions.push(session);
            }
        }
        Ok(sessions)
    }

    /// 列出会话里所有结构化路径引用（confidence=High）。
    pub fn list_path_references(session: &ParsedSession) -> Vec<PathReference> {
        let mut refs = Vec::new();
        for (idx, record) in session.records.iter().enumerate() {
            collect_structured_paths(&record.value, idx, String::new(), &mut refs);
        }
        refs
    }

    /// 将（可能已重写的）会话写回 target_dir，文件名为 `<session_id>.jsonl`，
    /// 放在以 encoded_dir_name 命名的子目录下。保持 byte-identical（除已重写字段）。
    pub fn write_session(session: &ParsedSession, target_dir: &Path) -> Result<PathBuf> {
        let dir = target_dir.join(&session.encoded_dir_name);
        fs::create_dir_all(&dir)?;
        let file = dir.join(format!("{}.jsonl", session.session_id));
        let content = serialize_session(session)?;
        fs::write(&file, content)?;
        Ok(file)
    }

    /// 序列化为 JSONL 字符串（供传输层使用，无需落盘）。
    pub fn serialize(session: &ParsedSession) -> Result<String> {
        serialize_session(session)
    }

    /// 对会话中的结构化路径字段执行重写（X8：只动路径字段）。
    /// 返回重写报告。文本内容（对话正文等）不在此函数处理范围。
    pub fn rewrite_structured_paths(
        session: &mut ParsedSession,
        rewriter: &RuleBasedRewriter,
        direction: RewriteDirection,
    ) -> RewriteReport {
        let mut report = RewriteReport::default();
        for record in session.records.iter_mut() {
            let mut changed = false;
            rewrite_structured_in_value(
                &mut record.value,
                rewriter,
                direction,
                &mut report,
                &mut changed,
            );
            if changed {
                record.dirty = true;
            }
        }
        // original_project_path 同步更新为重写后的首个 cwd（如有变化）。
        if let Some(cwd) = session
            .records
            .iter()
            .find_map(|r| r.value.get("cwd").and_then(Value::as_str))
        {
            session.original_project_path = cwd.to_string();
        }
        report
    }
}

/// 读取并解析单个 jsonl 文件。
fn parse_session_file(path: &Path, encoded_dir_name: &str) -> Result<ParsedSession> {
    let raw = fs::read_to_string(path)
        .map_err(|e| AisyncError::Session(format!("read {}: {e}", path.display())))?;
    let trailing_newline = raw.ends_with('\n');

    let mut records = Vec::new();
    for (lineno, line) in raw.lines().enumerate() {
        if line.is_empty() {
            records.push(RecordLine::from_raw(String::new(), Value::Null));
            continue;
        }
        let value: Value = serde_json::from_str(line).map_err(|e| {
            AisyncError::Session(format!(
                "{}:{}: invalid json: {e}",
                path.display(),
                lineno + 1
            ))
        })?;
        records.push(RecordLine::from_raw(line.to_string(), value));
    }

    let session_id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();

    let original_project_path =
        extract_original_path(&records).unwrap_or_else(|| encoded_dir_name.to_string());

    Ok(ParsedSession {
        session_id,
        original_project_path,
        encoded_dir_name: encoded_dir_name.to_string(),
        records,
        trailing_newline,
    })
}

/// 从记录中提取原始项目路径：取首个出现的顶层 `cwd`（G4）。
fn extract_original_path(records: &[RecordLine]) -> Option<String> {
    records
        .iter()
        .find_map(|r| r.value.get("cwd").and_then(Value::as_str))
        .map(str::to_string)
}

/// 序列化会话为 JSONL。每行用 [`RecordLine::emit`]（未改动用原始字节）。
fn serialize_session(session: &ParsedSession) -> Result<String> {
    let mut out = String::new();
    for (i, record) in session.records.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&record.emit()?);
    }
    if session.trailing_newline {
        out.push('\n');
    }
    Ok(out)
}

/// 递归收集结构化路径字段。json_pointer 记录定位（RFC6901 风格）。
fn collect_structured_paths(
    value: &Value,
    record_index: usize,
    pointer: String,
    out: &mut Vec<PathReference>,
) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                let child_ptr = format!("{pointer}/{}", escape_pointer_token(k));
                if STRUCTURED_PATH_KEYS.contains(&k.as_str()) {
                    if let Some(s) = v.as_str() {
                        out.push(PathReference {
                            value: s.to_string(),
                            location: PathLocation {
                                record_index,
                                json_pointer: child_ptr.clone(),
                            },
                            confidence: Confidence::High,
                        });
                    }
                }
                collect_structured_paths(v, record_index, child_ptr, out);
            }
        }
        Value::Array(arr) => {
            for (i, v) in arr.iter().enumerate() {
                let child_ptr = format!("{pointer}/{i}");
                collect_structured_paths(v, record_index, child_ptr, out);
            }
        }
        _ => {}
    }
}

/// 递归重写结构化路径字段。只替换 STRUCTURED_PATH_KEYS 命中的字符串值。
/// changed 置 true 表示本记录有改动。
fn rewrite_structured_in_value(
    value: &mut Value,
    rewriter: &RuleBasedRewriter,
    direction: RewriteDirection,
    report: &mut RewriteReport,
    changed: &mut bool,
) {
    match value {
        Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                if STRUCTURED_PATH_KEYS.contains(&k.as_str()) {
                    if let Some(s) = v.as_str() {
                        if let Some(rewritten) = rewriter.rewrite_structured_value(s, direction) {
                            report.applied.push(RewriteRecord {
                                before: s.to_string(),
                                after: rewritten.clone(),
                                confidence: Confidence::High,
                            });
                            *v = Value::String(rewritten);
                            *changed = true;
                            continue;
                        }
                    }
                }
                rewrite_structured_in_value(v, rewriter, direction, report, changed);
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                rewrite_structured_in_value(v, rewriter, direction, report, changed);
            }
        }
        _ => {}
    }
}

fn escape_pointer_token(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path_rewriter::PathRule;
    use std::io::Write;

    fn write_jsonl(dir: &Path, name: &str, lines: &[&str]) -> PathBuf {
        let f = dir.join(name);
        let mut file = fs::File::create(&f).unwrap();
        for l in lines {
            writeln!(file, "{l}").unwrap();
        }
        f
    }

    #[test]
    fn parses_cwd_as_original_path() {
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path().join("projects");
        let dir = projects.join("-Users-alice-code---");
        fs::create_dir_all(&dir).unwrap();
        write_jsonl(
            &dir,
            "sess1.jsonl",
            &[
                r#"{"type":"mode","sessionId":"sess1"}"#,
                r#"{"type":"user","cwd":"/Users/alice/code/中文项目","sessionId":"sess1"}"#,
            ],
        );

        let sessions = ClaudeCodeParser::parse_sessions(&projects).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions[0].original_project_path,
            "/Users/alice/code/中文项目"
        );
        assert_eq!(sessions[0].encoded_dir_name, "-Users-alice-code---");
        assert_eq!(sessions[0].session_id, "sess1");
    }

    #[test]
    fn parse_sessions_filtered_only_scans_matching_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path().join("projects");

        // 目标项目目录
        let target = projects.join("-Users-alice-code-target");
        fs::create_dir_all(&target).unwrap();
        write_jsonl(
            &target,
            "s.jsonl",
            &[r#"{"type":"user","cwd":"/Users/alice/code/target","sessionId":"s"}"#],
        );

        // 不相关的大目录：若被解析会读进内容；过滤后应整体跳过。
        let other = projects.join("-Users-alice-code-other");
        fs::create_dir_all(&other).unwrap();
        write_jsonl(
            &other,
            "x.jsonl",
            &[r#"{"type":"user","cwd":"/Users/alice/code/other","sessionId":"x"}"#],
        );

        let sessions = ClaudeCodeParser::parse_sessions_filtered(&projects, |encoded| {
            encoded == "-Users-alice-code-target"
        })
        .unwrap();
        assert_eq!(sessions.len(), 1, "only the matching dir should be parsed");
        assert_eq!(sessions[0].original_project_path, "/Users/alice/code/target");

        // 前缀过滤（workspace 场景）：root 编码名是子目录编码名的前缀。
        let sub = projects.join("-Users-alice-code-target-sub");
        fs::create_dir_all(&sub).unwrap();
        write_jsonl(
            &sub,
            "y.jsonl",
            &[r#"{"type":"user","cwd":"/Users/alice/code/target/sub","sessionId":"y"}"#],
        );
        let prefix_hits = ClaudeCodeParser::parse_sessions_filtered(&projects, |encoded| {
            encoded.starts_with("-Users-alice-code-target")
        })
        .unwrap();
        assert_eq!(
            prefix_hits.len(),
            2,
            "prefix filter should catch root + subdir, not the unrelated dir"
        );

        // 兼容性：无过滤等价于全量扫描。
        let all = ClaudeCodeParser::parse_sessions(&projects).unwrap();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn write_back_is_byte_identical() {
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path().join("projects");
        let dir = projects.join("-Users-alice-x");
        fs::create_dir_all(&dir).unwrap();
        // 故意使用非字母序的键顺序，验证不依赖 serde_json 键序。
        let lines = [
            r#"{"type":"mode","sessionId":"s","aaa":1}"#,
            r#"{"zzz":"end","type":"user","cwd":"/Users/alice/x","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"/Users/alice/x/a.rs"}}]}}"#,
        ];
        let src = write_jsonl(&dir, "s.jsonl", &lines);
        let original_bytes = fs::read(&src).unwrap();

        let sessions = ClaudeCodeParser::parse_sessions(&projects).unwrap();
        let out_dir = tmp.path().join("out");
        let written = ClaudeCodeParser::write_session(&sessions[0], &out_dir).unwrap();
        let written_bytes = fs::read(&written).unwrap();

        assert_eq!(
            written_bytes, original_bytes,
            "round-trip must be byte-identical"
        );
    }

    #[test]
    fn lists_structured_path_references() {
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path().join("projects");
        let dir = projects.join("enc");
        fs::create_dir_all(&dir).unwrap();
        write_jsonl(
            &dir,
            "s.jsonl",
            &[
                r#"{"type":"user","cwd":"/Users/alice/x"}"#,
                r#"{"message":{"content":[{"type":"tool_use","input":{"file_path":"/Users/alice/x/a.rs"}}]}}"#,
            ],
        );
        let sessions = ClaudeCodeParser::parse_sessions(&projects).unwrap();
        let refs = ClaudeCodeParser::list_path_references(&sessions[0]);
        let values: Vec<_> = refs.iter().map(|r| r.value.as_str()).collect();
        assert!(values.contains(&"/Users/alice/x"));
        assert!(values.contains(&"/Users/alice/x/a.rs"));
        assert!(refs.iter().all(|r| r.confidence == Confidence::High));
    }

    #[test]
    fn detects_encoding_conflict() {
        // 两个不同中文路径编码为相同目录名。
        let s1 = ParsedSession::from_parts(
            "a".into(),
            "/Users/alice/code/项目一".into(),
            "-Users-alice-code---".into(),
            vec![],
            true,
        );
        let s2 = ParsedSession::from_parts(
            "b".into(),
            "/Users/alice/code/项目二".into(),
            "-Users-alice-code---".into(),
            vec![],
            true,
        );
        let index = SessionIndex::from_sessions(&[s1, s2]);
        let conflicts = index.conflicts();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].original_paths.len(), 2);
    }

    #[test]
    fn rewrite_structured_paths_updates_fields_and_original() {
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path().join("projects");
        let dir = projects.join("enc");
        fs::create_dir_all(&dir).unwrap();
        write_jsonl(
            &dir,
            "s.jsonl",
            &[
                r#"{"type":"user","cwd":"/Users/alice/x"}"#,
                r#"{"message":{"content":[{"type":"tool_use","input":{"file_path":"/Users/alice/x/a.rs"}}]}}"#,
            ],
        );
        let mut sessions = ClaudeCodeParser::parse_sessions(&projects).unwrap();
        let rewriter = RuleBasedRewriter::new(vec![PathRule::unix_to_unix(
            "/Users/alice/x",
            "/home/bob/x",
        )])
        .unwrap();
        let report = ClaudeCodeParser::rewrite_structured_paths(
            &mut sessions[0],
            &rewriter,
            RewriteDirection::SourceToTarget,
        );
        assert_eq!(report.applied.len(), 2);
        assert_eq!(sessions[0].original_project_path, "/home/bob/x");

        let refs = ClaudeCodeParser::list_path_references(&sessions[0]);
        assert!(refs.iter().any(|r| r.value == "/home/bob/x/a.rs"));
        assert!(refs.iter().any(|r| r.value == "/home/bob/x"));
    }

    #[test]
    fn rewrite_then_reverse_restores_session() {
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path().join("projects");
        let dir = projects.join("enc");
        fs::create_dir_all(&dir).unwrap();
        write_jsonl(
            &dir,
            "s.jsonl",
            &[
                r#"{"type":"user","cwd":"/Users/alice/x","message":{"content":[{"input":{"file_path":"/Users/alice/x/a.rs"}}]}}"#,
            ],
        );
        let mut sessions = ClaudeCodeParser::parse_sessions(&projects).unwrap();
        // 用语义值比较：G3 要求路径内容可逆还原（不是被改记录的字节布局——改过的记录在
        // 目标端本就会重新序列化）。
        let before = sessions[0].record_values();
        let rewriter = RuleBasedRewriter::new(vec![PathRule::unix_to_windows(
            "/Users/alice/x",
            "C:\\Users\\bob\\x",
        )])
        .unwrap();
        ClaudeCodeParser::rewrite_structured_paths(
            &mut sessions[0],
            &rewriter,
            RewriteDirection::SourceToTarget,
        );
        ClaudeCodeParser::rewrite_structured_paths(
            &mut sessions[0],
            &rewriter,
            RewriteDirection::TargetToSource,
        );
        let after = sessions[0].record_values();
        assert_eq!(
            before, after,
            "rewrite round-trip must restore path content"
        );
    }

    #[test]
    fn unmodified_record_keeps_raw_bytes_after_partial_rewrite() {
        // 一条命中、一条不命中：未命中的那条应原样保留 raw（即便键序非字母序）。
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path().join("projects");
        let dir = projects.join("enc");
        fs::create_dir_all(&dir).unwrap();
        let untouched = r#"{"zzz":1,"aaa":2,"note":"no path here"}"#;
        write_jsonl(&dir, "s.jsonl", &[r#"{"cwd":"/Users/alice/x"}"#, untouched]);
        let mut sessions = ClaudeCodeParser::parse_sessions(&projects).unwrap();
        let rewriter =
            RuleBasedRewriter::new(vec![PathRule::unix_to_unix("/Users/alice/x", "/home/bob")])
                .unwrap();
        ClaudeCodeParser::rewrite_structured_paths(
            &mut sessions[0],
            &rewriter,
            RewriteDirection::SourceToTarget,
        );
        let out = ClaudeCodeParser::serialize(&sessions[0]).unwrap();
        assert!(
            out.contains(untouched),
            "untouched line must keep original bytes/key order"
        );
        assert!(out.contains(r#""cwd":"/home/bob""#));
    }
}
