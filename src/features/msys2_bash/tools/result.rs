//! 工具结果的统一封装与参数解析辅助。
//!
//! 四个工具都返回同一个 JSON 形状（与宿主 `AgentToolResult` 对齐）：
//! `{"content":[{"type":"text","text":"..."}],"details":{...}}`；
//! 失败时返回 `{"error":"..."}`（宿主会把它作为错误结果交给模型）。

use serde_json::{json, Value};

/// 构造成功结果。
///
/// 参数：`text` 给模型看的正文；`details` 结构化明细（`null` 表示不带明细）。
/// 返回：序列化后的工具结果 JSON。
pub fn ok_result(text: &str, details: Value) -> String {
    let mut value = json!({
        "content": [ { "type": "text", "text": text } ]
    });
    // details 为 null 时省略该键，避免给模型塞一堆 null。
    if !details.is_null() {
        if let Some(object) = value.as_object_mut() {
            object.insert("details".to_string(), details);
        }
    }
    value.to_string()
}

/// 构造错误结果。
///
/// 参数：`message` 错误原因（不含敏感信息）。
/// 返回：序列化后的 `{"error": ...}` JSON。
pub fn error_result(message: &str) -> String {
    json!({ "error": message }).to_string()
}

/// 解析工具参数 JSON。
///
/// 参数：`raw` 宿主传入的参数字符串（可能含宿主注入的 `__rpi`，必须容忍）。
/// 返回：`Ok(Value)`；不是合法 JSON 或不是对象时 `Err(原因)`。
pub fn parse_params(raw: &str) -> Result<Value, String> {
    let value: Value =
        serde_json::from_str(raw).map_err(|error| format!("invalid params JSON: {error}"))?;
    if !value.is_object() {
        return Err("params must be a JSON object".to_string());
    }
    Ok(value)
}

/// 取整数字段并夹紧到范围内。
///
/// 参数：`value` 参数对象；`key` 字段名；`range` 允许范围；`fallback` 字段缺失时的默认值。
/// 返回：夹紧后的值。非整数（字符串 / null / 浮点）一律按缺失处理，回落到默认值。
pub fn int_field(value: &Value, key: &str, range: (u64, u64), fallback: u64) -> u64 {
    value
        .get(key)
        .and_then(Value::as_i64)
        .map(|number| {
            let clamped = number.max(range.0 as i64).min(range.1 as i64);
            clamped as u64
        })
        .unwrap_or(fallback)
}

/// 取必填字符串字段（trim 后非空）。
///
/// 参数：`value` 参数对象；`key` 字段名。
/// 返回：`Ok(trim 后的字符串)` 或 `Err(原因)`。
pub fn required_str(value: &Value, key: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("missing or empty `{key}`"))
}

/// 取可选字符串字段（空串视为缺失）。
///
/// 参数：`value` 参数对象；`key` 字段名。
/// 返回：非空字符串或 `None`。
pub fn optional_str(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// 取宿主注入的工作目录（`__rpi.cwd`）。
///
/// 参数：`value` 参数对象（通常是工具参数本体）。
/// 返回：宿主给出的 cwd；缺省或格式不符时为 `None`。
pub fn host_cwd(value: &Value) -> Option<String> {
    value
        .get(rpi_plugin_sdk::HOST_CONTEXT_KEY)
        .and_then(|host| host.get("cwd"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// 取 job id 参数。
///
/// 参数：`value` 参数对象。
/// 返回：`Ok(id >= 1)` 或 `Err(原因)`。
pub fn required_id(value: &Value) -> Result<u64, String> {
    match value.get("id").and_then(Value::as_i64) {
        Some(id) if id >= 1 => Ok(id as u64),
        Some(id) => Err(format!("`id` must be >= 1, got {id}")),
        None => Err("missing or invalid `id`".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ok_result_carries_content_and_details() {
        let json = ok_result("hello", json!({ "id": 1, "status": "completed" }));
        let value: Value = serde_json::from_str(&json).expect("valid json");
        assert_eq!(value["content"][0]["type"], "text");
        assert_eq!(value["content"][0]["text"], "hello");
        assert_eq!(value["details"]["id"], 1);
    }

    #[test]
    fn ok_result_omits_null_details() {
        let json = ok_result("hello", Value::Null);
        let value: Value = serde_json::from_str(&json).expect("valid json");
        assert!(value.get("details").is_none());
    }

    #[test]
    fn parse_params_rejects_non_objects() {
        assert!(parse_params("not json").is_err());
        assert!(parse_params("[]").is_err());
        assert!(parse_params("null").is_err());
        assert!(parse_params(r#"{"a":1}"#).is_ok());
    }

    #[test]
    fn int_field_clamps_and_falls_back() {
        let params = parse_params(r#"{"waitSeconds": 999}"#).expect("ok");
        assert_eq!(int_field(&params, "waitSeconds", (1, 120), 30), 120);

        let params = parse_params(r#"{"waitSeconds": 0}"#).expect("ok");
        assert_eq!(int_field(&params, "waitSeconds", (1, 120), 30), 1);

        let params = parse_params(r#"{"waitSeconds": "abc"}"#).expect("ok");
        assert_eq!(int_field(&params, "waitSeconds", (1, 120), 30), 30);

        let params = parse_params("{}").expect("ok");
        assert_eq!(int_field(&params, "waitSeconds", (1, 120), 30), 30);
    }

    #[test]
    fn string_helpers_trim_and_reject_blank() {
        let params = parse_params(r#"{"command":"  ls -la  ","cwd":"   "}"#).expect("ok");
        assert_eq!(required_str(&params, "command").unwrap(), "ls -la");
        assert!(required_str(&params, "cwd").is_err());
        assert_eq!(optional_str(&params, "cwd"), None);
    }

    #[test]
    fn host_cwd_reads_reserved_key_and_tolerates_absence() {
        let params = parse_params(r#"{"command":"ls","__rpi":{"cwd":"C:\\work"}}"#).expect("ok");
        assert_eq!(host_cwd(&params).as_deref(), Some(r"C:\work"));

        let params = parse_params(r#"{"command":"ls"}"#).expect("ok");
        assert_eq!(host_cwd(&params), None);
    }

    #[test]
    fn required_id_rejects_missing_and_nonpositive() {
        let params = parse_params(r#"{"id": 3}"#).expect("ok");
        assert_eq!(required_id(&params).unwrap(), 3);

        assert!(required_id(&parse_params(r#"{"id": 0}"#).expect("ok")).is_err());
        assert!(required_id(&parse_params("{}").expect("ok")).is_err());
        assert!(required_id(&parse_params(r#"{"id": "3"}"#).expect("ok")).is_err());
    }
}
