//! 宿主配置读取：定位 agent 目录、解析 `models.json`、筛出 opencode.ai provider。
//!
//! 这一层只负责"哪些 provider 需要被包装"，不关心注册方式，也不依赖 provider 名字。

use crate::error::GoError;
use crate::headers::is_opencode_ai_url;
use std::path::PathBuf;

/// 一个需要注入 header 的 provider 描述。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderSpec {
    /// `models.json` 里的 provider key，注册时作为 provider id。
    pub id: String,
    /// 该 provider 的 baseUrl，原样传给宿主注册槽位。
    pub base_url: String,
}

/// 定位 rpi 的 agent 配置目录。
///
/// 返回：`RPI_CODING_AGENT_DIR` 优先；否则 `<用户目录>/.rpi/agent`。
/// 错误：两个路径都拿不到时为 [`GoError::Config`]。
pub fn agent_dir() -> Result<PathBuf, GoError> {
    // 第一步：显式环境变量优先（宿主/测试可重定向配置目录）。
    if let Some(dir) = std::env::var_os("RPI_CODING_AGENT_DIR") {
        if !dir.is_empty() {
            return Ok(PathBuf::from(dir));
        }
    }

    // 第二步：回落到用户目录。Windows 用 USERPROFILE，类 Unix 用 HOME。
    for var in ["USERPROFILE", "HOME"] {
        if let Some(home) = std::env::var_os(var) {
            if !home.is_empty() {
                return Ok(PathBuf::from(home).join(".rpi").join("agent"));
            }
        }
    }

    Err(GoError::Config(
        "cannot locate agent dir: set RPI_CODING_AGENT_DIR or USERPROFILE/HOME".into(),
    ))
}

/// `models.json` 的完整路径。
///
/// 返回：`<agent_dir>/models.json`。
pub fn models_json_path() -> Result<PathBuf, GoError> {
    Ok(agent_dir()?.join("models.json"))
}

/// 读取并解析宿主 `models.json`。
///
/// 返回：解析后的 JSON 树。
/// 错误：文件不存在、读取失败或不是合法 JSON 时为 [`GoError::Config`]。
pub fn load_models_json() -> Result<serde_json::Value, GoError> {
    let path = models_json_path()?;
    let text = std::fs::read_to_string(&path).map_err(|error| {
        GoError::Config(format!("cannot read {}: {error}", path.display()))
    })?;
    serde_json::from_str(&text).map_err(|error| {
        GoError::Config(format!("cannot parse {}: {error}", path.display()))
    })
}

/// 从 `models.json` 的 JSON 树里筛出所有指向 opencode.ai 的 provider。
///
/// 参数：`models` 为 `models.json` 解析结果。
/// 返回：按 `models.json` 中的顺序排列的 provider 列表；没有任何 provider 时为空。
/// 结构不符合预期（缺 `providers` 对象）时同样返回空列表，交由调用方按"无需注册"处理。
pub fn select_opencode_providers(models: &serde_json::Value) -> Vec<ProviderSpec> {
    let Some(providers) = models.get("providers").and_then(|value| value.as_object()) else {
        return Vec::new();
    };

    providers
        .iter()
        .filter_map(|(id, provider)| {
            // 只按 URL 判断，provider 名字不参与决策。
            let base_url = provider.get("baseUrl").and_then(|value| value.as_str())?;
            if !is_opencode_ai_url(base_url) {
                return None;
            }
            Some(ProviderSpec {
                id: id.clone(),
                base_url: base_url.to_string(),
            })
        })
        .collect()
}

/// 读取宿主配置并返回需要包装的 provider 列表。
///
/// 返回：`models.json` 中 baseUrl 指向 opencode.ai 的 provider。
pub fn discover_opencode_providers() -> Result<Vec<ProviderSpec>, GoError> {
    let models = load_models_json()?;
    Ok(select_opencode_providers(&models))
}

#[cfg(test)]
mod tests {
    use super::select_opencode_providers;

    fn models(json: serde_json::Value) -> Vec<super::ProviderSpec> {
        select_opencode_providers(&json)
    }

    #[test]
    fn selection_follows_the_url_not_the_provider_name() {
        let json = serde_json::json!({
            "providers": {
                "go-test": {
                    "baseUrl": "https://opencode.ai/inference/go/openai/v1"
                },
                "notopencode": {
                    "baseUrl": "https://notopencode.ai/x"
                },
                "deepseek": {
                    "baseUrl": "https://api.deepseek.com/"
                }
            }
        });
        let selected = models(json);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].id, "go-test");
        assert_eq!(
            selected[0].base_url,
            "https://opencode.ai/inference/go/openai/v1"
        );
    }

    #[test]
    fn missing_providers_object_yields_no_selection() {
        assert!(models(serde_json::json!({})).is_empty());
        assert!(models(serde_json::json!({"providers": []})).is_empty());
    }

    #[test]
    fn providers_without_base_url_are_skipped() {
        let json = serde_json::json!({
            "providers": {
                "broken": {"api": "openai-completions"}
            }
        });
        assert!(models(json).is_empty());
    }
}
