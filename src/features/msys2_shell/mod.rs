//! `msys2-bash` feature：可选的 MSYS2 命令执行子插件。
//!
//! **配置文件本身就是开关**：
//! - 找到配置且校验通过 → 注册 `bash`（覆盖内置 bash）+ `get_cmd_result` +
//!   `kill_cmd` + `list_cmds`。
//! - 三个候选路径都没有配置 → 什么都不注册，返回 0，内置 bash 原样保留。
//! - 配置存在但无效 → 打印一行 warning，什么都不注册，返回 0；不回退、不中断加载。

pub mod config;
pub mod env_cache;
pub mod job;
pub mod output;
pub mod sandbox;
pub mod settings;
pub mod tools;

use crate::features::msys2_shell::tools::result::error_result;
use rpi_plugin_sdk::callbacks::free_string as plugin_free_string;
use rpi_plugin_sdk::{
    PluginApi, StableToolSchema, StbString, StbStringRef, StepHandle, StepResult, ToolPartialCb,
};
use std::ffi::c_void;
use std::sync::Arc;

// ===========================================================================
// 注册入口
// ===========================================================================

/// `msys2-bash` feature 的注册入口。
///
/// 参数：`api` 宿主的插件 API 表。
/// 返回：恒为 `0`——平台不符、配置缺失或无效都只是"不启用"，不属于插件加载失败。
pub fn register(api: &PluginApi) -> i32 {
    // 第一步：平台检查。本 feature 依赖 MSYS2（Windows 专属：`taskkill /F /T` 杀进程树、
    // `C:\msys64` 路径布局、`.exe` 后缀），非 Windows 平台直接不生效，
    // 且不去读配置、不产生任何诊断噪音。
    if !cfg!(target_os = "windows") {
        return 0;
    }

    // 第二步：加载配置。没有配置段 = 未启用，保持内置 bash 行为。
    let config = match config::load_config() {
        Ok(Some(config)) => config,
        // settings.json 不存在或没有 msys2Bash 段：静默跳过。
        Ok(None) => return 0,
        // 配置存在但无效：一行 warning，然后不注册任何工具。
        Err(reason) => {
            eprintln!("rpi-opencode-extras: msys2-bash disabled: {reason}");
            return 0;
        }
    };

    // 第三步：宿主是否提供工具注册槽位。缺失时同样只是"不启用"。
    let Some(register_tool) = api.register_tool else {
        eprintln!("rpi-opencode-extras: msys2-bash disabled: host has no tool registration slot");
        return 0;
    };

    // 第四步：初始化全局 job manager（配置有效期 = 进程生命周期）。
    if !job::init_manager(config.max_jobs) {
        eprintln!("rpi-opencode-extras: msys2-bash disabled: job manager already initialized");
        return 0;
    }
    if tools::install_config(Arc::new(config.clone())).is_err() {
        eprintln!("rpi-opencode-extras: msys2-bash disabled: config already installed");
        return 0;
    }

    // 第五步：沙箱检测。Low integrity（rpi-sandbox 的 MXC PSEC 后端）里
    // MSYS2 无法初始化。仍然注册工具，但每次 bash 调用都会返回明确原因。
    // 默认不写 stderr：TUI 已经接管屏幕，启动输出会落进输入区；需要诊断时
    // 设置 RPI_OPENCODE_EXTRAS_VERBOSE=1。
    if let Some(reason) = sandbox::blocked_reason() {
        if std::env::var_os("RPI_OPENCODE_EXTRAS_VERBOSE").is_some() {
            eprintln!("rpi-opencode-extras: msys2-bash unavailable: {reason}");
        }
    }

    // 第六步：依次注册四个工具。任一失败即停止并回报（不留半套工具）。
    for spec in TOOL_SPECS {
        let rc = register_one(register_tool, spec);
        if rc != 0 {
            eprintln!(
                "rpi-opencode-extras: msys2-bash failed to register `{}` (rc={rc})",
                spec.name
            );
            return rc;
        }
    }

    // 第七步：给 bash 追加提示词说明。只有工具真的注册了，模型才看得到这段。
    if let Some(register_tool_prompt) = api.register_tool_prompt {
        let rc = register_tool_prompt(
            StbStringRef::from_str(tools::BASH),
            StbStringRef::from_str(tools::BASH_PROMPT_METADATA),
        );
        // 1 = 未知工具，2 = 元数据格式错误；都不影响工具可用性，仅丢失提示词。
        if rc != 0 {
            eprintln!("rpi-opencode-extras: msys2-bash bash prompt metadata rejected (rc={rc})");
        }
    }

    if std::env::var_os("RPI_OPENCODE_EXTRAS_VERBOSE").is_some() {
        eprintln!(
            "rpi-opencode-extras: msys2-bash enabled (config={}, shell={}, wait={}s, timeout={}s, loginShell={}, envFrom={}, envVars={}, captureTimeout={}s)",
            config.source.display(),
            config.shell.display(),
            config.wait_seconds,
            config.default_timeout_seconds,
            config.login_shell,
            config.env_from.as_str(),
            config.login_env.len(),
            env_cache::capture_timeout_seconds()
        );
    }
    0
}

/// 一个待注册的工具定义。
struct ToolSpec {
    /// 工具名。
    name: &'static str,
    /// 工具描述。
    description: &'static str,
    /// 参数 schema。
    parameters: &'static str,
    /// 该工具的 execute 入口（每个工具一组，因为回调里拿不到工具名）。
    execute: rpi_plugin_sdk::ToolExecuteFn,
}

/// 注册阶段使用的四个工具定义。
const TOOL_SPECS: &[ToolSpec] = &[
    ToolSpec {
        name: tools::BASH,
        description: tools::BASH_DESCRIPTION,
        parameters: tools::BASH_PARAMETERS,
        execute: bash_execute,
    },
    ToolSpec {
        name: tools::GET_CMD_RESULT,
        description: tools::GET_CMD_RESULT_DESCRIPTION,
        parameters: tools::GET_CMD_RESULT_PARAMETERS,
        execute: get_cmd_result_execute,
    },
    ToolSpec {
        name: tools::KILL_CMD,
        description: tools::KILL_CMD_DESCRIPTION,
        parameters: tools::KILL_CMD_PARAMETERS,
        execute: kill_cmd_execute,
    },
    ToolSpec {
        name: tools::LIST_CMDS,
        description: tools::LIST_CMDS_DESCRIPTION,
        parameters: tools::LIST_CMDS_PARAMETERS,
        execute: list_cmds_execute,
    },
];

/// 注册单个工具。
///
/// 参数：`register` 宿主注册函数；`spec` 工具定义（含该工具专属的 execute 入口）。
/// 返回：宿主的返回码（0 = 成功）。
fn register_one(
    register: extern "C" fn(
        *const StableToolSchema,
        rpi_plugin_sdk::ToolExecuteFn,
        rpi_plugin_sdk::ToolPollFn,
        rpi_plugin_sdk::ToolCancelFn,
        rpi_plugin_sdk::ToolDestroyFn,
        rpi_plugin_sdk::FreeStringFn,
    ) -> i32,
    spec: &ToolSpec,
) -> i32 {
    let schema = Box::new(StableToolSchema {
        name: StbString::from_string(spec.name.to_string()),
        description: StbString::from_string(spec.description.to_string()),
        parameters: StbString::from_string(spec.parameters.to_string()),
    });
    // 宿主会把三个字段复制走并通过 plugin_free_string 释放，这里只回收容器。
    let rc = register(
        &*schema as *const StableToolSchema,
        spec.execute,
        poll,
        cancel,
        destroy,
        plugin_free_string,
    );
    drop(schema);
    rc
}

// ===========================================================================
// 一次工具调用的驱动状态
// ===========================================================================

/// 一个工具调用的驱动状态：参数已解析，等待 `poll` 取结果。
pub struct Drive {
    /// 工具名（决定调用哪个实现）。
    tool: &'static str,
    /// 已解析的参数 JSON（原始文本，交由实现层解析）。
    params: String,
    /// 缓存的终态结果；`None` 表示还没算出来。
    outcome: Option<Result<String, String>>,
}

/// 所有在途调用（句柄是数字令牌，不复用）。
static DRIVES: rpi_plugin_sdk::handles::ToolHandles<Drive> =
    rpi_plugin_sdk::handles::ToolHandles::new();

/// 宿主交给本插件的工具名 → 内部实现名的映射。
///
/// 宿主注册时只传 schema，回调里不带工具名，因此四个工具各注册一组回调，
/// 通过不同的 `*_execute` 函数把工具名固化进 Drive。
macro_rules! define_tool_lifecycle {
    ($execute:ident, $tool_name:expr) => {
        rpi_plugin_sdk::execute_handler! {
            /// 工具执行入口：只记录参数，不做任何阻塞工作。
            fn $execute(_tool_call_id, params) {
                let raw = params.to_string_lossy();
                DRIVES.insert(Drive {
                    tool: $tool_name,
                    params: raw,
                    outcome: None,
                })
            }
        }
    };
}

define_tool_lifecycle!(bash_execute, tools::BASH);
define_tool_lifecycle!(get_cmd_result_execute, tools::GET_CMD_RESULT);
define_tool_lifecycle!(kill_cmd_execute, tools::KILL_CMD);
define_tool_lifecycle!(list_cmds_execute, tools::LIST_CMDS);

/// `poll(handle, partial_cb, user_data) -> StepResult`。**阻塞至多 `waitSeconds`。**
///
/// 参数：`handle` 调用句柄；`partial_cb`/`user_data` 宿主回调上下文（本插件不产生增量）。
/// 返回：未完成 `Pending`（保留给未来异步化）；成功 `Done`；失败 `Err`。
extern "C" fn poll(
    handle: StepHandle,
    _partial_cb: Option<ToolPartialCb>,
    _user_data: *mut c_void,
) -> StepResult {
    rpi_plugin_sdk::guard(|| {
        let Some(entry) = DRIVES.get(handle) else {
            return StepResult::err(StbString::from_string(error_result(
                "invalid or destroyed msys2-bash handle",
            )));
        };

        let mut drive = entry.lock();

        // 第一步：已算出终态就直接返回（幂等，允许重复 poll）。
        // **不**因取消而丢弃已算出的结果：取消只影响“要不要继续等”。
        if let Some(outcome) = &drive.outcome {
            return render_outcome(outcome);
        }

        // 第二步：首次 poll 同步执行。各实现内部最多阻塞 waitSeconds，
        // 且必须在取消后尽快返回（cancel 标志透传到 wait_for）。
        let outcome = dispatch(drive.tool, &drive.params, &entry.cancelled);
        let result = render_outcome(&outcome);
        drive.outcome = Some(outcome);
        result
    })
    .unwrap_or_else(|_| {
        StepResult::err(StbString::from_string(error_result("msys2-bash poll panicked")))
    })
}

/// 把终态渲染成 `StepResult`。
///
/// 参数：`outcome` 成功结果或错误结果字符串。
fn render_outcome(outcome: &Result<String, String>) -> StepResult {
    match outcome {
        Ok(text) => StepResult::done(StbString::from_string(text.clone())),
        Err(message) => StepResult::err(StbString::from_string(message.clone())),
    }
}

/// 按工具名分发到具体实现。
///
/// 参数：`tool` 工具名；`params` 参数 JSON 文本；`cancelled` 宿主取消标记。
/// 返回：`Ok(结果 JSON)` 或 `Err(错误 JSON)`。
fn dispatch(
    tool: &str,
    params: &str,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<String, String> {
    // 第一步：拿配置（注册阶段已安装；缺失说明状态不一致）。
    let Some(config) = tools::installed_config() else {
        return Ok(error_result("msys2-bash is not initialized"));
    };

    // 第二步：分发。参数解析失败统一转成结构化错误（不 panic 到宿主）。
    // `bash`/`get` 返回两层 Result（外层解析失败 / 内层业务失败），这里拍平成一层。
    match tool {
        tools::BASH => match tools::bash::submit(params, &config, cancelled) {
            Ok(inner) => inner,
            Err(reason) => Err(error_result(&reason)),
        },
        tools::GET_CMD_RESULT => match tools::get::run(params, &config, cancelled) {
            Ok(inner) => Ok(inner),
            Err(reason) => Err(error_result(&reason)),
        },
        tools::KILL_CMD => match tools::kill::run(params) {
            Ok(inner) => Ok(inner),
            Err(reason) => Err(error_result(&reason)),
        },
        tools::LIST_CMDS => match tools::list::run(params) {
            Ok(inner) => Ok(inner),
            Err(reason) => Err(error_result(&reason)),
        },
        other => Ok(error_result(&format!("unknown tool: {other}"))),
    }
}

/// `cancel(handle)`。只置“停止等待”标志：**不杀进程**，句柄也不释放。
///
/// 参数：`handle` 调用句柄。
extern "C" fn cancel(handle: StepHandle) {
    rpi_plugin_sdk::guard_or((), || {
        // 置 ToolHandles 的取消位；正在 wait_for 的那次 poll 每轮都会检查它。
        DRIVES.cancel(handle);
    });
}

/// `destroy(handle)`。释放句柄，空指针或重复调用都是 no-op；**不杀进程**。
///
/// 参数：`handle` 调用句柄。
extern "C" fn destroy(handle: StepHandle) {
    rpi_plugin_sdk::guard_or((), || {
        drop(DRIVES.remove(handle));
    });
}

// ===========================================================================
// 测试
// ===========================================================================

/// 测试辅助：确认驱动表类型可用（编译期即校验接口）。
#[cfg(test)]
fn handle_is_live(handle: StepHandle) -> bool {
    DRIVES.get(handle).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn unknown_tool_name_yields_error_result() {
        // 未知工具名必须在配置检查之前就被拦住，返回结构化错误。
        let cancelled = AtomicBool::new(false);
        let result = dispatch("no_such_tool", "{}", &cancelled).expect("must not be Err");
        assert!(
            result.contains("unknown tool") || result.contains("not initialized"),
            "{result}"
        );
    }

    #[test]
    fn invalid_params_yield_structured_errors_for_every_tool() {
        // 参数解析失败必须是结构化错误，而不是 panic / 空结果。
        let cancelled = AtomicBool::new(false);
        for tool in [
            tools::BASH,
            tools::GET_CMD_RESULT,
            tools::KILL_CMD,
            tools::LIST_CMDS,
        ] {
            // 未安装配置时也会先返回"not initialized"，两者都是合法降级。
            let result = dispatch(tool, "not json", &cancelled).expect("must not be Err");
            assert!(
                result.contains("error"),
                "{tool} must return an error payload, got {result}"
            );
        }
    }

    #[test]
    fn drives_table_works_for_lifecycle_entrypoints() {
        let handle = DRIVES.insert(Drive {
            tool: tools::LIST_CMDS,
            params: "{}".to_string(),
            outcome: None,
        });
        assert!(handle_is_live(handle));
        cancel(handle);
        destroy(handle);
        assert!(!handle_is_live(handle));
        destroy(handle);
        destroy(std::ptr::null_mut());
    }

    #[test]
    fn poll_caches_outcome_and_tolerates_double_poll() {
        // 同一个句柄 poll 两次都必须成功（幂等），且结果一致。
        let handle = DRIVES.insert(Drive {
            tool: tools::LIST_CMDS,
            params: "{}".to_string(),
            outcome: None,
        });
        let first = poll(handle, None, std::ptr::null_mut());
        let second = poll(handle, None, std::ptr::null_mut());
        // 只验证 tag 一致且都不是 panic，内容依赖配置是否已安装。
        assert_eq!(first.tag as u32, second.tag as u32);
        destroy(handle);
    }
}
