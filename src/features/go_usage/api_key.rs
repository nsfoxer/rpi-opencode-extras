//! opencode.ai Go 套餐凭据解析。
//!
//! 只读 `models.json`，**不读环境变量**：套餐用量必须和实际发请求用的 key 一致，
//! 环境变量里的 key 可能指向别的账户。错误原因里**绝不出现** key 内容。

use crate::config;

/// opencode.ai 用量端点（Go 套餐）。
///
/// `zen/go/v1/usage` 是旧别名、返回同一份数据，作为兜底路径。
pub const USAGE_ENDPOINTS: [&str; 2] = [
    "https://opencode.ai/inference/go/v1/usage",
    "https://opencode.ai/zen/go/v1/usage",
];

/// 从 `models.json` 取指向 opencode.ai 的 provider 的 apiKey。
///
/// 返回：`Ok((key, base_url))`；`key` 已 trim 且非空，`base_url` 是该 provider 的
/// `baseUrl`（仅用于诊断，不含凭据）。
/// 错误：文件缺失、解析失败、没有 opencode.ai provider、或该 provider 没有 apiKey。
pub fn resolve_api_key() -> Result<(String, String), String> {
    // 第一步：定位并解析 models.json（复用共享核心的目录定位）。
    let models = config::load_models_json().map_err(|error| error.describe())?;

    // 第二步：按 baseUrl 挑出 opencode.ai provider（与 provider feature 同一套判定）。
    let specs = config::select_opencode_providers(&models);
    let spec = specs
        .first()
        .ok_or_else(|| "no opencode.ai provider in models.json".to_string())?;

    // 第三步：从原始 JSON 里取该 provider 的 apiKey（ProviderSpec 刻意不携带凭据）。
    let key = key_for_provider(&models, &spec.id)
        .ok_or_else(|| format!("provider `{}` has no apiKey", spec.id))?;

    Ok((key, spec.base_url.clone()))
}

/// 从 `models.json` 树里读取指定 provider 的 apiKey。
///
/// 参数：`models` 已解析的 `models.json`；`provider_id` provider key。
/// 返回：`Some(key)`（已 trim、非空）；缺失或空白时 `None`。
pub fn key_for_provider(models: &serde_json::Value, provider_id: &str) -> Option<String> {
    let provider = models.get("providers")?.get(provider_id)?;
    // 兼容 camelCase 与 snake_case 两种写法。
    let key = provider
        .get("apiKey")
        .or_else(|| provider.get("api_key"))
        .and_then(|value| value.as_str())
        .map(str::trim)?;
    (!key.is_empty()).then(|| key.to_string())
}

#[cfg(test)]
mod tests {
    use super::key_for_provider;

    fn models() -> serde_json::Value {
        serde_json::json!({
            "providers": {
                "opencode-go": {
                    "baseUrl": "https://opencode.ai/inference/go/openai/v1",
                    "apiKey": "  oc_sk_test  "
                },
                "snake": { "base_url": "https://opencode.ai/x", "api_key": "k2" },
                "blank": { "baseUrl": "https://opencode.ai/x", "apiKey": "   " },
                "no-key": { "baseUrl": "https://opencode.ai/x" }
            }
        })
    }

    #[test]
    fn reads_and_trims_the_key() {
        assert_eq!(
            key_for_provider(&models(), "opencode-go").as_deref(),
            Some("oc_sk_test")
        );
        assert_eq!(key_for_provider(&models(), "snake").as_deref(), Some("k2"));
    }

    #[test]
    fn blank_or_missing_key_is_none() {
        let value = models();
        assert!(key_for_provider(&value, "blank").is_none());
        assert!(key_for_provider(&value, "no-key").is_none());
        assert!(key_for_provider(&value, "unknown-provider").is_none());
        assert!(key_for_provider(&serde_json::json!({}), "any").is_none());
    }

    #[test]
    fn endpoints_are_opencode_domains() {
        // 保护性断言：两个端点都必须落在 opencode.ai，避免将来误改成别处。
        for endpoint in super::USAGE_ENDPOINTS {
            assert!(
                crate::headers::is_opencode_ai_url(endpoint),
                "usage endpoint must be on opencode.ai: {endpoint}"
            );
        }
    }
}
