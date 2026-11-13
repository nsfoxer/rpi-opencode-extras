//! DeepSeek 凭据解析：环境变量优先，其次 rpi 的 `models.json`。
//!
//! 错误原因里**绝不出现** key 内容。

use crate::config;

/// 读取 DeepSeek API key。
///
/// 顺序：`DEEPSEEK_API_KEY` → `models.json` 里 `baseUrl` 含 `api.deepseek.com` 的 provider。
/// 返回：`Ok(key)`（已 trim，绝不为空串）或 `Err(原因)`；原因里不含任何 key 内容。
pub fn resolve_api_key() -> Result<String, String> {
    // 第一步：环境变量优先，空白值视为未设置。
    if let Ok(value) = std::env::var("DEEPSEEK_API_KEY") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }

    // 第二步：回落到 rpi 的模型配置（复用共享核心的 agent 目录定位，
    // 不再自带一份重复的路径逻辑与写死的兜底路径）。
    let path = models_json_path().ok_or_else(|| "missing DeepSeek API key".to_string())?;
    let text =
        std::fs::read_to_string(&path).map_err(|_| "missing DeepSeek API key".to_string())?;
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|_| "missing DeepSeek API key".to_string())?;

    key_from_models_json(&value).ok_or_else(|| "missing DeepSeek API key".to_string())
}

/// 定位 rpi 的 `models.json`。
///
/// 返回：`Some(<agent_dir>/models.json)`；agent 目录不可用或文件不存在时 `None`。
pub fn models_json_path() -> Option<std::path::PathBuf> {
    let path = config::agent_dir().ok()?.join("models.json");
    path.is_file().then_some(path)
}

/// 从 `models.json` 的结构里挑出 DeepSeek 的 apiKey。
///
/// 参数：`value` 已解析的 models.json。
/// 返回：找到则 `Some(key)`；否则 `None`。
pub fn key_from_models_json(value: &serde_json::Value) -> Option<String> {
    let providers = value.get("providers")?.as_object()?;
    // 遍历 providers.*，按 baseUrl 是否指向 api.deepseek.com 判定（与 provider 名字无关）。
    for provider in providers.values() {
        let base_url = provider
            .get("baseUrl")
            .or_else(|| provider.get("base_url"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !base_url.contains("api.deepseek.com") {
            continue;
        }
        let key = provider
            .get("apiKey")
            .or_else(|| provider.get("api_key"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("");
        if !key.is_empty() {
            return Some(key.to_string());
        }
    }
    None
}

// ===========================================================================
// 测试
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::key_from_models_json;

    #[test]
    fn key_is_selected_by_base_url_and_missing_key_is_reported() {
        let models = serde_json::json!({
            "providers": {
                "other": { "baseUrl": "https://api.openai.com/v1", "apiKey": "nope" },
                "deepseek": { "baseUrl": "https://api.deepseek.com/", "apiKey": "  key-from-models  " }
            }
        });
        assert_eq!(
            key_from_models_json(&models).as_deref(),
            Some("key-from-models")
        );

        let without = serde_json::json!({ "providers": { "other": { "baseUrl": "https://x", "apiKey": "v" } } });
        assert!(key_from_models_json(&without).is_none());
        assert!(key_from_models_json(&serde_json::json!({})).is_none());
    }

    #[test]
    fn snake_case_keys_are_also_accepted() {
        let models = serde_json::json!({
            "providers": {
                "ds": { "base_url": "https://api.deepseek.com/v1", "api_key": " snake-key " }
            }
        });
        assert_eq!(key_from_models_json(&models).as_deref(), Some("snake-key"));
    }
}
