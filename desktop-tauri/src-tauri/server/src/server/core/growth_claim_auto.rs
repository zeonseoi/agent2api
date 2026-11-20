//! WorkBuddy 成长任务的**到期前自动领取**：独立的后台轮询调度。
//!
//! ── 为什么独立成模块 ────────────────────────────────────────
//! 它曾是本 fork 旅行循环上搭着的一段（共用那条 2 小时循环、
//! 共用一个启停）。但两件事的**触发条件与风险都不同**：旅行会真花掉账号当日
//! 的派猫次数（用户要显式开），任务领取是把**已经该拿的奖励**领回来（动作更
//! 轻）。合成一条循环会带来两个副作用 —— 「只开领取」时循环根本不起（启停
//! 只认旅行开关），以及关掉旅行会把领取一起停掉。现在各起一条循环、各有各的
//! 开关与生命周期，关一个对另一个零影响。
//!
//! ── 判据 ────────────────────────────────────────────────────
//! 只碰**有结束时间**（`valid_end` 非空）且今天 ≥ 到期日 − 1 天的任务；已
//! `claimed` 的跳过。已完成 → 直接 claim；未完成 → 先点亮（accept + 上报）
//! 再 claim。无期限的任务不碰。
//!
//! ── 分工：定时器与账号循环在网关，单账号执行在插件 ──────────
//! WASM 插件拿不到「这一家的全部账号」的枚举能力（宿主不向包暴露整库），
//! 所以账号循环只能留在网关：`store.list`（全部账号，含禁用的）→ 逐账号
//! `growth.status` 拿任务清单 → 按 `validEnd` 判到期 → `growth.task`
//! （`mode=full` 的一体流程）。这与官方 `tasks.v1`「批量由宿主逐账号分发」
//! 的架构一致，也不再依赖 fork 私有的批量动作。
//!
//! 到期判据依赖插件在任务行里透传 `validEnd`（上游的 `valid_end`）；装的
//! 插件版本没有该字段时本模块不会命中任何任务（判据保守返回 false，
//! 不报错、也不误领），每轮只记一条 warning —— 升级插件即可，无需改本文件。
//!
//! ── 台账 ────────────────────────────────────────────────────
//! 每领回一条落一行 `travel_records`（`outcome = task_claimed`，见
//! `core::travel_records`）—— 与旅行共用同一张「成长记录」表，界面一处看全。
//!
//! ── 执行时刻 ────────────────────────────────────────────────
//! 与插件任务同一口径：北京时间每天多个 `HH:MM`，由用户在设置里配置；
//! 30 秒扫一拍，走到某个时刻后按「最近已过时段」执行一次；跨重启用
//! `task_state` 的 `last_attempt_at` 去重，停机期间错过的那一拍在启动后补一次，
//! 不补跑期间每一个时段。未配置时默认 09:00、21:00 两拍。
//!
//! ── 硬约束：release 是 `panic=abort`，本文件零 unwrap/expect/panic ──

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Timelike;
use serde_json::{json, Value};

use crate::server::config;
use crate::server::core::account_store::AccountStore;
use crate::server::core::plugin_host::schedules::normalize_time;
use crate::server::core::providers::workbuddy::Region as WorkBuddyRegion;
use crate::server::core::task_state;
use crate::server::core::travel_records;
use crate::server::logging;

/// 扫描间隔：30 秒（与插件任务的宿主扫描同一节奏）。
pub const TICK_MS: u64 = 30 * 1000;

/// 未配置时刻时的默认档：每天两次。
const DEFAULT_TIMES: [&str; 2] = ["09:00", "21:00"];

/// 与签到 / 插件任务表单一致的北京时间。
const UTC_OFFSET_MINUTES: i32 = 480;

/// 跨重启的时段去重键（与插件任务的 `pluginTask:*` 命名空间分开）。
const STATE_KEY: &str = "growthClaimAuto:workbuddy";

/// config.json 里承载本功能开关的键（独立于官方旅行排程 `growthSchedule`）。
const CONFIG_KEY: &str = "growthClaimAuto";

// ─── 开关与时刻 ──────────────────────────────────────────────

/// 读自动领取开关（缺失 → 关；默认关闭，理由见模块头）。
pub fn claim_auto_enabled() -> bool {
    config::current()
        .raw()
        .get(CONFIG_KEY)
        .and_then(|value| value.get("enabled"))
        .and_then(Value::as_bool)
        == Some(true)
}

/// 读执行时刻（缺失 / 全部非法 → 默认 09:00、21:00）。
pub fn claim_auto_times() -> Vec<String> {
    let mut times: Vec<String> = config::current()
        .raw()
        .get(CONFIG_KEY)
        .and_then(|value| value.get("times"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(normalize_time)
        .collect();
    if times.is_empty() {
        times = DEFAULT_TIMES.iter().map(|time| (*time).to_string()).collect();
    }
    times.sort();
    times.dedup();
    times
}

/// 写自动领取开关（保留已存的执行时刻）。
pub fn set_claim_auto(enabled: bool) {
    let _ = config::try_update_raw_field(
        CONFIG_KEY,
        |value| {
            if !value.is_object() {
                *value = json!({});
            }
            value["enabled"] = json!(enabled);
            Ok::<(), String>(())
        },
        || "自动领取配置保存失败".to_string(),
    );
}

/// 存开关 / 执行时刻并起停调度；时段先规范化再落盘。
pub fn save_settings(enabled: Option<bool>, times: Option<Vec<String>>) -> Result<Value, String> {
    let times = match times {
        Some(items) => {
            if items.is_empty() || items.len() > 24 {
                return Err("执行时刻需要 1 到 24 个".into());
            }
            let mut normalized = Vec::with_capacity(items.len());
            for item in &items {
                normalized.push(
                    normalize_time(item)
                        .ok_or_else(|| format!("执行时刻「{item}」无效，格式为 HH:MM"))?,
                );
            }
            normalized.sort();
            normalized.dedup();
            Some(normalized)
        }
        None => None,
    };
    config::try_update_raw_field(
        CONFIG_KEY,
        |value| {
            if !value.is_object() {
                *value = json!({});
            }
            if let Some(enabled) = enabled {
                value["enabled"] = json!(enabled);
            }
            if let Some(times) = &times {
                value["times"] = json!(times);
            }
            Ok::<(), String>(())
        },
        || "自动领取配置保存失败".to_string(),
    )?;
    if let Some(enabled) = enabled {
        set_enabled_global(enabled);
    }
    Ok(snapshot())
}

/// 界面读数（`/api/claim-auto` 与签到中心快照共用）。
pub fn snapshot() -> Value {
    json!({ "enabled": claim_auto_enabled(), "times": claim_auto_times() })
}

// ─── 台账来源档位 ────────────────────────────────────────────

/// 触发来源 `reason` → 台账 `source` 档位（字面集合见 `core::travel_records`）。
fn reason_source(reason: &str) -> &'static str {
    match reason {
        "启动补跑" => "startup",
        "手动触发" => "manual",
        _ => "auto",
    }
}

// ─── 领取扫描 ────────────────────────────────────────────────

/// 国内版账号（成长任务只对这些账号有意义）。
///
/// 与签到 / 旅行同一口径：**不看启用状态**（禁用只影响转发、不影响后台任务），
/// 凭证必须齐（`provider_account_ids` 已过滤）。企业号没有个人成长中心，跳过
/// —— 判据与插件原来的批量实现一致。
fn cn_accounts(store: &AccountStore) -> Vec<Value> {
    store
        .provider_account_ids(WorkBuddyRegion::Cn.provider_id(), false)
        .into_iter()
        .filter_map(|id| store.plugin_account(&id))
        .filter(|account| {
            !account
                .get("enterpriseId")
                .and_then(Value::as_str)
                .is_some_and(|value| !value.is_empty())
        })
        .collect()
}

/// 任务是否「快到期」：`validEnd` 非空、且北京时间的今天 ≥ 到期日 − 1 天。
///
/// `validEnd` 是 ISO 8601 带时区串（如 `2026-11-02T23:59:00+08:00`），由插件
/// 从上游 `valid_end` 归一后透传。取它的北京日期键往前推一天得到「该领的那天」；
/// 今天到了或过了就返回 true（到期当天/之后仍会尝试，直到上游判成已领或任务下线）。
///
/// 无期限（`validEnd` 为 null / 缺失）的任务**不碰** —— 用户要的是「有结束时间的
/// 任务」。解析不出日期按「不快到」保守处理：不报错，也不误领。
fn due_for_claim(task: &Value) -> bool {
    let end = task
        .get("validEnd")
        .or_else(|| task.get("valid_end"))
        .and_then(Value::as_str);
    let Some(end) = end else {
        return false;
    };
    let Some(threshold) = claim_threshold_day(end) else {
        return false;
    };
    // 日期键定长，字典序即时间序（见 beijing 模块的说明）
    crate::server::core::beijing::today_key() >= threshold
}

/// `valid_end`（ISO 串）→「该领的那天」= 到期日 − 1 天的北京日期键。
///
/// 拆成纯函数便于单测：判据里只有它涉及日期运算，`due_for_claim` 只做一次
/// 字符串比较。先换到 +08:00 再取日：晚上 23:59 的时刻不能被时区差推到次日。
fn claim_threshold_day(end: &str) -> Option<String> {
    let at = chrono::DateTime::parse_from_rfc3339(end).ok()?;
    let end_day = at
        .with_timezone(&crate::server::core::beijing::offset())
        .date_naive();
    Some((end_day - chrono::Days::new(1)).format("%Y-%m-%d").to_string())
}

/// 「插件没透传 validEnd」的告警只记一次，避免每轮刷同一条。
static VALID_END_WARNED: AtomicBool = AtomicBool::new(false);

/// 对所有国内版账号扫一遍成长任务，领掉「快到期」的。
///
/// ── 判据与动作 ──────────────────────────────────────────────
/// 只看 `validEnd` 非空、且今天 ≥ 到期日 − 1 天的任务；已 `claimed` 的跳过。
/// 领取一律走插件的 `growth.task` + `mode=full`（它内部是「accept → 上报 →
/// 回读 → claim」的一体流程，含 accept 回读验证）；未映射 / 不可伪造 / 未解锁
/// 的任务由插件自己保守跳过，这里不重复判。
///
/// 返回 `(领取成功数, 失败数)`，供轮次汇总日志用。
pub async fn claim_due_tasks(store: &AccountStore, reason: &str) -> (usize, usize) {
    let Some(adapter) = crate::server::core::plugin_host::lookup(
        WorkBuddyRegion::Cn.provider_id(),
    ) else {
        logging::verbose("[Growth]", "未装入 WorkBuddy 提供商插件，到期领取跳过");
        return (0, 0);
    };
    let mut claimed = 0usize;
    let mut failed = 0usize;
    let mut scanned = 0usize;
    let mut with_expiry = 0usize;
    // 插件是否上报了这个字段（值为 null 也算上报；全空只是今天没有临期任务）。
    let mut valid_end_field = false;
    for account in cn_accounts(store) {
        let id = account.get("id").and_then(Value::as_str).unwrap_or("");
        if id.is_empty() {
            continue;
        }
        let name = account
            .get("name")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .unwrap_or(id);
        let Ok(status) = adapter
            .admin_action(store, id, "growth.status", &Value::Null)
            .await
        else {
            continue;
        };
        // 插件已把默认口径与小程序口径合并去重（见插件 growth/status.rs）
        let tasks = status
            .get("tasks")
            .and_then(|block| block.get("tasks"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        scanned += 1;
        for task in &tasks {
            let code = task.get("code").and_then(Value::as_str).unwrap_or("");
            if code.is_empty() {
                continue;
            }
            if task.get("status").and_then(Value::as_str) == Some("claimed") {
                continue;
            }
            if task.get("validEnd").is_some() {
                valid_end_field = true;
            }
            if task.get("validEnd").and_then(Value::as_str).is_some() {
                with_expiry += 1;
            }
            if !due_for_claim(task) {
                continue;
            }
            let payload = json!({ "code": code, "mode": "full" });
            let report = match adapter
                .admin_action(store, id, "growth.task", &payload)
                .await
            {
                Ok(report) => report,
                Err(error) => {
                    failed += 1;
                    travel_records::record(
                        reason_source(reason),
                        reason,
                        id,
                        name,
                        WorkBuddyRegion::Cn.provider_id(),
                        travel_records::OUTCOME_TASK_FAILED,
                        None,
                        Some(format!("{code}：{}", error.message)),
                    );
                    continue;
                }
            };
            let message = report.get("message").and_then(Value::as_str).unwrap_or("");
            let credit = report.get("credit").and_then(Value::as_i64).unwrap_or(0);
            match report.get("outcome").and_then(Value::as_str).unwrap_or("") {
                "done" => {
                    claimed += 1;
                    travel_records::record(
                        reason_source(reason),
                        reason,
                        id,
                        name,
                        WorkBuddyRegion::Cn.provider_id(),
                        travel_records::OUTCOME_TASK_CLAIMED,
                        Some(format!("{message}（+{credit} 积分）")),
                        None,
                    );
                }
                "failed" => {
                    failed += 1;
                    travel_records::record(
                        reason_source(reason),
                        reason,
                        id,
                        name,
                        WorkBuddyRegion::Cn.provider_id(),
                        travel_records::OUTCOME_TASK_FAILED,
                        None,
                        Some(format!("{code}：{message}")),
                    );
                }
                // already / skipped：不是事件（已领 / 未映射 / 不可伪造 / 未解锁）
                _ => {}
            }
        }
    }
    if scanned > 0 && with_expiry == 0 && !valid_end_field && !VALID_END_WARNED.swap(true, Ordering::Relaxed) {
        logging::log_with_level(
            "[Growth]",
            "插件未上报任务有效期（validEnd），到期领取不会命中任何任务；请安装支持该字段的插件版本",
            "warn",
        );
    }
    (claimed, failed)
}

// ─── 调度句柄 ────────────────────────────────────────────────

/// 到期领取的调度句柄：内部一把锁 + 后台任务（与 `TravelAuto` 同形，但
/// **完全独立** —— 各有各的开关、循环与防重入闸，关一个不影响另一个）。
#[derive(Clone)]
pub struct ClaimAuto {
    store: AccountStore,
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    /// 是否有一次轮次在跑（防重入）
    running: bool,
    task: Option<tokio::task::JoinHandle<()>>,
    stopped: Arc<AtomicBool>,
}

/// 抢到「执行中」标记的守卫：无论正常返回还是中途被 abort，Drop 时都复位
/// `running`（对应 `auto_checkin::RunningGuard` / `auto_checkin::RunningGuard`）。
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

/// 计算「今天最近一个已过时段」与下一个时段（与插件任务同一套语义）：
/// 返回 `(是否到点, 距下一拍的毫秒, 本拍的计划时刻)`。多个时段按北京时间逐拍触发；
/// 停机/睡眠错过的一拍在下一拍补一次（只补最近一拍，不补期间每个时段）。
fn interval(times: &[String], now: i64) -> (bool, i64, i64) {
    let offset = i64::from(UTC_OFFSET_MINUTES) * 60_000;
    let local = now + offset;
    let day = local.div_euclid(86_400_000) * 86_400_000 - offset;
    let mut due = 0_i64;
    let mut next = i64::MAX;
    for time in times {
        let Ok(parsed) = chrono::NaiveTime::parse_from_str(time, "%H:%M") else {
            continue;
        };
        let at = day + i64::from(parsed.num_seconds_from_midnight()) * 1000;
        if at <= now {
            due = due.max(at);
        } else {
            next = next.min(at);
        }
    }
    if next == i64::MAX {
        next = due + 86_400_000;
    }
    (due > 0, (next - now).max(1000), due)
}

impl ClaimAuto {
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
    /// ── 启动补跑 ────────────────────────────────────────────────
    /// 循环第一拍**不睡**，立刻按执行时刻判定一次：今天最近一个时刻已过且尚未
    /// 尝试过，就补跑一轮（跨重启由 `task_state` 去重）；还没到第一个时刻就等到点。
    pub fn start(&self) {
        if !claim_auto_enabled() {
            return;
        }
        self.schedule();
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
                    if stopped.load(Ordering::SeqCst) {
                        break;
                    }
                    // 每拍现读开关与时刻：用户中途改动不必重启进程
                    if claim_auto_enabled() {
                        service.tick().await;
                    }
                    tokio::time::sleep(Duration::from_millis(TICK_MS)).await;
                }
            }));
        }
        logging::log(
            "[Growth]",
            &format!(
                "🎁 成长任务到期领取调度已启动（每天 {}）",
                claim_auto_times().join(", ")
            ),
        );
    }

    /// 一拍：到点且本时段未尝试过才跑。插件未就绪时**不消耗时段**，下一拍重试。
    async fn tick(&self) {
        let provider = WorkBuddyRegion::Cn.provider_id();
        if !crate::server::core::plugin_host::serves_provider(provider) {
            return;
        }
        let (due, _next, planned) = interval(&claim_auto_times(), logging::now_ms());
        if !due || planned <= 0 {
            return;
        }
        match task_state::read(STATE_KEY) {
            Ok(state) if state.last_attempt_at >= planned => return,
            Ok(_) => {}
            Err(error) => {
                logging::verbose("[Growth]", &format!("到期领取排期读取失败，本拍跳过：{error}"));
                return;
            }
        }
        // 用「手动」口径占用：时段判定与去重已在上面的 due/planned 检查里做完，
        // 这里不能再让上一次时刻配置留下的 nextRunAt 挡住新增时段。
        let guard = match task_state::claim(
            STATE_KEY,
            0,
            true,
            task_state::ManualBackoff::Respect,
            1000,
        ) {
            Ok(task_state::Claim::Acquired(guard)) => guard,
            Ok(task_state::Claim::Deferred(_)) => return,
            Err(error) => {
                logging::verbose("[Growth]", &format!("到期领取排期占用失败，本拍跳过：{error}"));
                return;
            }
        };
        // task_state 的 claim 已先记一笔尝试（防止重启/双实例重发），与官方插件任务一致。
        let Some((claimed, failed)) = self.run_round("后台巡检").await else {
            drop(guard);
            return;
        };
        let summary = if failed > 0 {
            format!("领回 {claimed} 项，失败 {failed} 项")
        } else {
            format!("领回 {claimed} 项")
        };
        if let Err(error) = guard.finish(
            failed == 0,
            summary,
            Some(json!({ "claimed": claimed, "failed": failed })),
            0,
            0,
        ) {
            logging::verbose("[Growth]", &format!("到期领取排期收尾失败：{error}"));
        }
    }

    /// 停调度（改开关 / 进程退出时用）。**只停本调度**，与旅行互不影响。
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

    /// 开/关到期领取（写配置 + 起停调度）。
    ///
    /// 打开后由循环第一拍按执行时刻判定：今天最近一个时段已过且没跑过就补跑，
    /// 否则等到下一个设置时刻；不再「一开就跑」，与官方插件任务同口径。
    pub fn set_enabled(&self, enabled: bool) {
        set_claim_auto(enabled);
        if enabled {
            self.schedule();
        } else {
            self.stop();
            logging::log("[Growth]", "🎁 成长任务到期领取调度已停止");
        }
    }

    /// 跑一轮（防重入）。返回 `None` 表示已有一轮在跑。
    async fn run_round(&self, reason: &str) -> Option<(usize, usize)> {
        let _guard = {
            let mut inner = self.lock();
            if inner.running {
                logging::verbose("[Growth]", "上一轮到期领取仍在执行，本次跳过");
                return None;
            }
            inner.running = true;
            RunningGuard { inner: self.inner.clone() }
        };
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
        Some((claimed, failed))
    }

    /// 跑一轮（供启动之外的手动入口使用），防重入。
    ///
    /// 只做一件事 —— 领取到期任务。旅行是官方的定点排程（`core::growth_scheduler`），
    /// 两者不共享闸、不互相触发。
    pub async fn fire(&self, reason: &str) {
        self.run_round(reason).await;
    }

    /// 手动「立即执行一轮」（受开关控制：关着时不跑，与后台循环同口径 —— 用户
    /// 关掉开关后不该被「立即执行」绕过）。
    pub async fn run_now(&self) -> (usize, usize) {
        if !claim_auto_enabled() {
            return (0, 0);
        }
        let _guard = {
            let mut inner = self.lock();
            if inner.running {
                return (0, 0);
            }
            inner.running = true;
            RunningGuard { inner: self.inner.clone() }
        };
        claim_due_tasks(&self.store, "手动触发").await
    }
}

/// 全局单例（`init_global` / `stop_global` 与 `auto_checkin` 同一模式）。
static GLOBAL: std::sync::OnceLock<ClaimAuto> = std::sync::OnceLock::new();

/// 装入全局句柄（重复调用忽略）。
pub fn init_global(service: ClaimAuto) -> ClaimAuto {
    let _ = GLOBAL.set(service.clone());
    service
}

/// 启动期装配：装句柄并在开关打开时起调度。
///
/// 收成一个入口是为了让 `server/mod.rs` 只留一行 —— 句柄、开关判定、
/// 启动日志都留在这里，官方那个文件不必为 fork 多出字段或访问器。
pub fn install(store: AccountStore) {
    let service = init_global(ClaimAuto::new(store));
    if !claim_auto_enabled() {
        return;
    }
    logging::log(
        "[Growth]",
        &format!(
            "🎁 成长任务到期领取已启用（每天 {}）",
            claim_auto_times().join(", ")
        ),
    );
    service.start();
}

/// 改开关并起停调度（没有全局句柄时只写配置）。
pub fn set_enabled_global(enabled: bool) {
    match GLOBAL.get() {
        Some(service) => service.set_enabled(enabled),
        None => set_claim_auto(enabled),
    }
}

/// 签到中心快照里的 `claimAuto` 段（没有国内版账号时给 `Null`）。
pub fn center_entry(has_cn_account: bool) -> Value {
    if !has_cn_account {
        return Value::Null;
    }
    snapshot()
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

    /// 触发来源归档：三种 reason 各归各的 source 档位。
    #[test]
    fn reason_maps_to_ledger_source() {
        assert_eq!(reason_source("启动补跑"), "startup");
        assert_eq!(reason_source("手动触发"), "manual");
        assert_eq!(reason_source("后台巡检"), "auto");
        assert_eq!(reason_source("开启自动"), "auto");
    }

    /// 「该领的那天」= 到期日 − 1 天的北京日期键。
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

    /// 无 `validEnd` 的任务不参与到期领取（用户要的是「有结束时间的任务」）。
    #[test]
    fn due_for_claim_ignores_tasks_without_valid_end() {
        assert!(!due_for_claim(&json!({ "code": "x" })));
        assert!(!due_for_claim(&json!({ "validEnd": Value::Null })));
    }
}
