//! `go-usage` feature：在 TUI 页脚展示 opencode.ai Go 套餐用量。
//!
//! 只有当前 provider 指向 opencode.ai 时才显示（判定见 [`worker`]），
//! 非 go 套餐时组件被 `hide`（不是 `remove`，切回来能立刻恢复）。
//!
//! 模块划分：
//! - [`api_key`]：从 `models.json` 取 opencode.ai provider 的凭据（不落日志）
//! - [`fetch`]：`/usage` 端点 HTTP 拉取（错误文本不含 key）
//! - [`usage`]：响应解析的纯函数与单测
//! - [`format`]：页脚文本与倒计时的纯函数与单测
//! - [`worker`]：工作线程、节流、`UiComponent` 调用

pub mod api_key;
pub mod fetch;
pub mod format;
#[cfg(test)]
mod live_check;
pub mod usage;
pub mod worker;

use rpi_plugin_sdk::{events::EventView, EventHandlerFn, EventTag, PluginApi};
use std::ffi::c_void;

/// 触发刷新的宿主事件。
///
/// - `AgentStart`：一轮开始，顺带确认 provider 是否变了。
/// - `AgentSettled`：一轮结束，用量最可能发生变化（主力刷新点）。
/// - `ModelSelect`：切模型，需要立即重判 provider（非 go 就 hide）。
///
/// 必须有 `AgentStart`/`AgentSettled` 兜底：实测 `-p` 模式下 `ModelSelect`
/// 有时不发，只订阅它会漏掉刷新。
const TRIGGER_TAGS: [EventTag; 3] = [
    EventTag::AgentStart,
    EventTag::AgentSettled,
    EventTag::ModelSelect,
];

/// TUI 启用门控事件。
///
/// 实测：`BeforeTuiStart` **只在交互式 TUI 启动时**触发，`-p` 模式下从不触发。
/// 用它作为"是否有页脚可画"的判据：`-p` 模式既无页脚，又会在退出时
/// 销毁 harness，此时任何后台线程调用宿主都会输出
/// `runtime action invoked after harness was dropped`。
const TUI_GATE_TAG: EventTag = EventTag::BeforeTuiStart;

/// 关闭等待的轮询间隔。
const SHUTDOWN_WAIT_STEP: std::time::Duration = std::time::Duration::from_millis(50);

/// 关闭等待的轮询步数（50ms × 60 = 3s）。
const SHUTDOWN_WAIT_STEPS: usize = 60;

/// `go-usage` feature 的注册入口。
///
/// 参数：`api` 宿主的插件 API 表。
/// 返回：`0` 表示注册完成（或宿主缺少能力、本功能静默禁用）；非 0 表示宿主拒绝了
/// 事件订阅（由 [`crate::registry::register_all`] 上报）。
///
/// 设计取向：**这个 feature 是"锦上添花"**，缺任何宿主能力都只是不显示，
/// 绝不让整个插件加载失败。
pub fn register(api: &PluginApi) -> i32 {
    // 第一步：保存 API 表。工作线程需要它来调 runtime_action；
    // 事件回调本身绝不直接调用（实测会 segfault）。
    worker::install_api(api);

    // 第二步：订阅触发事件。宿主没有该槽位就静默禁用。
    let Some(register_event) = api.register_event_handler else {
        return 0;
    };

    // 第 1.5 步：先订阅 TUI 门控。没它就不启用（`-p` / headless 无页脚可画）。
    let gate_rc = register_event(TUI_GATE_TAG, on_tui_start as EventHandlerFn, std::ptr::null_mut());
    if gate_rc != 0 {
        eprintln!("rpi-opencode-extras: go-usage disabled: cannot subscribe {TUI_GATE_TAG:?} (rc={gate_rc})");
        return 0;
    }

    for tag in TRIGGER_TAGS {
        let rc = register_event(tag, on_event as EventHandlerFn, std::ptr::null_mut());
        if rc != 0 {
            // 事件订阅失败说明宿主拒绝，但不应连带让其他 feature 失败。
            eprintln!("rpi-opencode-extras: go-usage disabled: cannot subscribe {tag:?} (rc={rc})");
            return 0;
        }
    }

    // 第三步：注册关闭钩子。必须有：工作线程在进程退出时仍可能调宿主，
    // 实测会输出 `runtime action invoked after harness was dropped`。
    if let Some(register_lifecycle) = api.register_plugin_lifecycle {
        let rc = register_lifecycle(
            on_begin_shutdown,
            on_finish_shutdown,
            std::ptr::null_mut(),
        );
        if rc != 0 {
            eprintln!("rpi-opencode-extras: go-usage lifecycle hook rejected (rc={rc})");
        }
    }

    0
}

/// TUI 启动事件：打开门控并触发首次刷新。
///
/// 这是本功能**唯一**的启用入口：没有它（`-p` / headless）就不启用，
/// 既不会在无页脚的场景里白花请求，也不会在退出期打到已销毁的 harness。
extern "C" fn on_tui_start(_event: rpi_plugin_sdk::StablePluginEvent, _user_data: *mut c_void) -> i32 {
    worker::enable_for_tui();
    // 首次立即刷新（忽略节流）。
    worker::request_refresh(true);
    0
}

/// 触发事件处理器。
///
/// **只做一件事：起线程**。任何宿主调用（`runtime_action`）都必须离开这个
/// 回调线程，否则宿主进程会崩溃（实测 Segmentation fault）。
///
/// 参数：`event` 宿主事件；`_user_data` 未使用。
/// 返回：恒为 `0`（本处理器不拦截事件）。
extern "C" fn on_event(event: rpi_plugin_sdk::StablePluginEvent, _user_data: *mut c_void) -> i32 {
    // SAFETY: 宿主保证回调期间事件载荷有效；`EventView` 立即借用并只读取。
    let view = unsafe { EventView::from_raw(&event) };

    // 模型切换需要立即反映（可能是切走 go 套餐），其余事件走 60s 节流。
    let force = view.tag() == EventTag::ModelSelect;

    // 起线程后立即返回：绝不阻塞宿主事件分发。
    worker::request_refresh(force);
    0
}

/// 插件关闭开始：置位停止标志，拒绝新刷新。
///
/// **不得在此 join 线程或调宿主**（SDK 契约：begin 必须非阻塞）。
extern "C" fn on_begin_shutdown(_user_data: *mut c_void) {
    worker::begin_shutdown();
}

/// 插件关闭完成：只有当所有工作线程都退出后才返回 0。
///
/// 返回：`0` 表示已无后台线程会执行插件代码（宿主可安全卸载 DLL）；
/// 非 0 表示仍有线程在跑，宿主会保留 DLL，避免 use-after-unload。
///
/// 这里最多等 [`SHUTDOWN_WAIT`]，超时则返回非 0 让宿主保留库（宁可泄漏也不崩）。
extern "C" fn on_finish_shutdown(_user_data: *mut c_void) -> i32 {
    // 轮询等待：单个工作线程最长受 HTTP 10s 超时约束，此处给 3s 足够。
    for _ in 0..SHUTDOWN_WAIT_STEPS {
        if !worker::workers_still_running() {
            return 0;
        }
        std::thread::sleep(SHUTDOWN_WAIT_STEP);
    }
    // 仍在跑：报告非零，让宿主保留这个 DLL 而不是卸载正在运行的代码。
    1
}

#[cfg(test)]
mod tests {
    use super::{TRIGGER_TAGS, TUI_GATE_TAG};
    use rpi_plugin_sdk::EventTag;

    #[test]
    fn settlement_events_are_subscribed_as_refresh_fallbacks() {
        // 实测 `-p` 模式下 ModelSelect 有时不发，必须有 AgentStart/AgentSettled 兜底。
        assert!(TRIGGER_TAGS.contains(&EventTag::AgentSettled));
        assert!(TRIGGER_TAGS.contains(&EventTag::AgentStart));
        assert!(TRIGGER_TAGS.contains(&EventTag::ModelSelect));
    }

    #[test]
    fn trigger_tags_are_unique() {
        let mut seen: Vec<EventTag> = Vec::new();
        for tag in TRIGGER_TAGS {
            assert!(!seen.contains(&tag), "duplicate trigger tag: {tag:?}");
            seen.push(tag);
        }
    }

    #[test]
    fn model_select_is_a_trigger_so_provider_switches_take_effect() {
        // 切走 go 套餐时必须能触发一次重判，否则页脚会一直显示旧用量。
        assert!(TRIGGER_TAGS.contains(&EventTag::ModelSelect));
    }

    #[test]
    fn tui_gate_is_the_only_enable_signal() {
        // 实测：BeforeTuiStart 只在交互式 TUI 触发，`-p` 模式永不触发。
        // 它必须不在 TRIGGER_TAGS 里（否则非 TUI 场景也会刷新），
        // 而是一个独立的一次性门控。
        assert_eq!(TUI_GATE_TAG, EventTag::BeforeTuiStart);
        assert!(!TRIGGER_TAGS.contains(&TUI_GATE_TAG));
    }
}
