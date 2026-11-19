//! 插件自身的开关配置：全局 `settings.json` 的 `opencodeExtras` 段。
//!
//! ```jsonc
//! {
//!   "opencodeExtras": {
//!     "provider": true,
//!     "deepseekSearch": true,
//!     "goUsage": true,
//!     "notify": true,
//!     "msys2Bash": { "enabled": true, "msys2Root": "C:\\msys64" }
//!   }
//! }
//! ```
//!
//! 规则：
//! - 段或字段缺失 ⇒ 默认开启，行为与旧版本一致。
//! - 显式 `false` ⇒ 对应 feature 完全不注册（不订阅事件、不起线程、不注册工具）。
//! - 类型不对 ⇒ 打一行 warning，该字段回落到默认值；绝不让插件加载失败。
//! - 只在插件加载时读一次，修改后需要重启 `rpi`。
//!
//! 历史兼容：`msys2Bash` 曾经位于 `settings.json` 顶层。新位置
//! `opencodeExtras.msys2Bash` 优先，旧位置仍可读。

use serde_json::Value;
use std::path::PathBuf;
use std::sync::OnceLock;

/// `settings.json` 的文件名。
pub const SETTINGS_FILE_NAME: &str = "settings.json";

/// `settings.json` 里承载本插件配置的键名。
pub const EXTRAS_KEY: &str = "opencodeExtras";

/// 旧版 `msys2Bash` 的顶层键名（只读兼容）。
pub const LEGACY_MSYS2_KEY: &str = "msys2Bash";

/// 各 feature 的开关。默认全部开启。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeatureToggles {
    pub provider: bool,
    pub deepseek_search: bool,
    pub go_usage: bool,
    pub notify: bool,
}

impl Default for FeatureToggles {
    fn default() -> Self {
        Self {
            provider: true,
            deepseek_search: true,
            go_usage: true,
            notify: true,
        }
    }
}

impl FeatureToggles {
    /// 从 `settings.json` 根对象解析。`None` 表示文件不存在或无法解析。
    pub fn from_root(root: Option<&Value>) -> Self {
        let mut toggles = Self::default();
        let Some(root) = root else {
            return toggles;
        };
        let Some(section) = root.get(EXTRAS_KEY).filter(|value| !value.is_null()) else {
            return toggles;
        };
        let Some(object) = section.as_object() else {
            eprintln!(
                "rpi-opencode-extras: `{EXTRAS_KEY}` must be an object; using default feature toggles"
            );
            return toggles;
        };
        toggles.provider = read_bool(object, "provider", toggles.provider);
        toggles.deepseek_search = read_bool(object, "deepseekSearch", toggles.deepseek_search);
        toggles.go_usage = read_bool(object, "goUsage", toggles.go_usage);
        toggles.notify = read_bool(object, "notify", toggles.notify);
        toggles
    }
}

fn read_bool(object: &serde_json::Map<String, Value>, key: &str, default: bool) -> bool {
    match object.get(key) {
        None | Some(Value::Null) => default,
        Some(Value::Bool(value)) => *value,
        Some(other) => {
            eprintln!(
                "rpi-opencode-extras: `{EXTRAS_KEY}.{key}` must be a boolean (got {other}); using {default}"
            );
            default
        }
    }
}

/// 本插件的开关（进程内只读取一次）。
pub fn toggles() -> FeatureToggles {
    static CACHE: OnceLock<FeatureToggles> = OnceLock::new();
    *CACHE.get_or_init(|| FeatureToggles::from_root(load_root().as_ref()))
}

/// 定位 rpi 的全局 agent 配置目录。
///
/// 返回：`RPI_CODING_AGENT_DIR` 优先；否则 `<用户目录>/.rpi/agent`；都拿不到时 `None`。
pub fn agent_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("RPI_CODING_AGENT_DIR") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    for var in ["USERPROFILE", "HOME"] {
        if let Some(home) = std::env::var_os(var) {
            if !home.is_empty() {
                return Some(PathBuf::from(home).join(".rpi").join("agent"));
            }
        }
    }
    None
}

/// 全局 `settings.json` 的完整路径。
pub fn settings_path() -> Option<PathBuf> {
    agent_dir().map(|dir| dir.join(SETTINGS_FILE_NAME))
}

/// 读取整份 `settings.json`。缺失/损坏都返回 `None`（并在损坏时警告一次）。
fn load_root() -> Option<Value> {
    let path = settings_path()?;
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            eprintln!(
                "rpi-opencode-extras: cannot read {}: {error}; using default feature toggles",
                path.display()
            );
            return None;
        }
    };
    match serde_json::from_str(&text) {
        Ok(value) => Some(value),
        Err(error) => {
            eprintln!(
                "rpi-opencode-extras: cannot parse {}: {error}; using default feature toggles",
                path.display()
            );
            None
        }
    }
}

/// Shell 配置的当前键名：`opencodeExtras.bash`。
pub const BASH_KEY: &str = "bash";

/// 从 `settings.json` 根对象取出 shell 配置段。
///
/// 新位置 `opencodeExtras.bash` 优先；随后兼容 `opencodeExtras.msys2Bash`
/// 和旧顶层 `msys2Bash`，让升级中的用户不必立刻改配置。
pub fn bash_section(root: &Value) -> Option<&Value> {
    let extras = root.get(EXTRAS_KEY);
    extras
        .and_then(|value| value.get(BASH_KEY))
        .filter(|value| !value.is_null())
        .or_else(|| {
            extras
                .and_then(|value| value.get(LEGACY_MSYS2_KEY))
                .filter(|value| !value.is_null())
        })
        .or_else(|| root.get(LEGACY_MSYS2_KEY).filter(|value| !value.is_null()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn missing_root_or_section_keeps_everything_on() {
        let defaults = FeatureToggles::default();
        assert_eq!(FeatureToggles::from_root(None), defaults);
        assert_eq!(FeatureToggles::from_root(Some(&json!({}))), defaults);
        assert_eq!(
            FeatureToggles::from_root(Some(&json!({"opencodeExtras": null}))),
            defaults
        );
        assert_eq!(
            FeatureToggles::from_root(Some(&json!({"opencodeExtras": {}}))),
            defaults
        );
    }

    #[test]
    fn explicit_false_disables_only_that_feature() {
        let root = json!({
            "opencodeExtras": {
                "provider": false,
                "deepseekSearch": false,
                "goUsage": false,
                "notify": false
            }
        });
        let toggles = FeatureToggles::from_root(Some(&root));
        assert!(!toggles.provider);
        assert!(!toggles.deepseek_search);
        assert!(!toggles.go_usage);
        assert!(!toggles.notify);
    }

    #[test]
    fn partial_config_leaves_other_features_on() {
        let root = json!({"opencodeExtras": {"notify": false}});
        let toggles = FeatureToggles::from_root(Some(&root));
        assert!(!toggles.notify);
        assert!(toggles.provider);
        assert!(toggles.deepseek_search);
        assert!(toggles.go_usage);
    }

    #[test]
    fn wrong_types_fall_back_to_defaults_instead_of_failing() {
        let root = json!({
            "opencodeExtras": {
                "provider": "off",
                "notify": 0
            }
        });
        let toggles = FeatureToggles::from_root(Some(&root));
        assert!(toggles.provider);
        assert!(toggles.notify);
    }

    #[test]
    fn non_object_section_falls_back_to_defaults() {
        let root = json!({"opencodeExtras": [1, 2, 3]});
        assert_eq!(
            FeatureToggles::from_root(Some(&root)),
            FeatureToggles::default()
        );
    }

    #[test]
    fn bash_prefers_new_location_and_falls_back_to_legacy() {
        let new = json!({
            "opencodeExtras": {
                "bash": {"root": "E:\\msys64"},
                "msys2Bash": {"msys2Root": "D:\\msys64"}
            },
            "msys2Bash": {"msys2Root": "C:\\msys64"}
        });
        assert_eq!(bash_section(&new).unwrap()["root"], json!("E:\\msys64"));

        let extras_legacy = json!({
            "opencodeExtras": {"msys2Bash": {"msys2Root": "D:\\msys64"}}
        });
        assert_eq!(
            bash_section(&extras_legacy).unwrap()["msys2Root"],
            json!("D:\\msys64")
        );

        let legacy = json!({"msys2Bash": {"msys2Root": "C:\\msys64"}});
        assert_eq!(
            bash_section(&legacy).unwrap()["msys2Root"],
            json!("C:\\msys64")
        );

        let neither = json!({"theme": "dark"});
        assert!(bash_section(&neither).is_none());

        let explicit_null = json!({"opencodeExtras": {"bash": null}});
        assert!(bash_section(&explicit_null).is_none());
    }
}
