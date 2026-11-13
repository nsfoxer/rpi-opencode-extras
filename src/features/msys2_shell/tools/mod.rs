//! 四个工具的公共定义：名称、JSON Schema、以及「等待到终态或截止」的共享逻辑。
//!
//! 三个新工具加上 `bash`（覆盖宿主内置 bash）都在这里声明 schema，注册入口在
//! [`super::register`]。工具名同时用于 `register_tool_prompt`。

pub mod bash;
pub mod get;
pub mod kill;
pub mod list;
pub mod result;

use crate::features::msys2_shell::config::BashConfig;
use std::sync::{Arc, Mutex};

/// `waitSeconds` 允许范围（秒）。
pub const WAIT_SECONDS_RANGE: (u64, u64) = crate::features::msys2_shell::config::WAIT_SECONDS_RANGE;

/// `timeout` 允许范围（秒）。
pub const TIMEOUT_SECONDS_RANGE: (u64, u64) =
    crate::features::msys2_shell::config::TIMEOUT_SECONDS_RANGE;

/// 已安装的配置（注册期写入一次，之后只读）。
static CONFIG: Mutex<Option<Arc<BashConfig>>> = Mutex::new(None);

/// 安装配置。
///
/// 参数：`config` 已校验配置。
/// 返回：`Ok(())` 首次安装；`Err(())` 已安装过（重复注册，调用方应放弃）。
pub fn install_config(config: Arc<BashConfig>) -> Result<(), ()> {
    let mut slot = CONFIG.lock().unwrap_or_else(|p| p.into_inner());
    if slot.is_some() {
        return Err(());
    }
    *slot = Some(config);
    Ok(())
}

/// 读取已安装的配置。
///
/// 返回：`Some(config)` 表示已启用；`None` 表示未启用。
pub fn installed_config() -> Option<Arc<BashConfig>> {
    CONFIG
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .map(Arc::clone)
}

/// `bash` 工具名（与宿主内置工具同名 → 覆盖它）。
pub const BASH: &str = "bash";

/// 查询命令结果。
pub const GET_CMD_RESULT: &str = "get_cmd_result";

/// 杀掉运行中的命令。
pub const KILL_CMD: &str = "kill_cmd";

/// 列出所有命令。
pub const LIST_CMDS: &str = "list_cmds";

/// `bash` 的工具描述。
pub const BASH_DESCRIPTION: &str = "Execute a shell command in MSYS2 (MINGW64). Waits up to 30s: \
returns the finished result, or {status:'running', id} if still running (then use \
get_cmd_result). This is the only way to run commands.";

/// `get_cmd_result` 的工具描述。
pub const GET_CMD_RESULT_DESCRIPTION: &str =
    "Fetch the output and status of a command started by bash. Waits up to waitSeconds.";

/// `kill_cmd` 的工具描述。
pub const KILL_CMD_DESCRIPTION: &str =
    "Kill a running command (whole process tree). No-op if the command already finished.";

/// `list_cmds` 的工具描述。
pub const LIST_CMDS_DESCRIPTION: &str =
    "List running commands and the most recent finished ones.";

/// `bash` 的参数 schema。
pub const BASH_PARAMETERS: &str = r#"{
  "type": "object",
  "properties": {
    "command":     { "type": "string" },
    "waitSeconds": { "type": "integer", "description": "默认 30，范围 1-120" },
    "timeout":     { "type": "number",  "description": "命令总截止，默认 900，最大 1800" },
    "cwd":         { "type": "string" }
  },
  "required": ["command"]
}"#;

/// `get_cmd_result` 的参数 schema。
pub const GET_CMD_RESULT_PARAMETERS: &str = r#"{
  "type": "object",
  "properties": {
    "id":          { "type": "integer" },
    "waitSeconds": { "type": "integer", "description": "默认 30，范围 1-120" }
  },
  "required": ["id"]
}"#;

/// `kill_cmd` 的参数 schema。
pub const KILL_CMD_PARAMETERS: &str = r#"{
  "type": "object",
  "properties": {
    "id": { "type": "integer" }
  },
  "required": ["id"]
}"#;

/// `list_cmds` 的参数 schema（无参数）。
pub const LIST_CMDS_PARAMETERS: &str = r#"{ "type": "object", "properties": {} }"#;

/// `bash` 的提示词元数据（仅在工具真正注册时下发给宿主）。
pub const BASH_PROMPT_METADATA: &str = r#"{
  "promptSnippet": "Runs commands in MSYS2 (MINGW64).",
  "promptGuidelines": [
    "Long commands return {status:'running', id}; use get_cmd_result.",
    "Use kill_cmd to stop a running job."
  ]
}"#;

/// 在 `wait_seconds` 内轮询 job，直到结束、超时，或被要求取消等待。
///
/// 参数：
/// - `job`：目标 job。
/// - `wait_seconds`：最多阻塞多久（调用方已夹紧到 1-120）。
/// - `cancelled`：宿主 `cancel` 置位的取消标记。**只影响等待**，不杀进程。
///
/// 返回：`Some(终态快照)` 表示等到结束；`None` 表示仍在运行或已取消等待。
///
/// 轮询间隔从 5ms 逐步放宽到 50ms，避免长任务把 CPU 打满。
pub fn wait_for(
    job: &super::job::Job,
    wait_seconds: u64,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Option<super::job::JobView> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(wait_seconds);
    let mut interval = std::time::Duration::from_millis(5);

    loop {
        // 第一步：先看状态，已结束就立刻返回（短命令零等待）。
        let view = job.view();
        if view.status.is_finished() {
            return Some(view);
        }

        // 第二步：宿主取消等待 → 直接返回 None，命令继续跑，之后仍可取结果。
        if cancelled.load(std::sync::atomic::Ordering::SeqCst) || job.should_stop_waiting() {
            return None;
        }

        // 第三步：到点返回 None。
        let now = std::time::Instant::now();
        if now >= deadline {
            return None;
        }

        // 第四步：按当前间隔睡一小段，然后逐步放宽（5→10→20→50ms 封顶）。
        let remaining = deadline.saturating_duration_since(now);
        std::thread::sleep(interval.min(remaining));
        interval = (interval * 2).min(std::time::Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_names_are_distinct() {
        let names = [BASH, GET_CMD_RESULT, KILL_CMD, LIST_CMDS];
        let mut unique = names.to_vec();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), names.len());
    }

    #[test]
    fn every_parameters_blob_is_valid_json_object() {
        for schema in [
            BASH_PARAMETERS,
            GET_CMD_RESULT_PARAMETERS,
            KILL_CMD_PARAMETERS,
            LIST_CMDS_PARAMETERS,
        ] {
            let value: serde_json::Value =
                serde_json::from_str(schema).expect("schema must be valid JSON");
            assert!(value.is_object(), "schema must be an object: {schema}");
            assert_eq!(value["type"], "object");
        }
    }

    #[test]
    fn bash_schema_requires_command_and_documents_ranges() {
        let value: serde_json::Value =
            serde_json::from_str(BASH_PARAMETERS).expect("valid json");
        assert_eq!(value["required"][0], "command");
        assert_eq!(value["properties"]["waitSeconds"]["type"], "integer");
        assert_eq!(value["properties"]["timeout"]["type"], "number");
        assert_eq!(value["properties"]["cwd"]["type"], "string");
    }

    #[test]
    fn prompt_metadata_is_valid_and_mentions_both_tools() {
        let value: serde_json::Value =
            serde_json::from_str(BASH_PROMPT_METADATA).expect("valid json");
        let snippet = value["promptSnippet"].as_str().expect("snippet");
        assert!(snippet.contains("MSYS2"));
        let guidelines = value["promptGuidelines"]
            .as_array()
            .expect("guidelines array");
        assert_eq!(guidelines.len(), 2);
        let joined = guidelines
            .iter()
            .filter_map(|item| item.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(joined.contains("get_cmd_result"));
        assert!(joined.contains("kill_cmd"));
    }
}
