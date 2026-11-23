//! opencode.ai 专属请求头的纯函数实现。
//!
//! 这一层不做任何 IO：输入 URL / 已有 header / 会话 id，输出最终 header 表，
//! 便于单元测试直接覆盖判定边界（尤其是域名后缀匹配）。

use std::collections::BTreeMap;

/// 注入的 User-Agent，标识请求来自本插件而非裸 rpi。
pub const USER_AGENT: &str = "rpi-opencode-extras/0.1";

/// opencode.ai 会话头的 header 名（小写）。
pub const SESSION_HEADER: &str = "x-opencode-session";

/// opencode 会话 id 的必需前缀。
pub const SESSION_PREFIX: &str = "ses_";

/// 判断 baseUrl 是否指向 opencode.ai。
///
/// 参数：`base_url` 为 provider 的 `baseUrl` 原始字符串。
/// 返回：host 恰好是 `opencode.ai`，或以 `.opencode.ai` 结尾时为 `true`；
/// URL 解析失败时返回 `false`（宁可不注入，也不误伤其他域名）。
///
/// 不用字符串 `contains`，避免 `notopencode.ai` 这类域名被误判。
pub fn is_opencode_ai_url(base_url: &str) -> bool {
    // 解析失败直接判否：调用方据此保留原始行为。
    let Ok(parsed) = url::Url::parse(base_url) else {
        return false;
    };
    // `host_str` 已由 url crate 归一化为小写。
    let Some(host) = parsed.host_str() else {
        return false;
    };
    host == "opencode.ai" || host.ends_with(".opencode.ai")
}

/// 构造最终要发送的 header 表。
///
/// 参数：
/// - `existing`：宿主/配置已有的 header（来自 `SimpleStreamOptions.headers`）。
/// - `session_id`：宿主真实会话 id；为空时用 `rpi-<pid>` 兜底，保证该头非空。
/// - `_model_id`：模型 id，当前不参与 header，保留以固定调用契约（后续按模型分流用）。
///
/// 返回：合并后的 header 表。保留 `existing` 中其他 header，header 名统一小写
/// （同名不同大小写视为同一个 key，后写入者覆盖），并写入
/// `x-opencode-session` 与 `user-agent`。
pub fn build_headers(
    existing: Option<BTreeMap<String, String>>,
    session_id: Option<&str>,
    _model_id: &str,
) -> BTreeMap<String, String> {
    // 第一步：把已有 header 全部小写化后收进结果表，实现大小写不敏感合并。
    let mut headers = BTreeMap::new();
    for (name, value) in existing.unwrap_or_default() {
        headers.insert(name.to_ascii_lowercase(), value);
    }

    // 第二步：归一化 session 值（空则兜底、缺失前缀则补前缀）。
    headers.insert(SESSION_HEADER.to_string(), normalize_session_id(session_id));

    // 第三步：覆盖 User-Agent，声明请求来自本插件。
    headers.insert("user-agent".to_string(), USER_AGENT.to_string());

    headers
}

/// 归一化会话 id。
///
/// 参数：`session_id` 为宿主传入的会话 id，可能为 `None` 或空白。
/// 返回：以 `ses_` 开头的非空字符串；宿主没有会话时返回 `rpi-<pid>`。
///
/// **一致性保证（重要）**：
/// - 会话 id **有值**时，本函数是纯函数映射（`<id>` → `ses_<id>`），不含 pid、
///   时间或随机数。因此**同一会话无论重启多少次进程、发多少轮请求，header 值都一致**
///   （已实测：恢复同一会话的 3 个不同进程、以及同一进程内 3 轮请求，header 完全相同）。
/// - 会话 id **为空/空白**时，回落到 `rpi-<pid>`。这个值**每次进程启动都会变**，
///   不具跨进程稳定性。该分支只用于“无会话上下文”的退化场景（如离线诊断），
///   正常的会话流程（新会话 / `--session` 恢复 / `-c` 继续）宿主都会传入真实 id，
///   不会走到这里。
fn normalize_session_id(session_id: Option<&str>) -> String {
    let trimmed = session_id.unwrap_or("").trim();
    if trimmed.is_empty() {
        // 退化分支：无会话上下文时只能给进程级稳定值（跨进程不稳定，见文档）。
        return format!("rpi-{}", std::process::id());
    }
    if trimmed.starts_with(SESSION_PREFIX) {
        // 宿主已给出带前缀的 id，不再重复拼接。
        return trimmed.to_string();
    }
    format!("{SESSION_PREFIX}{trimmed}")
}

#[cfg(test)]
mod tests {
    use super::{build_headers, is_opencode_ai_url, USER_AGENT};
    use std::collections::BTreeMap;

    #[test]
    fn opencode_inference_url_matches() {
        assert!(is_opencode_ai_url(
            "https://opencode.ai/inference/go/openai/v1"
        ));
        assert!(is_opencode_ai_url("https://opencode.ai"));
        assert!(is_opencode_ai_url("https://gw.opencode.ai/v1"));
    }

    #[test]
    fn lookalike_and_unparsable_urls_do_not_match() {
        assert!(!is_opencode_ai_url("https://notopencode.ai/x"));
        assert!(!is_opencode_ai_url("https://opencode.ai.evil.example/x"));
        assert!(!is_opencode_ai_url("not a url"));
        assert!(!is_opencode_ai_url(""));
    }

    #[test]
    fn session_id_gains_the_ses_prefix() {
        let headers = build_headers(None, Some("abc"), "m");
        assert_eq!(headers["x-opencode-session"], "ses_abc");
        assert_eq!(headers["user-agent"], USER_AGENT);
    }

    #[test]
    fn existing_prefix_is_not_duplicated() {
        let headers = build_headers(None, Some("ses_abc"), "m");
        assert_eq!(headers["x-opencode-session"], "ses_abc");
    }

    #[test]
    fn same_session_yields_identical_header_across_calls() {
        // 一致性契约：会话 id 有值时，header 只由 id 决定，
        // 与进程、调用次数无关（因此恢复同一会话不会导致 header 漂移）。
        let id = "01a1d379-4ee4-70c2-907a-8be88cb554ce";
        let first = build_headers(None, Some(id), "m");
        for _ in 0..10 {
            assert_eq!(build_headers(None, Some(id), "m"), first);
        }
        assert_eq!(first["x-opencode-session"], format!("ses_{id}"));

        // 不同会话必须得到不同 header（防止误写成常量）。
        let other = build_headers(None, Some("01a1d379-64f7-77f3-95c5-bd25a606cd08"), "m");
        assert_ne!(first["x-opencode-session"], other["x-opencode-session"]);
    }

    #[test]
    fn missing_session_id_falls_back_to_pid() {
        let headers = build_headers(None, None, "m");
        let session = headers
            .get("x-opencode-session")
            .expect("x-opencode-session must always be present");
        assert!(!session.is_empty());
        assert!(session.starts_with("rpi-"));

        // 空白 session 与 None 等价。
        let blank = build_headers(None, Some("   "), "m");
        assert_eq!(blank["x-opencode-session"], session.as_str());
    }

    #[test]
    fn other_headers_are_kept_and_lowercased() {
        let existing = BTreeMap::from([
            ("X-Tenant".to_string(), "tenant".to_string()),
            ("User-Agent".to_string(), "rpi/0.22.1".to_string()),
        ]);
        let headers = build_headers(Some(existing), Some("s1"), "m");
        // 其他 header 保留（名字小写）。
        assert_eq!(headers["x-tenant"], "tenant");
        // 大小写不同的同名 header 只留一份，且被插件值覆盖。
        assert_eq!(headers["user-agent"], USER_AGENT);
        assert_eq!(
            headers
                .keys()
                .filter(|name| name.eq_ignore_ascii_case("user-agent"))
                .count(),
            1
        );
        assert_eq!(headers["x-opencode-session"], "ses_s1");
    }
}
