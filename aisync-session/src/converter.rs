//! T12 跨 AI 工具会话格式转换。
//!
//! 插件化：每个目标工具实现 [`SessionConverter`]，把 Claude Code 的 [`ParsedSession`]
//! 转成目标工具可加载的格式（[`ConvertedSession`]：一组 JSONL 记录 + 落盘文件名）。
//!
//! 约束：
//! - **路径重写在格式转换之前**：调用方先用 [`ClaudeCodeParser::rewrite_structured_paths`]
//!   把 ParsedSession 的结构化路径字段重写好，再交给转换器（见 tasks T12.2）。
//! - **X8 不篡改非路径内容**：转换只做结构搬运，不改写/摘要/注入消息文本；无法干净映射的
//!   Claude 专有结构（tool_use 等）不强行塞进目标 schema，避免臆造。
//! - **X7 不解析用户代码**：只读会话 JSON 字段，不解析消息正文里的代码语义。

use aisync_core::{AisyncError, Result};
use serde_json::{json, Value};

use crate::claude_code::ParsedSession;

/// 转换产物：目标工具的会话记录（逐行 JSONL）+ 建议文件名。
#[derive(Debug, Clone, PartialEq)]
pub struct ConvertedSession {
    /// 目标工具名（"codex" / "gemini"）。
    pub tool_name: String,
    /// 建议的落盘文件名（不含目录）。
    pub file_name: String,
    /// 逐行 JSON 记录。
    pub records: Vec<Value>,
}

impl ConvertedSession {
    /// 序列化为 JSONL 文本（每行一条 compact JSON）。
    pub fn to_jsonl(&self) -> Result<String> {
        let mut out = String::new();
        for (i, r) in self.records.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            let line = serde_json::to_string(r)
                .map_err(|e| AisyncError::Session(format!("serialize converted record: {e}")))?;
            out.push_str(&line);
        }
        out.push('\n');
        Ok(out)
    }
}

/// 会话格式转换器插件接口。
pub trait SessionConverter {
    /// 目标格式名。
    fn target_tool(&self) -> &str;
    /// 把（已完成路径重写的）Claude 会话转成目标格式。
    fn convert(&self, session: &ParsedSession) -> Result<ConvertedSession>;
}

/// 从 Claude 单条记录提取角色（"user"/"assistant"/...）。
fn claude_role(record: &Value) -> Option<&str> {
    // 优先 message.role，回退到顶层 type。
    record
        .get("message")
        .and_then(|m| m.get("role"))
        .and_then(Value::as_str)
        .or_else(|| record.get("type").and_then(Value::as_str))
}

/// 从 Claude message.content 提取纯文本块（仅 text / 字符串），按出现顺序。
/// 不提取 thinking / tool_use / tool_result——这些是 Claude 专有结构，
/// 强行塞进 Codex message 会臆造内容（X8），故跳过。
fn claude_text_blocks(record: &Value) -> Vec<String> {
    let content = match record.get("message").and_then(|m| m.get("content")) {
        Some(c) => c,
        None => return Vec::new(),
    };
    match content {
        Value::String(s) => vec![s.clone()],
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| {
                if b.get("type").and_then(Value::as_str) == Some("text") {
                    b.get("text").and_then(Value::as_str).map(str::to_string)
                } else {
                    None
                }
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Claude Code → Codex rollout 转换器。
#[derive(Debug, Default)]
pub struct ClaudeToCodexConverter;

impl ClaudeToCodexConverter {
    pub fn new() -> Self {
        Self
    }
}

impl SessionConverter for ClaudeToCodexConverter {
    fn target_tool(&self) -> &str {
        "codex"
    }

    fn convert(&self, session: &ParsedSession) -> Result<ConvertedSession> {
        let mut records = Vec::new();

        // 1) session_meta（首行）：携带 cwd（已被上游路径重写）与会话 id。
        records.push(json!({
            "type": "session_meta",
            "payload": {
                "id": session.session_id,
                "cwd": session.original_project_path,
                "originator": "aisync-import",
                "source": "import",
            }
        }));

        // 2) 逐条 user/assistant 消息 → response_item message。
        //    role 映射：user→user(input_text)，assistant→assistant(output_text)。
        //    其余角色/类型（system/tool 等）不映射，避免臆造 Codex 不认的结构（X8）。
        for record in session.records.iter() {
            let value = record.value();
            let role = match claude_role(value) {
                Some("user") => "user",
                Some("assistant") => "assistant",
                _ => continue,
            };
            let texts = claude_text_blocks(value);
            if texts.is_empty() {
                continue;
            }
            let block_type = if role == "assistant" {
                "output_text"
            } else {
                "input_text"
            };
            let content: Vec<Value> = texts
                .into_iter()
                .map(|t| json!({ "type": block_type, "text": t }))
                .collect();
            records.push(json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": role,
                    "content": content,
                }
            }));
        }

        Ok(ConvertedSession {
            tool_name: "codex".to_string(),
            file_name: format!("rollout-{}.jsonl", session.session_id),
            records,
        })
    }
}

/// Gemini CLI 解析器骨架（T12.3 预留）。格式待调研。
#[derive(Debug, Default)]
pub struct GeminiParser;

impl GeminiParser {
    pub fn new() -> Self {
        Self
    }

    pub fn tool_name(&self) -> &str {
        "gemini"
    }
}

/// Claude Code → Gemini CLI 转换器骨架（T12.3 预留）。
#[derive(Debug, Default)]
pub struct ClaudeToGeminiConverter;

impl ClaudeToGeminiConverter {
    pub fn new() -> Self {
        Self
    }
}

impl SessionConverter for ClaudeToGeminiConverter {
    fn target_tool(&self) -> &str {
        "gemini"
    }

    fn convert(&self, _session: &ParsedSession) -> Result<ConvertedSession> {
        Err(AisyncError::Session(
            "gemini converter not yet implemented (format pending research, T12.3)".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_code::ClaudeCodeParser;
    use crate::path_rewriter::{PathRule, RuleBasedRewriter};
    use aisync_core::RewriteDirection;
    use std::fs;
    use std::io::Write;
    use std::path::{Path, PathBuf};

    fn write_jsonl(dir: &Path, name: &str, lines: &[&str]) -> PathBuf {
        let f = dir.join(name);
        let mut file = fs::File::create(&f).unwrap();
        for l in lines {
            writeln!(file, "{l}").unwrap();
        }
        f
    }

    fn sample_session() -> ParsedSession {
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path().join("projects");
        let dir = projects.join("enc");
        fs::create_dir_all(&dir).unwrap();
        write_jsonl(
            &dir,
            "s.jsonl",
            &[
                r#"{"type":"user","cwd":"/Users/alice/x","message":{"role":"user","content":"实现一个功能"}}"#,
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"略"},{"type":"text","text":"好的，我来做"},{"type":"tool_use","name":"Read","input":{"file_path":"/Users/alice/x/a.rs"}}]}}"#,
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"ok"}]}}"#,
            ],
        );
        ClaudeCodeParser::parse_sessions(&projects)
            .unwrap()
            .pop()
            .unwrap()
    }

    #[test]
    fn converts_to_codex_structure() {
        let session = sample_session();
        let converted = ClaudeToCodexConverter::new().convert(&session).unwrap();
        assert_eq!(converted.tool_name, "codex");
        assert!(converted.file_name.starts_with("rollout-"));

        // 首行 session_meta，cwd 正确。
        let meta = &converted.records[0];
        assert_eq!(meta["type"], "session_meta");
        assert_eq!(meta["payload"]["cwd"], "/Users/alice/x");
        assert_eq!(meta["payload"]["id"], "s");

        // user 消息 → input_text；assistant text 块 → output_text。
        let msgs: Vec<&Value> = converted
            .records
            .iter()
            .filter(|r| r["type"] == "response_item")
            .collect();
        // user(实现一个功能) + assistant(好的，我来做)；
        // 第三条 user 只有 tool_result（无 text 块）→ 跳过。
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["payload"]["role"], "user");
        assert_eq!(msgs[0]["payload"]["content"][0]["type"], "input_text");
        assert_eq!(msgs[0]["payload"]["content"][0]["text"], "实现一个功能");
        assert_eq!(msgs[1]["payload"]["role"], "assistant");
        assert_eq!(msgs[1]["payload"]["content"][0]["type"], "output_text");
        assert_eq!(msgs[1]["payload"]["content"][0]["text"], "好的，我来做");
    }

    #[test]
    fn path_rewrite_before_conversion_reflected_in_codex_cwd() {
        let mut session = sample_session();
        let rewriter = RuleBasedRewriter::new(vec![PathRule::unix_to_windows(
            "/Users/alice/x",
            "C:\\Users\\bob\\x",
        )])
        .unwrap();
        // 先重写路径，再转换（T12.2 顺序）。
        ClaudeCodeParser::rewrite_structured_paths(
            &mut session,
            &rewriter,
            RewriteDirection::SourceToTarget,
        );
        let converted = ClaudeToCodexConverter::new().convert(&session).unwrap();
        assert_eq!(converted.records[0]["payload"]["cwd"], "C:\\Users\\bob\\x");
    }

    #[test]
    fn does_not_inject_or_alter_message_text() {
        // X8：消息文本原样搬运，不增删改。
        let session = sample_session();
        let converted = ClaudeToCodexConverter::new().convert(&session).unwrap();
        let all_text: Vec<String> = converted
            .records
            .iter()
            .filter(|r| r["type"] == "response_item")
            .flat_map(|r| {
                r["payload"]["content"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|c| c["text"].as_str().unwrap().to_string())
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(all_text, vec!["实现一个功能", "好的，我来做"]);
    }

    #[test]
    fn jsonl_serialization_round_trips() {
        let session = sample_session();
        let converted = ClaudeToCodexConverter::new().convert(&session).unwrap();
        let text = converted.to_jsonl().unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), converted.records.len());
        for (line, rec) in lines.iter().zip(&converted.records) {
            let parsed: Value = serde_json::from_str(line).unwrap();
            assert_eq!(&parsed, rec);
        }
    }

    #[test]
    fn gemini_skeleton_returns_not_implemented() {
        let session = sample_session();
        let err = ClaudeToGeminiConverter::new().convert(&session);
        assert!(err.is_err());
        assert_eq!(ClaudeToGeminiConverter::new().target_tool(), "gemini");
        assert_eq!(GeminiParser::new().tool_name(), "gemini");
    }
}
