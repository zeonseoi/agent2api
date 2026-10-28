//! 猫猫旅行的**明细台账记账**（签到中心「签到记录」里的猫猫旅行行）。
//!
//! ── 定时排程交给官方 ────────────────────────────────────────
//! 官方 v3.1.0 起有 `core::growth_scheduler`（每天定点巡检、开关与时刻可配），
//! 本模块原先自带的那条 2 小时轮询与「自动开关」已经退役 —— 同一件事只留
//! 官方那一套，避免两个调度同时派猫。这里只剩下官方没有的一层：**台账**。
//!
//! ── 记什么 ──────────────────────────────────────────────────
//! 只记「真动手」的结论（派出 / 领养 / 领回 / 失败），跳过类（在途、已达当日
//! 上限、领养门槛未达、防抖挡下）不落账 —— 那些是预期内的正常态，不是事件。
//! 一行落在 `travel_records` 表（见 [`travel_records`]），与签到记录合并展示。
//!
//! ── 两个调用点 ──────────────────────────────────────────────
//!   - `core::growth_scheduler::run_once`：官方定时 / 手动「立即巡检全部」；
//!   - `api::growth::travel_run`：签到中心成长任务行里的单账号「巡检一次」。
//!
//! ── 为什么按 JSON 解而不是按类型 ─────────────────────────────
//! 官方 v3.3.0 把执行体整体搬进了提供商插件（`ProvidersPlugin/Agent2Providers`），
//! `AccountTravelReport` / `TravelReport` 都不再随本仓库编译。宿主这一侧拿到的是
//! 插件定义的摘要 JSON（键集由插件 `admin.rs` 的 `batch_travel` 给出，
//! 与官方 `lastResult` 同形），所以这里按 `Value` 读，**不重建那套类型**。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use serde_json::Value;

use crate::server::core::providers::workbuddy::Region as WorkBuddyRegion;
use crate::server::core::travel_records;

/// 官方状态机的一次结论 → 台账行（`outcome` 的字面集合见 [`travel_records`]）。
fn record_outcome(
    source: &str,
    reason: &str,
    account_id: &str,
    account_name: &str,
    outcome: &str,
    message: &str,
    credit: i64,
) {
    let provider = WorkBuddyRegion::Cn.provider_id();
    let (outcome, message, error) = match outcome {
        "depart" => (travel_records::OUTCOME_DEPARTED, None, None),
        // 领回 / 领养都有真入账：收益文案取官方给的 credit
        "claim" => (
            travel_records::OUTCOME_CLAIMED,
            Some(format!("+{credit} 积分")),
            None,
        ),
        "adopt" => (
            travel_records::OUTCOME_ADOPTED,
            Some(format!("+{credit} 积分")),
            None,
        ),
        // 失败：原因原文在官方给的 message 里（它把上游错误放进那一栏）
        "failed" => (travel_records::OUTCOME_FAILED, None, Some(message.to_string())),
        // skip / no_buddy：预期正常态，不是事件
        _ => return,
    };
    travel_records::record(
        source,
        reason,
        account_id,
        account_name,
        provider,
        outcome,
        message,
        error,
    );
}

/// 官方定时 /「立即巡检全部」一轮的记账入口（`core::growth_scheduler` 调用）。
///
/// `plan_time` 有值 = 到点触发的计划时刻；无值 = 用户在界面上手动点的那一轮。
/// 只影响台账里的 `source` 档位，不改变记录内容。
///
/// 入参是插件返回的摘要（含 `actions` 数组），每项 `{accountId, name, outcome,
/// message, credit}`；插件已过滤掉跳过类，这里仍按 `outcome` 再核一次。
pub fn record_schedule_from_summary(plan_time: Option<&str>, summary: &Value) {
    let (source, reason) = match plan_time {
        Some(time) => ("auto", format!("到点触发（{time}）")),
        None => ("manual", "手动巡检".to_string()),
    };
    let Some(actions) = summary.get("actions").and_then(Value::as_array) else {
        return;
    };
    for action in actions {
        let account_id = action.get("accountId").and_then(Value::as_str).unwrap_or("");
        if account_id.is_empty() {
            continue;
        }
        record_outcome(
            source,
            &reason,
            account_id,
            action.get("name").and_then(Value::as_str).unwrap_or(""),
            action.get("outcome").and_then(Value::as_str).unwrap_or(""),
            action.get("message").and_then(Value::as_str).unwrap_or(""),
            action.get("credit").and_then(Value::as_i64).unwrap_or(0),
        );
    }
}

/// 官方单账号「巡检一次」按钮的记账入口（`api::growth::travel_run` 调用）。
///
/// 单账号那条走的是插件 `admin.action` 的同一条路，回包是 `{outcome, message,
/// credit}` 的平铺对象（无 `actions` 数组）。
pub fn record_manual(account_id: &str, account_name: &str, report: &Value) {
    record_outcome(
        "manual",
        "手动巡检",
        account_id,
        account_name,
        report.get("outcome").and_then(Value::as_str).unwrap_or(""),
        report.get("message").and_then(Value::as_str).unwrap_or(""),
        report.get("credit").and_then(Value::as_i64).unwrap_or(0),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 跳过类结论不落账 —— 这是「只记真动作」的回归保护：若把 skip 也记上，
    /// 台账每天会被「在途等待」刷满，真正的派出 / 失败反而看不见。
    #[test]
    fn ignored_outcomes_do_not_reach_the_ledger() {
        // 不装 Db 时 `record` 是空操作，这里只能验证分类本身不 panic。
        record_outcome("manual", "测试", "acc", "猫", "skip", "在途", 0);
        record_outcome("manual", "测试", "acc", "猫", "no_buddy", "", 0);
    }
}
