//! aisync-session：会话解析（T4）+ 路径重写（T5）+ 跨工具格式转换（T12）。
//!
//! - [`claude_code::ClaudeCodeParser`]：解析 Claude Code 的 JSONL 会话，按原始路径映射。
//! - [`path_rewriter::RuleBasedRewriter`]：跨平台路径重写，可逆、显式、不篡改非路径内容。
//! - [`converter::ClaudeToCodexConverter`]：Claude → Codex 格式转换（路径重写在转换之前）。

use std::path::Path;

use codebaton_core::{AisyncError, PathRewriter, Result, RewriteDirection, Session, SessionParser};

pub mod claude_code;
pub mod converter;
pub mod path_rewriter;

pub use claude_code::{
    ClaudeCodeParser, EncodingConflict, ParsedSession, PathLocation, PathReference, SessionIndex,
};
pub use converter::{
    ClaudeToCodexConverter, ClaudeToGeminiConverter, ConvertedSession, GeminiParser,
    SessionConverter,
};
pub use path_rewriter::{
    Confidence, PathRule, RewriteRecord, RewriteReport, RuleBasedRewriter, SkipRecord,
};

/// 把 [`ParsedSession`] 装进 core 的通用 [`Session`] 信封（供传输层 / 协调器使用）。
/// data 中保存 records 数组与映射所需的元信息，不丢字段。
pub fn to_core_session(parsed: &ParsedSession) -> Session {
    Session {
        id: parsed.session_id.clone(),
        tool_name: "claude-code".to_string(),
        project_id: Some(parsed.original_project_path.clone()),
        data: serde_json::json!({
            "encoded_dir_name": parsed.encoded_dir_name,
            "original_project_path": parsed.original_project_path,
            "trailing_newline": parsed.trailing_newline(),
            "records": parsed.record_values(),
        }),
    }
}

/// 让 [`ClaudeCodeParser`] 满足 core 的 [`SessionParser`] trait，便于协调器以 trait object 使用。
impl SessionParser for ClaudeCodeParser {
    fn tool_name(&self) -> &str {
        "claude-code"
    }

    fn detect(&self, path: &Path) -> bool {
        ClaudeCodeParser::detect(path)
    }

    fn parse(&self, config_dir: &Path) -> Result<Vec<Session>> {
        let parsed = ClaudeCodeParser::parse_sessions(config_dir)?;
        Ok(parsed.iter().map(to_core_session).collect())
    }

    /// trait 版本：对通用 Session 的 records 做结构化路径重写（走精确字段重写）。
    fn rewrite_paths(&self, session: &mut Session, rewriter: &dyn PathRewriter) -> Result<()> {
        // core 的 PathRewriter trait 只暴露 rewrite(content, direction)（文本启发式）。
        // 这里对每条记录序列化后整体重写，再反序列化写回。结构化精确重写（confidence High）
        // 请直接用 ClaudeCodeParser::rewrite_structured_paths。
        let records = session
            .data
            .get_mut("records")
            .and_then(|v| v.as_array_mut())
            .ok_or_else(|| AisyncError::Session("session data missing records array".into()))?;
        for record in records.iter_mut() {
            if record.is_null() {
                continue;
            }
            let line = serde_json::to_string(record)
                .map_err(|e| AisyncError::Session(format!("serialize record: {e}")))?;
            let rewritten = rewriter.rewrite(&line, RewriteDirection::SourceToTarget)?;
            *record = serde_json::from_str(&rewritten)
                .map_err(|e| AisyncError::Session(format!("reparse record: {e}")))?;
        }
        Ok(())
    }

    fn write_session(&self, session: &Session, target_dir: &Path) -> Result<()> {
        let parsed = from_core_session(session)?;
        ClaudeCodeParser::write_session(&parsed, target_dir)?;
        Ok(())
    }
}

/// 从通用 [`Session`] 信封还原 [`ParsedSession`]。
fn from_core_session(session: &Session) -> Result<ParsedSession> {
    let encoded_dir_name = session
        .data
        .get("encoded_dir_name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AisyncError::Session("missing encoded_dir_name".into()))?
        .to_string();
    let original_project_path = session
        .data
        .get("original_project_path")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let trailing_newline = session
        .data
        .get("trailing_newline")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let records = session
        .data
        .get("records")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(ParsedSession::from_parts(
        session.id.clone(),
        original_project_path,
        encoded_dir_name,
        records,
        trailing_newline,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_session_round_trips_through_envelope() {
        let parsed = ParsedSession::from_parts(
            "sid".into(),
            "/Users/alice/x".into(),
            "enc".into(),
            vec![serde_json::json!({"type":"user","cwd":"/Users/alice/x"})],
            true,
        );
        let core = to_core_session(&parsed);
        let back = from_core_session(&core).unwrap();
        assert_eq!(back.session_id, "sid");
        assert_eq!(back.original_project_path, "/Users/alice/x");
        assert_eq!(back.records.len(), 1);
    }

    #[test]
    fn parser_trait_object_usable() {
        let parser: Box<dyn SessionParser> = Box::new(ClaudeCodeParser::new());
        assert_eq!(parser.tool_name(), "claude-code");
    }
}
