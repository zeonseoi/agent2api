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
//! ── 分工：定时器在网关，判据与动作在插件 ────────────────────
//! 与官方的签到同一形状（`core::auto_checkin` 定时、`checkin.claim` 执行）：
//! 本模块只剩「每 2 小时触发一次 + 开关 + 防重入 + 记台账」，扫哪些账号、
//! 哪些任务算到期、怎么领全在插件的一条批量动作 `growth.claim.due` 里
//! （`ProvidersPlugin/Agent2Providers` 的 `workbuddy::admin`）。
//!
//! 为什么这样切：到期判据要用插件自己归一的 `validEnd`，账号枚举也要按归属
//! 过滤（国内版、排除企业号）—— 两件都是 provider 知识。留在网关等于把 provider
//! 的判据抄一份到宿主，改一次判据要动两边。
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

/// 让插件扫一遍本家账号、领掉「快到期」的任务。
///
/// ── 判据与动作都在插件 ──────────────────────────────────────
/// 「扫哪些账号、哪些任务算到期、怎么领」是 provider 知识，跟签到一样归插件
/// （`admin.action = growth.claim.due`）。网关只保留定时器、开关、防重入，
/// 以及把插件回的摘要记进台账。
///
/// 插件版本过旧（没有这条动作）时 `admin.action` 报「不认识管理动作」，这里
/// 记一条 warning 后跳过 —— 不能静默失效，否则界面上开了开关却什么都不发生。
///
/// 返回 `(领取成功数, 失败数)`，供轮次汇总日志用。
pub async fn claim_due_tasks(store: &AccountStore, reason: &str) -> (usize, usize) {
    let Some(adapter) = crate::server::core::plugin_host::lookup(
        WorkBuddyRegion::Cn.provider_id(),
    ) else {
        logging::verbose("[Growth]", "未装入 WorkBuddy 提供商插件，到期领取跳过");
        return (0, 0);
    };
    let summary = match adapter
        .admin_action(store, "", "growth.claim.due", &Value::Null)
        .await
    {
        Ok(summary) => summary,
        Err(error) => {
            logging::log_with_level(
                "[Growth]",
                &format!("到期领取未执行：{}", error.message),
                "warn",
            );
            return (0, 0);
        }
    };
    record_summary(reason, &summary);
    let claimed = summary.get("claimed").and_then(Value::as_u64).unwrap_or(0) as usize;
    let failed = summary.get("failed").and_then(Value::as_u64).unwrap_or(0) as usize;
    (claimed, failed)
}

/// 把插件回包里的 `actions[]` 记进台账（`travel_records`）。
///
/// 只认两种结论：`claimed`（真领到）/ `failed`（真失败）。跳过类（已领 /
/// 未到期 / 未解锁 / 不可伪造）插件不会回，这里也不兜底 —— 台账只记事件。
fn record_summary(reason: &str, summary: &Value) {
    let Some(actions) = summary.get("actions").and_then(Value::as_array) else {
        return;
    };
    let source = reason_source(reason);
    let provider = WorkBuddyRegion::Cn.provider_id();
    for action in actions {
        let id = action.get("accountId").and_then(Value::as_str).unwrap_or("");
        if id.is_empty() {
            continue;
        }
        let name = action.get("name").and_then(Value::as_str).unwrap_or("");
        let code = action.get("code").and_then(Value::as_str).unwrap_or("");
        match action.get("outcome").and_then(Value::as_str).unwrap_or("") {
            "claimed" => {
                let message = action.get("message").and_then(Value::as_str).unwrap_or("");
                let credit = action.get("credit").and_then(Value::as_i64).unwrap_or(0);
                travel_records::record(
                    source,
                    reason,
                    id,
                    name,
                    provider,
                    travel_records::OUTCOME_TASK_CLAIMED,
                    Some(format!("{message}（+{credit} 积分）")),
                    None,
                );
            }
            "failed" => {
                let error = action.get("error").and_then(Value::as_str).unwrap_or("");
                travel_records::record(
                    source,
                    reason,
                    id,
                    name,
                    provider,
                    travel_records::OUTCOME_TASK_FAILED,
                    None,
                    Some(format!("{code}：{error}")),
                );
            }
            _ => {}
        }
    }
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
}
