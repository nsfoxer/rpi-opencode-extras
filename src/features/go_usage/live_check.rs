//! 联网集成检查（`#[ignore]`，默认不跑）。
//!
//! 默认测试套件必须离线可跑，因此这里把真实网络调用隔离在 `#[ignore]` 后面。
//! 需要显式执行：
//!
//! ```sh
//! env -u RPI_CODING_AGENT_DIR cargo test --offline -- --ignored --nocapture live_
//! ```
//!
//! 它验证的是"整条链路真能跑通"：从 `models.json` 取凭据 → 打 `/usage` →
//! 解析 → 渲染页脚文本。单测只覆盖纯函数，这一段必须靠真实端点证明。

use crate::features::go_usage::api_key::resolve_api_key;
use crate::features::go_usage::format::{format_footer, parse_rfc3339_unix};

/// 真实拉取一次用量并渲染页脚。
///
/// 断言：
/// 1. 能从 `models.json` 解析出 opencode.ai 凭据；
/// 2. `/usage` 返回 200 且能被解析（至少一个月度窗口）；
/// 3. 渲染出的文本以 `Go ` 开头且含 `%`；
/// 4. 文本里**不含**任何凭据内容（防止 key 泄漏到 UI）。
#[test]
#[ignore = "requires network and a real opencode.ai apiKey in models.json"]
fn live_usage_fetch_renders_a_footer_line() {
    // 测试构建启用 `test` cfg 时，本模块只能被编译不被使用；真实调用在函数体内，
    // 所以 import 放在函数里，避免非测试构建下的 unused-import 警告。
    use crate::features::go_usage::fetch::fetch_usage;

    // 第一步：解析凭据。
    let (api_key, base_url) = resolve_api_key().expect("models.json must hold an opencode.ai apiKey");

    // 第二步：真实请求。
    let snapshot = fetch_usage(&api_key).expect("usage endpoint must answer 200");

    // 第三步：至少要有月度窗口（端点契约）。
    let monthly = snapshot.monthly.as_ref().expect("monthly window must be present");
    assert!(monthly.percent <= 100, "percent must be clamped: {monthly:?}");

    // 第四步：渲染并检查形状。
    let now = parse_rfc3339_unix("2026-11-13T07:24:00Z").expect("fixture timestamp");
    let text = format_footer(&snapshot, now);
    println!("base_url = {base_url}");
    println!("snapshot = {snapshot:?}");
    println!("footer   = {text}");

    assert!(text.starts_with("Go "), "footer must start with `Go `: {text}");
    assert!(text.contains('%'), "footer must contain a percent: {text}");

    // 第五步：凭据绝不能出现在展示文本里。
    assert!(!text.contains(&api_key), "api key must never leak into the footer");
    assert!(!text.contains("oc_sk_"), "api key prefix must never leak into the footer");
}
