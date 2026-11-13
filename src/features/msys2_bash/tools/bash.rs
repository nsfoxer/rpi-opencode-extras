//! `bash` 工具：在 MSYS2 (MINGW64) 里执行命令，阻塞至多 `waitSeconds` 秒。
//!
//! 语义要点：
//! - `waitSeconds`（默认取配置，夹紧 1-120）只决定**这次调用等多久**；
//! - `timeout`（默认取配置，夹紧 1-1800）是**命令总截止**，到点杀进程树；
//! - 等不到就返回 `{status:'running', id}`，后续用 `get_cmd_result` 取结果；
//! - `cancel` 只让本次调用提前返回，命令继续跑。

use crate::features::msys2_bash::config::{check_blacklist, BashConfig};
use crate::features::msys2_bash::job::{self, resolve_cwd, JobStatus};
use crate::features::msys2_bash::tools::result::{
    error_result, host_cwd, int_field, ok_result, optional_str, parse_params, required_str,
};
use crate::features::msys2_bash::tools::{wait_for, TIMEOUT_SECONDS_RANGE, WAIT_SECONDS_RANGE};
use serde_json::json;
use std::sync::atomic::AtomicBool;

/// 解析 `bash` 参数并提交 job。
///
/// 参数：
/// - `raw`：宿主传入的参数 JSON。
/// - `config`：已校验配置。
/// - `cancelled`：宿主取消标记（只影响等待，不杀进程）。
///
/// 返回：
/// - `Ok(Ok(json))`：参数合法，job 已创建（可能是 running 或已完成）。
/// - `Ok(Err(json))`：参数或环境问题，返回结构化错误。
/// - `Err(reason)`：参数解析失败（由调用方转成错误结果）。
pub fn submit(
    raw: &str,
    config: &BashConfig,
    cancelled: &AtomicBool,
) -> Result<Result<String, String>, String> {
    let params = parse_params(raw)?;

    // 第一步：command 必填。
    let command = required_str(&params, "command")?;

    // 第二步：黑名单检查（命中即拒绝，不创建 job）。
    if let Some(pattern) = check_blacklist(&config.blacklist, &command) {
        return Ok(Err(error_result(&format!(
            "command rejected by blacklist rule: {pattern}"
        ))));
    }

    // 第三步：解析 working directory（工具参数 > 宿主 __rpi.cwd > 进程当前目录）。
    let requested = optional_str(&params, "cwd");
    let host = host_cwd(&params);
    let process_cwd = std::env::current_dir().unwrap_or_else(|_| config.msys2_root.clone());
    let cwd = match resolve_cwd(
        requested.as_deref(),
        host.as_deref(),
        &process_cwd,
        &config.msys2_root,
    ) {
        Ok(path) => path,
        Err(reason) => return Ok(Err(error_result(&reason))),
    };

    // 第四步：夹紧两个时间参数。waitSeconds 控制本次等待，timeout 控制命令总截止。
    let wait_seconds = int_field(
        &params,
        "waitSeconds",
        WAIT_SECONDS_RANGE,
        config.wait_seconds,
    );
    let timeout_seconds = int_field(
        &params,
        "timeout",
        TIMEOUT_SECONDS_RANGE,
        config.default_timeout_seconds,
    );

    // 第五步：提交 job。timeout 按本次请求值传入（不污染配置默认值）。
    let Some(manager) = job::manager() else {
        return Ok(Err(error_result("msys2-bash is not initialized")));
    };
    let (job, id) = {
        let mut manager = manager.lock().unwrap_or_else(|p| p.into_inner());
        manager.spawn(config, &command, &cwd, timeout_seconds)
    };

    Ok(Ok(render_wait(&job, id, wait_seconds, cancelled)))
}

/// 等待 job 到终态或截止，并渲染响应。
///
/// 参数：`job` 目标 job；`id` job id；`wait_seconds` 本次最多等待秒数；
/// `cancelled` 宿主取消标记。
/// 返回：完成 → 输出 + details；运行中 → 提示用 get_cmd_result。
pub fn render_wait(
    job: &job::Job,
    id: u64,
    wait_seconds: u64,
    cancelled: &AtomicBool,
) -> String {
    match wait_for(job, wait_seconds, cancelled) {
        Some(view) if view.status == JobStatus::TimedOut => {
            // 总截止到点：契约要求返回 {"error":"timeout","id":N}。
            json!({ "error": "timeout", "id": view.id }).to_string()
        }
        Some(view) => {
            let mut details = json!({
                "id": view.id,
                "status": view.status.as_str(),
            });
            if let Some(object) = details.as_object_mut() {
                if let Some(code) = view.exit_code {
                    object.insert("exitCode".to_string(), json!(code));
                }
                if view.truncated {
                    object.insert("truncated".to_string(), json!(true));
                }
                if let Some(error) = &view.error {
                    object.insert("error".to_string(), json!(error));
                }
            }
            ok_result(&render_text(&view), details)
        }
        None => {
            // 仍在运行（或调用方取消了等待）：两者都返回同一个可继续操作的结果。
            ok_result(
                &format!("running; use get_cmd_result with id {id}"),
                json!({ "id": id, "status": JobStatus::Running.as_str() }),
            )
        }
    }
}

/// 渲染给模型看的正文。
///
/// 参数：`view` 终态 job 快照。
/// 返回：优先用命令输出；输出为空时用状态摘要（否则模型只能拿到空字符串，
/// 看不到 `exitCode`——`details` 不展示给模型）。
fn render_text(view: &job::JobView) -> String {
    if !view.output.trim().is_empty() {
        return view.output.clone();
    }
    let mut summary = format!("[{}]", view.status.as_str());
    if let Some(code) = view.exit_code {
        summary.push_str(&format!(" exit={code}"));
    }
    if let Some(error) = &view.error {
        summary.push_str(&format!(" {error}"));
    }
    summary
}

/// 判断路径是否可用作 cwd（供测试与诊断复用）。
#[cfg(test)]
fn cwd_is_dir(path: &std::path::Path) -> bool {
    path.is_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_command_is_reported() {
        let params = parse_params(r#"{"waitSeconds": 5}"#).expect("ok");
        assert!(required_str(&params, "command").is_err());
    }
    #[test]
    fn blacklisted_command_is_rejected_before_spawning() {
        // 直接验证黑名单分支的判定逻辑（不启动真实进程）。
        let patterns = vec![(
            r"(?i)shutdown".to_string(),
            regex::Regex::new(r"(?i)shutdown").expect("regex"),
        )];
        assert!(check_blacklist(&patterns, "shutdown /s /t 0").is_some());
        assert!(check_blacklist(&patterns, "echo shutdown").is_some());
        assert!(check_blacklist(&patterns, "echo hello").is_none());
    }

    #[test]
    fn wait_and_timeout_are_clamped_independently() {
        // waitSeconds 上限 120，timeout 上限 1800：两者不能互相影响。
        let params = parse_params(r#"{"command":"echo x","waitSeconds":999,"timeout":99999}"#)
            .expect("ok");
        let wait = int_field(&params, "waitSeconds", WAIT_SECONDS_RANGE, 30);
        let total = int_field(&params, "timeout", TIMEOUT_SECONDS_RANGE, 900);
        assert_eq!(wait, 120, "waitSeconds must clamp to 120");
        assert_eq!(total, 1800, "timeout must clamp to 1800");
        assert_ne!(wait, total, "30s wait must never become the total timeout");
    }

    #[test]
    fn empty_output_falls_back_to_status_summary() {
        let view = job::JobView {
            id: 1,
            command: "sleep 40".to_string(),
            status: JobStatus::Completed,
            exit_code: Some(0),
            started_at_millis: 0,
            elapsed_millis: 40000,
            output: String::new(),
            truncated: false,
            error: None,
        };
        assert_eq!(render_text(&view), "[completed] exit=0");
    }

    #[test]
    fn cwd_helper_matches_std_behaviour() {
        let dir = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf))
            .expect("test dir");
        assert!(cwd_is_dir(&dir));
        assert!(!cwd_is_dir(std::path::Path::new(r"C:\definitely-not-here-9f3a")));
    }
}
