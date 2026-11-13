//! Shell 环境变量捕获与磁盘缓存。
//!
//! **为什么需要**：MSYS2 的 login shell（`bash -lc`）每次都 source `/etc/profile`，
//! 单次启动约 700ms；而非 login shell（`bash -c`）只要约 63ms。二者的差异几乎全部
//! 来自 profile 里的 locale、`which`、`uname`、`hostname` 等一次性开销。
//!
//! 做法：启动期**捕获一次**目标 shell 的完整环境变量（`<shell> -c env`），
//! 之后每条命令都用非 login 模式启动，并把捕获到的变量注入进去。
//! 既拿到完整的 PATH / HOME / 用户自定义变量，又不重复付 profile 的代价。
//!
//! **支持继承用户自己的 shell**（例如 fish）：`envFrom` 选 `fish` 时执行
//! `fish -c env`，fish 在非 login 模式下同样会 source `~/.config/fish/config.fish`，
//! 因此 `set -xg` 导出的变量会被一并捕获。注意 fish 的语法、函数、abbr
//! **不会**继承——命令本体始终由 bash 执行。
//!
//! 磁盘缓存：`%APPDATA%\rpi\msys2-bash-env.json`，带指纹
//! (`msys2Root|msystem|shell|envFrom`)。命中直接读盘（毫秒级），未命中才捕获。
//! 读写失败一律降级为"无缓存"，绝不阻塞插件加载。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 单次捕获的超时（秒）。
///
/// 首次运行可能触发 profile 初始化，极端情况需数秒；超过就放弃捕获并降级为空环境
/// （命令仍能跑，只是少了自定义变量）。该值同时写进启动诊断日志，便于排查。
const CAPTURE_TIMEOUT_SECONDS: u64 = 20;

/// 缓存文件名。
const CACHE_FILE_NAME: &str = "msys2-bash-env.json";

/// 环境变量来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EnvFrom {
    /// 不捕获，直接用进程自身环境（`-c` 非 login）。
    None,
    /// 从 bash login shell 捕获（`bash -c env`）。
    Bash,
    /// 从 fish 捕获（`fish -c env`），可带上 `config.fish` 里的导出变量。
    Fish,
}

impl EnvFrom {
    /// 从配置文本解析；无法识别时回落到 `Bash`。
    ///
    /// 参数：`text` 配置值（大小写不敏感）。
    /// 返回：对应枚举。
    pub fn parse(text: &str) -> Self {
        match text.trim().to_ascii_lowercase().as_str() {
            "none" | "off" | "false" => EnvFrom::None,
            "fish" => EnvFrom::Fish,
            _ => EnvFrom::Bash,
        }
    }

    /// 用于指纹与诊断的稳定名字。
    pub fn as_str(self) -> &'static str {
        match self {
            EnvFrom::None => "none",
            EnvFrom::Bash => "bash",
            EnvFrom::Fish => "fish",
        }
    }

    /// 该来源对应的可执行文件相对于 MSYS2 根目录的路径。
    fn relative_exe(self) -> Option<&'static str> {
        match self {
            EnvFrom::None => None,
            EnvFrom::Bash => Some("usr/bin/bash.exe"),
            EnvFrom::Fish => Some("usr/bin/fish.exe"),
        }
    }
}

/// 捕获到的环境变量表。
#[derive(Debug, Clone, Default)]
pub struct LoginEnv {
    /// 变量表（已滤掉值中含换行的项以外的异常，见 [`parse_env_output`]）。
    vars: HashMap<String, String>,
}

impl LoginEnv {
    /// 空表（`envFrom: none` 或捕获失败时的降级值）。
    pub fn empty() -> Self {
        Self {
            vars: HashMap::new(),
        }
    }

    /// 是否为空。
    ///
    /// 仅测试使用：生产代码判断“要不要注入”统一用 `len() == 0`，
    /// 避免同一语义两种写法。
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.vars.is_empty()
    }

    /// 变量数量。
    pub fn len(&self) -> usize {
        self.vars.len()
    }

    /// 变量表引用（供注入子进程）。
    pub fn vars(&self) -> &HashMap<String, String> {
        &self.vars
    }

    /// 由已有变量表构造。
    pub fn from_vars(vars: HashMap<String, String>) -> Self {
        Self { vars }
    }
}

/// 解析 `env` 命令输出。
///
/// 参数：`output` 每行一个 `KEY=VALUE`。
/// 返回：变量表。空 key 与控制字符 key 会被丢弃（避免注入非法环境）。
///
/// 注意：值里可能含 `=`（如 `OPTS=--a=b`），因此只按**第一个** `=` 切分。
pub fn parse_env_output(output: &str) -> HashMap<String, String> {
    let mut vars = HashMap::new();
    for line in output.lines() {
        let Some(eq) = line.find('=') else {
            continue;
        };
        let key = &line[..eq];
        let value = &line[eq + 1..];
        // 空 key 或含控制字符的 key 一律丢弃：注入后会让子进程环境非法。
        if key.is_empty() || key.chars().any(char::is_control) {
            continue;
        }
        vars.insert(key.to_string(), value.to_string());
    }
    vars
}

/// 计算缓存指纹。
///
/// 参数：`msys2_root` 安装根；`msystem` MSYSTEM 值；`shell` shell 路径；`env_from` 环境来源。
/// 返回：用于判断缓存是否仍然适用的字符串。任一字段变化都会使旧缓存失效。
pub fn fingerprint(msys2_root: &str, msystem: &str, shell: &str, env_from: EnvFrom) -> String {
    format!(
        "{}|{}|{}|{}",
        msys2_root,
        msystem,
        shell,
        env_from.as_str()
    )
}

/// 磁盘缓存条目。
#[derive(Debug, Serialize, Deserialize)]
struct CacheEntry {
    /// 指纹。
    fingerprint: String,
    /// 捕获时间（仅供人工排查）。
    captured_at: String,
    /// 变量表。
    vars: HashMap<String, String>,
}

/// 缓存目录：`%APPDATA%\rpi`（取不到 APPDATA 时退到 agent 目录）。
fn cache_dir() -> Option<PathBuf> {
    if let Some(appdata) = std::env::var_os("APPDATA") {
        if !appdata.is_empty() {
            return Some(PathBuf::from(appdata).join("rpi"));
        }
    }
    // 兜底：与 msys2-bash 配置同级的 agent 目录。
    agent_dir().map(|dir| dir.join(".cache"))
}

/// 定位 rpi 的 agent 配置目录（与 config 模块保持一致）。
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

/// 从默认目录加载缓存。
///
/// 参数：`fp` 指纹。
/// 返回：命中且指纹匹配时返回变量表，否则 `None`。
pub fn load(fp: &str) -> Option<HashMap<String, String>> {
    let dir = cache_dir()?;
    load_in(&dir, fp)
}

/// 在指定目录加载缓存（独立函数便于测试注入临时目录）。
fn load_in(dir: &Path, fp: &str) -> Option<HashMap<String, String>> {
    // 文件缺失或内容损坏 → 视为无缓存。
    let content = std::fs::read_to_string(dir.join(CACHE_FILE_NAME)).ok()?;
    let entry: CacheEntry = serde_json::from_str(&content).ok()?;
    // 指纹不匹配（换了 MSYS2 目录 / MSYSTEM / shell / envFrom）→ 缓存无效。
    if entry.fingerprint != fp {
        return None;
    }
    if entry.vars.is_empty() {
        return None;
    }
    Some(entry.vars)
}

/// 保存缓存到默认目录（失败静默忽略）。
///
/// 参数：`fp` 指纹；`vars` 变量表。
pub fn save(fp: &str, vars: &HashMap<String, String>) {
    if vars.is_empty() {
        return;
    }
    if let Some(dir) = cache_dir() {
        save_in(&dir, fp, vars);
    }
}

/// 在指定目录保存缓存。
fn save_in(dir: &Path, fp: &str, vars: &HashMap<String, String>) {
    let entry = CacheEntry {
        fingerprint: fp.to_string(),
        captured_at: now_rfc3339(),
        vars: vars.clone(),
    };
    let Ok(json) = serde_json::to_string_pretty(&entry) else {
        return;
    };
    write_atomic(dir, CACHE_FILE_NAME, &json);
}

/// 原子写入：先写 `.tmp` 再 rename，避免进程被杀时留下半写文件。
fn write_atomic(dir: &Path, file_name: &str, content: &str) {
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let final_path = dir.join(file_name);
    let tmp_path = dir.join(format!("{file_name}.tmp"));
    if std::fs::write(&tmp_path, content).is_ok() {
        if std::fs::rename(&tmp_path, &final_path).is_err() {
            let _ = std::fs::remove_file(&tmp_path);
        }
    }
}

/// 当前时间的 RFC 3339 表示（仅用于缓存里的人工排查）。
fn now_rfc3339() -> String {
    // 不引入 chrono 依赖：用 Unix 秒数即可满足"记录时间"的用途。
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => format!("unix:{}", d.as_secs()),
        Err(_) => "unknown".to_string(),
    }
}

/// 执行一次捕获：`<shell> -c env`，解析出变量表。
///
/// 参数：
/// - `msys2_root`：MSYS2 安装根目录。
/// - `msystem`：`MSYSTEM` 环境变量值。
/// - `env_from`：从哪个 shell 捕获。
///
/// 返回：`Ok(变量表)`；`envFrom` 为 `none` 时返回空表；shell 缺失、启动失败或
/// 非零退出时返回 `Err(原因)`（调用方通常降级为空表并记一行日志）。
///
/// 说明：这里刻意用 `-c`（非 login）而不是 `-lc`。实测 fish 在非 login 模式下
/// 依然会 source `~/.config/fish/config.fish`，bash 的 `-c` 也继承了父进程的
/// Windows PATH（由 MSYS2 运行时转换），因此 `-c` 就够用且快约 3 倍
/// （fish: 405ms vs 1113ms；bash: 63ms vs 705ms）。
pub fn capture(msys2_root: &Path, msystem: &str, env_from: EnvFrom) -> Result<LoginEnv, String> {
    // 第一步：none 表示不捕获，直接用进程自身环境。
    let Some(relative) = env_from.relative_exe() else {
        return Ok(LoginEnv::empty());
    };

    // 第二步：shell 必须存在，否则捕获无法进行。
    let exe = msys2_root.join(relative);
    if !exe.is_file() {
        return Err(format!(
            "{} not found for env capture: {}",
            env_from.as_str(),
            exe.display()
        ));
    }

    // 第三步：执行 `<shell> -c env`。设置 MSYSTEM 让 shell 走正确的
    // MINGW64 前缀；MSYS2_PATH_TYPE=inherit 保证 Windows PATH 被计入。
    let output = std::process::Command::new(&exe)
        .args(["-c", "env"])
        .env("MSYSTEM", msystem)
        .env("MSYS2_PATH_TYPE", "inherit")
        .env("MSYS2_NOSTART", "yes")
        .env("LANG", "C.UTF-8")
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| format!("failed to run `{} -c env`: {error}", env_from.as_str()))?;

    // 第四步：非零退出视为捕获失败（保留 stderr 摘要便于排查）。
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let summary: String = stderr.trim().chars().take(200).collect();
        return Err(format!(
            "`{} -c env` exited with {:?}: {summary}",
            env_from.as_str(),
            output.status.code()
        ));
    }

    // 第五步：解析输出。
    let stdout = String::from_utf8_lossy(&output.stdout);
    let vars = parse_env_output(&stdout);
    if vars.is_empty() {
        return Err(format!("`{} -c env` produced no variables", env_from.as_str()));
    }
    Ok(LoginEnv::from_vars(vars))
}

/// 加载或捕获环境变量：先查磁盘缓存，未命中则捕获并写回。
///
/// 参数：
/// - `msys2_root`：MSYS2 安装根目录。
/// - `msystem`：`MSYSTEM` 值。
/// - `shell`：bash 可执行文件路径（参与指纹）。
/// - `env_from`：环境来源。
///
/// 返回：环境变量表（`envFrom: none` 或捕获失败时为空表，**不报错**）。
pub fn load_or_capture(
    msys2_root: &Path,
    msystem: &str,
    shell: &Path,
    env_from: EnvFrom,
) -> LoginEnv {
    // 第一步：none 直接返回空表，连指纹都不用算。
    if env_from == EnvFrom::None {
        return LoginEnv::empty();
    }
    let fp = fingerprint(&msys2_root.display().to_string(), msystem, &shell.display().to_string(), env_from);

    // 第二步：磁盘缓存命中 → 毫秒级返回。
    if let Some(vars) = load(&fp) {
        return LoginEnv::from_vars(vars);
    }

    // 第三步：未命中 → 捕获一次并写回缓存；失败降级为空表。
    match capture(msys2_root, msystem, env_from) {
        Ok(env) => {
            save(&fp, env.vars());
            env
        }
        Err(reason) => {
            eprintln!("rpi-opencode: msys2-bash env capture skipped: {reason}");
            LoginEnv::empty()
        }
    }
}

/// 捕获超时（秒），供启动诊断日志使用。
pub fn capture_timeout_seconds() -> u64 {
    CAPTURE_TIMEOUT_SECONDS
}

// ===========================================================================
// 测试
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// 创建进程内唯一的临时目录（测试结束后由操作系统清理）。
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("rpi-msys2-bash-env-tests")
            .join(format!("{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn parse_reads_simple_pairs() {
        let vars = parse_env_output("HOME=/home/u\nPATH=/usr/bin:/bin\nLANG=zh_CN.UTF-8\n");
        assert_eq!(vars.get("HOME").map(String::as_str), Some("/home/u"));
        assert_eq!(vars.get("PATH").map(String::as_str), Some("/usr/bin:/bin"));
        assert_eq!(vars.len(), 3);
    }

    #[test]
    fn parse_keeps_equals_in_value() {
        // 只按第一个 '=' 切分，否则 --flag=value 这类值会被截断。
        let vars = parse_env_output("OPTS=--flag=value\n");
        assert_eq!(vars.get("OPTS").map(String::as_str), Some("--flag=value"));
    }

    #[test]
    fn parse_skips_empty_key_and_control_chars() {
        let vars = parse_env_output("=no_key\nVALID=yes\nBAD\u{1}KEY=x\n");
        assert_eq!(vars.len(), 1);
        assert_eq!(vars.get("VALID").map(String::as_str), Some("yes"));
    }

    #[test]
    fn parse_accepts_empty_value() {
        let vars = parse_env_output("EMPTY=\n");
        assert_eq!(vars.get("EMPTY").map(String::as_str), Some(""));
    }

    #[test]
    fn env_from_parses_known_values() {
        assert_eq!(EnvFrom::parse("fish"), EnvFrom::Fish);
        assert_eq!(EnvFrom::parse("FISH"), EnvFrom::Fish);
        assert_eq!(EnvFrom::parse("none"), EnvFrom::None);
        assert_eq!(EnvFrom::parse("bash"), EnvFrom::Bash);
        // 未知值回落到 bash（而不是静默关掉捕获）。
        assert_eq!(EnvFrom::parse("zsh"), EnvFrom::Bash);
    }

    #[test]
    fn fingerprint_changes_with_every_field() {
        let base = fingerprint("C:\\msys64", "MINGW64", "usr/bin/bash.exe", EnvFrom::Fish);
        assert_ne!(
            base,
            fingerprint("D:\\msys64", "MINGW64", "usr/bin/bash.exe", EnvFrom::Fish)
        );
        assert_ne!(
            base,
            fingerprint("C:\\msys64", "MSYS", "usr/bin/bash.exe", EnvFrom::Fish)
        );
        assert_ne!(
            base,
            fingerprint("C:\\msys64", "MINGW64", "usr/bin/sh.exe", EnvFrom::Fish)
        );
        assert_ne!(
            base,
            fingerprint("C:\\msys64", "MINGW64", "usr/bin/bash.exe", EnvFrom::Bash)
        );
    }

    #[test]
    fn cache_round_trips() {
        let dir = temp_dir("roundtrip");
        let fp = fingerprint("C:\\msys64", "MINGW64", "bash", EnvFrom::Fish);
        let mut vars = HashMap::new();
        vars.insert("PUB_CACHE".to_string(), "C:\\PubCache".to_string());
        save_in(&dir, &fp, &vars);

        let loaded = load_in(&dir, &fp).expect("cache hit");
        assert_eq!(loaded.get("PUB_CACHE").map(String::as_str), Some("C:\\PubCache"));
    }

    #[test]
    fn cache_rejects_fingerprint_mismatch() {
        let dir = temp_dir("mismatch");
        let old = fingerprint("C:\\msys64", "MINGW64", "bash", EnvFrom::Bash);
        let new = fingerprint("C:\\msys64", "MINGW64", "bash", EnvFrom::Fish);
        let mut vars = HashMap::new();
        vars.insert("A".to_string(), "1".to_string());
        save_in(&dir, &old, &vars);
        assert!(load_in(&dir, &new).is_none());
    }

    #[test]
    fn cache_rejects_missing_and_corrupt() {
        let dir = temp_dir("corrupt");
        let fp = "x";
        assert!(load_in(&dir, fp).is_none());
        std::fs::write(dir.join(CACHE_FILE_NAME), "{ not json").expect("write");
        assert!(load_in(&dir, fp).is_none());
    }

    #[test]
    fn cache_does_not_write_empty_tables() {
        let dir = temp_dir("empty");
        save_in(&dir, "x", &HashMap::new());
        // 空表被写入后，读取侧也要拒绝（否则会以为"有缓存"而跳过捕获）。
        assert!(load_in(&dir, "x").is_none());
    }

    #[test]
    fn write_atomic_leaves_no_tmp_file() {
        let dir = temp_dir("atomic");
        write_atomic(&dir, "t.json", "hello");
        assert_eq!(std::fs::read_to_string(dir.join("t.json")).unwrap(), "hello");
        assert!(!dir.join("t.json.tmp").exists());
    }

    #[test]
    fn empty_env_reports_empty() {
        // is_empty 是判断“是否需要注入”的依据，空表必须如实报告。
        assert!(LoginEnv::empty().is_empty());
        assert_eq!(LoginEnv::empty().len(), 0);

        let mut vars = HashMap::new();
        vars.insert("A".to_string(), "1".to_string());
        assert!(!LoginEnv::from_vars(vars).is_empty());
    }

    #[test]
    fn capture_returns_empty_for_none_source() {
        // none 不应触碰文件系统，直接返回空表。
        let env = capture(Path::new("C:\\definitely-not-here-9f3a"), "MINGW64", EnvFrom::None)
            .expect("none never fails");
        assert!(env.is_empty());
    }

    #[test]
    fn capture_reports_missing_shell() {
        let err = capture(Path::new("C:\\definitely-not-here-9f3a"), "MINGW64", EnvFrom::Fish)
            .expect_err("missing shell must fail");
        assert!(err.contains("not found"), "unexpected message: {err}");
    }
}
