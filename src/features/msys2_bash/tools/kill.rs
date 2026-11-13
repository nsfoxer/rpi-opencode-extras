//! `kill_cmd` 工具：杀掉运行中 job 的整个进程树。
//!
//! 幂等：job 已结束时返回 no-op（不报错）。被杀后 job 仍留在 manager 里，
//! 因此被杀前的部分输出仍可用 `get_cmd_result` 取回。

use crate::features::msys2_bash::job::{self, JobStatus};
use crate::features::msys2_bash::tools::result::{
    error_result, ok_result, parse_params, required_id,
};
use serde_json::json;

/// 解析参数并执行 kill。
///
/// 参数：`raw` 宿主传入的参数 JSON。
/// 返回：
/// - `Ok(json)`：成功结果（含 no-op 情形）。
/// - `Err(reason)`：参数解析失败（由调用方转成错误结果）。
pub fn run(raw: &str) -> Result<String, String> {
    let params = parse_params(raw)?;
    let id = required_id(&params)?;

    let Some(manager) = job::manager() else {
        return Ok(error_result("msys2-bash is not initialized"));
    };
    let mut manager = manager.lock().unwrap_or_else(|p| p.into_inner());

    match manager.kill(id) {
        // 刚杀掉：按契约返回 killed。
        Ok(Some(status)) => Ok(ok_result(
            &format!("killed job {id}"),
            json!({ "id": id, "status": status.as_str() }),
        )),
        // 已结束：幂等 no-op，用当前真实状态回报，而不是谎报 killed。
        Ok(None) => {
            let status = manager
                .get(id)
                .map(|job| job.status())
                .unwrap_or(JobStatus::Completed);
            Ok(ok_result(
                &format!("job {id} already finished ({}), nothing to kill", status.as_str()),
                json!({ "id": id, "status": status.as_str(), "noop": true }),
            ))
        }
        // id 不存在：交给模型看清错误。
        Err(reason) => Ok(error_result(&reason)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bad_id_is_reported_without_touching_any_job() {
        // 非法参数在锁外就被拦住：无论结果包装在哪一层，文本里必须有原因。
        match run(r#"{"id": 0}"#) {
            Ok(output) => assert!(output.contains("must be >= 1"), "{output}"),
            Err(reason) => assert!(reason.contains("must be >= 1"), "{reason}"),
        }

        match run("{}") {
            Ok(output) => assert!(output.contains("missing or invalid `id`"), "{output}"),
            Err(reason) => assert!(reason.contains("missing or invalid `id`"), "{reason}"),
        }
    }

    #[test]
    fn non_json_params_are_errors() {
        assert!(run("not json").is_err());
    }

    #[test]
    fn unknown_id_reports_an_error_result() {
        // 未初始化或 id 不存在都必须返回结构化错误，而不是 panic。
        let output = run(r#"{"id": 99999}"#).expect("ok");
        assert!(output.contains("error"), "{output}");
    }
}
