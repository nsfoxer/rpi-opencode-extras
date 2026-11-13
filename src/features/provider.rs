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

use crate::error::GoError;
use crate::headers::{build_headers, is_opencode_ai_url};
use async_trait::async_trait;
use rpi_ai::event_stream::create_assistant_message_event_stream_with_cancellation;
use rpi_ai::{
    AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream, Context, ErrorReason,
    Model, Provider, SimpleStreamOptions, StopReason,
};
use rpi_plugin_sdk::PluginApi;
use std::sync::Arc;

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
    // 第一步：找出需要包装的 provider。配置缺失时插件照常加载，宿主继续用原有配置。
    let specs = match crate::config::discover_opencode_providers() {
        Ok(specs) => specs,
        Err(error) => {
            eprintln!("rpi-opencode: {}, no provider wrapped", error.describe());
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
            "rpi-opencode: {}",
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
                "rpi-opencode: {}",
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
// Factory 与包装层
// ===========================================================================

/// 本插件的 provider factory。
///
/// 参数：
/// - `model`：宿主解析出的模型（含 baseUrl，决定是否注入 header）。
/// - `options`：本次请求的执行配置（含 apiKey / headers / session id）。
///
/// 返回：openai-completions 实现外层套一个 [`HeaderInjectingProvider`]；构造失败时
/// 返回 [`ErrorProvider`]，把失败作为 Error 终态交给宿主，而不是 panic 或吞掉。
fn factory(model: &Model, options: &SimpleStreamOptions) -> Arc<dyn Provider> {
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

/// header 注入包装层：在每次请求前把 opencode.ai 的专属 header 合并进 options，
/// 再交给内层 provider。
///
/// 之所以不直接在 factory 里改 options：宿主桥接调用 provider 时传入的是它自己
/// 解析出的 options，factory 的修改不会生效。
pub struct HeaderInjectingProvider {
    /// 真正发请求的 provider（宿主 SDK 的 openai-completions 实现）。
    inner: Arc<dyn Provider>,
}

impl HeaderInjectingProvider {
    /// 包装一个内层 provider。
    ///
    /// 参数：`inner` 为实际执行请求的 provider。
    pub fn new(inner: Arc<dyn Provider>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl Provider for HeaderInjectingProvider {
    /// id 与内层一致，宿主/诊断看到的仍是它注册的那个 provider。
    fn id(&self) -> &str {
        self.inner.id()
    }

    /// 模型目录与内层一致。
    fn models(&self) -> &[Model] {
        self.inner.models()
    }

    /// 注入 header 后转发给内层 provider。
    ///
    /// 参数：`model`/`ctx`/`opts` 与内层同签名；只有 `model.base_url` 参与判定。
    /// 返回：内层 provider 的事件流。
    async fn stream_simple(
        &self,
        model: &Model,
        ctx: &Context,
        opts: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        // 第一步：clone 一份配置，只改自己这份副本。
        let mut opts = opts.clone();

        // 第二步：仅 opencode.ai 端点注入 header；其他端点原样透传。
        if is_opencode_ai_url(&model.base_url) {
            opts.headers = Some(build_headers(
                opts.headers.clone(),
                opts.session_id.as_deref(),
                &model.id,
            ));
        }

        // 第三步：交给内层 provider 真正发请求。
        self.inner.stream_simple(model, ctx, &opts).await
    }
}

// ===========================================================================
// 错误兜底 provider
// ===========================================================================

/// 一个"只会失败"的 provider，用于把传输层构造错误转成正常的 Error 终态。
///
/// 宿主只通过事件流观察失败（`Provider::stream_simple` 不返回 `Result`），
/// 因此这里产出一个 `Error` 事件并立即结束，不再 panic。
pub struct ErrorProvider {
    /// 失败原因，原样写进 `AssistantMessage::error_message`。
    message: String,
    /// 模型目录：错误 provider 不提供任何模型。
    models: Vec<Model>,
}

impl ErrorProvider {
    /// 用失败原因构造错误 provider。
    ///
    /// 参数：`message` 为底层组件的错误文本（例如 HTTP 客户端构造失败）。
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            models: Vec::new(),
        }
    }
}

#[async_trait]
impl Provider for ErrorProvider {
    /// 固定 id：该 provider 不与任何真实 provider 竞争模型目录。
    fn id(&self) -> &str {
        "rpi-opencode-error"
    }

    /// 错误 provider 不声明任何模型。
    fn models(&self) -> &[Model] {
        &self.models
    }

    /// 产生一个 `Error` 终态事件后结束。
    ///
    /// 参数：`model`/`ctx`/`opts` 与真实 provider 同签名；只有 `model` 用于
    /// 填充终态消息的 api / provider / model 字段。
    async fn stream_simple(
        &self,
        model: &Model,
        _ctx: &Context,
        opts: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        // 第一步：建立事件流，取消信号继承自本次请求。
        let (mut producer, stream) =
            create_assistant_message_event_stream_with_cancellation(&opts.signal);

        // 第二步：构造 StopReason::Error 的终态消息（无内容、无 usage）。
        let error = AssistantMessage::terminal(
            model.api.clone(),
            model.provider.clone(),
            model.id.clone(),
            StopReason::Error,
            self.message.clone(),
            0,
        );

        // 第三步：提交 Error 事件。调用方已取消或已关闭时 push 返回 false，
        // 此时请求本就无人接收，无需额外处理（生产者 drop 时释放剩余资源）。
        producer
            .push(AssistantMessageEvent::Error {
                reason: ErrorReason::Error,
                error,
            })
            .await;

        stream
    }
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
    let factory: rpi_plugin_sdk::provider::Factory = factory;
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
    let factory: rpi_plugin_sdk::provider::Factory = factory;
    // SAFETY: 宿主保证 req_json / control 在调用期间有效，emit 只在本次调用内使用。
    unsafe {
        rpi_plugin_sdk::provider::drive(factory, req_json, emit, emit_user_data, control, out)
    }
}

// ===========================================================================
// 测试
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::HeaderInjectingProvider;
    use async_trait::async_trait;
    use rpi_ai::event_stream::create_assistant_message_event_stream_with_cancellation;
    use rpi_ai::{AssistantMessageEventStream, Context, Model, Provider, SimpleStreamOptions};
    use std::collections::BTreeMap;
    use std::future::Future;
    use std::sync::{Arc, Mutex};
    use std::task::{Context as TaskContext, Poll, Wake, Waker};

    /// 单次 poll 的极简 executor：被测 future 不会挂起，所以不需要真正的运行时。
    fn block_on<F: Future>(future: F) -> Option<F::Output> {
        struct NoopWake;
        impl Wake for NoopWake {
            fn wake(self: Arc<Self>) {}
        }
        let waker = Waker::from(Arc::new(NoopWake));
        let mut task = TaskContext::from_waker(&waker);
        let mut future = Box::pin(future);
        match future.as_mut().poll(&mut task) {
            Poll::Ready(value) => Some(value),
            Poll::Pending => None,
        }
    }

    /// 捕获内层实际收到的 header，用于断言注入发生在 `stream_simple` 内。
    struct CapturingProvider {
        models: Vec<Model>,
        seen: Arc<Mutex<Option<BTreeMap<String, String>>>>,
    }

    #[async_trait]
    impl Provider for CapturingProvider {
        fn id(&self) -> &str {
            "capturing"
        }

        fn models(&self) -> &[Model] {
            &self.models
        }

        async fn stream_simple(
            &self,
            _model: &Model,
            _ctx: &Context,
            opts: &SimpleStreamOptions,
        ) -> AssistantMessageEventStream {
            *self.seen.lock().expect("capture lock") = opts.headers.clone();
            let (producer, stream) =
                create_assistant_message_event_stream_with_cancellation(&opts.signal);
            drop(producer);
            stream
        }
    }

    fn model(base_url: &str) -> Model {
        Model::new("m", "M", rpi_ai::Api::OpenaiCompletions, "probe", base_url)
    }

    #[test]
    fn streaming_injects_session_and_user_agent() {
        let seen = Arc::new(Mutex::new(None));
        let provider = HeaderInjectingProvider::new(Arc::new(CapturingProvider {
            models: Vec::new(),
            seen: Arc::clone(&seen),
        }));
        let options = SimpleStreamOptions {
            session_id: Some("01a1d2bb".into()),
            ..SimpleStreamOptions::default()
        };

        let done = block_on(provider.stream_simple(
            &model("https://opencode.ai/inference/go/openai/v1"),
            &Context::new(Vec::new()),
            &options,
        ));
        assert!(done.is_some(), "stream_simple must resolve immediately");

        let headers = seen
            .lock()
            .expect("capture lock")
            .clone()
            .expect("inner provider must receive headers");
        assert_eq!(headers["x-opencode-session"], "ses_01a1d2bb");
        assert_eq!(headers["user-agent"], crate::headers::USER_AGENT);
    }

    #[test]
    fn other_endpoints_pass_through_untouched() {
        let seen = Arc::new(Mutex::new(None));
        let provider = HeaderInjectingProvider::new(Arc::new(CapturingProvider {
            models: Vec::new(),
            seen: Arc::clone(&seen),
        }));

        let done = block_on(provider.stream_simple(
            &model("https://api.deepseek.com/"),
            &Context::new(Vec::new()),
            &SimpleStreamOptions::default(),
        ));
        assert!(done.is_some(), "stream_simple must resolve immediately");

        let headers = seen.lock().expect("capture lock").clone();
        assert!(headers.is_none(), "no header for non-opencode hosts");
    }
}
