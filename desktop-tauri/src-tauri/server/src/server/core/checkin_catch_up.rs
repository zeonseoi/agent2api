//! 启动补签的判据：**今天没签过、且今天的触发时刻已过**。
//!
//! ── 为什么不放在 `core::auto_checkin` 里 ─────────────────────
//! 那是官方文件。这个判据是本 fork 对官方行为的修正（官方 `start()` 只看
//! 「今天签过没有」，不看时点），写在官方文件里每次合并都要重解一次冲突。
//! 放这里之后，官方文件只剩 `start()` 里那个调用点。
//!
//! ── 为什么未到点不该补 ──────────────────────────────────────
//! `auto_checkin` 的 `fire` 在执行前就**无条件**落 `lastFiredDate`（防进程
//! 中途被杀后反复补签）。若启动时不看时刻就补签，「设定 10:10、早上 9 点开机」
//! 会在 9 点把当天的名额用掉，10:10 到点时被去重挡下、整天不再重跑 —— 需要
//! 时间窗口的提供商（Qoder 的国际版与国内版活动都在 10:00 后才有）就此漏掉，
//! 且没有任何重试机会。
//!
//! 触发时刻是用户自定的任意时刻，所以只判「到了没有」，不假设具体几点。
//! 「错过会补签」的语义保留：设 10:10、下午 3 点才开机，仍然马上签一次。

use chrono::{DateTime, FixedOffset};

use super::auto_checkin::{date_key, due_now};

/// 启动时是否该立刻补签一次。
///
/// 两个条件**缺一不可**：今天还没签过（`lastFiredDate` 不是今天）、今天的
/// 触发时刻**已过**。时刻文本非法（`due_now` 给 None）按「不补」处理 ——
/// 宁可漏补，也不凭猜测打上游。
pub fn due(time_text: &str, last_fired_date: Option<&str>, now: DateTime<FixedOffset>) -> bool {
    if last_fired_date == Some(date_key(now).as_str()) {
        return false;
    }
    due_now(time_text, now).is_some()
}

#[cfg(test)]
mod tests {
    //! 启动补签的回归保护：修复前 `start` 只看「今天签过没有」，
    //! 「设定 10:10、早上 9 点开机」会在 9 点补签并把当天名额用掉，10:10 到点
    //! 被去重挡下、整天不再重跑 —— 需要 10 点后窗口的 Qoder 就此漏掉。
    //! 下面每一条各钉一个方向：未到点不跑、已过点才补、当天不重复、非法时刻不补。
    use super::due;
    use crate::server::core::beijing;
    use chrono::TimeZone;

    /// 造一个北京时间当天 HH:MM 的时刻
    fn at(hour: u32, minute: u32) -> chrono::DateTime<chrono::FixedOffset> {
        beijing::offset()
            .with_ymd_and_hms(2026, 10, 12, hour, minute, 0)
            .single()
            .expect("北京时间固定偏移恒可构造")
    }

    /// 未到点：即使今天还没签，也不该补 —— 否则会吃掉当天设定时刻那一轮。
    #[test]
    fn startup_before_the_configured_time_does_not_fire() {
        // 设定 10:10，9:00 启动
        assert!(!due("10:10", None, at(9, 0)));
        // 用户自定任意时刻都成立：设定 11:00，10:59 启动
        assert!(!due("11:00", None, at(10, 59)));
        // 边界：恰好到点前一分钟
        assert!(!due("10:10", None, at(10, 9)));
    }

    /// 已过点且今天没签：照旧立刻补（「错过会补签」的语义保留）。
    #[test]
    fn startup_after_the_configured_time_catches_up() {
        // 设定 10:10，下午 3 点才开机
        assert!(due("10:10", None, at(15, 0)));
        // 恰好到点（now == target，`due_now` 取 >=）
        assert!(due("10:10", None, at(10, 10)));
    }

    /// 今天签过了：任何时刻都不再补（同一天不跑两次）。
    #[test]
    fn already_fired_today_never_repeats() {
        let today = beijing::date_key(at(9, 0));
        // 未到点、已过点都返回 false
        assert!(!due("10:10", Some(&today), at(9, 0)));
        assert!(!due("10:10", Some(&today), at(15, 0)));
    }

    /// 时刻文本非法：不补（与 `due_now` 的 None 一致，不 panic）。
    #[test]
    fn invalid_time_text_does_not_fire() {
        assert!(!due("not-a-time", None, at(15, 0)));
        assert!(!due("", None, at(15, 0)));
    }
}
