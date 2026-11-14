//! `notify` 的工作线程：能力探测、D-Bus 通知、关闭/替换。
//!
//! **硬约束**：事件回调里不能做 D-Bus 调用（会阻塞宿主事件分发），
//! 因此回调只 `send` 一条命令，真正的 `Notify` 都在这里执行。
//!
//! 能力探测只做一次：`org.freedesktop.Notifications` 不可达（纯 SSH / headless）
//! 就整个 worker 静默禁用，不打印、不回退、不影响其他 feature。

use notify_rust::{Notification, NotificationHandle, Timeout, Urgency};
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
    // 能力探测。失败即禁用：不打印、不回退（纯 SSH 远端要完全安静）。
    let mut enabled = notify_rust::get_server_information().is_ok();
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
                match show(
                    "rpi · 等待你回答",
                    &body,
                    Urgency::Normal,
                    "dialog-question",
                ) {
                    Ok(handle) => waiting = Some(handle),
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
                let (urgency, icon) = if error {
                    (Urgency::Normal, "dialog-error")
                } else {
                    (Urgency::Low, "utilities-terminal")
                };
                match show(summary, &body, urgency, icon) {
                    Ok(handle) => done = Some(handle),
                    Err(_) => enabled = false,
                }
            }
        }
    }

    close(&mut waiting);
    close(&mut done);
    FINISHED.store(true, Ordering::SeqCst);
}

fn show(
    summary: &str,
    body: &str,
    urgency: Urgency,
    icon: &str,
) -> Result<NotificationHandle, notify_rust::error::Error> {
    Notification::new()
        .appname("rpi")
        .summary(summary)
        .body(body)
        .icon(icon)
        .urgency(urgency)
        .timeout(Timeout::Milliseconds(NOTIFICATION_TIMEOUT_MS))
        .show()
}

fn close(handle: &mut Option<NotificationHandle>) {
    if let Some(handle) = handle.take() {
        handle.close();
    }
}
