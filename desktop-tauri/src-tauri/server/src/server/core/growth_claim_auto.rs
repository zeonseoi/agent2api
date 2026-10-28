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
//! ── 上游调用走提供商插件 ────────────────────────────────────
//! 官方 v3.3.0 把执行体搬进了插件（`ProvidersPlugin/Agent2Providers` 的
//! `workbuddy::admin`），网关这一侧只剩 `admin.action` 的透传。所以本模块
//! 通过两条动作干活：`growth.status` 拿任务清单、`growth.task`（`mode=full`）
//! 点亮并领取。**到期判据依赖插件在任务行里透传 `validEnd`**（上游的
//! `valid_end`）—— 官方 v0.2.1 的 `task_row` 只回 `unlockAt`，本 fork 用的
//! `agent2api-providers` 3.3.0-beta.4-fork.1 补了 `validStart` / `validEnd`。
//! 装了不含该字段的插件时本模块不会命中任何任务（判据保守返回 false，
//! 不报错、也不误领）—— 升级插件即可，无需改本文件。
//!
//! ── 台账 ────────────────────────────────────────────────────
//! 每领回一条落一行 `travel_records`（`outcome = task_claimed`，见
//! `core::travel_records`）—— 与旅行共用同一张「成长记录」表，界面一处看全。
//!
//! ── 硬约束：release 是 `panic=abort`，本文件零 unwrap/expect/panic ──

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use crate::server::config;
use crate::server::core::account_store::AccountStore;
use crate::server::core::providers::workbuddy::Region as WorkBuddyRegion;
use crate::server::core::travel_records;
use crate::server::logging;

/// 轮询间隔：2 小时（领取判据按天，这个间隔足够早地撞上「到期前一天」，
/// 且大幅减少对上游的请求次数）。
///
/// 软件刚启动时不等这一轮 —— `start()` 会先补跑一轮（见那里）。
pub const TICK_MS: u64 = 2 * 60 * 60 * 1000;

/// config.json 里承载本功能开关的键（独立于官方旅行排程 `growthSchedule`）。
const CONFIG_KEY: &str = "growthClaimAuto";

// ─── 开关 ────────────────────────────────────────────────────

/// 读自动领取开关（缺失 → 关；默认关闭，理由见模块头）。
pub fn claim_auto_enabled() -> bool {
    config::current()
        .raw()
        .get(CONFIG_KEY)
        .and_then(|value| value.get("enabled"))
        .and_then(Value::as_bool)
        == Some(true)
}

/// 写自动领取开关。
pub fn set_claim_auto(enabled: bool) {
    config::update_raw_field(CONFIG_KEY, json!({ "enabled": enabled }));
}

// ─── 判据 ────────────────────────────────────────────────────

/// 任务是否「快到期」：`validEnd` 非空、且北京时间的今天 ≥ 到期日 − 1 天。
///
/// `validEnd` 是 ISO 8601 带时区串（如 `2026-11-02T23:59:00+08:00`）。取它
/// 的北京日期键，往前推一天得到「该领的那天」；今天到了或过了就返回 true
/// （到期当天/之后仍会尝试，直到上游把它判成已领或任务下线）。
///
/// 无期限（`validEnd` 为 null）的任务**不碰** —— 用户要的是「有结束时间的任务」。
///
/// ── 键名为什么是 `validEnd` 而不是 `valid_end` ────────────────
/// 这条判据读的是**插件回包**（`growth.status` 的 `tasks[].*`），不是上游原始
/// JSON：插件那层已经把 `task_code` / `accept_status` 归一连成 `code` /
/// `status`。到期字段同理归一到驼峰（与界面读的键一致）。
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

/// 国内版账号（成长任务只对这些账号有意义）。
///
/// 与官方旅行排程的账号过滤是同一段判据。**不去共用一个公共函数**
/// ——两个调度器要彼此完全独立，共享一个辅助函数会把它们的生命周期重新绑在一起
/// （改一处影响两处）。这段只有几行，重复比耦合划算。
fn cn_accounts(store: &AccountStore) -> Vec<Value> {
    store
        .accounts_for_provider(WorkBuddyRegion::Cn.provider_id())
        .into_iter()
        .filter(|account| {
            !account
                .get("enterpriseId")
                .and_then(Value::as_str)
                .is_some_and(|value| !value.is_empty())
        })
        .collect()
}

/// 触发来源 `reason` → 台账 `source` 档位（字面集合见 `core::travel_records`）。
fn reason_source(reason: &str) -> &'static str {
    match reason {
        "启动补跑" => "startup",
        "手动触发" => "manual",
        _ => "auto",
    }
}

// ─── 领取扫描 ────────────────────────────────────────────────

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
    let accounts = cn_accounts(store);
    let mut claimed = 0usize;
    let mut failed = 0usize;
    let Some(adapter) = crate::server::core::plugin_host::lookup(
        WorkBuddyRegion::Cn.provider_id(),
    ) else {
        logging::verbose("[Growth]", "未装入 WorkBuddy 提供商插件，到期领取跳过");
        return (0, 0);
    };
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
        let Ok(status) = adapter
            .admin_action(store, id, "growth.status", &Value::Null)
            .await
        else {
            continue;
        };
        // 插件已把默认口径与小程序口径合并去重（见插件 admin.rs 的 growth_status）
        let tasks = status
            .get("tasks")
            .and_then(|block| block.get("tasks"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for task in &tasks {
            let code = task.get("code").and_then(Value::as_str).unwrap_or("");
            if code.is_empty() {
                continue;
            }
            if task.get("status").and_then(Value::as_str) == Some("claimed") {
                continue;
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
    /// ── 启动补跑一轮（重要）────────────────────────────────────
    /// 循环体是「先睡 2 小时再跑」，若只是排上循环，刚开软件时要干等 2 小时才
    /// 处理一次。这里**立即补跑一轮**再接循环（与旅行的启动补跑同一意图）。
    /// 补跑不阻塞启动：`fire` 内部有防重入闸，且它自己在一个 spawn 里跑。
    pub fn start(&self) {
        if !claim_auto_enabled() {
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
                    if !claim_auto_enabled() {
                        continue;
                    }
                    service.fire("后台巡检").await;
                }
            }));
        }
        logging::log("[Growth]", "🎁 成长任务到期领取调度已启动（每 2 小时一轮）");
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
    /// 打开时**立即跑一轮**再排循环：用户按下开关的预期是「现在就管起来」。
    pub fn set_enabled(&self, enabled: bool) {
        set_claim_auto(enabled);
        if enabled {
            self.schedule();
            let service = self.clone();
            crate::spawn_task(async move {
                service.fire("开启自动").await;
            });
        } else {
            self.stop();
            logging::log("[Growth]", "🎁 成长任务到期领取调度已停止");
        }
    }

    /// 跑一轮（自动循环与「立即执行」共用）。防重入：已在跑就跳过。
    ///
    /// 只做一件事 —— 领取到期任务。旅行是官方的定点排程（`core::growth_scheduler`），
    /// 两者不共享闸、不互相触发。
    pub async fn fire(&self, reason: &str) {
        let _guard = {
            let mut inner = self.lock();
            if inner.running {
                logging::verbose("[Growth]", "上一轮到期领取仍在执行，本次跳过");
                return;
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

    /// 无 `validEnd` 的任务不参与到期领取（用户要的是「有结束时间的任务」）。
    #[test]
    fn due_for_claim_ignores_tasks_without_valid_end() {
        assert!(!due_for_claim(&json!({ "code": "x" })));
        assert!(!due_for_claim(&json!({ "validEnd": Value::Null })));
        // 上游原始键名也认（插件未归一时仍能判）
        assert!(!due_for_claim(&json!({ "valid_end": Value::Null })));
    }
}
