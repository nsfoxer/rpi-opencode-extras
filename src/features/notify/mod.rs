//! `notify` feature：agent 跑完或等待用户回答时发桌面通知。
//!
//! 零配置：注册时启动一个 worker，由它探测 `org.freedesktop.Notifications`
//! 是否可用；不可用（纯 SSH 远端 / headless 服务端）就完全静默。
//!
//! 事件映射：
//! - [`EventTag::BeforeAgentStart`] / [`EventTag::AgentStart`]：标记 run 开始，清掉旧通知
//! - [`EventTag::ToolExecutionStart`]（`ask_user`）：run 进行中 → 「等待你回答」
//! - [`EventTag::UiPromptStart`]：run 进行中 → 「需要确认」（与 `ask_user` 3 秒去重）
//! - [`EventTag::UiPromptEnd`]：关闭等待通知
//! - [`EventTag::TurnEndPayload`]：记录 outcome，用于区分正常/出错/中止
//! - [`EventTag::AgentSettled`]：run 结束 → 运行够久则「已完成」，`aborted` 静默
//!
//! 所有处理器只做内存读写 + `send`，绝不阻塞宿主事件分发。

mod content;
mod worker;

use content::Outcome;
use rpi_plugin_sdk::{events::EventView, EventHandlerFn, EventTag, PluginApi};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// 少于这个时长的 run 不弹「已完成」，避免一问一答也刷屏。
const MIN_RUN_MS: i64 = 5_000;

/// 同一轮里 `ask_user` 与 `UiPromptStart` 可能前后脚触发，3 秒内只弹一条。
const WAIT_DEDUPE_MS: i64 = 3_000;

/// 关闭钩子最长等待时间（50ms × 40 = 2s）。
const SHUTDOWN_WAIT_STEP_MS: u64 = 50;
const SHUTDOWN_WAIT_STEPS: usize = 40;

static RUNNING: AtomicBool = AtomicBool::new(false);
static RUN_START_MS: AtomicI64 = AtomicI64::new(0);
static WAITING: AtomicBool = AtomicBool::new(false);
static LAST_WAIT_NOTIFY_MS: AtomicI64 = AtomicI64::new(0);
/// 最近一次 `turn_end` 的 outcome：0 未知，1 stop，2 aborted，3 error。
static OUTCOME: AtomicU8 = AtomicU8::new(0);

const OUTCOME_UNKNOWN: u8 = 0;
const OUTCOME_STOP: u8 = 1;
const OUTCOME_ABORTED: u8 = 2;
const OUTCOME_ERROR: u8 = 3;

/// 本 feature 订阅的全部事件（测试用；注册表在 [`register`] 里显式列出处理器）。
#[cfg(test)]
const TAGS: [EventTag; 7] = [
    EventTag::BeforeAgentStart,
    EventTag::AgentStart,
    EventTag::ToolExecutionStart,
    EventTag::UiPromptStart,
    EventTag::UiPromptEnd,
    EventTag::TurnEndPayload,
    EventTag::AgentSettled,
];

/// `notify` feature 的注册入口。
///
/// 缺少事件槽位或生命周期槽位都只是让本 feature 静默禁用，不影响其他 feature。
pub fn register(api: &PluginApi) -> i32 {
    // 第一步：`opencodeExtras.notify=false` 时既不订阅事件，也不起通知线程。
    if !crate::settings::toggles().notify {
        return 0;
    }

    let Some(register_event) = api.register_event_handler else {
        return 0;
    };
    worker::install();

    for (tag, handler) in [
        (EventTag::BeforeAgentStart, on_run_start as EventHandlerFn),
        (EventTag::AgentStart, on_run_start as EventHandlerFn),
        (
            EventTag::ToolExecutionStart,
            on_tool_start as EventHandlerFn,
        ),
        (
            EventTag::UiPromptStart,
            on_ui_prompt_start as EventHandlerFn,
        ),
        (EventTag::UiPromptEnd, on_ui_prompt_end as EventHandlerFn),
        (EventTag::TurnEndPayload, on_turn_end as EventHandlerFn),
        (EventTag::AgentSettled, on_agent_settled as EventHandlerFn),
    ] {
        let rc = register_event(tag, handler, std::ptr::null_mut());
        if rc != 0 {
            eprintln!("rpi-opencode-extras: notify disabled: cannot subscribe {tag:?} (rc={rc})");
            return 0;
        }
    }

    if let Some(register_lifecycle) = api.register_plugin_lifecycle {
        let rc = register_lifecycle(on_begin_shutdown, on_finish_shutdown, std::ptr::null_mut());
        if rc != 0 {
            eprintln!("rpi-opencode-extras: notify lifecycle hook rejected (rc={rc})");
        }
    }
    0
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

fn begin_run() {
    RUN_START_MS.store(now_ms(), Ordering::Relaxed);
    RUNNING.store(true, Ordering::Relaxed);
    WAITING.store(false, Ordering::Relaxed);
    LAST_WAIT_NOTIFY_MS.store(0, Ordering::Relaxed);
    OUTCOME.store(OUTCOME_UNKNOWN, Ordering::Relaxed);
    worker::send(worker::Command::CloseAll);
}

fn notify_waiting(body: String) {
    let now = now_ms();
    let last = LAST_WAIT_NOTIFY_MS.load(Ordering::Relaxed);
    if last != 0 && now.saturating_sub(last) < WAIT_DEDUPE_MS {
        return;
    }
    LAST_WAIT_NOTIFY_MS.store(now, Ordering::Relaxed);
    WAITING.store(true, Ordering::Relaxed);
    worker::send(worker::Command::Waiting { body });
}

/// run 开始（`BeforeAgentStart` 与 `AgentStart` 可能都到，覆盖写是幂等的）。
extern "C" fn on_run_start(
    _event: rpi_plugin_sdk::StablePluginEvent,
    _user_data: *mut c_void,
) -> i32 {
    begin_run();
    0
}

/// 工具开始：只关心 `ask_user`，且仅在 run 进行中。
extern "C" fn on_tool_start(
    event: rpi_plugin_sdk::StablePluginEvent,
    _user_data: *mut c_void,
) -> i32 {
    // SAFETY: 宿主保证回调期间载荷有效；EventView 只读取并复制字符串。
    let view = unsafe { EventView::from_raw(&event) };
    let Some(call) = view.tool_call() else {
        return 0;
    };
    if call.tool_name != "ask_user" || !RUNNING.load(Ordering::Relaxed) {
        return 0;
    }
    let question = content::question_from_params(&call.params);
    notify_waiting(content::waiting_body(question.as_deref()));
    0
}

/// 选择器/对话框打开：run 进行中才提示，避免切模型也弹通知。
extern "C" fn on_ui_prompt_start(
    _event: rpi_plugin_sdk::StablePluginEvent,
    _user_data: *mut c_void,
) -> i32 {
    if RUNNING.load(Ordering::Relaxed) {
        notify_waiting("需要你回答或确认".to_string());
    }
    0
}

/// 对话框关闭：撤销「等待你回答」通知。
extern "C" fn on_ui_prompt_end(
    _event: rpi_plugin_sdk::StablePluginEvent,
    _user_data: *mut c_void,
) -> i32 {
    if WAITING.swap(false, Ordering::Relaxed) {
        worker::send(worker::Command::CloseWaiting);
    }
    0
}

/// turn 结束：记录 outcome，供 `AgentSettled` 决定文案与是否提示。
extern "C" fn on_turn_end(
    event: rpi_plugin_sdk::StablePluginEvent,
    _user_data: *mut c_void,
) -> i32 {
    // SAFETY: 同上。
    let view = unsafe { EventView::from_raw(&event) };
    let Some(text) = view.data_text() else {
        return 0;
    };
    let code = match Outcome::from_payload(&text) {
        Outcome::Stop => OUTCOME_STOP,
        Outcome::Aborted => OUTCOME_ABORTED,
        Outcome::Error => OUTCOME_ERROR,
        Outcome::Unknown => return 0,
    };
    OUTCOME.store(code, Ordering::Relaxed);
    0
}

/// run 终止：这是「完成」的权威信号（不是 per-attempt 的 `AgentEnd`）。
extern "C" fn on_agent_settled(
    _event: rpi_plugin_sdk::StablePluginEvent,
    _user_data: *mut c_void,
) -> i32 {
    let started = RUN_START_MS.swap(0, Ordering::Relaxed);
    RUNNING.store(false, Ordering::Relaxed);
    WAITING.store(false, Ordering::Relaxed);
    worker::send(worker::Command::CloseAll);

    let outcome = match OUTCOME.load(Ordering::Relaxed) {
        OUTCOME_STOP => Outcome::Stop,
        OUTCOME_ABORTED => Outcome::Aborted,
        OUTCOME_ERROR => Outcome::Error,
        _ => Outcome::Unknown,
    };
    if outcome == Outcome::Aborted || started == 0 {
        return 0;
    }
    let duration_ms = now_ms().saturating_sub(started);
    if duration_ms < MIN_RUN_MS {
        return 0;
    }
    worker::send(worker::Command::Done {
        body: content::done_body(&content::project_name(), duration_ms, outcome),
        error: outcome == Outcome::Error,
    });
    0
}

/// 插件关闭开始：禁止新通知（SDK 要求非阻塞）。
extern "C" fn on_begin_shutdown(_user_data: *mut c_void) {
    worker::begin_shutdown();
}

/// 插件关闭完成：等到 worker 退出才允许宿主卸载 DLL。
extern "C" fn on_finish_shutdown(_user_data: *mut c_void) -> i32 {
    for _ in 0..SHUTDOWN_WAIT_STEPS {
        if !worker::still_running() {
            return 0;
        }
        std::thread::sleep(std::time::Duration::from_millis(SHUTDOWN_WAIT_STEP_MS));
    }
    1
}

#[cfg(test)]
mod tests {
    use super::{TAGS, WAIT_DEDUPE_MS};
    use rpi_plugin_sdk::EventTag;

    #[test]
    fn subscribes_every_signal_the_state_machine_needs() {
        for tag in [
            EventTag::BeforeAgentStart,
            EventTag::AgentStart,
            EventTag::ToolExecutionStart,
            EventTag::UiPromptStart,
            EventTag::UiPromptEnd,
            EventTag::TurnEndPayload,
            EventTag::AgentSettled,
        ] {
            assert!(TAGS.contains(&tag), "missing subscription for {tag:?}");
        }
    }

    #[test]
    fn tags_are_unique() {
        let mut seen: Vec<EventTag> = Vec::new();
        for tag in TAGS {
            assert!(!seen.contains(&tag), "duplicate tag: {tag:?}");
            seen.push(tag);
        }
    }

    #[test]
    fn dedupe_window_is_short_but_nonzero() {
        const { assert!(WAIT_DEDUPE_MS > 0) };
        const { assert!(WAIT_DEDUPE_MS <= 10_000) };
    }
}
