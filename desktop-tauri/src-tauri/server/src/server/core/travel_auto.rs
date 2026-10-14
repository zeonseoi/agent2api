//! 派猫猫旅行的**后台调度**：独立轮询循环、跨日缓存与明细台账。
//!
//! ── 上游调用全部交给官方的成长任务实现 ───────────────────────
//! 旅行端点的客户端（猫档案 / 状态 / 派出 / 领奖 / 领养）在
//! `providers::workbuddy::growth::travel` —— 官方 v3.0.3 从参考项目
//! workbuddy2api 移植的实现（含领养链路与「门槛未达」的当日防抖）。本模块
//! **不再自带一份**：同一家上游只留一套客户端，状态机语义才只有一个事实源。
//! 官方那侧只有手动「巡检一次」按钮；这里补的是它没有的两件事 —— **定时自动跑**
//! 与**明细台账**（见下）。
//!
//! ── 为什么是一条独立循环（不能挂进自动签到）──────────────────
//! 自动签到是**单阶段、当天封账**：到点跑一轮 → 写 `lastFiredDate` → 当天不再
//! 重跑（见 `auto_checkin::tick`）。而旅行是**两阶段跨小时**：
//! `idle ──(depart)──▶ traveling ──(几小时后)──▶ arrived ──(claim)──▶ idle`。
//! 塞进签到调度会出现「派完就封账、当天回不来领」的破洞。因此这里另起一个
//! 循环，`auto_checkin` 与十条签到链路一个字不改。
//!
//! ── 循环语义 ────────────────────────────────────────────────
//! 每 [`TICK_MS`] 醒一次，对**每个国内版账号**跑一趟官方状态机
//! （`growth::travel::run_once`：无猫领养 / 空闲派出 / 到站领奖 / 在途跳过，
//! 一趟只推进一步）。只在国内版跑：国际版没有成长中心，对它会持续打无效请求，
//! 所以账号集合按 provider 过滤（与 `growth::supports_growth` 的判据同源）。
//!
//! ── 当天完成封账（一个账号一天只领一次）──────────────────────
//! 账号领回一次奖励后，缓存条目置 `done: true`，此后**当天不再派、也不再打上游
//! 读它**（次日随跨日滚动自动清掉 `done` 恢复）。这个「今天完成过」推不出来
//! （领完后上游是 `idle`，`daily_limit_reached` 也不一定为真），只能本地记。
//!
//! ── 跨日缓存（重启不丢未领奖励）──────────────────────────────
//! 一条旅行可能跨自然日（晚上派出、次日回来）。缓存记 `{date, results:{账号 →
//! {state, done}}}`，跨日时**只丢已结束/失败的记录，保留仍在 traveling /
//! arrived 的** —— 否则跨日会把「还没领的奖励」一起丢掉。`done` 条目是 `idle`，
//! 不是 in_flight，跨日会被一并丢弃 —— 这正是「次日恢复」的实现方式。
//!
//! ── 明细台账（签到中心「旅行记录」）──────────────────────────
//! 真执行的一轮（Act）里，对每个**真动手**的账号落一行明细（派出 / 领养 /
//! 领回 / 失败）到 `travel_records` 表（见 `core::travel_records`）；只读刷新
//! （Refresh）不记。官方那个手动「巡检一次」按钮也记（source = manual，见
//! [`record_manual`]，由 `api::growth::travel_run` 调用）—— 台账是旅行这一路
//! 的统一日志，不分手动与自动。
//!
//! ── 自动开关 ─────────────────────────────────────────────────
//! 默认**关闭**（与签到不同：旅行会真花掉账号的每日次数，用户要显式开）。
//! 开关存 config.json 的 `travelAuto` 键（`{enabled}`），读写照
//! `checkin_history` 的 `update_raw_field` 模式。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::server::config;
use crate::server::core::account_store::AccountStore;
use crate::server::core::providers::workbuddy::growth::{self, GrowthAccount};
use crate::server::core::providers::workbuddy::Region as WorkBuddyRegion;
use crate::server::core::travel_records;
use crate::server::logging;

/// 轮询间隔：2 小时。
///
/// 旅行时长是「几小时」量级，2 小时一轮足够把回来的奖励领回，且大幅减少对上游的
/// 请求次数（早期实现是 15 分钟，过密）。软件刚启动时不等这一轮 —— `start()` 会
/// 先补跑一轮（见那里的说明），所以「开软件即领回未领奖励」不依赖这个间隔。
pub const TICK_MS: u64 = 2 * 60 * 60 * 1000;

/// config.json 里承载自动旅行开关的键
const CONFIG_KEY: &str = "travelAuto";
/// config.json 里承载**成长任务自动领取**开关的键（独立于旅行开关，见
/// [`claim_auto_enabled`]）
const CLAIM_CONFIG_KEY: &str = "growthClaimAuto";
/// config.json 里承载旅行状态缓存的键
const CACHE_KEY: &str = "travelCache";

/// 读自动开关（缺失 → 关；与签到「缺省开」相反，理由见模块头）。
pub fn auto_enabled() -> bool {
    config::current()
        .raw()
        .get(CONFIG_KEY)
        .and_then(|value| value.get("enabled"))
        .and_then(Value::as_bool)
        == Some(true)
}

/// 写自动开关。
pub fn set_auto(enabled: bool) {
    config::update_raw_field(CONFIG_KEY, json!({ "enabled": enabled }));
}

// ─── 当天完成封账（本地标记）─────────────────────────────────

/// 缓存里的一条是否「今天已完成」（今天已领回一次奖励）。
///
/// ── 为什么要有这个本地标记 ──────────────────────────────────
/// 需求：一个账号当天领回一次后，当天不再派、也不再打上游读它（`完全不碰`），
/// 次日恢复。官方状态机按上游的 `daily_limit_reached` 判「今天已派出过」，但那
/// 个字段在领完后的瞬时读里不保证为真（实测过「领完仍是 idle 且未达上限」的
/// 形态）—— 只靠它会让自动循环在领回后当天又派一次。所以本地记一笔。
fn is_done(entry: &Value) -> bool {
    entry.get("done").and_then(Value::as_bool) == Some(true)
}

/// 今日日期键（北京时间，与签到/台账同一口径）。
fn today_key() -> String {
    crate::server::core::beijing::today_key()
}

/// 跨日滚动：日期变了就清空完成标记（`done` 只对当天有效）。
///
/// 旅行状态本身不缓存在本地（权威在上游），这里只留「今天已领回」这一笔 ——
/// 跨日清掉即「次日自动恢复」，不必保留任何在途记录（未领的奖励在上游，
/// 下一轮 `run_once` 自然会领）。
fn roll_cache_to_today(today: &str) -> Map<String, Value> {
    // 绑到局部变量：`config::current()` 返回临时值，`raw()` 借用它，
    // 直接 `.raw().get(...)` 会在语句结束时把临时值丢掉（借用悬空）
    let snapshot = config::current();
    let raw = snapshot.raw();
    let cache = raw.get(CACHE_KEY);
    let same_day = cache
        .and_then(|value| value.get("date"))
        .and_then(Value::as_str)
        == Some(today);
    let results = cache
        .and_then(|value| value.get("results"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if same_day {
        return results;
    }
    let kept: Map<String, Value> = results
        .into_iter()
        .filter(|(_, entry)| is_done(entry))
        .collect();
    config::update_raw_field(CACHE_KEY, json!({ "date": today, "results": kept }));
    kept
}

/// 写回缓存的 results 表（保留 `date` 为今天）。
fn write_cache_results(results: &Map<String, Value>) {
    config::update_raw_field(CACHE_KEY, json!({ "date": today_key(), "results": results }));
}

/// 国内版账号（旅行只对这些账号有意义；判据与 `growth::supports_growth` 同源）。
fn cn_accounts(store: &AccountStore) -> Vec<Value> {
    store
        .accounts_for_provider(WorkBuddyRegion::Cn.provider_id())
        .into_iter()
        .filter(|account| growth::supports_growth(account))
        .collect()
}

// ─── 明细台账（core::travel_records）─────────────────────────

/// 官方状态机的一次结论 → 台账行（`outcome` 的字面集合见 `core::travel_records`）。
///
/// `skip` 是**预期**的正常态（在途、已达当日上限、领养门槛未达、防抖挡下），
/// 不落账 —— 与官方 `travel.rs` 模块头「跳过与失败必须分开」同一取舍。
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

/// 官方手动「巡检一次」按钮的记账入口（`api::growth::travel_run` 调用）。
///
/// 手动与自动走同一张台账（source = `manual`），界面上才看得到「这一趟是谁触发的」。
pub fn record_manual(account_id: &str, account_name: &str, report: &growth::travel::TravelReport) {
    record_outcome(
        "manual",
        "手动巡检",
        account_id,
        account_name,
        report.outcome,
        &report.message,
        report.credit,
    );
}

// ─── 到期前自动领取成长任务 ──────────────────────────────────

/// 自动领取的开关（config.json 的 `growthClaimAuto` 键，**独立于旅行开关**）。
///
/// ── 为什么与旅行开关分开 ────────────────────────────────────
/// 两件事的风险不同：旅行会真花掉账号的每日次数（用户要显式开），任务领取是
/// 把**已经该拿的奖励**领回来（动作更轻）。合成一个开关会让「只想派猫」的用户
/// 被迫接受任务领取（或反之）。两个键各自读写，界面各一颗开关。
pub fn claim_auto_enabled() -> bool {
    config::current()
        .raw()
        .get(CLAIM_CONFIG_KEY)
        .and_then(|value| value.get("enabled"))
        .and_then(Value::as_bool)
        == Some(true)
}

/// 写自动领取开关。
pub fn set_claim_auto(enabled: bool) {
    config::update_raw_field(CLAIM_CONFIG_KEY, json!({ "enabled": enabled }));
}

/// 任务是否「快到期」：`valid_end` 非空、且北京时间的今天 ≥ 到期日 − 1 天。
///
/// `valid_end` 是 ISO 8601 带时区串（如 `2026-11-02T23:59:00+08:00`）。取它
/// 的北京日期键，往前推一天得到「该领的那天」；今天到了或过了就返回 true
/// （到期当天/之后仍会尝试，直到上游把它判成已领或任务下线）。
///
/// 无期限（`valid_end` 为 null）的任务**不碰** —— 用户要的是「有结束时间的任务」。
fn due_for_claim(task: &Value) -> bool {
    let Some(end) = task.get("valid_end").and_then(Value::as_str) else {
        return false;
    };
    let Some(threshold) = claim_threshold_day(end) else {
        return false;
    };
    // 日期键定长，字典序即时间序（见 beijing 模块的说明）
    crate::server::core::beijing::today_key() >= threshold
}

/// `valid_end`（ISO 串）→ 「该领的那天」= 到期日 − 1 天的北京日期键。
///
/// 拆成纯函数便于单测：判据里只有它涉及日期运算，`due_for_claim` 只做一次
/// 字符串比较（日期键 `YYYY-MM-DD` 定长、字典序即时间序）。
/// 解析不出来（非法串 / 无时区）给 None，调用方按「不快到」保守处理。
fn claim_threshold_day(end: &str) -> Option<String> {
    let at = chrono::DateTime::parse_from_rfc3339(end).ok()?;
    let end_day = at
        .with_timezone(&crate::server::core::beijing::offset())
        .date_naive();
    Some((end_day - chrono::Days::new(1)).format("%Y-%m-%d").to_string())
}

/// 对一个账号扫一遍成长任务，领掉「快到期」的。
///
/// ── 判据与动作 ──────────────────────────────────────────────
/// 只看 `valid_end` 非空、且今天 ≥ 到期日 − 1 天的任务；已 `claimed` 的跳过。
/// 已完成 → 直接 claim；**未完成 → 先点亮（accept + 上报）再 claim**（用户选：
/// 「未完成的也顺便点亮再领」）。未映射 / 不可伪造 / 未解锁的任务由官方
/// `process_task` 自己保守跳过，这里不重复判。
///
/// ── 为什么复用 `process_task(auto_claim = true)` ─────────────
/// 它已经是「accept → 上报 → 回读 → claim」的一体流程（含 accept 回读验证），
/// 正是「点亮 + 领取」。这里只是把「挑哪几条」的逻辑放在外面。
///
/// 返回 `(领取成功数, 失败数)`，供轮次汇总日志用。
async fn claim_due_tasks(store: &AccountStore, reason: &str) -> (usize, usize) {
    let accounts = cn_accounts(store);
    let mut claimed = 0usize;
    let mut failed = 0usize;
    for account in &accounts {
        let id = account.get("id").and_then(Value::as_str).unwrap_or("");
        if id.is_empty() {
            continue;
        }
        let name = account
            .get("name")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .unwrap_or(id);
        let Some(account_ctx) = GrowthAccount::from_store(store, id) else {
            continue;
        };
        // 两个口径都拉（小程序限定任务只在带 mp 头的列表里下发），合并去重
        let mut tasks: Vec<Value> = Vec::new();
        let mut seen: Vec<String> = Vec::new();
        for mp in [false, true] {
            let Ok(list) = growth::list_tasks(&account_ctx, mp).await else {
                continue;
            };
            for task in list {
                let code = task.get("task_code").and_then(Value::as_str).unwrap_or("");
                if code.is_empty() || seen.iter().any(|item| item == code) {
                    continue;
                }
                seen.push(code.to_string());
                tasks.push(task);
            }
        }
        for task in &tasks {
            let code = task.get("task_code").and_then(Value::as_str).unwrap_or("");
            if code.is_empty() {
                continue;
            }
            let status = task.get("accept_status").and_then(Value::as_str).unwrap_or("");
            if status == "claimed" {
                continue;
            }
            if !due_for_claim(task) {
                continue;
            }
            let report = growth::process_task(&account_ctx, code, task, true).await;
            match report.outcome {
                growth::TaskOutcome::Done => {
                    claimed += 1;
                    travel_records::record(
                        reason_source(reason),
                        reason,
                        id,
                        name,
                        WorkBuddyRegion::Cn.provider_id(),
                        travel_records::OUTCOME_TASK_CLAIMED,
                        Some(format!("{}（+{} 积分）", report.message, report.credit)),
                        None,
                    );
                }
                growth::TaskOutcome::Failed => {
                    failed += 1;
                    travel_records::record(
                        reason_source(reason),
                        reason,
                        id,
                        name,
                        WorkBuddyRegion::Cn.provider_id(),
                        travel_records::OUTCOME_FAILED,
                        None,
                        Some(format!("{code}：{}", report.message)),
                    );
                }
                // Already / Skipped：不是事件（已领 / 未映射 / 不可伪造 / 未解锁）
                _ => {}
            }
        }
    }
    (claimed, failed)
}

// ─── 一轮执行 ────────────────────────────────────────────────

/// 一轮里对单个账号实际做了什么（供轮次汇总统计，对齐签到的 `X/Y` 汇总）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum TravelAction {
    /// 派出旅行（idle → traveling）
    Departed,
    /// 领回奖励（arrived → idle）
    Claimed,
    /// 领养成功（无猫 → 有猫）
    Adopted,
    /// 未动手（在途等待 / 已达上限 / 只读刷新 / 出错 / 跳过）
    None,
}

/// 取数口径：决定这一轮**打不打上游、打上游时动不动手**。
///
/// ── 为什么用枚举而不是两个 bool ─────────────────────────────
/// 三个档位互斥（不打上游 / 打但只读 / 打且派发领取）。用 `refresh: bool` +
/// `query_only: bool` 表达时，「refresh=false 却 query_only=true」这种自相矛盾的
/// 组合在类型上是合法的 —— 本模块曾栽在这里（`query_only` 被传成常量 `false`，
/// 「刷新状态」于是悄悄变成了真派真领）。枚举让「只读刷新」与「执行一轮」在
/// 类型上就分开。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TravelMode {
    /// 只读缓存，不打上游（页面首屏）。
    Cache,
    /// 打上游读每个账号的最新状态，但**不派不发**（界面「刷新状态」）。
    Refresh,
    /// 打上游并执行（该派的派、该领的领）：后台循环与「立即执行」。
    Act,
}

impl TravelMode {
    /// 这一档是否要打上游。
    fn hits_upstream(self) -> bool {
        !matches!(self, Self::Cache)
    }
}

/// 对一个账号跑一趟官方状态机（`growth::travel::run_once`），返回写进缓存的快照。
///
/// `query_only` 为真时只看状态不动作（界面「刷新状态」）—— 避免用户点一下刷新
/// 就真的把猫派出去。
async fn run_one(
    store: &AccountStore,
    account: &Value,
    query_only: bool,
) -> (Value, TravelAction) {
    let id = account.get("id").and_then(Value::as_str).unwrap_or("");
    let Some(account_ctx) = GrowthAccount::from_store(store, id) else {
        return (
            json!({ "state": "error", "error": "账号不可用（无凭证或已禁用）" }),
            TravelAction::None,
        );
    };

    // 只读刷新：查档案 + 状态，不改上游
    if query_only {
        let buddy = match growth::travel::buddy_info(&account_ctx).await {
            Ok(buddy) => buddy,
            Err(message) => {
                return (json!({ "state": "error", "error": message }), TravelAction::None)
            }
        };
        let Some(buddy) = buddy else {
            return (
                json!({ "state": "no_buddy", "buddyName": "", "hasBuddy": false }),
                TravelAction::None,
            );
        };
        return match growth::travel::travel_status(&account_ctx).await {
            Ok(status) => (
                json!({
                    "state": status.state,
                    "buddyName": buddy.name,
                    "hasBuddy": true,
                    "dailyLimitReached": status.daily_limit_reached,
                    "recordId": status.record_id,
                    "rewardCredit": status.reward_credit,
                }),
                TravelAction::None,
            ),
            Err(message) => (
                json!({
                    "state": "error", "buddyName": buddy.name, "hasBuddy": true, "error": message,
                }),
                TravelAction::None,
            ),
        };
    }

    // 执行：官方状态机跑一步（无猫领养 / 空闲派出 / 到站领奖 / 在途跳过）
    let report = growth::travel::run_once(&account_ctx).await;
    let action = match report.outcome {
        "depart" => TravelAction::Departed,
        "claim" => TravelAction::Claimed,
        "adopt" => TravelAction::Adopted,
        _ => TravelAction::None,
    };
    let mut snapshot = json!({
        "state": match report.outcome {
            "depart" => "traveling",
            "claim" => "idle",
            "adopt" => "idle",
            "failed" => "error",
            "no_buddy" => "no_buddy",
            _ => "unknown",
        },
        "buddyName": Value::Null,
        "hasBuddy": report.outcome != "no_buddy",
        "message": report.message,
        "credit": report.credit,
    });
    // 领回即当天完成封账（此后本轮不再碰它，见 `is_done`）
    if report.outcome == "claim" {
        snapshot["done"] = json!(true);
    }
    if report.outcome == "failed" {
        snapshot["error"] = json!(report.message);
    }
    (snapshot, action)
}

/// 把所有前端的行合并出来：跑一轮（按 `mode`），返回每个账号的快照。
///
/// `reason` 是触发来源（后台巡检 / 启动补跑 / 开启自动 / 手动触发），只进
/// 时间线台账（`Act` 轮次），不影响取数。
///
/// ── 当天完成封账（重要）──────────────────────────────────────
/// 某账号今天已领回一次奖励（缓存带 `done`），本轮**完全不碰它**：不打上游、
/// 不再派。`Act` 与 `Refresh` 都遵守 —— 两者都要打上游，done 就都不打
/// （界面点「刷新状态」也不会把它读成 `idle` 又派一次）。跨日由
/// [`roll_cache_to_today`] 清掉 done 标记后自动恢复。
pub async fn collect(store: &AccountStore, mode: TravelMode, reason: &str) -> Value {
    let accounts = cn_accounts(store);
    let today = today_key();
    let mut results = roll_cache_to_today(&today);

    if mode.hits_upstream() {
        // 串行（与签到同一取舍：并发打上游会触发风控）
        let mut departed = 0usize;
        let mut claimed = 0usize;
        let mut failures: Vec<String> = Vec::new();
        for account in &accounts {
            let id = account.get("id").and_then(Value::as_str).unwrap_or("").to_string();
            if id.is_empty() {
                continue;
            }
            let name = account
                .get("name")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .unwrap_or(&id)
                .to_string();
            // 今天已完成 → 完全不碰上游（见函数头「当天完成封账」）
            if results.get(&id).is_some_and(is_done) {
                continue;
            }
            let (snapshot, action) = run_one(store, account, mode != TravelMode::Act).await;
            match action {
                TravelAction::Departed => departed += 1,
                TravelAction::Claimed => claimed += 1,
                TravelAction::Adopted | TravelAction::None => {}
            }
            if let Some(error) = snapshot.get("error").and_then(Value::as_str) {
                failures.push(format!("{name}（{error}）"));
            }
            // 明细台账：只在实际执行的一轮（Act）里、对**真动手**的账号落一行
            // （在途等待 / 已达上限跳过 / 只读刷新都不是事件）。
            if mode == TravelMode::Act {
                let outcome = match action {
                    TravelAction::Departed => "depart",
                    TravelAction::Claimed => "claim",
                    TravelAction::Adopted => "adopt",
                    TravelAction::None => {
                        if snapshot.get("error").is_some() { "failed" } else { "skip" }
                    }
                };
                let message = snapshot.get("message").and_then(Value::as_str).unwrap_or("");
                let credit = snapshot.get("credit").and_then(Value::as_i64).unwrap_or(0);
                record_outcome(reason_source(reason), reason, &id, &name, outcome, message, credit);
            }
            results.insert(id, snapshot);
        }
        write_cache_results(&results);
        // 轮次汇总日志：只在实际执行的一轮（Act）记，界面「刷新状态」（只读）不刷屏。
        if mode == TravelMode::Act {
            logging::log(
                "[Travel]",
                &format!(
                    "🐾 旅行一轮完成（{reason}）: 派出 {departed} 个，领回 {claimed} 个{}（共 {} 个账号）",
                    if failures.is_empty() {
                        String::new()
                    } else {
                        format!("，失败 {} 个", failures.len())
                    },
                    accounts.len(),
                ),
            );
        }
    }

    let rows: Vec<Value> = accounts
        .iter()
        .filter_map(|account| {
            let id = account.get("id").and_then(Value::as_str).unwrap_or("");
            if id.is_empty() {
                return None;
            }
            // 缓存里没有这个账号（首次打开、或刚加了账号）：给一个默认快照
            // （未派）而不是把它整条丢掉 —— 丢掉会让默认视图（不打上游）在
            // 有账号时也显示空列表，用户以为功能坏了。真实状态由「刷新状态」补。
            let default = json!({ "state": "untraveled" });
            let entry = results.get(id).unwrap_or(&default);
            Some(json!({
                "id": account.get("id").cloned().unwrap_or(Value::Null),
                "name": account.get("name").cloned().unwrap_or(Value::Null),
                "state": entry.get("state").cloned().unwrap_or(json!("untraveled")),
                "buddyName": entry.get("buddyName").cloned().unwrap_or(Value::Null),
                "hasBuddy": entry.get("hasBuddy").cloned().unwrap_or(Value::Null),
                "rewardCredit": entry.get("rewardCredit").cloned().unwrap_or(Value::Null),
                "dailyLimitReached": entry.get("dailyLimitReached").cloned().unwrap_or(Value::Bool(false)),
                // 今天已领回一次 → 界面显示「今日已完成」，且本轮不再碰它（见 `is_done`）
                "done": entry.get("done").cloned().unwrap_or(Value::Bool(false)),
                "message": entry.get("message").cloned().unwrap_or(Value::Null),
                "error": entry.get("error").cloned().unwrap_or(Value::Null),
            }))
        })
        .collect();

    json!({ "accounts": rows, "enabled": auto_enabled() })
}

/// 触发来源 `reason` → 台账 `source` 档位（字面集合见 `core::travel_records`）。
fn reason_source(reason: &str) -> &'static str {
    match reason {
        "启动补跑" => "startup",
        "手动触发" => "manual",
        _ => "auto",
    }
}

// ─── 调度句柄 ────────────────────────────────────────────────

/// 调度句柄：内部一把锁 + 后台任务（与 `AutoCheckin` 同形）。
#[derive(Clone)]
pub struct TravelAuto {
    store: AccountStore,
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    /// 是否有一次轮次在跑（防重入）
    running: bool,
    task: Option<tokio::task::JoinHandle<()>>,
    stopped: Arc<AtomicBool>,
}

/// 抢到「执行中」标记的守卫：无论正常返回还是中途被 abort，Drop 时都复位 `running`
/// —— 对应 `auto_checkin::RunningGuard`，避免 abort 把它永久卡在 `true`。
struct RunningGuard {
    inner: Arc<Mutex<Inner>>,
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        match self.inner.lock() {
            Ok(mut guard) => guard.running = false,
            Err(poisoned) => poisoned.into_inner().running = false,
        }
    }
}

impl TravelAuto {
    pub fn new(store: AccountStore) -> Self {
        Self {
            store,
            inner: Arc::new(Mutex::new(Inner {
                running: false,
                task: None,
                stopped: Arc::new(AtomicBool::new(false)),
            })),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// 起调度循环（幂等：已有存活任务就不重复起）。开关关闭时不排定时器。
    ///
    /// ── 启动补跑一轮（重要）────────────────────────────────────
    /// 循环体是「先睡 2 小时再跑」，若只是排上循环，刚开软件时要干等 2 小时才
    /// 处理一次 —— 而用户关了一夜的软件、早上打开，往往正有一只猫已经回来了在等
    /// 领。所以这里**立即补跑一轮**，再接循环（与自动签到的「启动补签」同一意图）。
    ///
    /// 补跑不阻塞启动：`fire` 内部有防重入闸，且它自己在一个 spawn 里跑。
    pub fn start(&self) {
        if !auto_enabled() {
            return;
        }
        self.schedule();
        let service = self.clone();
        crate::spawn_task(async move {
            service.fire("启动补跑").await;
        });
    }

    fn schedule(&self) {
        {
            let mut inner = self.lock();
            if let Some(task) = &inner.task {
                if !task.is_finished() {
                    return;
                }
            }
            inner.stopped = Arc::new(AtomicBool::new(false));
            let stopped = inner.stopped.clone();
            let service = self.clone();
            inner.task = Some(crate::spawn_task(async move {
                loop {
                    // 先睡再跑：与 setInterval 的「间隔后首次触发」一致
                    tokio::time::sleep(Duration::from_millis(TICK_MS)).await;
                    if stopped.load(Ordering::SeqCst) {
                        break;
                    }
                    // 每拍现读开关：用户中途关掉就不跑（不必重启进程）
                    if !auto_enabled() {
                        continue;
                    }
                    service.fire("后台巡检").await;
                }
            }));
        }
        logging::log("[Travel]", "🐾 派猫猫旅行自动调度已启动（每 2 小时一轮）");
    }

    /// 停调度（改开关 / 进程退出时用）。
    pub fn stop(&self) {
        let task = {
            let mut inner = self.lock();
            if inner.task.is_none() {
                return;
            }
            inner.stopped.store(true, Ordering::SeqCst);
            inner.task.take()
        };
        if let Some(task) = task {
            task.abort();
        }
    }

    /// 开/关自动旅行（写配置 + 起停调度）。
    ///
    /// 打开时**立即跑一轮**再排循环：用户按下开关的预期是「现在就管起来」，
    /// 而不是等 2 小时。与 `start()` 的启动补跑同一取舍。
    pub fn set_enabled(&self, enabled: bool) {
        set_auto(enabled);
        if enabled {
            self.schedule();
            let service = self.clone();
            crate::spawn_task(async move {
                service.fire("开启自动").await;
            });
        } else {
            self.stop();
            logging::log("[Travel]", "🐾 派猫猫旅行自动调度已停止");
        }
    }

    /// 跑一轮（自动循环与「立即执行」共用）。防重入：已在跑就跳过。
    ///
    /// ── 一轮做两件事 ────────────────────────────────────────────
    /// ① 猫猫旅行状态机（`collect`，受旅行开关控制，`collect` 内部自己判）；
    /// ② 成长任务到期领取（受领取开关控制）。两者开关独立，但共用这一条循环
    ///    与这一道防重入闸 —— 不新起第二个循环，避免两套生命周期与并发打上游。
    ///
    /// ── 为什么用 Drop 守卫而不是手动复位 ────────────────────────
    /// `fire` 会在中途 await（网络请求）。若这一轮正跑着，用户把开关关掉，`stop()`
    /// 会 `abort` 掉任务 —— 那会让 await 之后的复位代码永不执行，`running` 永久卡在
    /// `true`，此后再也跑不起来（功能静默死掉，直到重启进程）。守卫在 Drop 里复位，
    /// 无论正常返回还是被 abort 都能解开。与 `auto_checkin::RunningGuard` 同一手法。
    pub async fn fire(&self, reason: &str) {
        let _guard = {
            let mut inner = self.lock();
            if inner.running {
                logging::verbose("[Travel]", "上一轮旅行调度仍在执行，本次跳过");
                return;
            }
            inner.running = true;
            RunningGuard { inner: self.inner.clone() }
        };
        let _ = collect(&self.store, TravelMode::Act, reason).await;
        // 成长任务到期领取：独立开关，开才跑（本轮已打上游，顺带扫描不额外起循环）
        if claim_auto_enabled() {
            let (claimed, failed) = claim_due_tasks(&self.store, reason).await;
            if claimed > 0 || failed > 0 {
                logging::log(
                    "[Growth]",
                    &format!(
                        "🎁 成长任务到期领取（{reason}）：领回 {claimed} 项{}",
                        if failed > 0 { format!("，失败 {failed} 项") } else { String::new() },
                    ),
                );
            }
        }
    }

    /// 手动「立即执行一轮」（界面按钮）：旅行一轮 + 到期任务领取（各受自己
    /// 的开关控制），与后台循环 [`Self::fire`] 同口径。
    pub async fn run_now(&self) -> Value {
        let data = collect(&self.store, TravelMode::Act, "手动触发").await;
        if claim_auto_enabled() {
            let _ = claim_due_tasks(&self.store, "手动触发").await;
        }
        data
    }
}

/// 全局单例（`init_global` / `stop_global` 与 `auto_checkin` 同一模式）。
static GLOBAL: std::sync::OnceLock<TravelAuto> = std::sync::OnceLock::new();

/// 装入全局句柄（重复调用忽略）。
pub fn init_global(service: TravelAuto) -> TravelAuto {
    let _ = GLOBAL.set(service.clone());
    service
}

/// 进程退出时停调度（`backend::shutdown` 调用）。
pub fn stop_global() {
    if let Some(service) = GLOBAL.get() {
        service.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 取数口径的三档各打不打上游 —— 这是「刷新状态不会派猫」的回归保护：
    /// 曾经 `query_only` 被调用方传成常量 false，刷新变成了真派真领。
    #[test]
    fn travel_mode_splits_read_only_refresh_from_act() {
        assert!(!TravelMode::Cache.hits_upstream());
        assert!(TravelMode::Refresh.hits_upstream());
        assert!(TravelMode::Act.hits_upstream());
    }

    /// `done` 只在明确为 true 时成立 —— 缺失 / false / 非布尔都当「今天没完成」。
    /// 这决定了 `collect` 会不会跳过这个账号。
    #[test]
    fn is_done_only_when_explicitly_true() {
        assert!(is_done(&json!({ "state": "idle", "done": true })));
        assert!(!is_done(&json!({ "state": "idle", "done": false })));
        assert!(!is_done(&json!({ "state": "idle" })));
        assert!(!is_done(&json!({ "done": "true" })));
        assert!(!is_done(&Value::Null));
    }

    /// 触发来源归档：三种 reason 各归各的 source 档位。
    #[test]
    fn reason_maps_to_ledger_source() {
        assert_eq!(reason_source("启动补跑"), "startup");
        assert_eq!(reason_source("手动触发"), "manual");
        assert_eq!(reason_source("后台巡检"), "auto");
        assert_eq!(reason_source("开启自动"), "auto");
    }

    /// 「该领的那天」= 到期日 − 1 天的北京日期键（`due_for_claim` 的核心判据）。
    ///
    /// 上游 `valid_end` 是带时区的 ISO 串，取**北京日期**再减一天：晚 23:59 的
    /// 时刻不能被时区差推到次日（那会让「前一天」算错），所以先换到 +08:00 再取日。
    #[test]
    fn claim_threshold_is_one_day_before_expiry() {
        assert_eq!(
            claim_threshold_day("2026-11-02T23:59:00+08:00").as_deref(),
            Some("2026-11-01")
        );
        // 跨月边界：11-01 的前一天是 10-31
        assert_eq!(
            claim_threshold_day("2026-11-01T00:00:00+08:00").as_deref(),
            Some("2026-10-31")
        );
        // 非 +08:00 的来源：按北京日期换算后再减一天
        assert_eq!(
            claim_threshold_day("2026-11-02T16:30:00Z").as_deref(),
            Some("2026-11-02")
        );
    }

    /// 非法 / 无时区的串给 None（调用方据此保守不领）。
    #[test]
    fn claim_threshold_rejects_unparseable() {
        assert_eq!(claim_threshold_day("").as_deref(), None);
        assert_eq!(claim_threshold_day("2026-11-02").as_deref(), None);
        assert_eq!(claim_threshold_day("not-a-date").as_deref(), None);
    }

    /// 无 `valid_end` 的任务不参与到期领取（用户要的是「有结束时间的任务」）。
    #[test]
    fn due_for_claim_ignores_tasks_without_valid_end() {
        assert!(!due_for_claim(&json!({ "task_code": "x" })));
        assert!(!due_for_claim(&json!({ "valid_end": Value::Null })));
    }
}
