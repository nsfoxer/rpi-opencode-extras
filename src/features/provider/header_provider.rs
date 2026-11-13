//! header 注入包装层：在每次请求前把 opencode.ai 的专属 header 合并进 options，
//! 再交给内层 provider。
//!
//! 之所以不直接在 factory 里改 options：宿主桥接调用 provider 时传入的是它自己
//! 解析出的 options，factory 的修改不会生效。

use crate::headers::{build_headers, is_opencode_ai_url};
use async_trait::async_trait;
use rpi_ai::{
    AssistantMessageEventStream, Context, Model, Provider, SimpleStreamOptions,
};
use std::sync::Arc;

/// 包装层：注入 header 后转发给内层 provider。
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
