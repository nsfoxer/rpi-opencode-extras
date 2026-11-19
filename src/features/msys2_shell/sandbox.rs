//! Windows 沙箱检测：判断当前进程能否运行 MSYS2。
//!
//! rpi-sandbox 的 MXC ProcessContainer 后端把负载放在 Low integrity 下。
//! `msys-2.0.dll` 会用绝对 NT 路径创建 `\BaseNamedObjects\msys-2.0S5-<hash>`，
//! AppContainer 风格的命名对象重定向不会改写绝对路径，于是 DLL 初始化失败
//! （microsoft/mxc#1061）。这不是文件系统、capability 或 UI 策略能修的，所以
//! 在这里提前给出可操作的错误，而不是把 `0xC0000142` 丢给模型去猜。

use std::sync::OnceLock;

/// 返回阻止 MSYS2 运行的原因；`None` 表示当前进程可以正常启动 bash。
///
/// 结果按进程缓存：完整性级别在进程生命周期内不会变化。
pub fn blocked_reason() -> Option<&'static str> {
    static REASON: OnceLock<Option<String>> = OnceLock::new();
    REASON.get_or_init(detect).as_deref()
}

#[cfg(windows)]
fn detect() -> Option<String> {
    if !is_low_integrity() {
        return None;
    }
    Some(
        "msys2 bash cannot run inside rpi-sandbox on Windows: the MXC \
         ProcessContainer backend starts the workload at Low integrity, and \
         msys-2.0.dll cannot create \\BaseNamedObjects\\msys-2.0* there \
         (microsoft/mxc#1061; no filesystem, capability, or UI setting can \
         grant it). Use `rpis --no-sandbox` for MSYS2 commands, or run the \
         Linux Landlock backend."
            .to_string(),
    )
}

#[cfg(not(windows))]
fn detect() -> Option<String> {
    None
}

/// 当前进程是否处于 Low integrity（低于 medium）。
#[cfg(windows)]
fn is_low_integrity() -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenIntegrityLevel, TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    // 第一步：打开当前进程令牌。
    let mut token = std::ptr::null_mut();
    // SAFETY: `GetCurrentProcess` returns a pseudo-handle; `token` is a valid
    // out pointer for the duration of the call.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return false;
    }

    // 第二步：先问所需长度，再读 TOKEN_MANDATORY_LABEL。
    let mut needed = 0u32;
    // SAFETY: a null buffer of length 0 is the documented size query.
    unsafe {
        GetTokenInformation(
            token,
            TokenIntegrityLevel,
            std::ptr::null_mut(),
            0,
            &mut needed,
        );
    }
    if needed == 0 {
        // SAFETY: `token` came from a successful OpenProcessToken.
        unsafe { CloseHandle(token) };
        return false;
    }

    let mut buffer = vec![0u8; needed as usize];
    // SAFETY: `buffer` is `needed` bytes and `needed` is passed as its length.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenIntegrityLevel,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    };
    // SAFETY: `token` is still owned by this function.
    unsafe { CloseHandle(token) };
    if ok == 0 {
        return false;
    }

    // 第三步：完整性级别就是 SID 的最后一个 sub-authority。
    // SAFETY: `buffer` holds a TOKEN_MANDATORY_LABEL with a valid SID.
    let rid = unsafe {
        let label = &*(buffer.as_ptr() as *const TOKEN_MANDATORY_LABEL);
        let sid = label.Label.Sid.cast::<windows_sys::Win32::Security::SID>();
        if sid.is_null() || (*sid).SubAuthorityCount == 0 {
            return false;
        }
        let count = (*sid).SubAuthorityCount as usize;
        *(*sid).SubAuthority.as_ptr().add(count - 1)
    };

    // SECURITY_MANDATORY_LOW_RID = 0x1000；低于 medium 就按受限处理。
    rid < 0x2000
}
