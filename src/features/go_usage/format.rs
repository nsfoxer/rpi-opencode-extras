//! 用量快照 → 页脚单行文本的纯函数。
//!
//! 这一层不做 IO、不读系统时钟（`now` 由调用方传入），因此倒计时逻辑可以
//! 用固定时间点精确单测，不会因为跑测试的机器时间不同而偶发失败。

use crate::features::go_usage::usage::{UsageSnapshot, UsageWindow};

/// 月份窗口的中文标签。倒计时只跟月度到期走（用户明确要求月度到期时间）。
const MONTHLY_LABEL: &str = "月";

/// 窗口分隔符。
const SEPARATOR: &str = " · ";

/// 倒计时标记的前缀。
///
/// 用 `⟳`(U+27F3 CLOCKWISE GAPPED CIRCLE ARROW) 而不用 `↻`(U+21BB)：
/// 后者在部分终端字体里被当宽字符排版，会与紧跟的倒计时数字叠在一起。
/// `⟳` 在各种等宽字体里覆盖更广，且显式跟一个空格，双重保险。
const RESET_GLYPH: &str = "⟳ ";

/// 把用量快照渲染成页脚单行文本。
///
/// 参数：
/// - `snapshot`：解析后的用量快照。
/// - `now_unix`：当前时刻（UTC 秒），由调用方提供以便单测。
///
/// 返回：形如 `Go 5h 0% · 周 22% · 月 65% ⟳ 2d17h` 的单行文本。
///
/// 规则：
/// - 缺失的窗口直接跳过（不显示 0%，因为 0% 是真实值）。
/// - 月度窗口若带合法 `resetsAt`，在末尾追加 `⟳ <倒计时>`；否则省略。
/// - 三个窗口全缺失时返回空串，调用方据此不显示组件。
pub fn format_footer(snapshot: &UsageSnapshot, now_unix: i64) -> String {
    // 第一步：按固定顺序拼窗口片段，缺失的跳过。
    let mut parts: Vec<String> = Vec::new();
    if let Some(window) = &snapshot.rolling {
        parts.push(format_window("5h", window));
    }
    if let Some(window) = &snapshot.weekly {
        parts.push(format_window("周", window));
    }
    if let Some(window) = &snapshot.monthly {
        parts.push(format_window(MONTHLY_LABEL, window));
    }

    // 第二步：全缺失时返回空串（调用方不显示组件，而不是显示 "Go"）。
    if parts.is_empty() {
        return String::new();
    }

    // 第三步：月度到期倒计时追加在最末，只跟月度走。
    let countdown = snapshot
        .monthly
        .as_ref()
        .and_then(|window| window.resets_at.as_deref())
        .and_then(parse_rfc3339_unix)
        .map(|resets_at| format_reset_marker(resets_at - now_unix));

    match countdown {
        Some(marker) => format!("Go {} {marker}", parts.join(SEPARATOR)),
        None => format!("Go {}", parts.join(SEPARATOR)),
    }
}

/// 渲染单个窗口片段，如 `周 22%`。
///
/// 参数：`label` 窗口标签；`window` 该窗口数据。
/// 返回：`<label> <percent>%`。
fn format_window(label: &str, window: &UsageWindow) -> String {
    format!("{label} {}%", window.percent)
}

/// 把"距到期剩余秒数"渲染成 `⟳ <倒计时>` 标记。
///
/// 参数：`remaining_secs` 距到期的剩余秒数；负数或 0 表示已到期。
/// 返回：
/// - 已到期：`⟳ now`
/// - ≥ 1 天：`⟳ 2d17h`
/// - ≥ 1 小时：`⟳ 5h13m`
/// - 其余：`⟳ 47m`
///
/// 零值分量省略高位（`2d0h` 输出 `2d`），保持页脚宽度稳定。
pub fn format_reset_marker(remaining_secs: i64) -> String {
    // 第一步：已到期（含恰好 0）不再显示倒计时。
    if remaining_secs <= 0 {
        return format!("{RESET_GLYPH}now");
    }

    // 第二步：拆成天 / 时 / 分，向下取整（不四舍五入，避免显示"还有"却已过期）。
    let days = remaining_secs / 86_400;
    let hours = (remaining_secs % 86_400) / 3_600;
    let minutes = (remaining_secs % 3_600) / 60;

    // 第三步：按量级选择精度，零值分量不补位。
    let body = if days > 0 {
        if hours > 0 {
            format!("{days}d{hours}h")
        } else {
            format!("{days}d")
        }
    } else if hours > 0 {
        if minutes > 0 {
            format!("{hours}h{minutes}m")
        } else {
            format!("{hours}h")
        }
    } else {
        // 不足 1 分钟但仍未到期：显示 1m，避免和 now 混淆。
        format!("{}m", minutes.max(1))
    };

    format!("{RESET_GLYPH}{body}")
}

/// 解析 RFC 3339 时间戳为 UTC Unix 秒。
///
/// 参数：`text` 形如 `2026-11-16T01:21:47.000Z`、`2026-11-16T01:21:47Z`
/// 或带数字时区偏移的 `2026-11-16T09:21:47+08:00`。
/// 返回：`Some(unix 秒)`；格式不合法时 `None`（调用方据此省略倒计时）。
///
/// 只支持本端点实际会返回的形式；不支持秒以下精度的其它变体（如 `+08:00` 带小数秒），
/// 遇到即返回 `None`，宁可不显示也不显示错的倒计时。
pub fn parse_rfc3339_unix(text: &str) -> Option<i64> {
    let trimmed = text.trim();
    let bytes = trimmed.as_bytes();

    // 最短合法长度：`2026-11-16T01:21:47Z` = 20。
    if bytes.len() < 20 {
        return None;
    }

    // 第一步：固定位置的日期与时间字段。
    let year: i64 = trimmed.get(0..4)?.parse().ok()?;
    let month: i64 = trimmed.get(5..7)?.parse().ok()?;
    let day: i64 = trimmed.get(8..10)?.parse().ok()?;
    let hour: i64 = trimmed.get(11..13)?.parse().ok()?;
    let minute: i64 = trimmed.get(14..16)?.parse().ok()?;
    let second: i64 = trimmed.get(17..19)?.parse().ok()?;

    // 第二步：分隔符必须是 `-` `-` `T`/`t` `:` `:`，防止把任意 20 字节当时间。
    if bytes[4] != b'-' || bytes[7] != b'-' || bytes[13] != b':' || bytes[16] != b':' {
        return None;
    }
    if bytes[10] != b'T' && bytes[10] != b't' {
        return None;
    }

    // 第三步：字段取值范围校验（宽松但足以拦住 2026-13-45 这类明显错误）。
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }

    // 第四步：跳过可选小数秒，定位时区标记。
    let mut index = 19;
    if index < bytes.len() && bytes[index] == b'.' {
        index += 1;
        let digits_start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        // 小数点后必须至少有一位数字。
        if index == digits_start {
            return None;
        }
    }
    if index >= bytes.len() {
        return None;
    }

    // 第五步：时区偏移（秒）。`Z` 为 0；`+HH:MM` / `-HH:MM` 换算。
    let offset_secs = match bytes[index] {
        b'Z' | b'z' => {
            // Z 之后不应再有内容。
            if index + 1 != bytes.len() {
                return None;
            }
            0
        }
        sign @ (b'+' | b'-') => {
            let rest = trimmed.get(index + 1..)?;
            // 只接受 `HH:MM`。
            if rest.len() != 5 {
                return None;
            }
            let rest_bytes = rest.as_bytes();
            if rest_bytes[2] != b':' {
                return None;
            }
            let offset_hour: i64 = rest.get(0..2)?.parse().ok()?;
            let offset_minute: i64 = rest.get(3..5)?.parse().ok()?;
            if offset_hour > 23 || offset_minute > 59 {
                return None;
            }
            let magnitude = offset_hour * 3_600 + offset_minute * 60;
            if sign == b'+' {
                magnitude
            } else {
                -magnitude
            }
        }
        _ => return None,
    };

    // 第六步：民用日期 → Unix 秒（Howard Hinnant 的 days_from_civil 算法）。
    let days = days_from_civil(year, month, day);
    let local = days * 86_400 + hour * 3_600 + minute * 60 + second;
    // 本地时间减去偏移得到 UTC。
    Some(local - offset_secs)
}

/// 把民用日期（年 / 月 / 日）换算为"距 1970-01-01 的天数"。
///
/// 参数：`year` 年、`month` 月（1-12）、`day` 日（1-31）。
/// 返回：天数（1970-01-01 之前为负）。
///
/// 算法对公历有效、无闰年特判分支，是 Howard Hinnant 的公开算法。
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    // 把 3 月当作年初，使闰日落在年末，省掉闰年特判。
    let shifted_year = if month <= 2 { year - 1 } else { year };
    let era = if shifted_year >= 0 {
        shifted_year
    } else {
        shifted_year - 399
    } / 400;
    let year_of_era = shifted_year - era * 400;
    let shifted_month = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::{format_footer, format_reset_marker, parse_rfc3339_unix};
    use crate::features::go_usage::usage::{UsageSnapshot, UsageWindow};

    /// 构造一个完整的三窗口快照（数值取自本机真实响应）。
    fn live_snapshot() -> UsageSnapshot {
        UsageSnapshot {
            rolling: Some(UsageWindow {
                percent: 0,
                resets_at: Some("2026-11-13T12:02:55.000Z".to_string()),
            }),
            weekly: Some(UsageWindow {
                percent: 22,
                resets_at: Some("2026-11-16T00:00:00.000Z".to_string()),
            }),
            monthly: Some(UsageWindow {
                percent: 65,
                resets_at: Some("2026-11-16T01:21:47.000Z".to_string()),
            }),
        }
    }

    #[test]
    fn parses_the_reset_timestamp_from_the_live_response() {
        // 2026-11-16T01:21:47Z 的 Unix 秒（用独立工具核对过）。
        assert_eq!(
            parse_rfc3339_unix("2026-11-16T01:21:47.000Z"),
            Some(1_794_792_107)
        );
        // 无小数秒、带正偏移、带负偏移都要支持。
        assert_eq!(
            parse_rfc3339_unix("2026-11-16T01:21:47Z"),
            Some(1_794_792_107)
        );
        assert_eq!(
            parse_rfc3339_unix("2026-11-16T09:21:47+08:00"),
            Some(1_794_792_107)
        );
        assert_eq!(
            parse_rfc3339_unix("2026-11-15T20:21:47-05:00"),
            Some(1_794_792_107)
        );
    }

    #[test]
    fn rejects_malformed_timestamps() {
        assert!(parse_rfc3339_unix("").is_none());
        assert!(parse_rfc3339_unix("not a time").is_none());
        assert!(parse_rfc3339_unix("2026-11-16").is_none());
        // 分隔符错位。
        assert!(parse_rfc3339_unix("2026/11/16T01:21:47Z").is_none());
        // 月份越界。
        assert!(parse_rfc3339_unix("2026-13-16T01:21:47Z").is_none());
        // 缺少时区标记。
        assert!(parse_rfc3339_unix("2026-11-16T01:21:47").is_none());
        // Z 后面还有垃圾。
        assert!(parse_rfc3339_unix("2026-11-16T01:21:47ZX").is_none());
        // 小数秒没有数字。
        assert!(parse_rfc3339_unix("2026-11-16T01:21:47.Z").is_none());
    }

    #[test]
    fn countdown_prefers_days_then_hours_then_minutes() {
        // 2d17h = 2*86400 + 17*3600。
        assert_eq!(format_reset_marker(2 * 86_400 + 17 * 3_600), "⟳ 2d17h");
        // 整 2 天时省略 0h。
        assert_eq!(format_reset_marker(2 * 86_400), "⟳ 2d");
        // 不足一天用 h+m。
        assert_eq!(format_reset_marker(5 * 3_600 + 13 * 60), "⟳ 5h13m");
        assert_eq!(format_reset_marker(5 * 3_600), "⟳ 5h");
        // 不足一小时用 m，且最小显示 1m。
        assert_eq!(format_reset_marker(47 * 60), "⟳ 47m");
        assert_eq!(format_reset_marker(30), "⟳ 1m");
    }

    #[test]
    fn reset_marker_uses_a_glyph_safe_in_monospace_fonts() {
        // 回归保护：曾经用 ↻(U+21BB)，在部分终端里宽度算错，与倒计时数字重叠。
        // 这里锁定用 ⟳(U+27F3) 且跟一个空格，避免以后又被改回去。
        let marker = format_reset_marker(60);
        assert!(
            marker.starts_with('⟳'),
            "marker must start with U+27F3, got: {marker}"
        );
        assert!(
            !marker.starts_with('↻'),
            "must not regress to U+21BB (renders overlapped), got: {marker}"
        );
        // 字符与倒计时之间必须有空格，双重保险防重叠。
        assert_eq!(
            &marker[..('⟳'.len_utf8() + 1)],
            "⟳ ",
            "glyph must be followed by a space"
        );
        // 倒计时本体必须是纯 ASCII，字体覆盖无风险。
        assert!(
            marker[('⟳'.len_utf8() + 1)..].is_ascii(),
            "countdown body must be ASCII: {marker}"
        );
    }

    #[test]
    fn expired_or_zero_reset_shows_now() {
        assert_eq!(format_reset_marker(0), "⟳ now");
        assert_eq!(format_reset_marker(-1), "⟳ now");
        assert_eq!(format_reset_marker(-86_400), "⟳ now");
    }

    #[test]
    fn footer_matches_the_agreed_shape() {
        // 基准时刻取 2026-11-13T07:24:00Z；月度到期 2026-11-16T01:21:47Z。
        // 剩余 = 2d17h57m47s → 向下取整显示 ⟳ 2d17h。
        let now = parse_rfc3339_unix("2026-11-13T07:24:00Z").expect("valid");
        assert_eq!(
            format_footer(&live_snapshot(), now),
            "Go 5h 0% · 周 22% · 月 65% ⟳ 2d17h"
        );
    }

    #[test]
    fn footer_omits_absent_windows_and_countdown() {
        // 只有月度、且没有 resetsAt：不显示倒计时。
        let snapshot = UsageSnapshot {
            rolling: None,
            weekly: None,
            monthly: Some(UsageWindow {
                percent: 42,
                resets_at: None,
            }),
        };
        assert_eq!(format_footer(&snapshot, 0), "Go 月 42%");

        // 只有周窗口。
        let weekly_only = UsageSnapshot {
            rolling: None,
            weekly: Some(UsageWindow {
                percent: 7,
                resets_at: None,
            }),
            monthly: None,
        };
        assert_eq!(format_footer(&weekly_only, 0), "Go 周 7%");
    }

    #[test]
    fn footer_is_empty_when_no_window_is_reported() {
        let empty = UsageSnapshot::default();
        assert_eq!(format_footer(&empty, 0), "");

        // 月度存在但 resetsAt 非法：仍显示百分比，只是不带倒计时。
        let bad_reset = UsageSnapshot {
            rolling: None,
            weekly: None,
            monthly: Some(UsageWindow {
                percent: 3,
                resets_at: Some("garbage".to_string()),
            }),
        };
        assert_eq!(format_footer(&bad_reset, 0), "Go 月 3%");
    }

    #[test]
    fn footer_shows_now_when_monthly_already_reset() {
        // now 晚于月度到期时刻 → ⟳ now。
        let now = parse_rfc3339_unix("2026-11-16T02:00:00Z").expect("valid");
        assert_eq!(
            format_footer(&live_snapshot(), now),
            "Go 5h 0% · 周 22% · 月 65% ⟳ now"
        );
    }

    #[test]
    fn footer_text_fits_the_footer_width_budget() {
        // 页脚组件 width 上限 240 列；约定文本必须远小于该值。
        let now = parse_rfc3339_unix("2026-11-13T07:24:00Z").expect("valid");
        let text = format_footer(&live_snapshot(), now);
        assert!(text.chars().count() < 60, "footer too long: {text}");
    }
}
