//! 派猫猫旅行与成长任务领取的**每账号明细台账**（`travel_records` 表，schema v14）。
//!
//! ── 解决什么问题（与 `checkin_records` 同一动机）────────────────
//! 官方旅行排程只把上一轮摘要写进配置（`growthSchedule.lastResult`），只反映
//! 「刚刚那轮做了什么」，回看不出「谁哪一天被派出去 / 领回了多少 / 谁失败了」。
//! 签到记录页要的是**可翻阅的明细**：哪个账号、哪天、什么来源触发、
//! 派出还是领回、领了多少、失败错在哪 —— 这张表就是那个落点。它取代了改造前
//! config.json 的 `travelHistory`（只记「每轮一行」的汇总，看不见账号粒度）。
//!
//! ── 只记「真动作」────────────────────────────────────────────
//! 旅行一趟跨小时才领，多数轮次对多数账号是「在途等待」或「当天已领满
//! 跳过」—— 那些不是事件。只把 `departed`（真派出）/ `claimed`（真领回）/
//! `failed`（真失败）记下来，台账才可读（与旧 `travel_history` 的取舍一致）。
//!
//! ── 它也承载「成长任务到期前自动领取」────────────────────────
//! 任务领取不是猫猫旅行的动作，但同属「WorkBuddy 国内版成长中心」这一路、同由
//! 一条两小时循环触发（`core::growth_claim_auto`，与旅行循环相互独立）。**不另开
//! 一张表**：多开一套查询 / 筛选 / 清空只是重复建设。因此多一个 `task_claimed`
//! 档位、失败也多一个 `task_failed` 档位（见那两个常量的说明）。
//!
//! ── 读侧在 `core::checkin_ledger` ────────────────────────────
//! 本模块**只负责写**（记账）。查询 / 筛选 / 清空由 `core::checkin_ledger` 统一
//! 处理 —— 它把本表与 `checkin_records` 合并成一张表给签到记录页（见那里的模块头）。
//!
//! ── source 的字面集合 ────────────────────────────────────────
//! `auto` 后台循环 / `startup` 启动补跑 / `manual` 手动「立即执行」与单账号
//! 「派猫猫 / 领回」。前者由 `travel_ledger`（官方旅行排程的记账点）与 `growth_claim_auto::
//! claim_due_tasks` 按 `reason` 归并，后两者从 `api::growth::travel_run` 进来。
//!
//! ── 写失败不报错 ─────────────────────────────────────────────
//! 记账点在旅行动作路径上（动作已经打到上游）—— 台账缺一条不该让那次动作报错，
//! 失败打一行 **verbose** 日志就完（与 `checkin_records::insert` 同取舍）。
//!
//! ── 硬约束：持锁（连接锁）期间不打日志 ────────────────────────
//! 与 `core::checkin_records` 同一条：`Db::with*` 拿的是全局唯一的连接锁，
//! `logging::log` 要往同一个库写 `logs` 表 —— 闭包里打日志会当场死锁。

use std::sync::OnceLock;

use rusqlite::params;

use crate::server::core::beijing;
use crate::server::db::Db;
use crate::server::logging;

static DB: OnceLock<Option<Db>> = OnceLock::new();

/// 结果档位（`outcome` 列的字面集合）
pub const OUTCOME_DEPARTED: &str = "departed";
pub const OUTCOME_CLAIMED: &str = "claimed";
/// 领养成功（无猫 → 有猫；官方状态机的 `adopt` 档，同时入账 +300 积分）
pub const OUTCOME_ADOPTED: &str = "adopted";
/// 成长任务到期前的自动领取（不经猫猫旅行，落同一张表；见 `core::growth_claim_auto`
/// 的到期领取扫描）。message 是收益文案（如 `+300 积分`）。
pub const OUTCOME_TASK_CLAIMED: &str = "task_claimed";
/// 猫猫旅行的失败（派出 / 领回 / 领养任一步报错）。
pub const OUTCOME_FAILED: &str = "failed";
/// 成长任务领取的失败 —— 与 [`OUTCOME_FAILED`] 分开，界面才分得清「猫猫旅行失败」
/// 还是「成长任务领取失败」（两条线共用一张表，失败原因也只在这一个字面量上有别）。
pub const OUTCOME_TASK_FAILED: &str = "task_failed";

pub fn install(db: Option<Db>) {
    let _ = DB.set(db);
}

fn database() -> Option<&'static Db> {
    DB.get().and_then(Option::as_ref)
}

// ─── 记账 ────────────────────────────────────────────────────

/// 落一行明细。`outcome` 取上面六个字面量之一；`message` 是收益文案
/// （派出行为空），`error` 是失败原因原文（成功行为空）。
///
/// 一次调用 = 一次真动作（派出 / 领养 / 领回 / 领任务 / 失败），调用点在
/// `core::travel_ledger` 与 `core::growth_claim_auto`。
pub fn record(
    source: &str,
    reason: &str,
    account_id: &str,
    account_name: &str,
    provider: &str,
    outcome: &str,
    message: Option<String>,
    error: Option<String>,
) {
    insert(Entry {
        source: source.to_string(),
        reason: reason.to_string(),
        account_id: account_id.to_string(),
        account_name: account_name.to_string(),
        provider: provider.to_string(),
        outcome: outcome.to_string(),
        ok: matches!(
            outcome,
            OUTCOME_DEPARTED | OUTCOME_CLAIMED | OUTCOME_ADOPTED | OUTCOME_TASK_CLAIMED
        ),
        message,
        error,
    });
}

/// 一行明细的落库形态（`insert` 的入参；字段与表列一一对应）
struct Entry {
    source: String,
    reason: String,
    account_id: String,
    account_name: String,
    provider: String,
    outcome: String,
    ok: bool,
    message: Option<String>,
    error: Option<String>,
}

/// 落一行明细。失败打 verbose，不向上报（见模块头「写失败不报错」）。
///
/// ── 没有保留期裁剪（与 `checkin_records` 同口径）─────────────
/// 官方从 v3.0.4 起把请求日志 / 按天聚合 / 签到记录都改成**全量保留**，保留天数
/// 设置项一并移除。本表是 `checkin_records` 的同形兄弟（见模块头），跟着它走：
/// 不再顺手 DELETE 超期行 —— 要清理时走签到记录页的「清空 / 按筛选删除」那条
/// 显式路径（读侧在 `core::checkin_ledger`）。
fn insert(entry: Entry) {
    let Some(db) = database() else { return };
    let ts = logging::now_ms();
    let date = beijing::today_key();
    let failed = db
        .with_mut(|conn| -> Result<(), rusqlite::Error> {
            conn.execute(
                "INSERT INTO travel_records
                   (ts, date, source, reason, account_id, account_name, provider,
                    outcome, ok, message, error)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    ts,
                    date,
                    entry.source,
                    entry.reason,
                    entry.account_id,
                    entry.account_name,
                    entry.provider,
                    entry.outcome,
                    entry.ok as i64,
                    entry.message,
                    entry.error,
                ],
            )?;
            Ok(())
        })
        .map(|result| result.is_err())
        .unwrap_or(true);
    if failed {
        logging::verbose("[Travel]", "旅行记录未能落库（库不可用或写入失败）");
    }
}

