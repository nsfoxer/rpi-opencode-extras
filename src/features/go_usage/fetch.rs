//! `/usage` 端点的 HTTP 拉取。
//!
//! 凭据只在请求头里出现；所有错误文本只含状态码与 API 的错误类型，**不含 key**。
//! 该函数在**工作线程**里被调用，绝不跑在事件回调线程上（后者同步调宿主会崩溃）。

use crate::features::go_usage::api_key::USAGE_ENDPOINTS;
use crate::features::go_usage::usage::{parse_usage, UsageSnapshot};
use std::io::Read;
use std::time::Duration;

/// 单次请求总超时。页脚是"顺带展示"，不能拖住任何东西。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// 响应体读取上限，防止异常响应打爆内存（真实响应 < 1KB）。
const MAX_RESPONSE_BYTES: u64 = 64 * 1024;

/// 拉取当前套餐用量。
///
/// 参数：`api_key` opencode.ai 凭据（只进请求头）。
/// 返回：`Ok(快照)`；全部端点都失败时 `Err(原因)`。
///
/// 依次尝试 [`USAGE_ENDPOINTS`]，第一个返回 200 且能解析的就胜出。
/// 404/405 说明该路径在这台服务端不存在，继续试下一个；401 是凭据问题，
/// 换端点也不会好，直接返回（避免把一次失败放大成两次请求）。
pub fn fetch_usage(api_key: &str) -> Result<UsageSnapshot, String> {
    let authorization = format!("Bearer {api_key}");
    let agent = ureq::AgentBuilder::new().timeout(REQUEST_TIMEOUT).build();

    let mut last_error = String::from("opencode usage failed: no endpoint attempted");
    for endpoint in USAGE_ENDPOINTS {
        match agent.get(endpoint).set("Authorization", &authorization).call() {
            Ok(response) => {
                // 200：解析正文；解析失败视为该端点异常，继续下一个。
                let body = read_body_capped(response)?;
                return parse_usage(&body);
            }
            Err(ureq::Error::Status(code, response)) => {
                let detail = error_type(&read_body_capped(response).unwrap_or_default());
                last_error = if detail.is_empty() {
                    format!("opencode usage failed: HTTP {code}")
                } else {
                    format!("opencode usage failed: HTTP {code} ({detail})")
                };
                // 凭据被拒：换端点没有意义。
                if code == 401 || code == 403 {
                    return Err(last_error);
                }
                // 其余（404/405/5xx）继续尝试下一个端点。
            }
            Err(ureq::Error::Transport(error)) => {
                last_error = format!("opencode usage failed: transport error: {error}");
            }
        }
    }
    Err(last_error)
}

/// 读取响应正文，超过上限即截断。
///
/// 参数：`response` ureq 响应。
/// 返回：`Ok(正文)` 或 `Err(读取失败原因)`。
fn read_body_capped(response: ureq::Response) -> Result<String, String> {
    let mut buffer = Vec::new();
    response
        .into_reader()
        .take(MAX_RESPONSE_BYTES)
        .read_to_end(&mut buffer)
        .map_err(|error| format!("cannot read response body: {error}"))?;
    Ok(String::from_utf8_lossy(&buffer).into_owned())
}

/// 从错误响应正文里提取错误类型（如 `AuthError`），不含 message 以外的任何内容。
///
/// 参数：`body` 响应正文（期望形如 `{"type":"error","error":{"type":"AuthError",...}}`）。
/// 返回：错误类型字符串；解析不出来时为空串。
fn error_type(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| error.get("type"))
                .and_then(|kind| kind.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::error_type;

    #[test]
    fn extracts_the_error_type_from_an_auth_failure() {
        // 真实 401 响应体（本机 curl 取得）。
        let body = r#"{"type":"error","error":{"type":"AuthError","message":"Unauthorized"}}"#;
        assert_eq!(error_type(body), "AuthError");
    }

    #[test]
    fn unknown_shapes_yield_an_empty_type() {
        assert_eq!(error_type(""), "");
        assert_eq!(error_type("not json"), "");
        assert_eq!(error_type(r#"{"error":{}}"#), "");
        assert_eq!(error_type(r#"{"type":"error"}"#), "");
    }
}
