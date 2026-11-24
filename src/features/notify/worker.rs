//! `notify` 的工作线程：通知发送、关闭/替换。
//!
//! **硬约束**：事件回调里不能做 D-Bus 调用（会阻塞宿主事件分发），
//! 因此回调只 `send` 一条命令，真正的 `Notify` 都在这里执行。
//!
//! **不需要事前探测**：通知是尽力而为的能力，`show()` 自己就返回 `Result`。
//! 第一次失败就整体禁用并保持静默（宿主没有给插件提供日志通道，用户也不该被
//! 一行无用的提示打扰）；纯 SSH / headless 环境下因此完全无声。
//!
//! **平台差异**（`notify-rust` 4.x 默认后端）：
//! - Linux/BSD：`show()` 真连 D-Bus，失败返回 `Err`；handle 支持 `close()` 撤回。
//! - Windows：winrt 后端，失败返回 `Err`；handle 无 `close()`。
//! - macOS：`NSUserNotificationCenter` 后端，`show()` 恒返回 `Ok`，真正发送在
//!   handle 的 `Drop` 里，且没有 `close()`，也不支持 `urgency`。
//!   因此该平台不留 handle（丢弃即发送、不可撤回）。

#[cfg(not(target_os = "macos"))]
use notify_rust::Urgency;
use notify_rust::{Notification, NotificationHandle, Timeout};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Mutex;

/// 通知在桌面上停留的时间。
const NOTIFICATION_TIMEOUT_MS: u32 = 8_000;

/// 一次通知请求。
pub enum Command {
    /// 关闭等待中与已完成的两种通知。
    CloseAll,
    /// 只关闭「等待回答」通知。
    CloseWaiting,
    /// 展示「等待回答」，替换上一条等待通知。
    Waiting { body: String },
    /// 展示「已完成」，替换上一条完成通知。
    Done { body: String, error: bool },
    /// 工作线程退出。
    Shutdown,
}

static SENDER: Mutex<Option<Sender<Command>>> = Mutex::new(None);
static STARTED: AtomicBool = AtomicBool::new(false);
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);
static FINISHED: AtomicBool = AtomicBool::new(false);

/// 启动工作线程（幂等）。在 feature 注册时调用。
pub fn install() {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let (tx, rx) = mpsc::channel::<Command>();
    *SENDER.lock().unwrap() = Some(tx);
    let spawned = std::thread::Builder::new()
        .name("rpi-notify".to_string())
        .spawn(move || run(rx));
    if spawned.is_err() {
        // 起不了线程就放弃这个 feature；其他 feature 不受影响。
        STARTED.store(false, Ordering::SeqCst);
        FINISHED.store(true, Ordering::SeqCst);
        *SENDER.lock().unwrap() = None;
    }
}

/// 发送一条命令。关闭开始后拒绝新命令。
pub fn send(command: Command) {
    if SHUTTING_DOWN.load(Ordering::SeqCst) {
        return;
    }
    if let Ok(guard) = SENDER.lock() {
        if let Some(sender) = guard.as_ref() {
            let _ = sender.send(command);
        }
    }
}

/// 插件关闭开始：置位停止标志（SDK 要求非阻塞）。
pub fn begin_shutdown() {
    SHUTTING_DOWN.store(true, Ordering::SeqCst);
    if let Ok(guard) = SENDER.lock() {
        if let Some(sender) = guard.as_ref() {
            let _ = sender.send(Command::Shutdown);
        }
    }
}

/// 工作线程是否仍在执行插件代码。
pub fn still_running() -> bool {
    STARTED.load(Ordering::SeqCst) && !FINISHED.load(Ordering::SeqCst)
}

fn run(receiver: Receiver<Command>) {
    // 不做事前探测：`show()` 的 `Result` 已经足够驱动「失败即禁用」。
    let mut enabled = true;
    let mut waiting: Option<NotificationHandle> = None;
    let mut done: Option<NotificationHandle> = None;

    while let Ok(command) = receiver.recv() {
        match command {
            Command::Shutdown => break,
            Command::CloseAll => {
                close(&mut waiting);
                close(&mut done);
            }
            Command::CloseWaiting => close(&mut waiting),
            Command::Waiting { body } => {
                if !enabled {
                    continue;
                }
                close(&mut waiting);
                match show("rpi · 等待你回答", &body, Level::Normal, "dialog-question") {
                    Ok(handle) => keep(&mut waiting, handle),
                    Err(_) => enabled = false,
                }
            }
            Command::Done { body, error } => {
                if !enabled {
                    continue;
                }
                close(&mut done);
                let summary = if error {
                    "rpi · 运行结束（出错）"
                } else {
                    "rpi · 已完成"
                };
                let (level, icon) = if error {
                    (Level::Normal, "dialog-error")
                } else {
                    (Level::Low, "utilities-terminal")
                };
                match show(summary, &body, level, icon) {
                    Ok(handle) => keep(&mut done, handle),
                    Err(_) => enabled = false,
                }
            }
        }
    }

    close(&mut waiting);
    close(&mut done);
    FINISHED.store(true, Ordering::SeqCst);
}

/// 通知优先级。
///
/// macOS 默认后端不支持 `urgency`，且 `notify_rust::Urgency` 在该平台被标记为
/// deprecated（`-D warnings` 会直接失败），因此用自有枚举表达意图，
/// 只在支持它的平台映射过去。
#[derive(Clone, Copy)]
enum Level {
    Normal,
    Low,
}

fn show(
    summary: &str,
    body: &str,
    level: Level,
    icon: &str,
) -> Result<NotificationHandle, notify_rust::error::Error> {
    let mut notification = Notification::new();
    notification
        .appname("rpi")
        .summary(summary)
        .body(body)
        .icon(icon)
        .timeout(Timeout::Milliseconds(NOTIFICATION_TIMEOUT_MS));

    #[cfg(not(target_os = "macos"))]
    notification.urgency(match level {
        Level::Normal => Urgency::Normal,
        Level::Low => Urgency::Low,
    });
    // macOS 默认后端不支持 urgency，`level` 只用于这里的选择。
    #[cfg(target_os = "macos")]
    let _ = level;

    notification.show()
}

/// 保存 handle，供后续替换或撤回。
#[cfg(not(target_os = "macos"))]
fn keep(slot: &mut Option<NotificationHandle>, handle: NotificationHandle) {
    *slot = Some(handle);
}

/// macOS 默认后端把「发送」放在 handle 的 `Drop` 里，且没有 `close()`：
/// 立刻丢弃即「立即发送、不可撤回」，与决策的降级语义一致。
#[cfg(target_os = "macos")]
fn keep(_slot: &mut Option<NotificationHandle>, handle: NotificationHandle) {
    drop(handle);
}

fn close(handle: &mut Option<NotificationHandle>) {
    // 只有 XDG（Linux/BSD）后端能用 `close()` 撤回通知。
    #[cfg(all(unix, not(target_os = "macos")))]
    if let Some(handle) = handle.take() {
        handle.close();
    }
    // Windows winrt 与 macOS 默认后端都没有 `close`；丢弃 handle 即可。
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    let _ = handle.take();
}
