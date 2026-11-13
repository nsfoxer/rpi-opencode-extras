//! 入参解析与契约校验。
//!
//! 参数来自宿主，可能包含宿主注入的保留键（`__rpi`），必须容忍未知字段。

/// `deepseek_search` 的入参（已按契约校验并夹紧取值范围）。
#[derive(Debug, Clone)]
pub struct SearchParams {
    /// 搜索词，trim 后 1-128 字符。
    pub query: String,
    /// 期望返回条数，1-50。
    pub count: usize,
    /// 是否请求页面正文。
    pub need_content: bool,
}

/// 解析并校验宿主传入的参数 JSON。
///
/// 参数：`raw` 宿主参数的 JSON 文本（可能含宿主注入的保留键 `__rpi`，必须容忍）。
/// 返回：合法则 `Ok(SearchParams)`；否则 `Err(原因)`。
pub fn parse_params(raw: &str) -> Result<SearchParams, String> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| format!("invalid params JSON: {e}"))?;

    // 第一步：query 必填，trim 后长度 1-128（按字符计数，不按字节）。
    let query = value
        .get("query")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("");
    let query_len = query.chars().count();
    if query_len == 0 {
        return Err("missing or empty query".to_string());
    }
    if query_len > 128 {
        return Err(format!("query too long ({query_len} chars, max 128)"));
    }

    // 第二步：count 可选，夹紧到 1-50，缺省 5；非整数一律回落到默认值。
    let count = value
        .get("count")
        .and_then(|v| v.as_i64())
        .map(|n| n.clamp(1, 50) as usize)
        .unwrap_or(5);

    // 第三步：need_content 可选，非布尔一律当作 false。
    let need_content = value
        .get("need_content")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    Ok(SearchParams {
        query: query.to_string(),
        count,
        need_content,
    })
}

// ===========================================================================
// 测试
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::parse_params;

    #[test]
    fn params_are_clamped_to_contract() {
        let params = parse_params(r#"{"query":" rust ","count":999,"need_content":true}"#).unwrap();
        assert_eq!(params.query, "rust");
        assert_eq!(params.count, 50);
        assert!(params.need_content);

        let params = parse_params(r#"{"query":"rust","count":0}"#).unwrap();
        assert_eq!(params.count, 1);
        let params = parse_params(r#"{"query":"rust"}"#).unwrap();
        assert_eq!(params.count, 5);
        assert!(!params.need_content);

        // 宿主注入的保留键必须被容忍。
        assert!(parse_params(r#"{"query":"rust","__rpi":{"cwd":"C:\\x"}}"#).is_ok());
    }

    #[test]
    fn invalid_params_are_rejected() {
        for raw in [
            r#"{"query":"   "}"#,
            r#"{}"#,
            "not json",
            r#"{"query":123}"#,
        ] {
            assert!(parse_params(raw).is_err(), "should reject {raw}");
        }
        // 129 字符超出上限（按字符而非字节计数）。
        let long = format!(r#"{{"query":"{}"}}"#, "a".repeat(129));
        assert!(parse_params(&long).is_err());
        // 128 字符正好合法。
        let ok = format!(r#"{{"query":"{}"}}"#, "a".repeat(128));
        assert!(parse_params(&ok).is_ok());
    }
}
