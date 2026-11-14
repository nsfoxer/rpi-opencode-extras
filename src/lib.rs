//! rpi-opencode-extras：给所有 baseUrl 指向 opencode.ai 的 provider 注入
//! `x-opencode-session` 与 `user-agent` 请求头。
//!
//! 只产出一个 cdylib、只调用一次 [`rpi_plugin_sdk::export_plugin!`]。本文件只做入口
//! 装配，具体职责分散在：
//! - [`features`]：可插拔能力列表（provider / msys2-bash / deepseek-search /
//!   go-usage / notify），每个能力一个模块。
//! - [`registry`]：按 [`features::FEATURES`] 顺序执行注册，失败即中止。
//! - [`settings`]：`settings.json` 的 `opencodeExtras` 开关（缺省全开）。
//! - [`config`]：定位 agent 目录、解析 `models.json`、按 URL 筛选 provider（共享核心）。
//! - [`headers`]：纯函数的 header 构造与域名判定（共享核心）。
//! - [`error`]：统一错误类型（共享核心）。

mod config;
mod error;
mod features;
mod headers;
mod registry;
mod settings;

rpi_plugin_sdk::export_plugin!(|api| registry::register_all(api));
