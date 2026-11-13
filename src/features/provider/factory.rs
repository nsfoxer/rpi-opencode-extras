//! provider factory：按一次请求的 model / options 构造出可用的 [`Provider`]。
//!
//! 构造失败不 panic：转成 [`ErrorProvider`]，把失败作为 `Error` 终态交回宿主。

use crate::error::GoError;
use crate::features::provider::error_provider::ErrorProvider;
use crate::features::provider::header_provider::HeaderInjectingProvider;
use crate::headers::{build_headers, is_opencode_ai_url};
use rpi_ai::{Model, Provider, SimpleStreamOptions};
use std::sync::Arc;

/// 本插件的 provider factory。
///
/// 参数：
/// - `model`：宿主解析出的模型（含 baseUrl，决定是否注入 header）。
/// - `options`：本次请求的执行配置（含 apiKey / headers / session id）。
///
/// 返回：openai-completions 实现外层套一个 [`HeaderInjectingProvider`]；构造失败时
/// 返回 [`ErrorProvider`]，把失败作为 Error 终态交给宿主，而不是 panic 或吞掉。
pub fn factory(model: &Model, options: &SimpleStreamOptions) -> Arc<dyn Provider> {
    // 第一步：clone 一份本次请求的配置，避免改动宿主传入的对象。
    let mut options = options.clone();

    // 第二步：只有 opencode.ai 的端点才注入专属 header。
    // 这一步保证构造期（例如客户端代理配置）看到的是最终 header；
    // 每次请求的实际注入由包装层完成，见 `HeaderInjectingProvider`。
    if is_opencode_ai_url(&model.base_url) {
        options.headers = Some(build_headers(
            options.headers.clone(),
            options.session_id.as_deref(),
            &model.id,
        ));
    }

    // 第三步：复用宿主维护的 OpenAI Chat Completions 协议实现。
    match rpi_plugin_sdk::provider::completions(model, &options, false) {
        Ok(inner) => Arc::new(HeaderInjectingProvider::new(inner)),
        // 构造失败属于传输层问题：分类后交给 ErrorProvider 转成 Error 终态。
        Err(error) => Arc::new(ErrorProvider::new(GoError::Http(error).describe())),
    }
}
