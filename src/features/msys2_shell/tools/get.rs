//! `get_cmd_result` 工具：取回某个 job 的输出与状态，必要时阻塞等待。
//!
//! 与 `bash` 共用同一套等待语义：`waitSeconds` 只决定本次等多久（夹紧 1-120），
//! 命令的总截止在创建时已固定，这里不会再延长。

use crate::features::msys2_shell::config::BashConfig;
use crate::features::msys2_shell::job::{self, JobStatus};
use crate::features::msys2_shell::tools::result::{
    error_result, int_field, ok_result, parse_params, required_id,
};
use crate::features::msys2_shell::tools::{wait_for, WAIT_SECONDS_RANGE};
use serde_json::json;
use std::sync::atomic::AtomicBool;

/// 解析参数并查询 job。
///
/// 参数：
/// - `raw`：宿主传入的参数 JSON。
/// - `config`：已校验配置（`waitSeconds` 默认值来自它）。
/// - `cancelled`：宿主取消标记（只影响等待，不杀进程）。
///
/// 返回：`Ok(json)` 或 `Err(reason)`（参数不是合法对象）。
pub fn run(
    raw: &str,
    config: &BashConfig,
    cancelled: &AtomicBool,
) -> Result<String, String> {
    let params = parse_params(raw)?;
    let id = required_id(&params)?;
    let wait_seconds = int_field(
        &params,
        "waitSeconds",
        WAIT_SECONDS_RANGE,
        config.wait_seconds,
    );

    let Some(manager) = job::manager() else {
        return Ok(error_result("msys2-bash is not initialized"));
    };

    // 第一步：取 job 句柄后立刻释放 manager 锁，避免等待期间阻塞其他工具调用。
    let job = {
        let mut manager = manager.lock().unwrap_or_else(|p| p.into_inner());
        manager.get(id)
    };
    let Some(job) = job else {
        return Ok(error_result(&format!("no such job: {id}")));
    };

    // 第二步：等待到终态或截止（与 bash 完全相同的语义）。
    match wait_for(&job, wait_seconds, cancelled) {
        Some(view) => Ok(ok_result(&render_text(&view), details_for(&view))),
        None => Ok(ok_result(
            &format!("still running; call get_cmd_result with id {id}"),
            json!({ "id": id, "status": JobStatus::Running.as_str() }),
        )),
    }
}

/// 渲染给模型看的正文。
///
/// 参数：`view` 终态 job 快照。
/// 返回：优先用命令输出；**输出为空时用状态摘要**——否则模型看不到 stauts/exitCode
/// （`details` 不展示给模型），只能拿到空字符串。
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

/// 构造终态 job 的 details。
///
/// 参数：`view` job 快照。
/// 返回：含 id / status / exitCode（可选）/ truncated（仅有截断时）/ error（仅失败时）。
fn details_for(view: &job::JobView) -> serde_json::Value {
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
    details
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::msys2_shell::config::{
        BashConfig, DEFAULT_MAX_JOBS, DEFAULT_MAX_OUTPUT_BYTES, DEFAULT_TIMEOUT_SECONDS,
    };
    use crate::features::msys2_shell::env_cache::{EnvFrom, LoginEnv};
    use std::path::PathBuf;

    fn stub_config() -> BashConfig {
        BashConfig {
            msys2_root: PathBuf::from(r"C:\msys64"),
            msystem: "MINGW64".to_string(),
            shell: PathBuf::from(r"C:\msys64\usr\bin\bash.exe"),
            wait_seconds: 30,
            default_timeout_seconds: DEFAULT_TIMEOUT_SECONDS,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            max_jobs: DEFAULT_MAX_JOBS,
            blacklist: Vec::new(),
            login_shell: false,
            env_from: EnvFrom::Bash,
            login_env: LoginEnv::empty(),
            source: PathBuf::from("stub.json"),
        }
    }

    #[test]
    fn missing_id_is_reported() {
        // `id` 缺失时必须在碰任何全局状态前就被拦住。
        let cancelled = AtomicBool::new(false);
        let result = run("{}", &stub_config(), &cancelled);
        match result {
            // 未安装配置时会先返回成功但内容是错误 JSON。
            Ok(output) => assert!(output.contains("missing or invalid `id`"), "{output}"),
            Err(reason) => assert!(reason.contains("missing or invalid `id`"), "{reason}"),
        }
    }

    #[test]
    fn non_json_params_are_errors() {
        let cancelled = AtomicBool::new(false);
        assert!(run("nope", &stub_config(), &cancelled).is_err());
    }

    #[test]
    fn unknown_id_reports_error_result() {
        // manager 未初始化或 id 不存在都必须是结构化错误。
        let cancelled = AtomicBool::new(false);
        let output = run(r#"{"id": 424242}"#, &stub_config(), &cancelled).expect("ok");
        assert!(output.contains("error"), "{output}");
    }

    #[test]
    fn empty_output_falls_back_to_status_summary() {
        // sleep 类命令没有输出：模型必须能从 text 里看到状态与退出码。
        let view = job::JobView {
            id: 1,
            command: "sleep 35".to_string(),
            status: JobStatus::Completed,
            exit_code: Some(0),
            started_at_millis: 0,
            elapsed_millis: 1000,
            output: String::new(),
            truncated: false,
            error: None,
        };
        assert_eq!(render_text(&view), "[completed] exit=0");

        let with_output = job::JobView {
            output: "hello\n".to_string(),
            ..view
        };
        assert_eq!(render_text(&with_output), "hello\n");
    }

    #[test]
    fn details_include_exit_code_only_when_present() {
        let mut view = job::JobView {
            id: 7,
            command: "echo".to_string(),
            status: JobStatus::Completed,
            exit_code: Some(0),
            started_at_millis: 0,
            elapsed_millis: 1,
            output: "hi".to_string(),
            truncated: false,
            error: None,
        };
        let details = details_for(&view);
        assert_eq!(details["exitCode"], 0);
        assert!(details.get("truncated").is_none());
        assert!(details.get("error").is_none());

        view.exit_code = None;
        view.status = JobStatus::Killed;
        let details = details_for(&view);
        assert!(details.get("exitCode").is_none());
        assert_eq!(details["status"], "killed");
    }
}
