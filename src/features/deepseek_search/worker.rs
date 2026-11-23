//! 工作线程、句柄表与四个生命周期函数。
//!
//! `execute` 只解析参数并起线程；网络等待全部在工作线程内，绝不阻塞宿主。

use crate::features::deepseek_search::params::{parse_params, SearchParams};
use crate::features::deepseek_search::search::search_web;
use rpi_plugin_sdk::handles::ToolHandles;
use rpi_plugin_sdk::{StbString, StepHandle, StepResult, ToolPartialCb};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// 结果槽类型别名，避免在签名里反复写长类型。
type ResultSlot = Arc<Mutex<Option<Result<String, String>>>>;

/// 一次调用的可观测状态。
///
/// `slot` 由工作线程写入终态（`Ok` = 已格式化的工具结果 JSON，`Err` = 错误结果 JSON）；
/// `poll` 只做读取，因此始终非阻塞。
struct SearchDrive {
    /// `None` 表示工作线程仍在运行。
    slot: ResultSlot,
}

/// 所有在途调用；句柄是数字令牌，不复用。
static SEARCH_DRIVES: ToolHandles<SearchDrive> = ToolHandles::new();

/// 已完成工具结果的封装：`{"content":[{"type":"text","text":...}]}`。
///
/// 参数 `text` 是给模型看的正文。用 serde 构造，避免手拼 JSON 漏转义。
fn content_result(text: &str) -> String {
    serde_json::json!({
        "content": [ { "type": "text", "text": text } ]
    })
    .to_string()
}

/// 结构化错误结果的封装：`{"error":"<原因>"}`。
fn error_result(message: &str) -> String {
    serde_json::json!({ "error": message }).to_string()
}

/// 把终态写入结果槽；`Mutex` 中毒时回收内部值继续写，避免整次调用失败。
fn store_outcome(slot: &ResultSlot, outcome: Result<String, String>) {
    let mut guard = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = Some(outcome);
}

/// 搜索实现的可注入签名，便于单测完全离线地驱动生命周期。
pub type SearchFn = fn(&SearchParams, &AtomicBool) -> Result<String, String>;

/// 后台执行一次搜索并写回结果槽。
///
/// 参数：`slot` 结果槽；`cancelled` 取消标记；`params` 已校验参数；`search` 搜索实现。
/// 返回：无（终态写入 `slot`）；不 panic——任何失败都转成结构化错误。
fn run_search_with(
    slot: ResultSlot,
    cancelled: Arc<AtomicBool>,
    params: SearchParams,
    search: SearchFn,
) {
    // 线程启动前已被取消：直接落终态，不发请求。
    if cancelled.load(Ordering::Acquire) {
        store_outcome(&slot, Err(error_result("cancelled")));
        return;
    }
    let outcome = match search(&params, &cancelled) {
        Ok(text) => Ok(content_result(&text)),
        Err(reason) => Err(error_result(&reason)),
    };
    store_outcome(&slot, outcome);
}

/// 起工作线程并登记句柄。
///
/// 参数：`params` 已校验参数；`search` 搜索实现（生产中为 [`search_web`]）。
/// 返回：新句柄；线程创建失败时句柄仍有效，但 `poll` 立刻得到错误终态。
fn spawn_search(params: SearchParams, search: SearchFn) -> StepHandle {
    let slot: ResultSlot = Arc::new(Mutex::new(None));
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_slot = Arc::clone(&slot);
    let worker_cancelled = Arc::clone(&cancelled);

    match std::thread::Builder::new()
        .name("deepseek-search".to_string())
        .spawn(move || run_search_with(worker_slot, worker_cancelled, params, search))
    {
        Ok(join_handle) => {
            // 显式丢弃 JoinHandle：线程脱离，结果只经结果槽回传。
            drop(join_handle);
        }
        Err(err) => store_outcome(
            &slot,
            Err(error_result(&format!(
                "failed to spawn search worker: {err}"
            ))),
        ),
    }

    SEARCH_DRIVES.insert_with_cancel(SearchDrive { slot }, cancelled)
}

/// 起一个"立即失败"的调用：不再访问网络，终态就是给定的结构化错误。
///
/// 参数：`reason` 错误原因（会经 `error_result` 转成 `{"error":...}`）。
/// 返回：句柄；宿主第一次 `poll` 就拿到 `Err`。
fn spawn_immediate_error(reason: &str) -> StepHandle {
    let slot: ResultSlot = Arc::new(Mutex::new(None));
    store_outcome(&slot, Err(error_result(reason)));
    SEARCH_DRIVES.insert(SearchDrive { slot })
}

// ===========================================================================
// 四个生命周期函数（供 mod.rs 注册）
// ===========================================================================

rpi_plugin_sdk::execute_handler! {
    /// `execute(tool_call_id, params, free_params) -> StepHandle`。
    ///
    /// 解析参数后起工作线程执行搜索，**立即**返回句柄；网络等待绝不发生在
    /// `execute` 里（否则宿主调用线程会被阻塞）。
    /// 参数非法时同样返回一个句柄，其终态是结构化错误（而不是空句柄），
    /// 让模型能看到具体原因。
    fn execute(_tool_call_id, params) {
        let raw = params.to_string_lossy();
        match parse_params(&raw) {
            Ok(parsed) => spawn_search(parsed, search_web),
            Err(reason) => spawn_immediate_error(&format!("invalid params: {reason}")),
        }
    }
}

/// 供 `mod.rs` 注册时引用的 ABI 入口。
///
/// `execute_handler!` 生成的函数是模块私有的（宏不写可见性），这里做一层轻量转发，
/// 保留宏提供的参数接管与 panic 隔离。
pub(crate) extern "C" fn execute_tool(
    raw: rpi_plugin_sdk::StbStringRef,
    params: StbString,
    free: Option<rpi_plugin_sdk::FreeStringFn>,
) -> StepHandle {
    execute(raw, params, free)
}

/// `poll(handle, partial_cb, user_data) -> StepResult`。**非阻塞。**
///
/// 参数：`handle` 调用句柄；`partial_cb` 宿主 partial 回调（本工具不产生增量）；
/// `user_data` 宿主回调上下文（原样透传，不使用）。
/// 返回：未完成 `Pending`；成功 `Done`（工具结果 JSON）；失败 `Err`（错误 JSON）。
pub extern "C" fn poll(
    handle: StepHandle,
    partial_cb: Option<ToolPartialCb>,
    user_data: *mut c_void,
) -> StepResult {
    let _ = (partial_cb, user_data);
    // 任何 panic 都收敛成工具错误，绝不 unwind 回宿主。
    rpi_plugin_sdk::guard(|| {
        let Some(entry) = SEARCH_DRIVES.get(handle) else {
            return StepResult::err(StbString::from_string(error_result(
                "invalid or destroyed deepseek_search handle",
            )));
        };
        // 取消优先于迟到结果：取消后不再回传成功。
        if entry.cancelled.load(Ordering::Acquire) {
            return StepResult::err(StbString::from_string(error_result("cancelled")));
        }

        // 只在锁内取走终态副本，锁外再构造返回值（不持锁回调宿主）。
        let outcome = {
            let drive = entry.lock();
            let guard = drive
                .slot
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.clone()
        };

        match outcome {
            None => StepResult::pending(StbString::empty()),
            Some(Ok(text)) => StepResult::done(StbString::from_string(text)),
            Some(Err(message)) => StepResult::err(StbString::from_string(message)),
        }
    })
    .unwrap_or_else(|_| {
        StepResult::err(StbString::from_string(error_result(
            "deepseek_search poll panicked",
        )))
    })
}

/// `cancel(handle)`。只置取消标记：幂等、线程安全、**不释放**句柄。
pub extern "C" fn cancel(handle: StepHandle) {
    // cancel 无返回值，panic 只需被吞掉，不能 unwind 回宿主。
    rpi_plugin_sdk::guard_or((), || SEARCH_DRIVES.cancel(handle));
}

/// `destroy(handle)`。释放句柄，空指针或重复调用都是 no-op。
pub extern "C" fn destroy(handle: StepHandle) {
    rpi_plugin_sdk::guard_or((), || {
        drop(SEARCH_DRIVES.remove(handle));
    });
}

/// 供单测探测句柄是否仍在在途表里（不影响宿主行为）。
#[cfg(test)]
fn handle_is_live(handle: StepHandle) -> bool {
    SEARCH_DRIVES.get(handle).is_some()
}

// ===========================================================================
// 测试
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_plugin_sdk::callbacks::Step;
    use std::time::Duration;

    /// 离线桩：固定成功文本。
    fn stub_ok(_params: &SearchParams, _cancelled: &AtomicBool) -> Result<String, String> {
        Ok("1. Stub title / https://example.com / 2026-01-01 / 3 / stub summary".to_string())
    }

    /// 离线桩：固定失败原因（模拟 HTTP 错误）。
    fn stub_fail(_params: &SearchParams, _cancelled: &AtomicBool) -> Result<String, String> {
        Err("DeepSeek search failed: HTTP 500: boom".to_string())
    }

    /// 走 execute 入口（参数非法路径需要它）。
    fn execute_with(raw: &str) -> StepHandle {
        execute(
            rpi_plugin_sdk::StbStringRef::from_str("call"),
            StbString::from_string(raw.to_string()),
            // 测试自己接管参数内存：传 None 让 OwnedString 不做释放。
            None,
        )
    }

    /// 起一次离线调用。
    fn spawn_with(search: SearchFn, raw: &str) -> StepHandle {
        let params = parse_params(raw).expect("test params must be valid");
        spawn_search(params, search)
    }

    /// 轮询到终态（离线桩会立即完成）。
    fn poll_settled(handle: StepHandle) -> Step {
        for _ in 0..2000 {
            // SAFETY: poll 返回的 StepResult 由本 SDK 分配，所有权转移恰好一次。
            let step = unsafe {
                rpi_plugin_sdk::take_step_result!(poll(handle, None, std::ptr::null_mut()))
            };
            if !matches!(step, Step::Pending(_)) {
                return step;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("search did not settle in time");
    }

    #[test]
    fn invalid_params_yield_structured_error_handle() {
        // 参数非法时也必须给宿主一个可读的结构化错误，而不是空句柄。
        for raw in [
            r#"{"query":"   "}"#,
            r#"{}"#,
            "not json",
            &format!(r#"{{"query":"{}"}}"#, "a".repeat(129)),
        ] {
            let handle = execute_with(raw);
            assert!(!handle.is_null(), "handle must not be null for {raw}");
            let Step::Err(json) = poll_settled(handle) else {
                panic!("expected structured error for {raw}");
            };
            let value: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert!(value["error"]
                .as_str()
                .is_some_and(|e| e.starts_with("invalid params:")));
            destroy(handle);
        }
    }

    #[test]
    fn done_result_is_wrapped_into_text_content() {
        let handle = spawn_with(stub_ok, r#"{"query":"rust","count":3}"#);
        let Step::Done(json) = poll_settled(handle) else {
            panic!("expected done");
        };
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["content"][0]["type"], "text");
        assert!(value["content"][0]["text"]
            .as_str()
            .is_some_and(|t| t.contains("https://example.com")));
        destroy(handle);
    }

    #[test]
    fn failure_is_reported_as_structured_error() {
        let handle = spawn_with(stub_fail, r#"{"query":"rust"}"#);
        let Step::Err(json) = poll_settled(handle) else {
            panic!("expected error");
        };
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(value["error"]
            .as_str()
            .is_some_and(|e| e.contains("HTTP 500")));
        destroy(handle);
    }

    #[test]
    fn destroy_is_idempotent_and_retired_handles_report_errors() {
        let handle = spawn_with(stub_ok, r#"{"query":"rust"}"#);
        let _ = poll_settled(handle);
        destroy(handle);
        destroy(handle);
        destroy(std::ptr::null_mut());

        // SAFETY: 同上，poll 返回的 StepResult 由本 SDK 分配。
        let step =
            unsafe { rpi_plugin_sdk::take_step_result!(poll(handle, None, std::ptr::null_mut())) };
        let Step::Err(json) = step else {
            panic!("expected invalid handle error");
        };
        assert!(json.contains("invalid or destroyed"));
    }

    #[test]
    fn cancel_is_idempotent_and_does_not_free_the_handle() {
        let handle = spawn_with(stub_ok, r#"{"query":"rust"}"#);
        cancel(handle);
        cancel(handle);
        assert!(handle_is_live(handle));

        // SAFETY: 同上，poll 返回的 StepResult 由本 SDK 分配。
        let step =
            unsafe { rpi_plugin_sdk::take_step_result!(poll(handle, None, std::ptr::null_mut())) };
        let Step::Err(json) = step else {
            panic!("expected cancellation error");
        };
        assert!(json.contains("cancelled"));
        destroy(handle);
    }

    #[test]
    fn cancelled_before_start_never_calls_the_search() {
        fn must_not_run(_p: &SearchParams, _c: &AtomicBool) -> Result<String, String> {
            panic!("search must not run after cancellation");
        }
        let slot: ResultSlot = Arc::new(Mutex::new(None));
        let cancelled = Arc::new(AtomicBool::new(true));
        run_search_with(
            slot.clone(),
            cancelled,
            parse_params(r#"{"query":"x"}"#).unwrap(),
            must_not_run,
        );
        let stored = slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let message = stored
            .expect("cancelled run must set a terminal state")
            .unwrap_err();
        assert!(message.contains("cancelled"));
    }

    #[test]
    fn cancel_during_flight_wins_over_late_result_and_destroy_while_running_is_safe() {
        // 慢桩：故意比 cancel 晚完成，验证"取消优先于迟到成功"且不卡住。
        fn slow(_params: &SearchParams, _cancelled: &AtomicBool) -> Result<String, String> {
            std::thread::sleep(Duration::from_millis(300));
            Ok("late result".to_string())
        }

        let handle = spawn_with(slow, r#"{"query":"rust"}"#);
        // 第一次 poll 时工作线程肯定还没完成。
        // SAFETY: poll 返回的 StepResult 由本 SDK 分配，所有权转移恰好一次。
        let first =
            unsafe { rpi_plugin_sdk::take_step_result!(poll(handle, None, std::ptr::null_mut())) };
        assert!(matches!(first, Step::Pending(_)));

        cancel(handle);
        // SAFETY: 同上。
        let second =
            unsafe { rpi_plugin_sdk::take_step_result!(poll(handle, None, std::ptr::null_mut())) };
        let Step::Err(json) = second else {
            panic!("cancel must win over the late result");
        };
        assert!(json.contains("cancelled"));

        // 工作线程仍在跑时 destroy：必须安全（句柄表不等待、状态由 Arc 保活）。
        destroy(handle);
        destroy(handle);
        // 等慢桩自然结束，确认没有残留问题（测试结束时不应仍有引用）。
        std::thread::sleep(Duration::from_millis(400));
        assert!(!handle_is_live(handle));
    }

    #[test]
    fn many_concurrent_calls_settle_independently() {
        // 8 个并发调用：句柄互不干扰，各自能拿到自己的终态。
        let handles: Vec<StepHandle> = (0..8)
            .map(|_| spawn_with(stub_ok, r#"{"query":"rust","count":2}"#))
            .collect();
        for handle in &handles {
            let Step::Done(json) = poll_settled(*handle) else {
                panic!("each call must settle to done");
            };
            assert!(json.contains("https://example.com"));
        }
        for handle in handles {
            destroy(handle);
            destroy(handle);
        }
    }

    #[test]
    fn missing_key_error_never_contains_key_material() {
        // 错误文本只描述缺失原因，不带任何凭据内容。
        let message = error_result("missing DeepSeek API key");
        assert!(message.contains("missing DeepSeek API key"));
        assert!(!message.contains("sk-"));
    }
}
