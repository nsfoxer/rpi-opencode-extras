//! 错误兜底 provider：把传输层构造错误转成正常的 `Error` 终态事件。
//!
//! 宿主只通过事件流观察失败（`Provider::stream_simple` 不返回 `Result`），
//! 因此这里产出一个 `Error` 事件并立即结束，不 panic。

use async_trait::async_trait;
use rpi_ai::event_stream::create_assistant_message_event_stream_with_cancellation;
use rpi_ai::{
    AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream, Context, ErrorReason,
    Model, Provider, SimpleStreamOptions, StopReason,
};

/// 一个"只会失败"的 provider。
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
        "rpi-opencode-extras-error"
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
