//! `notify` feature 的纯函数：问题提取、正文裁剪、耗时格式化、outcome 解析。
//!
//! 全部无副作用，便于单测；事件处理器只负责读取 payload 并调用这里。

/// 通知正文上限（字符数）。桌面通知不是日志，超长会被桌面环境截断。
const MAX_BODY_CHARS: usize = 120;

/// 一次 run 的结束原因（来自 `turn_end` payload 的 `outcome`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Unknown,
    Stop,
    Aborted,
    Error,
}

impl Outcome {
    pub fn from_payload(text: &str) -> Self {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
            return Outcome::Unknown;
        };
        match value.get("outcome").and_then(|v| v.as_str()) {
            Some("stop") => Outcome::Stop,
            Some("aborted") => Outcome::Aborted,
            Some("error") => Outcome::Error,
            _ => Outcome::Unknown,
        }
    }
}

/// 把任意空白折叠成单个空格并去掉首尾空白。
fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 在**字符**边界截断，避免切坏多字节 UTF-8。
pub fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// 从 `ask_user` 的参数里取一条能直接展示的问题文本。
///
/// 兼容三种形态（宿主 schema）：
/// - `{"question": "..."}`（单问题）
/// - `{"questions": [{"question": "..."}]}`（批量问题取第一条）
/// - `{"type": "confirm", "summary": "..."}`（确认框）
pub fn question_from_params(params: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(params).ok()?;
    let candidates = [
        value.get("question").and_then(|v| v.as_str()),
        value
            .get("questions")
            .and_then(|v| v.as_array())
            .and_then(|items| items.first())
            .and_then(|item| item.get("question"))
            .and_then(|v| v.as_str()),
        value.get("summary").and_then(|v| v.as_str()),
    ];
    // 先收集到自有 `String`，避免末尾表达式借用 `value` 导致临时值提前释放。
    let text = candidates
        .into_iter()
        .flatten()
        .map(collapse_whitespace)
        .find(|text| !text.is_empty());
    text.map(|text| truncate(&text, MAX_BODY_CHARS))
}

/// 把毫秒格式化成通知里短小的耗时文本。
pub fn format_duration(ms: i64) -> String {
    let ms = ms.max(0);
    if ms < 60_000 {
        let tenths = (ms + 50) / 100;
        format!("{}.{}s", tenths / 10, tenths % 10)
    } else {
        let total_secs = ms / 1000;
        format!("{}m{:02}s", total_secs / 60, total_secs % 60)
    }
}

/// 当前工作目录名；取不到时退回 `rpi`。
pub fn project_name() -> String {
    std::env::current_dir()
        .ok()
        .and_then(|dir| {
            dir.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "rpi".to_string())
}

/// 完成通知正文：`项目 · 12.3s`，出错时追加 ` · 出错`。
pub fn done_body(project: &str, duration_ms: i64, outcome: Outcome) -> String {
    let mut body = format!("{project} · {}", format_duration(duration_ms));
    if outcome == Outcome::Error {
        body.push_str(" · 出错");
    }
    truncate(&body, MAX_BODY_CHARS)
}

/// 等待通知正文：优先展示问题原文，过长时截断。
pub fn waiting_body(question: Option<&str>) -> String {
    match question.map(collapse_whitespace) {
        Some(text) if !text.is_empty() => truncate(&text, MAX_BODY_CHARS),
        _ => "需要你回答或确认".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_maps_known_values_and_ignores_noise() {
        assert_eq!(
            Outcome::from_payload(r#"{"outcome":"stop"}"#),
            Outcome::Stop
        );
        assert_eq!(
            Outcome::from_payload(r#"{"outcome":"aborted"}"#),
            Outcome::Aborted
        );
        assert_eq!(
            Outcome::from_payload(r#"{"outcome":"error"}"#),
            Outcome::Error
        );
        assert_eq!(Outcome::from_payload("not json"), Outcome::Unknown);
        assert_eq!(
            Outcome::from_payload(r#"{"outcome":"weird"}"#),
            Outcome::Unknown
        );
    }

    #[test]
    fn question_prefers_single_then_batch_then_summary() {
        assert_eq!(
            question_from_params(r#"{"question":"用哪个包管理器？"}"#).as_deref(),
            Some("用哪个包管理器？")
        );
        assert_eq!(
            question_from_params(
                r#"{"questions":[{"question":"先做哪一步？"},{"question":"第二步？"}]}"#
            )
            .as_deref(),
            Some("先做哪一步？")
        );
        assert_eq!(
            question_from_params(r#"{"type":"confirm","summary":"要删掉分支吗？"}"#).as_deref(),
            Some("要删掉分支吗？")
        );
        assert_eq!(question_from_params(r#"{"question":"   "}"#), None);
        assert_eq!(question_from_params("nope"), None);
    }

    #[test]
    fn question_collapses_newlines_and_truncates() {
        let long = "a".repeat(200);
        let params = format!(r#"{{"question":"第一行\n\n第二行 {long}"}}"#);
        let text = question_from_params(&params).expect("question");
        assert!(text.starts_with("第一行 第二行 aaaa"));
        assert!(text.chars().count() <= MAX_BODY_CHARS);
        assert!(text.ends_with('…'));
    }

    #[test]
    fn duration_is_readable() {
        assert_eq!(format_duration(0), "0.0s");
        assert_eq!(format_duration(5_040), "5.0s");
        assert_eq!(format_duration(62_000), "1m02s");
        assert_eq!(format_duration(-5), "0.0s");
    }

    #[test]
    fn done_body_marks_errors() {
        assert_eq!(done_body("rpi", 5_000, Outcome::Stop), "rpi · 5.0s");
        assert_eq!(done_body("rpi", 5_000, Outcome::Error), "rpi · 5.0s · 出错");
    }

    #[test]
    fn waiting_body_falls_back_for_empty_questions() {
        assert_eq!(waiting_body(None), "需要你回答或确认");
        assert_eq!(waiting_body(Some("  \n ")), "需要你回答或确认");
        assert_eq!(waiting_body(Some("选哪个?")), "选哪个?");
    }
}
