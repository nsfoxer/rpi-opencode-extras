//! `list_cmds` 工具：列出运行中的命令与最近的已结束命令。
//!
//! 返回每条记录的 id / status / command / startedAt / exitCode；命令文本会压平
//! 换行并截断到合理长度，避免长命令把列表撑爆。

use crate::features::msys2_bash::job;
use crate::features::msys2_bash::tools::result::{error_result, ok_result, parse_params};
use serde_json::json;

/// 单条命令在列表里的最大展示字符数。
const MAX_COMMAND_CHARS: usize = 200;

/// 解析参数并生成列表。
///
/// 参数：`raw` 宿主传入的参数 JSON（本工具无参数，但仍要容忍 `__rpi`）。
/// 返回：`Ok(json)` 或 `Err(reason)`（参数不是对象）。
pub fn run(raw: &str) -> Result<String, String> {
    // 第一步：即使无参数也要校验，防止宿主传入非对象把后续逻辑搞乱。
    let _params = parse_params(raw)?;

    let Some(manager) = job::manager() else {
        return Ok(error_result("msys2-bash is not initialized"));
    };
    let views = {
        let mut manager = manager.lock().unwrap_or_else(|p| p.into_inner());
        manager.list()
    };

    // 第二步：转成契约要求的字段，命令文本压平 + 截断。
    let items: Vec<serde_json::Value> = views
        .iter()
        .map(|view| {
            json!({
                "id": view.id,
                "status": view.status.as_str(),
                "command": flatten_command(&view.command),
                "startedAt": view.started_at_millis,
                "exitCode": view.exit_code,
            })
        })
        .collect();

    let running = views
        .iter()
        .filter(|view| !view.status.is_finished())
        .count();

    Ok(ok_result(
        &render_text(&items, running, views.len()),
        json!({ "jobs": items }),
    ))
}

/// 把命令文本压平成一行并截断。
///
/// 参数：`command` 原始命令。
/// 返回：单行、最长 [`MAX_COMMAND_CHARS`] 字符（超出补 `…`）。
fn flatten_command(command: &str) -> String {
    let flattened = command.split_whitespace().collect::<Vec<_>>().join(" ");
    if flattened.chars().count() <= MAX_COMMAND_CHARS {
        return flattened;
    }
    let mut truncated: String = flattened.chars().take(MAX_COMMAND_CHARS).collect();
    truncated.push('…');
    truncated
}

/// 渲染给模型看的摘要文本。
///
/// 参数：`items` 已构造的 job 列表；`running` 运行中条数；`total` 总条数。
/// 返回：多行文本，一行一条。
fn render_text(items: &[serde_json::Value], running: usize, total: usize) -> String {
    if items.is_empty() {
        return "no commands yet".to_string();
    }
    let mut lines = Vec::with_capacity(items.len() + 1);
    lines.push(format!("{total} command(s), {running} running:"));
    for item in items {
        let id = item["id"].as_u64().unwrap_or(0);
        let status = item["status"].as_str().unwrap_or("unknown");
        let command = item["command"].as_str().unwrap_or("");
        match item["exitCode"].as_i64() {
            Some(code) => lines.push(format!("#{id} {status} exit={code} {command}")),
            None => lines.push(format!("#{id} {status} {command}")),
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_is_flattened_to_one_line() {
        assert_eq!(flatten_command("echo a\n echo b\t echo c"), "echo a echo b echo c");
    }

    #[test]
    fn long_command_is_truncated_by_chars_not_bytes() {
        let long = "中".repeat(MAX_COMMAND_CHARS + 50);
        let flat = flatten_command(&long);
        assert_eq!(flat.chars().count(), MAX_COMMAND_CHARS + 1);
        assert!(flat.ends_with('…'));
    }

    #[test]
    fn empty_list_says_so() {
        assert_eq!(render_text(&[], 0, 0), "no commands yet");
    }

    #[test]
    fn render_marks_exit_code_when_present() {
        let items = vec![
            json!({"id":1,"status":"running","command":"sleep 9","exitCode":null}),
            json!({"id":2,"status":"completed","command":"echo a","exitCode":0}),
        ];
        let text = render_text(&items, 1, 2);
        assert!(text.starts_with("2 command(s), 1 running:"), "{text}");
        assert!(text.contains("#1 running sleep 9"), "{text}");
        assert!(text.contains("#2 completed exit=0 echo a"), "{text}");
    }

    #[test]
    fn non_object_params_are_rejected() {
        assert!(run("[]").is_err());
    }
}
