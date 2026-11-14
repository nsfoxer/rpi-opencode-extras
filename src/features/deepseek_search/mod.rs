//! `deepseek_search`：通过 DeepSeek Search API 检索网页并返回排序结果。
//!
//! 生命周期契约（宿主驱动）：
//! `execute` 解析参数并起工作线程，立即返回句柄；`poll` 非阻塞轮询；
//! `cancel` 只置取消标记（幂等）；`destroy` 释放句柄（空指针/重复调用 no-op）。
//!
//! 网络调用只发生在工作线程内，`execute` / `poll` / `cancel` / `destroy` 都不会阻塞宿主。
//! API key 只在工作线程内存里短暂存在：不写入日志、不进入工具输出、不回显。
//! 所有 `extern "C"` 边界都用 `rpi_plugin_sdk::guard*` 包住，panic 不会 unwind 到宿主。
//!
//! 合并自独立的 `rpi-deepseek-search` 项目，改为本插件的 feature 模块。目录结构：
//! - [`params`]：入参解析与契约校验
//! - [`api_key`]：凭据解析（环境变量 → `models.json`）
//! - [`search`]：HTTP 调用、重试策略、结果解析与格式化
//! - [`worker`]：工作线程、句柄表、四个生命周期函数

mod api_key;
mod params;
mod search;
mod worker;

use rpi_plugin_sdk::callbacks::free_string as plugin_free_string;
use rpi_plugin_sdk::{PluginApi, StableToolSchema, StbString, StbStringRef};

/// 工具名（同时也是 schema 名）。
pub const TOOL_NAME: &str = "deepseek_search";

/// 工具描述（给模型看的用途说明）。
const TOOL_DESCRIPTION: &str = "Search the web through the DeepSeek Search API and return ranked \
web results (title / url / date / authority / summary).";

/// 工具参数 JSON Schema。
const TOOL_PARAMETERS: &str = r#"{
  "type": "object",
  "properties": {
    "query":        { "type": "string",  "description": "搜索词，1-128 字符" },
    "count":        { "type": "integer", "description": "返回条数，1-50，默认 5" },
    "need_content": { "type": "boolean", "description": "是否返回页面正文，默认 false" }
  },
  "required": ["query"]
}"#;

/// 工具提示词元数据（可选槽位；旧宿主缺失时跳过，工具本身仍可用）。
const TOOL_PROMPT_METADATA: &str = r#"{"promptSnippet":"Search the web via DeepSeek Search API","promptGuidelines":["Use deepseek_search when the user needs current web information or exact page URLs"]}"#;

/// `deepseek_search` feature 的注册入口。
///
/// 参数：`api` 宿主的插件 API 表。
/// 返回：`0` 表示注册完成；`1` 表示宿主没有工具注册槽位；其他非 0 为宿主拒绝注册的
/// 返回码（由 [`crate::registry::register_all`] 上报）。
pub fn register(api: &PluginApi) -> i32 {
    // 第一步：`opencodeExtras.deepseekSearch=false` 时不注册工具。
    if !crate::settings::toggles().deepseek_search {
        return 0;
    }

    // 第二步：工具注册是唯一硬依赖；宿主未提供该槽位时直接报错，避免静默无工具。
    let Some(register_tool) = api.register_tool else {
        return 1;
    };

    // 第二步：注册工具（schema + 四个生命周期回调）。
    let schema = Box::new(StableToolSchema {
        name: StbString::from_string(TOOL_NAME.to_string()),
        description: StbString::from_string(TOOL_DESCRIPTION.to_string()),
        parameters: StbString::from_string(TOOL_PARAMETERS.to_string()),
    });
    let rc = register_tool(
        &*schema as *const StableToolSchema,
        worker::execute_tool,
        worker::poll,
        worker::cancel,
        worker::destroy,
        plugin_free_string,
    );
    // 宿主已把 schema 字段复制走并通过 plugin_free_string 释放，这里只回收容器。
    drop(schema);
    if rc != 0 {
        return rc;
    }

    // 第三步：追加提示词摘要/规则（可选槽位）。
    if let Some(register_tool_prompt) = api.register_tool_prompt {
        let rc = register_tool_prompt(
            StbStringRef::from_str(TOOL_NAME),
            StbStringRef::from_str(TOOL_PROMPT_METADATA),
        );
        if rc != 0 {
            return rc;
        }
    }

    0
}
