//! 工作线程与 `UiComponent` 调用封装。
//!
//! **硬约束（实测）**：在 `EventHandlerFn` 里同步调用 `runtime_action` 会让宿主
//! 进程 Segmentation fault。因此所有宿主调用都必须发生在 `std::thread::spawn`
//! 出来的线程上：事件回调只做"置标志 + 起线程"，立即返回。

use crate::features::go_usage::api_key::resolve_api_key;
use crate::features::go_usage::fetch::fetch_usage;
use crate::features::go_usage::format::format_footer;
use crate::features::go_usage::usage::UsageSnapshot;
use rpi_plugin_sdk::{PluginApi, RuntimeActionId, StbString, StbStringRef};
use std::sync::atomic::{AtomicI64, AtomicU8, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// 页脚组件 id（插件内唯一）。
pub const COMPONENT_ID: &str = "go-usage";

/// 组件注册的布局参数：页脚最后一行，单行、无边框。
///
/// **注意**：`register` **不接受** `title`（实测返回
/// `unexpected component field`），标题只能在随后的 `update` 里设置。
const REGISTER_ARGS: &str = r#"{"op":"register","id":"go-usage","type":"text","slot":"footer.after","order":50,"maxHeight":1,"border":false}"#;

/// 两次成功拉取之间的最小间隔（毫秒）。
///
/// `percent` 是整数粒度，分钟级轮询没有意义；60s 既能跟上"跑完一轮用了几个百分点"，
/// 又不会被连续多轮打成请求风暴。
const MIN_FETCH_INTERVAL_MS: i64 = 60_000;

/// 组件可见性状态。
const VISIBILITY_UNKNOWN: u8 = 0;
const VISIBILITY_SHOWN: u8 = 1;
const VISIBILITY_HIDDEN: u8 = 2;

/// 宿主 API 表。整个进程只写一次（注册期），之后只读。
static API: Mutex<Option<PluginApi>> = Mutex::new(None);

/// 组件是否已注册（注册一次后不再重复 register，但 register 本身幂等）。
static REGISTERED: AtomicU8 = AtomicU8::new(0);

/// 上次成功拉取的时刻（Unix 毫秒）；0 表示从未成功。
static LAST_FETCH_MS: AtomicI64 = AtomicI64::new(0);

/// 最近一次成功快照，`None` 表示还没有数据。
static SNAPSHOT: Mutex<Option<UsageSnapshot>> = Mutex::new(None);

/// 当前组件可见性，避免重复 show/hide 打宿主。
static VISIBILITY: AtomicU8 = AtomicU8::new(VISIBILITY_UNKNOWN);

/// 正在运行的工作线程数。宿主关闭前必须等到 0，否则 DLL 可能在
/// 线程仍在调宿主时被卸载（UAF / 退出期崩溃）。
static ACTIVE_WORKERS: AtomicU8 = AtomicU8::new(0);

/// 宿主已开始关闭：此后不得再启动新线程、不得再调宿主。
///
/// 实测：不加这个标志时，进程退出阶段仍在跑的工作线程会打到已销毁的 harness，
/// 输出 `runtime action invoked after harness was dropped`。
static SHUTTING_DOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 是否已由 TUI 启动事件打开门控。
///
/// 实测：`BeforeTuiStart` 只在交互式 TUI 触发，`-p` 模式永不触发。
/// 用它当开关，`-p`/headless 下整个功能保持休眠（没有页脚可画，也不产生退出期噪声）。
static TUI_ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 打开门控（由 `BeforeTuiStart` 调用）。
pub fn enable_for_tui() {
    TUI_ENABLED.store(true, Ordering::Release);
}

/// 门控是否已打开。
///
/// 返回：`true` 表示当前处于交互式 TUI，可以注册组件并刷新。
pub fn is_enabled() -> bool {
    TUI_ENABLED.load(Ordering::Acquire)
}

/// 保存宿主 API 表（注册期调用一次）。
///
/// 参数：`api` 宿主的插件 API 表。
pub fn install_api(api: &PluginApi) {
    if let Ok(mut slot) = API.lock() {
        *slot = Some(*api);
    }
}

/// 调用一次宿主 runtime action。
///
/// 参数：`action` 动作 id；`args` JSON 参数。
/// 返回：`Ok(响应正文)`（可能为空串）；`Err(原因)` 表示宿主拒绝或 API 表缺失。
///
/// **只能在非事件回调线程上调用**（见模块文档）。
fn call_host(action: RuntimeActionId, args: &str) -> Result<String, String> {
    let api = {
        let slot = API.lock().map_err(|_| "api slot poisoned".to_string())?;
        slot.ok_or_else(|| "api table is not installed".to_string())?
    };

    let mut out = StbString::empty();
    let rc = (api.runtime_action)(
        action.into(),
        StbStringRef::from_str(args),
        &mut out,
        api.user_data,
    );
    // SAFETY: 宿主把 `out` 的所有权转移给我们，必须用它自己的 free_string 释放一次。
    let text = unsafe { out.to_string_lossy() };
    unsafe { (api.free_string)(out) };

    if rc != 0 {
        return Err(if text.trim().is_empty() {
            format!("runtime action {action:?} failed with rc={rc}")
        } else {
            text
        });
    }
    Ok(text)
}

/// 确保组件已注册（幂等）。
///
/// 返回：`Ok(())` 注册成功（或此前已成功）；`Err(原因)` 宿主拒绝。
fn ensure_registered() -> Result<(), String> {
    if REGISTERED.load(Ordering::Acquire) == 1 {
        return Ok(());
    }
    call_host(RuntimeActionId::UiComponent, REGISTER_ARGS)?;
    REGISTERED.store(1, Ordering::Release);
    Ok(())
}

/// 显示组件（已显示则跳过）。
fn show_component() -> Result<(), String> {
    if VISIBILITY.load(Ordering::Acquire) == VISIBILITY_SHOWN {
        return Ok(());
    }
    call_host(
        RuntimeActionId::UiComponent,
        &format!(r#"{{"op":"show","id":"{COMPONENT_ID}"}}"#),
    )?;
    VISIBILITY.store(VISIBILITY_SHOWN, Ordering::Release);
    Ok(())
}

/// 隐藏组件（已隐藏则跳过）。
fn hide_component() -> Result<(), String> {
    if VISIBILITY.load(Ordering::Acquire) == VISIBILITY_HIDDEN {
        return Ok(());
    }
    call_host(
        RuntimeActionId::UiComponent,
        &format!(r#"{{"op":"hide","id":"{COMPONENT_ID}"}}"#),
    )?;
    VISIBILITY.store(VISIBILITY_HIDDEN, Ordering::Release);
    Ok(())
}

/// 用指定文本更新组件内容并确保可见。
///
/// 参数：`text` 页脚单行文本；空串表示没有可展示的数据（隐藏组件）。
fn apply_text(text: &str) -> Result<(), String> {
    if text.trim().is_empty() {
        return hide_component();
    }
    ensure_registered()?;
    // text 走 JSON 序列化，避免引号或反斜杠破坏载荷。
    let payload = serde_json::json!({
        "op": "update",
        "id": COMPONENT_ID,
        "text": text,
    });
    call_host(RuntimeActionId::UiComponent, &payload.to_string())?;
    show_component()
}

/// 当前时刻（Unix 秒）。取不到系统时钟时回落到 0（倒计时退化为 ↻now）。
fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// 当前时刻（Unix 毫秒），用于节流判断。
fn now_unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// 查询当前 provider 是否为 opencode.ai。
///
/// 返回：`Ok(true)` 是 go 套餐；`Ok(false)` 不是；`Err(原因)` 宿主没有该能力。
///
/// 用 `GetExecutionContext` 而不是解析事件载荷：它只返回安全的执行元数据
/// （无凭据、无端点、无 header），且 `model.provider` 是判定 provider 的最直接来源。
/// 实测：注册期调用返回 `runtime action invoked before harness was built`，
/// 只有在事件派生的线程里才可用。
fn current_provider_is_opencode() -> Result<bool, String> {
    let text = call_host(RuntimeActionId::GetExecutionContext, "{}")?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| format!("invalid execution context: {error}"))?;

    // 优先用 provider 名判定（最直接）；退而在 models.json 里查该 provider 的 baseUrl。
    let provider = value
        .get("model")
        .and_then(|model| model.get("provider"))
        .and_then(|id| id.as_str())
        .unwrap_or("");

    if provider.is_empty() {
        return Ok(false);
    }
    if provider == "opencode-go" {
        return Ok(true);
    }

    // 兜底：provider 名不叫 opencode-go，但 baseUrl 指向 opencode.ai 也算。
    Ok(provider_points_to_opencode(provider))
}

/// 判断 `models.json` 里某个 provider 的 baseUrl 是否指向 opencode.ai。
///
/// 参数：`provider_id` provider key。
/// 返回：`true` 表示是 opencode.ai provider；读不到配置时为 `false`。
fn provider_points_to_opencode(provider_id: &str) -> bool {
    let Ok(models) = crate::config::load_models_json() else {
        return false;
    };
    models
        .get("providers")
        .and_then(|providers| providers.get(provider_id))
        .and_then(|provider| provider.get("baseUrl"))
        .and_then(|url| url.as_str())
        .map(crate::headers::is_opencode_ai_url)
        .unwrap_or(false)
}

/// 拉取用量并刷新组件。
///
/// 参数：`force` 为 `true` 时忽略 60s 节流（模型切换需要立即反映）。
/// 返回：`Ok(())`；`Err(原因)` 用于诊断输出（组件保持上一次内容）。
fn refresh_usage(force: bool) -> Result<(), String> {
    // 第一步：节流。距离上次成功不足 60s 就只重放已缓存的快照（不再发请求）。
    let last = LAST_FETCH_MS.load(Ordering::Acquire);
    let now_ms = now_unix_millis();
    let enough_time_passed = last == 0 || now_ms.saturating_sub(last) >= MIN_FETCH_INTERVAL_MS;

    if !enough_time_passed && !force {
        // 用缓存重放，保证"切回 go 套餐"时立即有内容而不是空白。
        let cached = SNAPSHOT
            .lock()
            .map_err(|_| "snapshot poisoned".to_string())?
            .clone();
        return match cached {
            Some(snapshot) => apply_text(&format_footer(&snapshot, now_unix_secs())),
            None => Ok(()),
        };
    }

    // 第二步：解析凭据。没有 key 时隐藏组件（而不是显示错误）。
    let (api_key, _base_url) = match resolve_api_key() {
        Ok(pair) => pair,
        Err(reason) => {
            apply_text("")?;
            return Err(reason);
        }
    };

    // 第三步：拉取并格式化。
    let snapshot = fetch_usage(&api_key)?;
    let text = format_footer(&snapshot, now_unix_secs());
    if text.is_empty() {
        apply_text("")?;
        return Ok(());
    }

    // 第四步：成功后才更新缓存与节流时间戳；失败时保留旧值。
    if let Ok(mut slot) = SNAPSHOT.lock() {
        *slot = Some(snapshot);
    }
    LAST_FETCH_MS.store(now_ms, Ordering::Release);
    apply_text(&text)
}

/// 一次刷新请求（由事件回调触发）。
///
/// 参数：`force` 为 `true` 时忽略节流（模型切换）。
///
/// **必须异步**：内部起线程后立即返回，绝不阻塞宿主事件回调。
/// 宿主已开始关闭时直接丢弃：此时 harness 已销毁，再调宿只会产生噪声。
pub fn request_refresh(force: bool) {
    // 第一步：关闭中或未启用（非 TUI）直接丢弃，不启动新线程。
    if SHUTTING_DOWN.load(Ordering::Acquire) || !is_enabled() {
        return;
    }

    // 第二步：登记活跃线程数（关闭钩子据此等待）。
    ACTIVE_WORKERS.fetch_add(1, Ordering::AcqRel);

    let spawned = std::thread::Builder::new()
        .name("go-usage".to_string())
        .spawn(move || {
            // 线程体结束时无论怎么退都要减计数，避免计数泄漏导致关不掉。
            struct WorkerGuard;
            impl Drop for WorkerGuard {
                fn drop(&mut self) {
                    super::worker::ACTIVE_WORKERS.fetch_sub(1, Ordering::AcqRel);
                }
            }
            let _guard = WorkerGuard;

            // 关闭后或门控关掉后不再碰宿主。
            if super::worker::SHUTTING_DOWN.load(Ordering::Acquire)
                || !super::worker::is_enabled()
            {
                return;
            }

            // 第一步：判定 provider。不是 opencode.ai 就隐藏组件并结束（不浪费一次请求）。
            match current_provider_is_opencode() {
                Ok(true) => {}
                Ok(false) => {
                    if let Err(reason) = hide_component() {
                        eprintln!("rpi-opencode-extras: go-usage hide failed: {reason}");
                    }
                    return;
                }
                Err(reason) => {
                    // 宿主还没有 harness（例如注册期）等退化场景：静默跳过，不刷屏。
                    eprintln!("rpi-opencode-extras: go-usage provider probe skipped: {reason}");
                    return;
                }
            }

            // 第二步：是 go 套餐 → 拉用量并刷新页脚。
            if let Err(reason) = refresh_usage(force) {
                eprintln!("rpi-opencode-extras: go-usage refresh failed: {reason}");
            }
        });

    // 线程创建失败：把计数还回去，不要永久占位。
    if spawned.is_err() {
        ACTIVE_WORKERS.fetch_sub(1, Ordering::AcqRel);
    }
}

/// 标记插件进入关闭阶段（由生命周期钩子调用）。
///
/// 参数：无。
/// 副作用：置位 `SHUTTING_DOWN`，此后 `request_refresh` 直接丢弃请求。
pub fn begin_shutdown() {
    SHUTTING_DOWN.store(true, Ordering::Release);
}

/// 关闭阶段是否还有工作线程在跑。
///
/// 返回：`true` 表示仍有线程可能执行插件代码（此时不能返回成功给宿主）。
pub fn workers_still_running() -> bool {
    ACTIVE_WORKERS.load(Ordering::Acquire) > 0
}

#[cfg(test)]
mod tests {
    use super::{COMPONENT_ID, MIN_FETCH_INTERVAL_MS, REGISTER_ARGS};

    #[test]
    fn register_payload_omits_title() {
        // 实测：register 带 title 会被宿主拒绝（unexpected component field）。
        assert!(!REGISTER_ARGS.contains("title"));
        // 但必须声明 slot 与单行限制，否则会占掉多行页脚。
        assert!(REGISTER_ARGS.contains("\"slot\":\"footer.after\""));
        assert!(REGISTER_ARGS.contains("\"maxHeight\":1"));
        assert!(REGISTER_ARGS.contains("\"type\":\"text\""));
    }

    #[test]
    fn register_payload_id_matches_the_constant() {
        assert!(REGISTER_ARGS.contains(&format!("\"id\":\"{COMPONENT_ID}\"")));
    }

    #[test]
    fn throttle_window_is_one_minute() {
        // percent 是整数粒度，节流必须真的存在且不小于 1 分钟。
        assert_eq!(MIN_FETCH_INTERVAL_MS, 60_000);
    }
}
