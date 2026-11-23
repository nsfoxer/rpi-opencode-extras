//! `/usage` 响应的数据模型与纯解析逻辑。
//!
//! 这一层不做 IO：输入响应正文，输出 [`UsageSnapshot`]，便于单测覆盖各种畸形输入。

use serde_json::Value;

/// 一个用量窗口（5 小时 / 周 / 月）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageWindow {
    /// 该窗口已用百分比（API 给整数；越界时被截断到 0..=100）。
    pub percent: u32,
    /// 窗口重置时刻（RFC 3339 UTC 字符串，如 `2026-11-16T01:21:47.000Z`）。
    /// 缺失或非法时为 `None`，展示层据此省略倒计时。
    pub resets_at: Option<String>,
}

/// 一次查询拿到的完整用量快照。
///
/// 三个窗口都可能缺失：API 只返回部分字段时，缺失的窗口为 `None`，
/// 展示层跳过对应片段，而不是显示 0%（0% 是真实值，缺失不是）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UsageSnapshot {
    /// 5 小时滚动窗口。
    pub rolling: Option<UsageWindow>,
    /// 周窗口。
    pub weekly: Option<UsageWindow>,
    /// 月度窗口。
    pub monthly: Option<UsageWindow>,
}

/// 从 `/usage` 响应正文解析用量快照。
///
/// 参数：`body` 为 HTTP 响应正文（期望形如 `{"usage":{"rolling":{...},...}}`）。
/// 返回：`Ok(快照)`；正文不是合法 JSON 或缺少 `usage` 对象时为 `Err(原因)`。
/// 单个窗口字段畸形（`percent` 缺失/非数字）只会让该窗口变成 `None`，不会整体失败。
pub fn parse_usage(body: &str) -> Result<UsageSnapshot, String> {
    // 第一步：整体必须是合法 JSON 对象。
    let root: Value =
        serde_json::from_str(body).map_err(|error| format!("invalid usage JSON: {error}"))?;

    // 第二步：必须存在 usage 对象；没有它说明不是这个端点（例如打到了网关首页）。
    let usage = root
        .get("usage")
        .and_then(Value::as_object)
        .ok_or_else(|| "usage object is missing".to_string())?;

    // 第三步：逐窗口解析，缺失或畸形只影响该窗口。
    Ok(UsageSnapshot {
        rolling: parse_window(usage.get("rolling")),
        weekly: parse_window(usage.get("weekly")),
        monthly: parse_window(usage.get("monthly")),
    })
}

/// 解析单个窗口对象。
///
/// 参数：`value` 为窗口的 JSON 值（可能为 `None`）。
/// 返回：`percent` 可解析时返回窗口；否则 `None`（视为该窗口未上报）。
fn parse_window(value: Option<&Value>) -> Option<UsageWindow> {
    let object = value?.as_object()?;

    // percent 是唯一必需字段；越界值截断到 0..=100，避免展示出 130% 这类脏数据。
    let raw = object.get("percent")?;
    let percent = raw.as_u64().or_else(|| {
        // 容忍浮点上报（API 未来可能改成 12.5），按截断取整。
        raw.as_f64().map(|v| v.max(0.0) as u64)
    })?;

    // resetsAt 是可选字段：缺失则展示层省略倒计时。
    let resets_at = object
        .get("resetsAt")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string);

    Some(UsageWindow {
        percent: percent.min(100) as u32,
        resets_at,
    })
}

#[cfg(test)]
mod tests {
    use super::{parse_usage, UsageSnapshot, UsageWindow};

    #[test]
    fn parses_the_live_response_shape() {
        // 真实响应（本机 curl 取得），三个窗口齐全。
        let body = r#"{"usage":{
            "rolling":{"status":"ok","percent":0,"resetsAt":"2026-11-13T12:02:55.000Z"},
            "weekly":{"status":"ok","percent":22,"resetsAt":"2026-11-16T00:00:00.000Z"},
            "monthly":{"status":"ok","percent":65,"resetsAt":"2026-11-16T01:21:47.000Z"}}}"#;
        let snapshot = parse_usage(body).expect("must parse");
        assert_eq!(
            snapshot.rolling,
            Some(UsageWindow {
                percent: 0,
                resets_at: Some("2026-11-13T12:02:55.000Z".to_string())
            })
        );
        assert_eq!(snapshot.weekly.as_ref().map(|w| w.percent), Some(22));
        assert_eq!(
            snapshot.monthly,
            Some(UsageWindow {
                percent: 65,
                resets_at: Some("2026-11-16T01:21:47.000Z".to_string())
            })
        );
    }

    #[test]
    fn missing_windows_stay_none_instead_of_zero() {
        let body = r#"{"usage":{"monthly":{"percent":42}}}"#;
        let snapshot = parse_usage(body).expect("must parse");
        // 缺失窗口必须是 None：0% 是真实值，不能与"未上报"混淆。
        assert!(snapshot.rolling.is_none());
        assert!(snapshot.weekly.is_none());
        assert_eq!(snapshot.monthly.as_ref().map(|w| w.percent), Some(42));
        // 没有 resetsAt 时该字段为 None。
        assert!(snapshot.monthly.unwrap().resets_at.is_none());
    }

    #[test]
    fn malformed_window_does_not_fail_the_whole_snapshot() {
        let body = r#"{"usage":{
            "rolling":{"percent":"not-a-number"},
            "weekly":{"status":"ok"},
            "monthly":{"percent":7,"resetsAt":"2026-12-01T00:00:00.000Z"}}}"#;
        let snapshot = parse_usage(body).expect("must parse");
        assert!(snapshot.rolling.is_none());
        assert!(snapshot.weekly.is_none());
        assert_eq!(snapshot.monthly.as_ref().map(|w| w.percent), Some(7));
    }

    #[test]
    fn percent_is_clamped_and_float_tolerated() {
        let body = r#"{"usage":{
            "rolling":{"percent":130},
            "weekly":{"percent":12.7},
            "monthly":{"percent":-5}}}"#;
        let snapshot = parse_usage(body).expect("must parse");
        assert_eq!(snapshot.rolling.as_ref().map(|w| w.percent), Some(100));
        assert_eq!(snapshot.weekly.as_ref().map(|w| w.percent), Some(12));
        // 负数 as_u64() 为 None、as_f64() 后 max(0.0) 得 0。
        assert_eq!(snapshot.monthly.as_ref().map(|w| w.percent), Some(0));
    }

    #[test]
    fn blank_resets_at_is_treated_as_absent() {
        let body = r#"{"usage":{"monthly":{"percent":5,"resetsAt":"   "}}}"#;
        let snapshot = parse_usage(body).expect("must parse");
        assert!(snapshot.monthly.unwrap().resets_at.is_none());
    }

    #[test]
    fn rejects_non_json_and_foreign_payloads() {
        assert!(parse_usage("").is_err());
        assert!(parse_usage("<html>hi</html>").is_err());
        // 401 响应体没有 usage 对象，必须报错而不是当成全空快照。
        let auth_error =
            r#"{"type":"error","error":{"type":"AuthError","message":"Unauthorized"}}"#;
        assert!(parse_usage(auth_error).is_err());
        assert!(parse_usage(r#"{"usage":[]}"#).is_err());
    }

    #[test]
    fn default_snapshot_has_no_windows() {
        assert_eq!(
            UsageSnapshot::default(),
            UsageSnapshot {
                rolling: None,
                weekly: None,
                monthly: None,
            }
        );
    }
}
