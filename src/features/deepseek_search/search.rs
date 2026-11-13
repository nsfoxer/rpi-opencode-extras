//! DeepSeek Search HTTP 调用、重试策略、结果解析与格式化。
//!
//! 凭据只在请求头里出现；错误文本里不含 key。

use crate::features::deepseek_search::api_key::resolve_api_key;
use crate::features::deepseek_search::params::SearchParams;
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// DeepSeek Search 端点。
const SEARCH_ENDPOINT: &str = "https://api.deepseek.com/v1/tools/search";

/// 单次 HTTP 请求的总超时（连接 + 读写各阶段都受它约束）。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// 最多尝试次数：首次 + 1 次重试（仅限 429 / 5xx）。
const MAX_ATTEMPTS: usize = 2;

/// 响应体读取上限，避免异常响应把插件内存打爆（正文通常远小于此）。
const MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;

/// 单条 summary 的展示字符上限，防止个别页面把整段正文塞进 summary。
const MAX_SUMMARY_CHARS: usize = 400;

/// 一条搜索结果（只保留展示需要的字段）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct SearchHit {
    /// 标题。
    title: String,
    /// 链接。
    url: String,
    /// 日期（可能带时间，展示时只取日期部分）。
    date: String,
    /// 权威度（API 可能给数字或字符串，统一转字符串）。
    authority: String,
    /// 摘要（缺失时回落到 snippet）。
    summary: String,
}

/// 执行完整搜索并返回格式化文本。
///
/// 参数：`params` 调用参数；`cancelled` 取消标记（每次重试前检查）。
/// 返回：`Ok(格式化后的多行文本)` 或 `Err(错误原因，含 HTTP 状态码 / API 错误文本)`。
pub fn search_web(params: &SearchParams, cancelled: &AtomicBool) -> Result<String, String> {
    let api_key = resolve_api_key()?;
    let hits = request_search(&api_key, params, cancelled)?;
    Ok(format_hits(&hits, params.count))
}

/// 发起搜索请求（429 / 5xx 最多重试 1 次）。
///
/// 参数：`api_key` 调用凭据（仅在请求头里使用）；`params` 调用参数；`cancelled` 取消标记。
/// 返回：`Ok(命中列表)` 或 `Err(错误原因)`。
fn request_search(
    api_key: &str,
    params: &SearchParams,
    cancelled: &AtomicBool,
) -> Result<Vec<SearchHit>, String> {
    // 每次调用单独建 agent：插件是长驻 DLL，不做跨调用连接池，避免持有失效连接。
    let agent = ureq::AgentBuilder::new().timeout(REQUEST_TIMEOUT).build();
    let body = serde_json::json!({
        "query": params.query,
        "count": params.count,
        "need_content": params.need_content,
    });
    let authorization = format!("Bearer {api_key}");

    let mut last_error = String::from("DeepSeek search failed: no attempt made");
    for attempt in 0..MAX_ATTEMPTS {
        // 宿主取消后不再发起新的网络请求。
        if cancelled.load(Ordering::Acquire) {
            return Err("cancelled".to_string());
        }

        let result = agent
            .post(SEARCH_ENDPOINT)
            .set("Authorization", &authorization)
            .set("Content-Type", "application/json")
            .send_json(&body);

        match result {
            Ok(response) => {
                // 状态码 200：解析 results[]。
                let text = read_body_capped(response)?;
                let value: serde_json::Value = serde_json::from_str(&text)
                    .map_err(|e| format!("DeepSeek search failed: invalid JSON response: {e}"))?;
                return parse_hits(&value);
            }
            Err(ureq::Error::Status(code, response)) => {
                let text = read_body_capped(response).unwrap_or_default();
                let detail = api_error_text(&text);
                last_error = if detail.is_empty() {
                    format!("DeepSeek search failed: HTTP {code}")
                } else {
                    format!("DeepSeek search failed: HTTP {code}: {detail}")
                };
                // 只对限流与服务端错误重试，且不超过 MAX_ATTEMPTS。
                if !is_retryable_status(code) || attempt + 1 >= MAX_ATTEMPTS {
                    return Err(last_error);
                }
            }
            Err(ureq::Error::Transport(err)) => {
                let message = err.to_string();
                last_error = format!("DeepSeek search failed: transport error: {message}");
                // 传输层错误（超时 / DNS / TLS）重试一次即可，避免长尾等待。
                if attempt + 1 >= MAX_ATTEMPTS {
                    return Err(last_error);
                }
            }
        }
    }
    Err(last_error)
}

/// 判断该 HTTP 状态码是否值得重试（429 限流与 5xx 服务端错误）。
fn is_retryable_status(code: u16) -> bool {
    code == 429 || (500..600).contains(&code)
}

/// 读取响应体，最多 `MAX_RESPONSE_BYTES` 字节；超限视为错误响应。
fn read_body_capped(response: ureq::Response) -> Result<String, String> {
    let mut buffer = Vec::new();
    response
        .into_reader()
        .take(MAX_RESPONSE_BYTES)
        .read_to_end(&mut buffer)
        .map_err(|e| format!("DeepSeek search failed: cannot read response body: {e}"))?;
    Ok(String::from_utf8_lossy(&buffer).into_owned())
}

/// 从 API 错误响应体里提取人类可读的错误文本。
///
/// 兼容 `{"error":{"message":...}}` 与 `{"error":"..."}`；都取不到时截断原始响应，
/// 便于定位问题（响应体里不含凭据）。
fn api_error_text(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if let Some(message) = value
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
        {
            return message.trim().to_string();
        }
        if let Some(message) = value.get("error").and_then(|e| e.as_str()) {
            return message.trim().to_string();
        }
        if let Some(message) = value.get("message").and_then(|m| m.as_str()) {
            return message.trim().to_string();
        }
    }
    truncate_chars(trimmed, 300)
}

/// 解析 `results[]` 为结构化命中列表。
///
/// 参数：`value` 已解析的响应 JSON。
/// 返回：`Ok(命中列表，可能为空)`；响应缺少 `results` 数组时 `Err(原因)`。
fn parse_hits(value: &serde_json::Value) -> Result<Vec<SearchHit>, String> {
    let results = value
        .get("results")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "DeepSeek search failed: response has no results array".to_string())?;

    Ok(results.iter().map(hit_from_value).collect())
}

/// 把单条 result 转成 [`SearchHit`]；缺字段用空串占位，authority 统一转字符串。
fn hit_from_value(item: &serde_json::Value) -> SearchHit {
    let text = |key: &str| -> String {
        item.get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string()
    };
    // summary 缺失时退回 snippet（API 两者都会给，缺一个也不至于整行空着）。
    let mut summary = text("summary");
    if summary.is_empty() {
        summary = text("snippet");
    }
    let authority = match item.get("authority") {
        Some(serde_json::Value::Number(n)) => n.to_string(),
        Some(serde_json::Value::String(s)) => s.trim().to_string(),
        _ => String::new(),
    };
    SearchHit {
        title: text("title"),
        url: text("url"),
        date: text("date"),
        authority,
        summary,
    }
}

/// 把命中列表格式化成「每条一行」的文本。
///
/// 每行形如 `标题 / URL / 日期 / authority / summary`，缺失字段用 `-` 占位；
/// 日期只保留 `YYYY-MM-DD` 部分（API 返回带时间的 ISO 串）。
/// 参数：`hits` 命中列表；`count` 最多输出的条数。
/// 返回：格式化文本；`hits` 为空时返回一句明确的无结果提示。
fn format_hits(hits: &[SearchHit], count: usize) -> String {
    if hits.is_empty() {
        return "no results returned by DeepSeek Search".to_string();
    }
    let mut lines = Vec::with_capacity(hits.len().min(count) + 1);
    lines.push(format!(
        "DeepSeek Search results ({}):",
        hits.len().min(count)
    ));
    for (index, hit) in hits.iter().take(count).enumerate() {
        lines.push(format!(
            "{}. {} / {} / {} / {} / {}",
            index + 1,
            field_or_dash(&hit.title),
            field_or_dash(&hit.url),
            date_or_dash(&hit.date),
            field_or_dash(&hit.authority),
            field_or_dash(&truncate_chars(&hit.summary, MAX_SUMMARY_CHARS)),
        ));
    }
    lines.join("\n")
}

/// 空字段用 `-` 占位，并把换行压平以保证「一条一行」。
fn field_or_dash(value: &str) -> String {
    let flattened = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if flattened.is_empty() {
        "-".to_string()
    } else {
        flattened
    }
}

/// 日期字段：取 `T` 之前的日期部分；为空时用 `-`。
fn date_or_dash(value: &str) -> String {
    let flattened = field_or_dash(value);
    match flattened.split_once('T') {
        Some((date, _)) if !date.is_empty() => date.to_string(),
        _ => flattened,
    }
}

/// 按字符（而非字节）截断，避免把 UTF-8 切坏；超长时补 `…`。
fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let mut out: String = value.chars().take(max_chars).collect();
    out.push('…');
    out
}

// ===========================================================================
// 测试
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hits_are_parsed_and_formatted_one_line_each() {
        let body = serde_json::json!({
            "results": [
                {
                    "title": "Spring Boot 3.5 Release Notes",
                    "url": "https://github.com/spring-projects/spring-boot/wiki",
                    "date": "2026-03-12T16:00:00+00:00",
                    "authority": 3,
                    "summary": "line one\nline two",
                    "snippet": "ignored",
                    "content": ""
                },
                { "title": "No summary", "url": "https://example.com", "authority": "4", "snippet": "from snippet" }
            ]
        });
        let hits = parse_hits(&body).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].authority, "3");
        assert_eq!(hits[1].authority, "4");
        assert_eq!(hits[1].summary, "from snippet");

        let text = format_hits(&hits, 5);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "header + one line per hit");
        assert!(lines[1].contains("Spring Boot 3.5 Release Notes"));
        assert!(lines[1].contains("2026-03-12 /"));
        assert!(lines[1].contains("/ 3 /"));
        assert!(!lines[1].contains('\n'));
        // count 截断生效。
        assert_eq!(format_hits(&hits, 1).lines().count(), 2);
    }

    #[test]
    fn missing_results_array_is_an_error() {
        assert!(parse_hits(&serde_json::json!({ "data": [] })).is_err());
        let hits = parse_hits(&serde_json::json!({ "results": [] })).unwrap();
        assert!(format_hits(&hits, 5).contains("no results"));
    }

    #[test]
    fn api_error_text_extracts_message() {
        let body = r#"{"error":{"message":"Authentication Fails","type":"authentication_error"}}"#;
        assert_eq!(api_error_text(body), "Authentication Fails");
        assert_eq!(api_error_text(r#"{"error":"plain"}"#), "plain");
        assert_eq!(api_error_text(""), "");
        assert_eq!(api_error_text("<html>boom</html>"), "<html>boom</html>");
    }

    #[test]
    fn retry_policy_covers_only_rate_limit_and_server_errors() {
        assert!(is_retryable_status(429));
        assert!(is_retryable_status(500));
        assert!(is_retryable_status(503));
        assert!(!is_retryable_status(400));
        assert!(!is_retryable_status(401));
        assert!(!is_retryable_status(404));
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        let long = "中文".repeat(300);
        let truncated = truncate_chars(&long, 10);
        assert_eq!(truncated.chars().count(), 11);
        assert!(truncated.ends_with('…'));
        assert_eq!(truncate_chars("short", 10), "short");
    }

    #[test]
    fn empty_fields_fall_back_to_dash() {
        assert_eq!(field_or_dash(""), "-");
        assert_eq!(field_or_dash("a\nb"), "a b");
        assert_eq!(date_or_dash(""), "-");
        assert_eq!(date_or_dash("2026-01-02T03:04:05Z"), "2026-01-02");
    }
}
