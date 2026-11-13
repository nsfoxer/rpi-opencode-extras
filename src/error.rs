//! 插件统一错误类型。
//!
//! 所有模块只产出 [`GoError`]，不在各自代码里零散 `format!` 出错误字符串，
//! 这样错误来源（配置 / 传输 / 宿主能力缺失）始终可区分。

use std::fmt;

/// 插件内部的错误分类。
///
/// - `Config`：读不到或解析不了宿主配置（例如 `models.json`）。
/// - `Http`：传输层或协议构造失败。
/// - `Unsupported`：宿主缺少本插件需要的注册槽位 / 能力。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoError {
    /// 配置读取或解析失败。
    Config(String),
    /// 传输层或请求构造失败。
    Http(String),
    /// 宿主不具备所需能力。
    Unsupported(String),
}

impl GoError {
    /// 错误分类的固定短标签，便于日志里快速定位来源。
    pub fn kind(&self) -> &'static str {
        match self {
            GoError::Config(_) => "config",
            GoError::Http(_) => "http",
            GoError::Unsupported(_) => "unsupported",
        }
    }

    /// 分类标签 + 明细，用于诊断输出。
    ///
    /// 返回：形如 `config: cannot read ...` 的字符串。
    pub fn describe(&self) -> String {
        format!("{}: {self}", self.kind())
    }
}

impl fmt::Display for GoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GoError::Config(detail) => write!(f, "{detail}"),
            GoError::Http(detail) => write!(f, "{detail}"),
            GoError::Unsupported(detail) => write!(f, "{detail}"),
        }
    }
}

impl std::error::Error for GoError {}

#[cfg(test)]
mod tests {
    use super::GoError;

    #[test]
    fn describe_prefixes_the_error_kind() {
        assert_eq!(
            GoError::Config("boom".into()).describe(),
            "config: boom"
        );
        assert_eq!(GoError::Http("502".into()).describe(), "http: 502");
        assert_eq!(
            GoError::Unsupported("no slot".into()).describe(),
            "unsupported: no slot"
        );
    }
}
