//! `msys2-bash` 的配置定位、解析与校验。
//!
//! **配置文件本身就是开关**：三个候选路径都不存在时返回 `Ok(None)`，调用方据此
//! 不注册任何工具（保留宿主内置 bash）。配置存在但无效时返回 `Err`，调用方打印一行
//! warning 后同样不注册——绝不回退到 Git Bash，也不中断插件加载。

use serde::Deserialize;
use std::path::{Path, PathBuf};

/// 配置文件在 agent 目录下的文件名。
const CONFIG_FILE_NAME: &str = "msys2-bash.json";

/// 环境变量：显式指定配置文件路径（优先级最高）。
const CONFIG_ENV_VAR: &str = "MSYS2_BASH_CONFIG";

/// 默认 `MSYSTEM` 环境变量值。
const DEFAULT_MSYSTEM: &str = "MINGW64";

/// 默认 shell 相对路径（相对 `msys2Root`）。
const DEFAULT_SHELL: &str = "usr/bin/bash.exe";

/// 默认单次阻塞等待秒数。
pub const DEFAULT_WAIT_SECONDS: u64 = 30;

/// 默认命令总截止秒数。
pub const DEFAULT_TIMEOUT_SECONDS: u64 = 900;

/// 默认单流输出上限（1 MiB）。
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 1_048_576;

/// 默认保留的已结束 job 条数。
pub const DEFAULT_MAX_JOBS: usize = 20;

/// `waitSeconds` 允许范围（秒）。
pub const WAIT_SECONDS_RANGE: (u64, u64) = (1, 120);

/// `defaultTimeoutSeconds` 允许范围（秒）。
pub const TIMEOUT_SECONDS_RANGE: (u64, u64) = (1, 1800);

// ===========================================================================
// 反序列化结构
// ===========================================================================

/// `msys2-bash.json` 的原始结构（全部字段可选，缺省取默认值）。
///
/// 注意：`msys2Root` 与 `shell` **没有内置默认值语义**——`msys2Root` 缺省时配置
/// 无法定位 shell，按无效处理。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawConfig {
    /// MSYS2 安装根目录，例如 `C:\msys64`。
    msys2_root: Option<String>,
    /// `MSYSTEM` 环境变量值，例如 `MINGW64`。
    msystem: Option<String>,
    /// shell 相对 `msys2Root` 的路径，例如 `usr/bin/bash.exe`。
    shell: Option<String>,
    /// bash 工具默认阻塞等待秒数。
    wait_seconds: Option<u64>,
    /// 命令总截止秒数。
    default_timeout_seconds: Option<u64>,
    /// 单流输出上限（字节）。
    max_output_bytes: Option<u64>,
    /// 保留的已结束 job 条数。
    max_jobs: Option<u64>,
    /// 安全黑名单配置。
    security: Option<RawSecurity>,
}

/// 黑名单配置的原始结构。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSecurity {
    /// 是否启用黑名单检查，默认 `true`。
    blacklist_enabled: Option<bool>,
    /// 自定义正则列表；缺省时使用内置默认列表。
    blacklist_patterns: Option<Vec<String>>,
}

/// 校验通过、可直接使用的配置。
#[derive(Debug, Clone)]
pub struct BashConfig {
    /// MSYS2 安装根目录（已确认存在且是目录）。
    pub msys2_root: PathBuf,
    /// `MSYSTEM` 环境变量值。
    pub msystem: String,
    /// shell 可执行文件的绝对路径（已确认存在且是文件）。
    pub shell: PathBuf,
    /// bash 工具默认阻塞等待秒数（已夹紧到 1-120）。
    pub wait_seconds: u64,
    /// 命令总截止秒数（已夹紧到 1-1800）。
    pub default_timeout_seconds: u64,
    /// 单流输出上限，字节（保证 > 0）。
    pub max_output_bytes: usize,
    /// 保留的已结束 job 条数（保证 >= 1）。
    pub max_jobs: usize,
    /// 编译好的黑名单；`enabled=false` 时为空。
    pub blacklist: Vec<(String, regex::Regex)>,
    /// 配置来源路径，仅用于诊断输出。
    #[allow(dead_code)]
    pub source: PathBuf,
}

// ===========================================================================
// 定位
// ===========================================================================

/// 按优先级列出配置文件候选路径。
///
/// 顺序：`MSYS2_BASH_CONFIG` → `<agent_dir>/msys2-bash.json` → `<home>/.rpi/agent/msys2-bash.json`。
/// 返回：去重后的候选路径列表（去重避免 agent_dir 恰为 `~/.rpi/agent` 时重复读取）。
pub fn config_candidates() -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    let mut push = |path: PathBuf| {
        if !candidates.contains(&path) {
            candidates.push(path);
        }
    };

    // 第一步：显式环境变量最高优先。
    if let Some(value) = std::env::var_os(CONFIG_ENV_VAR) {
        if !value.is_empty() {
            push(PathBuf::from(value));
        }
    }

    // 第二步：宿主 agent 目录（RPI_CODING_AGENT_DIR 优先，否则 ~/.rpi/agent）。
    if let Some(dir) = agent_dir() {
        push(dir.join(CONFIG_FILE_NAME));
    }

    // 第三步：用户目录下的固定位置（agent_dir 拿不到或指向别处时兜底）。
    for var in ["USERPROFILE", "HOME"] {
        if let Some(home) = std::env::var_os(var) {
            if !home.is_empty() {
                push(PathBuf::from(home).join(".rpi").join("agent").join(CONFIG_FILE_NAME));
            }
        }
    }

    candidates
}

/// 定位 rpi 的 agent 配置目录。
///
/// 返回：`RPI_CODING_AGENT_DIR` 优先；否则 `<用户目录>/.rpi/agent`；都拿不到时 `None`
/// （此时仍可经 `MSYS2_BASH_CONFIG` 或用户目录兜底找到配置）。
fn agent_dir() -> Option<PathBuf> {
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

/// 找到第一个存在的配置文件路径。
///
/// 返回：`Some(路径)` 表示存在配置（是否有效由 [`load_config`] 判断）；`None` 表示
/// 三个候选路径都没有配置——此时调用方**不得**注册任何工具。
pub fn find_config_path() -> Option<PathBuf> {
    config_candidates().into_iter().find(|path| path.is_file())
}

// ===========================================================================
// 加载与校验
// ===========================================================================

/// 加载并校验配置。
///
/// 返回：
/// - `Ok(None)`：三个候选路径都没有配置文件（未启用）。
/// - `Ok(Some(config))`：配置存在且校验通过（启用）。
/// - `Err(reason)`：配置存在但无效，或读取/解析失败。`reason` 不含敏感信息。
pub fn load_config() -> Result<Option<BashConfig>, String> {
    // 第一步：没有配置文件就什么都不做（这就是"未配置"模式）。
    let Some(path) = find_config_path() else {
        return Ok(None);
    };

    // 第二步：读取并反序列化。字段缺失用默认值，未知字段忽略（便于向前兼容）。
    let text = std::fs::read_to_string(&path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let raw: RawConfig = serde_json::from_str(&text)
        .map_err(|error| format!("cannot parse {}: {error}", path.display()))?;

    // 第三步：字段校验 + 夹紧。
    validate(raw, &path).map(Some)
}

/// 校验原始配置并构造可用配置。
///
/// 参数：`raw` 反序列化结果；`path` 配置文件路径（写进 `source`，仅用于诊断）。
/// 返回：`Ok(BashConfig)` 或 `Err(原因)`。
fn validate(raw: RawConfig, path: &Path) -> Result<BashConfig, String> {
    // 第一步：msys2Root 必填且必须存在（不存在即无效，不回退）。
    let root_text = raw
        .msys2_root
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| "msys2Root is required".to_string())?;
    let msys2_root = PathBuf::from(root_text);
    if !msys2_root.is_dir() {
        return Err(format!("msys2Root is not a directory: {root_text}"));
    }

    // 第二步：shell 相对 root 解析；绝对路径则直接使用。
    let shell_text = raw
        .shell
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or(DEFAULT_SHELL);
    let shell = if Path::new(shell_text).is_absolute() {
        PathBuf::from(shell_text)
    } else {
        msys2_root.join(shell_text)
    };
    if !shell.is_file() {
        return Err(format!("shell is not a file: {}", shell.display()));
    }

    // 第三步：msystem 空串视为未配置，回落到 MINGW64。
    let msystem = raw
        .msystem
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or(DEFAULT_MSYSTEM)
        .to_string();

    // 第四步：数值字段夹紧到契约范围；越界不报错，按边界处理并在日志里可见。
    let wait_seconds = raw
        .wait_seconds
        .unwrap_or(DEFAULT_WAIT_SECONDS)
        .clamp(WAIT_SECONDS_RANGE.0, WAIT_SECONDS_RANGE.1);
    let default_timeout_seconds = raw
        .default_timeout_seconds
        .unwrap_or(DEFAULT_TIMEOUT_SECONDS)
        .clamp(TIMEOUT_SECONDS_RANGE.0, TIMEOUT_SECONDS_RANGE.1);

    // maxOutputBytes 必须 > 0；给 0 属于非法配置（否则永远截断成空）。
    let max_output_bytes = match raw.max_output_bytes.unwrap_or(DEFAULT_MAX_OUTPUT_BYTES as u64) {
        0 => return Err("maxOutputBytes must be > 0".to_string()),
        value => value.min(usize::MAX as u64) as usize,
    };

    // maxJobs 必须 >= 1（否则已完成结果无法保留，工具形同虚设）。
    let max_jobs = match raw.max_jobs.unwrap_or(DEFAULT_MAX_JOBS as u64) {
        0 => return Err("maxJobs must be >= 1".to_string()),
        value => value.min(usize::MAX as u64) as usize,
    };

    // 第五步：编译黑名单。正则非法属于配置无效（不静默跳过）。
    let blacklist = build_blacklist(raw.security.as_ref())?;

    Ok(BashConfig {
        msys2_root,
        msystem,
        shell,
        wait_seconds,
        default_timeout_seconds,
        max_output_bytes,
        max_jobs,
        blacklist,
        source: path.to_path_buf(),
    })
}

/// 内置默认黑名单（与 msys2-mcp 项目保持一致）。
fn default_blacklist_patterns() -> Vec<String> {
    vec![
        r"rm\s+-rf\s+/".to_string(),
        r"mkfs\.".to_string(),
        r"dd\s+if=.*of=/dev/".to_string(),
        r"format\s+[A-Z]:".to_string(),
        r"reg\s+(add|delete)\s+".to_string(),
        r"(?i)shutdown".to_string(),
        r"rd\s+/s\s+/q\s+[A-Z]:\\".to_string(),
    ]
}

/// 编译黑名单正则。
///
/// 参数：`security` 用户配置；`None` 或字段缺省时用内置默认列表。
/// 返回：`enabled=false` 时为空列表；否则为 `(原文, 编译后正则)` 列表。
fn build_blacklist(security: Option<&RawSecurity>) -> Result<Vec<(String, regex::Regex)>, String> {
    let enabled = security
        .and_then(|raw| raw.blacklist_enabled)
        .unwrap_or(true);
    if !enabled {
        return Ok(Vec::new());
    }

    let patterns = security
        .and_then(|raw| raw.blacklist_patterns.clone())
        .unwrap_or_else(default_blacklist_patterns);

    let mut compiled = Vec::with_capacity(patterns.len());
    for pattern in patterns {
        let regex = regex::Regex::new(&pattern)
            .map_err(|error| format!("invalid blacklist pattern `{pattern}`: {error}"))?;
        compiled.push((pattern, regex));
    }
    Ok(compiled)
}

/// 用黑名单检查命令。
///
/// 参数：`blacklist` 已编译的规则；`command` 待检查命令。
/// 返回：命中时 `Some(命中的规则原文)`，否则 `None`。
pub fn check_blacklist(blacklist: &[(String, regex::Regex)], command: &str) -> Option<String> {
    blacklist
        .iter()
        .find(|(_, regex)| regex.is_match(command))
        .map(|(pattern, _)| pattern.clone())
}

// ===========================================================================
// 测试
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一份合法原始配置（root/shell 指向真实存在的位置，由调用方覆盖）。
    fn raw_config(root: &Path, shell: &Path) -> RawConfig {
        RawConfig {
            msys2_root: Some(root.display().to_string()),
            msystem: None,
            shell: Some(shell.display().to_string()),
            wait_seconds: None,
            default_timeout_seconds: None,
            max_output_bytes: None,
            max_jobs: None,
            security: None,
        }
    }

    /// 借当前可执行文件所在目录充当"存在的目录"，并给它自己当"存在的文件"。
    fn existing_pair() -> (PathBuf, PathBuf) {
        let exe = std::env::current_exe().expect("current exe");
        let dir = exe.parent().expect("exe parent").to_path_buf();
        (dir, exe)
    }

    #[test]
    fn valid_config_takes_defaults_and_compiles_blacklist() {
        let (dir, file) = existing_pair();
        let config = validate(raw_config(&dir, &file), Path::new("cfg.json")).expect("valid");
        assert_eq!(config.msystem, DEFAULT_MSYSTEM);
        assert_eq!(config.wait_seconds, DEFAULT_WAIT_SECONDS);
        assert_eq!(config.default_timeout_seconds, DEFAULT_TIMEOUT_SECONDS);
        assert_eq!(config.max_output_bytes, DEFAULT_MAX_OUTPUT_BYTES);
        assert_eq!(config.max_jobs, DEFAULT_MAX_JOBS);
        assert!(!config.blacklist.is_empty(), "default blacklist is enabled");
    }

    #[test]
    fn missing_root_is_invalid_without_fallback() {
        let (dir, file) = existing_pair();
        let mut raw = raw_config(&dir, &file);
        raw.msys2_root = None;
        assert!(validate(raw, Path::new("cfg.json")).is_err());

        let mut raw = raw_config(&dir, &file);
        raw.msys2_root = Some(r"C:\definitely-not-here-9f3a".to_string());
        assert!(validate(raw, Path::new("cfg.json")).is_err());
    }

    #[test]
    fn missing_shell_is_invalid() {
        let (dir, _file) = existing_pair();
        let mut raw = raw_config(&dir, &dir);
        raw.shell = Some("usr/bin/definitely-missing.exe".to_string());
        assert!(validate(raw, Path::new("cfg.json")).is_err());
    }

    #[test]
    fn numeric_fields_are_clamped_to_contract() {
        let (dir, file) = existing_pair();
        let mut raw = raw_config(&dir, &file);
        raw.wait_seconds = Some(1000);
        raw.default_timeout_seconds = Some(99_999);
        let config = validate(raw, Path::new("cfg.json")).expect("valid");
        assert_eq!(config.wait_seconds, WAIT_SECONDS_RANGE.1);
        assert_eq!(config.default_timeout_seconds, TIMEOUT_SECONDS_RANGE.1);

        let mut raw = raw_config(&dir, &file);
        raw.wait_seconds = Some(0);
        raw.default_timeout_seconds = Some(0);
        let config = validate(raw, Path::new("cfg.json")).expect("valid");
        assert_eq!(config.wait_seconds, WAIT_SECONDS_RANGE.0);
        assert_eq!(config.default_timeout_seconds, TIMEOUT_SECONDS_RANGE.0);
    }

    #[test]
    fn zero_output_bytes_and_zero_max_jobs_are_invalid() {
        let (dir, file) = existing_pair();
        let mut raw = raw_config(&dir, &file);
        raw.max_output_bytes = Some(0);
        assert!(validate(raw, Path::new("cfg.json")).is_err());

        let mut raw = raw_config(&dir, &file);
        raw.max_jobs = Some(0);
        assert!(validate(raw, Path::new("cfg.json")).is_err());
    }

    #[test]
    fn blacklist_can_be_disabled_or_customized() {
        let (dir, file) = existing_pair();
        let mut raw = raw_config(&dir, &file);
        raw.security = Some(RawSecurity {
            blacklist_enabled: Some(false),
            blacklist_patterns: None,
        });
        let config = validate(raw, Path::new("cfg.json")).expect("valid");
        assert!(config.blacklist.is_empty());

        let mut raw = raw_config(&dir, &file);
        raw.security = Some(RawSecurity {
            blacklist_enabled: Some(true),
            blacklist_patterns: Some(vec![r"^danger$".to_string()]),
        });
        let config = validate(raw, Path::new("cfg.json")).expect("valid");
        assert_eq!(config.blacklist.len(), 1);
        assert_eq!(check_blacklist(&config.blacklist, "danger").as_deref(), Some("^danger$"));
        assert!(check_blacklist(&config.blacklist, "safe").is_none());
    }

    #[test]
    fn invalid_regex_makes_config_invalid() {
        let (dir, file) = existing_pair();
        let mut raw = raw_config(&dir, &file);
        raw.security = Some(RawSecurity {
            blacklist_enabled: Some(true),
            blacklist_patterns: Some(vec!["(unclosed".to_string()]),
        });
        assert!(validate(raw, Path::new("cfg.json")).is_err());
    }

    #[test]
    fn default_blacklist_catches_destructive_commands() {
        let patterns = default_blacklist_patterns()
            .into_iter()
            .map(|p| {
                let regex = regex::Regex::new(&p).expect("default pattern compiles");
                (p, regex)
            })
            .collect::<Vec<_>>();
        assert!(check_blacklist(&patterns, "rm -rf / --no-preserve-root").is_some());
        assert!(check_blacklist(&patterns, "echo hello").is_none());
    }

    #[test]
    fn candidates_are_unique() {
        let candidates = config_candidates();
        let mut deduped = candidates.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(candidates.len(), deduped.len());
    }
}
