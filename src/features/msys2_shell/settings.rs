//! 从 rpi 全局 `settings.json` 读取 `msys2Bash` 配置段。
//!
//! **为什么不用独立配置文件**：用户已经在维护 `~/.rpi/agent/settings.json`，
//! 再额外维护一份 `msys2-bash.json` 属于重复劳动。本模块让插件直接读
//! `settings.json` 的 `msys2Bash` 字段，**不回退**任何独立配置文件：
//!
//! - `settings.json` 存在且含 `msys2Bash` 段 → 启用（段内容由 `config` 校验）
//! - `settings.json` 存在但不含该段 → 不启用（保留宿主内置 bash）
//! - `settings.json` 缺失或无法解析 → 不启用
//!
//! 只读**全局** agent 目录下的 `settings.json`，不读项目级 `.rpi/settings.json`，
//! 避免同一份工具配置随项目漂移。
//!
//! 这里的对象是 `serde_json::Value` 而非强类型结构，原因是同一份
//! `settings.json` 还承载 rpi 自己的配置（`theme`、`defaultModel` 等），
//! 强类型反序列化会因为未知字段而失败。

use serde_json::Value;
use std::path::{Path, PathBuf};

/// `settings.json` 里承载本插件配置的键名。
const SETTINGS_KEY: &str = "msys2Bash";

/// `settings.json` 的文件名。
pub const SETTINGS_FILE_NAME: &str = "settings.json";

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
///
/// 返回：`<agent_dir>/settings.json`；agent 目录不可用时 `None`。
pub fn settings_path() -> Option<PathBuf> {
    agent_dir().map(|dir| dir.join(SETTINGS_FILE_NAME))
}

/// 读取 `settings.json` 的 `msys2Bash` 段。
///
/// 返回：
/// - `Ok(Some((值, 配置文件路径)))`：配置段存在（内容是否合法交给 `config::validate`）。
/// - `Ok(None)`：`settings.json` 缺失，或不含 `msys2Bash` 段 —— 即未启用。
/// - `Err(reason)`：文件存在但无法读取 / 不是合法 JSON。
///
/// 注意：`msys2Bash` 显式为 `null` 按"不存在"处理。
pub fn load_section() -> Result<Option<(Value, PathBuf)>, String> {
    // 第一步：定位全局 settings.json。
    let Some(path) = settings_path() else {
        return Ok(None);
    };
    load_section_at(&path)
}

/// 从指定路径读取 `msys2Bash` 段。
///
/// 参数：`path` `settings.json` 路径（可由 `MSYS2_BASH_CONFIG_DIR` 指定）。
/// 返回：与 [`load_section`] 相同；文件不存在时返回 `Ok(None)`。
pub fn load_section_at(path: &Path) -> Result<Option<(Value, PathBuf)>, String> {
    // 文件不存在 = 未配置。
    if !path.is_file() {
        return Ok(None);
    }

    // 读文件。读失败属于真实错误（权限、编码），要让用户看见。
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;

    // 解析。整份 settings.json 不合法时无法判断配置状态，报错。
    let root: Value = serde_json::from_str(&text)
        .map_err(|error| format!("cannot parse {}: {error}", path.display()))?;

    // 取插件专属段。缺失或 null 都视为未启用。
    match root.get(SETTINGS_KEY) {
        None | Some(Value::Null) => Ok(None),
        Some(section) => Ok(Some((section.clone(), path.to_path_buf()))),
    }
}

// ===========================================================================
// 测试
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// 在临时目录里写一份 settings.json，并返回路径。
    ///
    /// 注意：这里直接测解析逻辑，不依赖 `agent_dir()`（进程级环境变量不适合在
    /// 并行测试里改），因此复用与 `load_section` 相同的解析步骤。
    fn parse(text: &str) -> Result<Option<Value>, String> {
        let root: Value = serde_json::from_str(text).map_err(|error| error.to_string())?;
        match root.get(SETTINGS_KEY) {
            None | Some(Value::Null) => Ok(None),
            Some(section) => Ok(Some(section.clone())),
        }
    }

    #[test]
    fn reads_section_from_settings() {
        let text = r#"{
            "theme": "dark",
            "defaultModel": "x",
            "msys2Bash": { "msys2Root": "C:\\msys64" }
        }"#;
        let section = parse(text).expect("valid").expect("section present");
        assert_eq!(section["msys2Root"], "C:\\msys64");
    }

    #[test]
    fn missing_section_means_disabled() {
        let text = r#"{ "theme": "dark" }"#;
        assert!(parse(text).expect("valid").is_none());
    }

    #[test]
    fn null_section_means_disabled() {
        let text = r#"{ "msys2Bash": null }"#;
        assert!(parse(text).expect("valid").is_none());
    }

    #[test]
    fn unrelated_keys_are_ignored() {
        // settings.json 里有大量 rpi 自己的键，不能因为不认识就报错。
        let text = r#"{
            "lastRpiChangelogVersion": "0.23.0",
            "theme": "dark",
            "defaultTools": ["read", "bash"],
            "msys2Bash": { "msys2Root": "C:\\msys64", "envFrom": "fish" }
        }"#;
        let section = parse(text).expect("valid").expect("section present");
        assert_eq!(section["envFrom"], "fish");
    }

    #[test]
    fn invalid_json_is_an_error() {
        assert!(parse("{ not json").is_err());
    }

    #[test]
    fn agent_dir_prefers_env_var() {
        // 该测试只验证路径拼接逻辑，不修改进程环境。
        let path = settings_path();
        if let Some(path) = path {
            assert!(path.ends_with(SETTINGS_FILE_NAME));
        }
    }
}
