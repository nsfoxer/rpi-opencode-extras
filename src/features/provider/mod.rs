//! `provider` feature：为所有 baseUrl 指向 opencode.ai 的 provider 注入
//! `x-opencode-session` 与 `user-agent` 请求头。
//!
//! 本模块把"哪些 provider 需要包装"（[`crate::config`]）和"怎么构造 header"
//! （[`crate::headers`]）接到宿主 ABI 上：注册时只按 baseUrl 判断，运行时也只按
//! `model.base_url` 判断，provider 名字不参与任何决策。
//!
//! 注入点必须在 [`Provider::stream_simple`] 内部：宿主桥接
//! （`rpi_plugin_sdk::provider::drive`）会用**自己解析出的** `SimpleStreamOptions`
//! 调用 provider，factory 里对 options 的修改不会传到请求上。
//!
//! 目录结构：
//! - [`factory`]：把宿主模型/配置包装成 provider（含构造失败兜底）
//! - [`header_provider`]：每次请求前注入 header 的包装层
//! - [`error_provider`]：把构造错误转成正常 `Error` 终态的兜底 provider

mod error_provider;
mod factory;
mod header_provider;

use crate::error::GoError;
use rpi_plugin_sdk::PluginApi;

/// 注册时使用的协议风格，宿主按 provider id + api_style 匹配扩展实现。
const API_STYLE: &str = "openai-completions";

// ===========================================================================
// Feature 入口
// ===========================================================================

/// `provider` feature 的注册入口。
///
/// 参数：`api` 为宿主的插件 API 表。
/// 返回：`0` 表示注册完成（含"没有匹配的 provider，无事可做"）；非 `0` 表示宿主
/// 注册失败，由 [`crate::registry::register_all`] 上报。
///
/// 不调用 `rpi_plugin_sdk::provider::register()`：那会 `declare` 一份模型目录，
/// 可能覆盖 `models.json` 里用户自己维护的模型定义。
pub fn register(api: &PluginApi) -> i32 {
    // 第一步：`opencodeExtras.provider` 显式关闭时完全不包装任何 provider。
    // 注意：关闭后 opencode.ai 端点会因缺少 `x-opencode-session` 返回 400。
    if !crate::settings::toggles().provider {
        return 0;
    }

    // 第二步：找出需要包装的 provider。配置缺失时插件照常加载，宿主继续用原有配置。
    let specs = match crate::config::discover_opencode_providers() {
        Ok(specs) => specs,
        Err(error) => {
            eprintln!(
                "rpi-opencode-extras: {}, no provider wrapped",
                error.describe()
            );
            return 0;
        }
    };

    // 第二步：没有匹配 provider 时什么也不做（工具数、模型目录都不变）。
    if specs.is_empty() {
        return 0;
    }

    // 第三步：确认宿主至少有一个可用的注册槽位。
    if api.register_streaming_provider.is_none() && api.register_provider.is_none() {
        eprintln!(
            "rpi-opencode-extras: {}",
            GoError::Unsupported("host exposes no provider registration slot".into()).describe()
        );
        return 0;
    }

    // 第四步：逐个注册。优先流式槽位，缺失时降级为一次性槽位。
    let free = rpi_plugin_sdk::callbacks::free_string;
    for spec in specs {
        let id = rpi_plugin_sdk::StbStringRef::from_str(&spec.id);
        let base_url = rpi_plugin_sdk::StbStringRef::from_str(&spec.base_url);
        let style = rpi_plugin_sdk::StbStringRef::from_str(API_STYLE);

        let rc = match api.register_streaming_provider {
            Some(register) => register(id, base_url, style, stream, free, std::ptr::null_mut()),
            None => match api.register_provider {
                Some(register) => {
                    register(id, base_url, style, request, free, std::ptr::null_mut())
                }
                None => return 0,
            },
        };

        if rc != 0 {
            eprintln!(
                "rpi-opencode-extras: {}",
                GoError::Unsupported(format!(
                    "host rejected provider registration for {} (rc={rc})",
                    spec.id
                ))
                .describe()
            );
            return rc;
        }
    }

    0
}

// ===========================================================================
// ABI 请求入口
// ===========================================================================

/// 一次性的（非流式）请求入口。
///
/// 参数与 [`rpi_plugin_sdk::ProviderRequestFn`] 一致：借用请求 JSON，写出最终消息。
/// 仅当宿主缺少 `register_streaming_provider` 槽位时才会被调用。
extern "C" fn request(
    req_json: rpi_plugin_sdk::StbStringRef,
    out: *mut rpi_plugin_sdk::StbString,
    _user_data: *mut std::ffi::c_void,
) -> i32 {
    let factory: rpi_plugin_sdk::provider::Factory = factory::factory;
    // SAFETY: 宿主保证 req_json 在本次调用期间有效、out 可写。
    unsafe {
        rpi_plugin_sdk::provider::drive(
            factory,
            req_json,
            None,
            std::ptr::null_mut(),
            std::ptr::null(),
            out,
        )
    }
}

/// 流式请求入口（与 `export_provider!` 宏展开一致）。
///
/// 参数与 [`rpi_plugin_sdk::ProviderStreamRequestFn`] 一致：逐块 `emit`，最后把
/// 完整消息写进 `out`。`completion` 在当前宿主恒为 `None`，桥接层不使用它。
extern "C" fn stream(
    req_json: rpi_plugin_sdk::StbStringRef,
    emit: Option<rpi_plugin_sdk::ProviderStreamEmitFn>,
    emit_user_data: *mut std::ffi::c_void,
    control: *const rpi_plugin_sdk::ProviderStreamControl,
    _completion: Option<rpi_plugin_sdk::ProviderStreamCompletionFn>,
    _completion_user_data: *mut std::ffi::c_void,
    out: *mut rpi_plugin_sdk::StbString,
    _user_data: *mut std::ffi::c_void,
) -> i32 {
    let factory: rpi_plugin_sdk::provider::Factory = factory::factory;
    // SAFETY: 宿主保证 req_json / control 在调用期间有效，emit 只在本次调用内使用。
    unsafe {
        rpi_plugin_sdk::provider::drive(factory, req_json, emit, emit_user_data, control, out)
    }
}
