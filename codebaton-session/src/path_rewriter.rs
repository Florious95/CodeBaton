//! T5 路径重写引擎。
//!
//! 跨平台路径映射与重写。核心保证（见 guidelines G3/X8）：
//! - 可逆：A→B 重写后再 B→A 重写，应还原为原始内容。
//! - 显式：只替换命中 source_prefix 的路径，不确定就保持原样。
//! - 不篡改：替换范围严格限定在路径前缀，不动其余字符。

use std::cmp::Reverse;

use codebaton_core::{AisyncError, PathRewriter, Result, RewriteDirection};

/// 单条映射规则。源前缀 + 目标前缀 + 各自的分隔符。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathRule {
    pub source_prefix: String,
    pub target_prefix: String,
    pub source_separator: char,
    pub target_separator: char,
}

impl PathRule {
    /// 显式构造。前缀末尾的分隔符会被去掉，便于统一边界判断。
    pub fn new(
        source_prefix: impl Into<String>,
        target_prefix: impl Into<String>,
        source_separator: char,
        target_separator: char,
    ) -> Self {
        let source_prefix = trim_trailing_sep(source_prefix.into(), source_separator);
        let target_prefix = trim_trailing_sep(target_prefix.into(), target_separator);
        Self {
            source_prefix,
            target_prefix,
            source_separator,
            target_separator,
        }
    }

    /// Unix → Windows（`/` → `\`）。
    pub fn unix_to_windows(
        source_prefix: impl Into<String>,
        target_prefix: impl Into<String>,
    ) -> Self {
        Self::new(source_prefix, target_prefix, '/', '\\')
    }

    /// Unix → Unix（macOS ↔ Linux/WSL，`/` → `/`）。
    pub fn unix_to_unix(
        source_prefix: impl Into<String>,
        target_prefix: impl Into<String>,
    ) -> Self {
        Self::new(source_prefix, target_prefix, '/', '/')
    }

    fn reversed(&self) -> Self {
        Self {
            source_prefix: self.target_prefix.clone(),
            target_prefix: self.source_prefix.clone(),
            source_separator: self.target_separator,
            target_separator: self.source_separator,
        }
    }
}

fn trim_trailing_sep(mut prefix: String, sep: char) -> String {
    while prefix.ends_with(sep) && prefix.chars().count() > 1 {
        prefix.pop();
    }
    prefix
}

/// 置信度。结构化字段为 High；文本启发式命中为 Medium；疑似但未命中规则为 Low（不替换）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    High,
    Medium,
    Low,
}

/// 一条已执行的替换记录（用于同步日志 / 可逆性追踪，见 G3/G7）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewriteRecord {
    pub before: String,
    pub after: String,
    pub confidence: Confidence,
}

/// 一条被跳过的记录（疑似路径但未命中任何规则）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkipRecord {
    pub value: String,
    pub reason: String,
}

/// 重写报告。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RewriteReport {
    pub applied: Vec<RewriteRecord>,
    pub skipped: Vec<SkipRecord>,
}

impl RewriteReport {
    /// 合并另一份报告（供调用方累积多次重写的结果）。
    pub fn merge(&mut self, other: RewriteReport) {
        self.applied.extend(other.applied);
        self.skipped.extend(other.skipped);
    }
}

/// 基于规则的路径重写器。
#[derive(Debug, Clone)]
pub struct RuleBasedRewriter {
    /// 规则按 source_prefix 长度降序排列，保证最长前缀优先匹配（T5.1）。
    rules: Vec<PathRule>,
}

impl RuleBasedRewriter {
    /// 构造并校验规则。空规则集合法（等价于 noop）。
    pub fn new(rules: Vec<PathRule>) -> Result<Self> {
        validate_rules(&rules)?;
        let mut rules = rules;
        // 最长 source_prefix 优先；长度相同则保持稳定顺序。
        rules.sort_by_key(|r| Reverse(r.source_prefix.chars().count()));
        Ok(Self { rules })
    }

    pub fn rules(&self) -> &[PathRule] {
        &self.rules
    }

    /// 生成反向重写器（target→source），用于可逆性（G3）。
    pub fn reverse(&self) -> Self {
        let rules = self.rules.iter().map(PathRule::reversed).collect();
        // 反向后 target_prefix 成为新的 source_prefix，需重新排序。
        Self::new(rules).expect("reversing validated rules cannot produce invalid rules")
    }

    /// 取按方向应用的规则视图：SourceToTarget 用原始规则，TargetToSource 用反向规则。
    fn directional_rules(&self, direction: RewriteDirection) -> Vec<PathRule> {
        match direction {
            RewriteDirection::SourceToTarget => self.rules.clone(),
            RewriteDirection::TargetToSource => {
                let mut rules: Vec<PathRule> = self.rules.iter().map(PathRule::reversed).collect();
                rules.sort_by_key(|r| Reverse(r.source_prefix.chars().count()));
                rules
            }
        }
    }

    /// 对单个路径字符串做精确前缀替换。命中返回重写后字符串，否则返回 None。
    fn rewrite_one(&self, value: &str, rules: &[PathRule]) -> Option<String> {
        for rule in rules {
            if let Some(rest) = match_prefix(value, &rule.source_prefix, rule.source_separator) {
                let mut out = String::with_capacity(rule.target_prefix.len() + rest.len());
                out.push_str(&rule.target_prefix);
                // rest 以源分隔符表达，需要逐字符转换分隔符。
                for ch in rest.chars() {
                    if ch == rule.source_separator {
                        out.push(rule.target_separator);
                    } else {
                        out.push(ch);
                    }
                }
                return Some(out);
            }
        }
        None
    }

    /// T5.2 结构化字段重写：对一批已知是路径的值做精确替换（confidence=High）。
    /// 输入 values 原地替换为重写结果，返回报告。
    pub fn rewrite_structured(
        &self,
        values: &mut [String],
        direction: RewriteDirection,
    ) -> RewriteReport {
        let rules = self.directional_rules(direction);
        let mut report = RewriteReport::default();
        for value in values.iter_mut() {
            if let Some(rewritten) = self.rewrite_one(value, &rules) {
                report.applied.push(RewriteRecord {
                    before: value.clone(),
                    after: rewritten.clone(),
                    confidence: Confidence::High,
                });
                *value = rewritten;
            }
            // 结构化字段未命中规则属正常（例如 /tmp 下的路径），不记 skip。
        }
        report
    }

    /// 对单个结构化路径值重写，命中返回 Some(新值)，否则 None。供解析器逐字段调用。
    pub fn rewrite_structured_value(
        &self,
        value: &str,
        direction: RewriteDirection,
    ) -> Option<String> {
        let rules = self.directional_rules(direction);
        self.rewrite_one(value, &rules)
    }

    /// T5.3 文本内容启发式重写。在自由文本中识别疑似路径，仅替换命中规则的部分。
    pub fn rewrite_text(&self, text: &str, direction: RewriteDirection) -> (String, RewriteReport) {
        let rules = self.directional_rules(direction);
        let mut report = RewriteReport::default();
        let candidates = scan_path_candidates(text);
        if candidates.is_empty() {
            return (text.to_string(), report);
        }

        let mut out = String::with_capacity(text.len());
        let mut cursor = 0usize;
        for cand in candidates {
            if cand.start < cursor {
                // 与已处理区间重叠（不应发生），跳过。
                continue;
            }
            // 复制候选之前的原样文本。
            out.push_str(&text[cursor..cand.start]);
            let slice = &text[cand.start..cand.end];
            if let Some(rewritten) = self.rewrite_one(slice, &rules) {
                report.applied.push(RewriteRecord {
                    before: slice.to_string(),
                    after: rewritten.clone(),
                    confidence: Confidence::Medium,
                });
                out.push_str(&rewritten);
            } else {
                // 疑似路径但未命中规则：保持原样，记录为 skipped（confidence Low）。
                report.skipped.push(SkipRecord {
                    value: slice.to_string(),
                    reason: "no matching rule".to_string(),
                });
                out.push_str(slice);
            }
            cursor = cand.end;
        }
        out.push_str(&text[cursor..]);
        (out, report)
    }
}

impl PathRewriter for RuleBasedRewriter {
    /// core trait 入口：整段内容按文本启发式重写。
    /// 报告通过 rewrite_text 暴露；trait 版本只返回重写后内容。
    fn rewrite(&self, content: &str, direction: RewriteDirection) -> Result<String> {
        Ok(self.rewrite_text(content, direction).0)
    }
}

/// 判断 value 是否以 prefix 开头，且边界是分隔符或字符串结束（避免 `/a/bc` 命中 `/a/b`）。
/// 命中返回剩余部分（含起始分隔符），如 value=`/a/b/c` prefix=`/a/b` → `/c`；
/// value 恰等于 prefix → 返回 ""。
fn match_prefix<'a>(value: &'a str, prefix: &str, sep: char) -> Option<&'a str> {
    if prefix.is_empty() {
        return None;
    }
    let rest = value.strip_prefix(prefix)?;
    if rest.is_empty() {
        return Some(rest);
    }
    if rest.starts_with(sep) {
        Some(rest)
    } else {
        None
    }
}

/// 校验规则集：检测重复源前缀、循环映射（source==target）。
fn validate_rules(rules: &[PathRule]) -> Result<()> {
    for (i, a) in rules.iter().enumerate() {
        if a.source_prefix.is_empty() || a.target_prefix.is_empty() {
            return Err(AisyncError::PathRewrite(
                "path rule prefixes must not be empty".to_string(),
            ));
        }
        if a.source_prefix == a.target_prefix && a.source_separator == a.target_separator {
            return Err(AisyncError::PathRewrite(format!(
                "circular mapping: source equals target ({})",
                a.source_prefix
            )));
        }
        for b in rules.iter().skip(i + 1) {
            if a.source_prefix == b.source_prefix {
                return Err(AisyncError::PathRewrite(format!(
                    "duplicate source prefix: {}",
                    a.source_prefix
                )));
            }
        }
    }
    Ok(())
}

/// 一个文本中疑似路径候选的字节区间 [start, end)。
#[derive(Debug, Clone, Copy)]
struct Candidate {
    start: usize,
    end: usize,
}

/// 在自由文本里扫描疑似绝对路径片段。识别以下起点：
///   Unix: `/Users/` `/home/` `/mnt/` `/tmp/` 以及一般 `/<seg>/`（保守起见限定已知根）
///   Windows: 盘符 `C:\` `D:\` ... 与 `\\?\`
/// 终点：遇到空白、引号、或路径中不合理的字符即停止。
/// 注意：本函数只负责切出候选，是否替换由规则匹配决定（未命中即原样保留）。
fn scan_path_candidates(text: &str) -> Vec<Candidate> {
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut candidates = Vec::new();
    let mut i = 0usize;
    while i < len {
        let b = bytes[i];
        // Windows 盘符路径： X:\ 或 X:/
        if b.is_ascii_alphabetic()
            && i + 2 < len
            && bytes[i + 1] == b':'
            && (bytes[i + 2] == b'\\' || bytes[i + 2] == b'/')
            && is_token_boundary_before(bytes, i)
        {
            let end = scan_to_path_end(bytes, i);
            candidates.push(Candidate { start: i, end });
            i = end;
            continue;
        }
        // UNC / 扩展长度前缀 \\?\ 或 \\
        if b == b'\\' && i + 1 < len && bytes[i + 1] == b'\\' && is_token_boundary_before(bytes, i)
        {
            let end = scan_to_path_end(bytes, i);
            candidates.push(Candidate { start: i, end });
            i = end;
            continue;
        }
        // Unix 绝对路径：以 / 开头且前一字符是 token 边界。
        if b == b'/' && is_token_boundary_before(bytes, i) && i + 1 < len && bytes[i + 1] != b'/' {
            let end = scan_to_path_end(bytes, i);
            // 至少要有一段（`/x`），否则不算。
            if end > i + 1 {
                candidates.push(Candidate { start: i, end });
                i = end;
                continue;
            }
        }
        i += 1;
    }
    candidates
}

/// 候选起点前的字符必须是“非路径上下文”——空白、引号、括号、冒号后空格等，
/// 避免把 `https://host/path` 的 `/path` 之类误切（紧贴在字母/数字后的 `/` 不作为新路径起点）。
fn is_token_boundary_before(bytes: &[u8], i: usize) -> bool {
    if i == 0 {
        return true;
    }
    let prev = bytes[i - 1];
    matches!(
        prev,
        b' ' | b'\t'
            | b'\n'
            | b'\r'
            | b'"'
            | b'\''
            | b'`'
            | b'('
            | b'['
            | b'{'
            | b'<'
            | b'='
            | b','
            | b';'
    )
}

/// 从路径起点扫描到结束（遇到明显的终止符停止）。保守地在空白/引号/控制字符处截断。
fn scan_to_path_end(bytes: &[u8], start: usize) -> usize {
    let len = bytes.len();
    let mut i = start;
    while i < len {
        let b = bytes[i];
        // ASCII 终止符。非 ASCII 字节（如中文 UTF-8）一律视为路径内容继续。
        if b < 0x80
            && matches!(
                b,
                b' ' | b'\t' | b'\n' | b'\r' | b'"' | b'\'' | b'`' | b'<' | b'>' | b'|' | 0
            )
        {
            break;
        }
        i += 1;
    }
    // 去掉结尾可能粘连的标点（句号、逗号、右括号等），避免把 "见 /a/b。" 的句号吃进去。
    while i > start {
        let last = bytes[i - 1];
        if matches!(last, b'.' | b',' | b')' | b']' | b'}' | b':' | b';') {
            i -= 1;
        } else {
            break;
        }
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mac_to_win() -> RuleBasedRewriter {
        RuleBasedRewriter::new(vec![PathRule::unix_to_windows(
            "/Users/alice",
            "C:\\Users\\bob",
        )])
        .unwrap()
    }

    #[test]
    fn structured_mac_to_windows() {
        let r = mac_to_win();
        let out = r
            .rewrite_structured_value("/Users/alice/projects/x", RewriteDirection::SourceToTarget)
            .unwrap();
        assert_eq!(out, "C:\\Users\\bob\\projects\\x");
    }

    #[test]
    fn structured_mac_to_wsl() {
        let r = RuleBasedRewriter::new(vec![PathRule::unix_to_unix("/Users/alice", "/home/bob")])
            .unwrap();
        let out = r
            .rewrite_structured_value("/Users/alice/x", RewriteDirection::SourceToTarget)
            .unwrap();
        assert_eq!(out, "/home/bob/x");
    }

    #[test]
    fn longest_prefix_wins() {
        let r = RuleBasedRewriter::new(vec![
            PathRule::unix_to_unix("/Users/alice", "/home/bob"),
            PathRule::unix_to_unix("/Users/alice/projects/myapp", "/srv/myapp"),
        ])
        .unwrap();
        let out = r
            .rewrite_structured_value(
                "/Users/alice/projects/myapp/src/main.rs",
                RewriteDirection::SourceToTarget,
            )
            .unwrap();
        assert_eq!(out, "/srv/myapp/src/main.rs");
    }

    #[test]
    fn boundary_not_partial_segment() {
        let r =
            RuleBasedRewriter::new(vec![PathRule::unix_to_unix("/Users/al", "/home/x")]).unwrap();
        // /Users/alice 不应被 /Users/al 命中。
        assert!(r
            .rewrite_structured_value("/Users/alice/x", RewriteDirection::SourceToTarget)
            .is_none());
        // 恰好等于前缀应命中。
        assert_eq!(
            r.rewrite_structured_value("/Users/al", RewriteDirection::SourceToTarget),
            Some("/home/x".to_string())
        );
    }

    #[test]
    fn reversible_round_trip_unix() {
        let r = RuleBasedRewriter::new(vec![PathRule::unix_to_unix("/Users/alice", "/home/bob")])
            .unwrap();
        let original = "/Users/alice/projects/x/y.rs";
        let fwd = r
            .rewrite_structured_value(original, RewriteDirection::SourceToTarget)
            .unwrap();
        let back = r
            .rewrite_structured_value(&fwd, RewriteDirection::TargetToSource)
            .unwrap();
        assert_eq!(back, original);
    }

    #[test]
    fn reversible_round_trip_windows() {
        let r = mac_to_win();
        let original = "/Users/alice/a/b/c.txt";
        let fwd = r
            .rewrite_structured_value(original, RewriteDirection::SourceToTarget)
            .unwrap();
        assert_eq!(fwd, "C:\\Users\\bob\\a\\b\\c.txt");
        let back = r
            .rewrite_structured_value(&fwd, RewriteDirection::TargetToSource)
            .unwrap();
        assert_eq!(back, original);
    }

    #[test]
    fn reverse_rewriter_matches_direction() {
        let r = mac_to_win();
        let rev = r.reverse();
        let back = rev
            .rewrite_structured_value("C:\\Users\\bob\\x", RewriteDirection::SourceToTarget)
            .unwrap();
        assert_eq!(back, "/Users/alice/x");
    }

    #[test]
    fn text_rewrite_hits_only_matching_prefix() {
        let r = mac_to_win();
        let text = "see /Users/alice/x and /tmp/other for details";
        let (out, report) = r.rewrite_text(text, RewriteDirection::SourceToTarget);
        assert_eq!(out, "see C:\\Users\\bob\\x and /tmp/other for details");
        assert_eq!(report.applied.len(), 1);
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].value, "/tmp/other");
    }

    #[test]
    fn text_rewrite_does_not_touch_url() {
        let r = mac_to_win();
        // http://host/Users/alice/x：/Users 紧贴在 host 字母后？这里前面是 '/'，
        // 关键是 URL scheme 内的路径前缀通常贴着 host，不是 token 边界。
        let text = "url=https://example.com/Users/alice/x done";
        let (out, _) = r.rewrite_text(text, RewriteDirection::SourceToTarget);
        // /Users/alice/x 前一字符是 'm'（com），不是边界 → 不切候选 → 不改。
        assert_eq!(out, text);
    }

    #[test]
    fn text_rewrite_chinese_path() {
        let r = RuleBasedRewriter::new(vec![PathRule::unix_to_unix(
            "/Users/alauda/Documents/code/金融最前沿策略",
            "/home/bob/code/金融最前沿策略",
        )])
        .unwrap();
        let text = "写入 /Users/alauda/Documents/code/金融最前沿策略/前沿量化策略.html 完成";
        let (out, report) = r.rewrite_text(text, RewriteDirection::SourceToTarget);
        assert_eq!(
            out,
            "写入 /home/bob/code/金融最前沿策略/前沿量化策略.html 完成"
        );
        assert_eq!(report.applied.len(), 1);
    }

    #[test]
    fn text_round_trip_preserves_non_path_content() {
        let r = mac_to_win();
        let text = "foo /Users/alice/p bar 123 /tmp/z";
        let (fwd, _) = r.rewrite_text(text, RewriteDirection::SourceToTarget);
        let (back, _) = r.rewrite_text(&fwd, RewriteDirection::TargetToSource);
        assert_eq!(back, text);
    }

    #[test]
    fn rejects_circular_rule() {
        let err = RuleBasedRewriter::new(vec![PathRule::unix_to_unix("/a", "/a")]);
        assert!(err.is_err());
    }

    #[test]
    fn rejects_duplicate_source() {
        let err = RuleBasedRewriter::new(vec![
            PathRule::unix_to_unix("/a", "/b"),
            PathRule::unix_to_unix("/a", "/c"),
        ]);
        assert!(err.is_err());
    }

    #[test]
    fn empty_rules_is_noop() {
        let r = RuleBasedRewriter::new(vec![]).unwrap();
        let (out, report) = r.rewrite_text("/Users/alice/x", RewriteDirection::SourceToTarget);
        assert_eq!(out, "/Users/alice/x");
        // 疑似路径但无规则 → skipped 记录。
        assert_eq!(report.skipped.len(), 1);
    }
}
