//! 派猫猫旅行的**每账号明细台账**（`travel_records` 表，schema v14）。
//!
//! ── 解决什么问题（与 `checkin_records` 同一动机）────────────────
//! 旅行的执行结果是逐账号状态缓存（`travelCache`，给界面读数用），只反映
//! 「此刻每个账号什么状态」，回看不出「谁哪一轮被派出去 / 领回了多少 / 谁失败了」。
//! 签到中心的「旅行记录」页要的是**可翻阅的明细**：哪个账号、哪天、什么来源触发、
//! 派出还是领回、领了多少、失败错在哪 —— 这张表就是那个落点。它取代了改造前
//! config.json 的 `travelHistory`（只记「每轮一行」的汇总，看不见账号粒度）。
//!
//! ── 只记「真动作」────────────────────────────────────────────
//! 旅行每 2 小时一轮、跨小时才领，多数轮次对多数账号是「在途等待」或「当天已领满
//! 跳过」—— 那些不是事件。只把 `departed`（真派出）/ `claimed`（真领回）/
//! `failed`（真失败）记下来，台账才可读（与旧 `travel_history` 的取舍一致）。
//!
//! ── 它也承载「成长任务到期前自动领取」────────────────────────
//! 任务领取不是猫猫旅行的动作，但同属「WorkBuddy 国内版成长中心」这一路、同由
//! `core::travel_auto` 的两小时循环触发。**不另开一张表**：界面把这张表读成
//! 「成长记录」（旅行 + 任务领取在一处看），多开一套查询 / 筛选 / 清空只是重复
//! 建设。因此多一个 `task_claimed` 档位（见该常量的说明）。
//!
//! ── source 的字面集合 ────────────────────────────────────────
//! `auto` 后台循环 / `startup` 启动补跑 / `manual` 手动「立即执行」与单账号
//! 「派猫猫 / 领回」。前者由 `travel_auto::collect` 按 `reason` 归并，后两者从
//! `travel_auto::depart_account / claim_account` 进来。
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
use serde_json::{json, Value};

use crate::server::config;
use crate::server::core::beijing;
use crate::server::db::Db;
use crate::server::logging;

static DB: OnceLock<Option<Db>> = OnceLock::new();

/// 结果档位（`outcome` 列的字面集合）
pub const OUTCOME_DEPARTED: &str = "departed";
pub const OUTCOME_CLAIMED: &str = "claimed";
/// 领养成功（无猫 → 有猫；官方状态机的 `adopt` 档，同时入账 +300 积分）
pub const OUTCOME_ADOPTED: &str = "adopted";
/// 成长任务到期前的自动领取（不经猫猫旅行，落同一张表；见 `core::travel_auto`
/// 的到期领取扫描）。message 是收益文案（如 `+300 积分`）。
pub const OUTCOME_TASK_CLAIMED: &str = "task_claimed";
pub const OUTCOME_FAILED: &str = "failed";

pub fn install(db: Option<Db>) {
    let _ = DB.set(db);
}

fn database() -> Option<&'static Db> {
    DB.get().and_then(Option::as_ref)
}

// ─── 记账 ────────────────────────────────────────────────────

/// 落一行明细。`outcome` 取上面四个字面量之一；`message` 是收益文案
/// （派出行为空），`error` 是失败原因原文（成功行为空）。
///
/// 一次调用 = 一次真动作（派出 / 领养 / 领回 / 失败），调用点在 `core::travel_auto`。
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

/// 落一行 + 顺手按保留期裁超期行（一次事务）。失败打 verbose，不向上报
/// （见模块头「写失败不报错」）。
fn insert(entry: Entry) {
    let Some(db) = database() else { return };
    let ts = logging::now_ms();
    let date = beijing::today_key();
    let cutoff = retention_cutoff();
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
            // 保留期沿用「请求日志」的天数（不新增设置项）。旅行记账一天几轮、
            // 每轮只记真动作的少数账号，顺手裁一条 DELETE 的代价可以忽略。
            conn.execute("DELETE FROM travel_records WHERE date < ?1", [cutoff])?;
            Ok(())
        })
        .map(|result| result.is_err())
        .unwrap_or(true);
    if failed {
        logging::verbose("[Travel]", "旅行记录未能落库（库不可用或写入失败）");
    }
}

// ─── 保留期 ──────────────────────────────────────────────────

/// 保留期的日期下界（北京时间键）：沿用「请求日志」的天数，口径与
/// `checkin_records::retention_cutoff` 逐字相同（保留 N 天 = 含今天在内 N 个
/// 自然日，往前推 N-1 天；下限 1 天、上限 10 年）。
fn retention_cutoff() -> String {
    let days = config::retention_settings().request_days.clamp(1, 3650);
    let today = beijing::today_key();
    match chrono::NaiveDate::parse_from_str(&today, "%Y-%m-%d") {
        Ok(today) => (today - chrono::Duration::days(days - 1))
            .format("%Y-%m-%d")
            .to_string(),
        Err(_) => today, // 系统时钟异常到连日期键都不是合法格式：不裁比乱裁好
    }
}

/// 「立即清理」（设置页 / 保留期改小后）与外部裁剪入口。
pub fn prune() {
    let Some(db) = database() else { return };
    let cutoff = retention_cutoff();
    let _ = db.with_mut(|conn| {
        conn.execute("DELETE FROM travel_records WHERE date < ?1", [cutoff])
            .ok()
    });
}

// ─── 查询与删除 ──────────────────────────────────────────────

/// 查询条件（GET 的查询串与 DELETE 的护栏共用一套字段）
#[derive(Clone, Debug, Default)]
pub struct Query {
    pub outcome: Option<String>,
    pub provider: Option<String>,
    pub keyword: Option<String>,
    pub start: Option<i64>,
    pub end: Option<i64>,
}

impl Query {
    /// 是否带了任一筛选条件（DELETE 的护栏判据：一个都没带 = 全清）
    pub fn has_filters(&self) -> bool {
        self.outcome.as_deref().is_some_and(|text| !text.is_empty())
            || self.provider.as_deref().is_some_and(|text| !text.is_empty())
            || self.keyword.as_deref().is_some_and(|text| !text.is_empty())
            || self.start.is_some()
            || self.end.is_some()
    }

    /// 拼 WHERE 子句与参数（查询与删除共用同一份过滤链 —— 「筛出什么就删什么」）
    fn clause(&self) -> (String, Vec<Value>) {
        let mut conditions: Vec<String> = Vec::new();
        let mut params_json: Vec<Value> = Vec::new();
        if let Some(outcome) = self.outcome.as_deref().filter(|text| !text.is_empty()) {
            conditions.push("outcome = ?".to_string());
            params_json.push(json!(outcome));
        }
        if let Some(provider) = self.provider.as_deref().filter(|text| !text.is_empty()) {
            conditions.push("provider = ?".to_string());
            params_json.push(json!(provider));
        }
        if let Some(keyword) = self.keyword.as_deref().filter(|text| !text.is_empty()) {
            // 关键词扫账号名 / 收益文案 / 错误原文 / 触发原因四个可读字段
            let pattern = format!("%{}%", keyword.replace('%', "\\%").replace('_', "\\_"));
            conditions.push(
                "(account_name LIKE ? ESCAPE '\\'
                  OR message LIKE ? ESCAPE '\\'
                  OR error LIKE ? ESCAPE '\\'
                  OR reason LIKE ? ESCAPE '\\')"
                    .to_string(),
            );
            for _ in 0..4 {
                params_json.push(json!(pattern));
            }
        }
        if let Some(start) = self.start {
            conditions.push("ts >= ?".to_string());
            params_json.push(json!(start));
        }
        if let Some(end) = self.end {
            conditions.push("ts <= ?".to_string());
            params_json.push(json!(end));
        }
        if conditions.is_empty() {
            (String::new(), params_json)
        } else {
            (format!(" WHERE {}", conditions.join(" AND ")), params_json)
        }
    }
}

/// 查询一页明细（`offset` 真分页，`limit` 缺省 200、上限 1000）。
/// 响应形状 `{records: [...], total}`：total 是**同条件**的总数（分页器用），
/// records 按时间倒序（新的在前）。库不可用给空表（界面显示空态，不报错）。
pub fn query(query: &Query, limit: usize, offset: usize) -> Value {
    let limit = limit.clamp(1, 1000);
    let Some(db) = database() else {
        return json!({ "records": [], "total": 0 });
    };
    let (where_clause, filter_params) = query.clause();
    let sql_rows = format!(
        "SELECT row_id, ts, date, source, reason, account_id, account_name,
                provider, outcome, ok, message, error
         FROM travel_records{where_clause}
         ORDER BY ts DESC, row_id DESC
         LIMIT {limit} OFFSET {offset}"
    );
    let sql_count = format!("SELECT COUNT(*) FROM travel_records{where_clause}");
    db.with(|conn| -> Result<Value, rusqlite::Error> {
        let params_from_json = |values: &[Value]| -> Vec<String> {
            values.iter().map(value_to_text).collect()
        };
        let total: i64 = conn.query_row(
            &sql_count,
            rusqlite::params_from_iter(params_from_json(&filter_params)),
            |row| row.get(0),
        )?;
        let mut statement = conn.prepare(&sql_rows)?;
        let rows = statement.query_map(
            rusqlite::params_from_iter(params_from_json(&filter_params)),
            |row| {
                Ok(json!({
                    "rowId": row.get::<_, i64>("row_id")?,
                    "ts": row.get::<_, i64>("ts")?,
                    "date": row.get::<_, String>("date")?,
                    "source": row.get::<_, String>("source")?,
                    "reason": row.get::<_, String>("reason")?,
                    "accountId": row.get::<_, String>("account_id")?,
                    "accountName": row.get::<_, String>("account_name")?,
                    "provider": row.get::<_, String>("provider")?,
                    "outcome": row.get::<_, String>("outcome")?,
                    "ok": row.get::<_, i64>("ok").unwrap_or(0) != 0,
                    "message": row.get::<_, Option<String>>("message")?,
                    "error": row.get::<_, Option<String>>("error")?,
                }))
            },
        )?;
        let records: Vec<Value> = rows.filter_map(|item| item.ok()).collect();
        Ok(json!({ "records": records, "total": total }))
    })
    .and_then(|result| result.ok())
    .unwrap_or_else(|| json!({ "records": [], "total": 0 }))
}

/// 按筛选条件删除（「清空筛选结果」）；无筛选条件的调用**必须**由 API 层
/// 先过显式 `all=1` 护栏（照抄 logs 的规则，理由见 api::travel_records）。
/// 返回删除条数；库不可用给 None（API 层给 500 信封）。
pub fn clear_where(query: &Query) -> Option<usize> {
    let db = database()?;
    let (where_clause, filter_params) = query.clause();
    let sql = format!("DELETE FROM travel_records{where_clause}");
    let params_from_json = |values: &[Value]| -> Vec<String> {
        values.iter().map(value_to_text).collect()
    };
    db.with_mut(|conn| {
        conn.execute(&sql, rusqlite::params_from_iter(params_from_json(&filter_params)))
            .ok()
    })
    .flatten()
}

/// 清空全部（API 层护栏已过）。返回删除条数。
pub fn clear_all() -> Option<usize> {
    let db = database()?;
    db.with_mut(|conn| conn.execute("DELETE FROM travel_records", []).ok()).flatten()
}

/// rusqlite 的参数都是 ToSql：这里统一转文本（数字 / 字符串都安全）
fn value_to_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::db::test_temp::TempDb;

    /// 记账三档（派出 / 领回 / 失败）都能落库、能按条件查出、能按结果过滤与清空。
    ///
    /// `install` 走 `OnceLock`：进程内只装一次。本模块是唯一安装方，用例之间
    /// 共用同一个临时库即可（此处只有一个用例）。
    #[test]
    fn record_query_filter_and_clear() {
        let (db, _guard) = TempDb::open("travel-records");
        install(Some(db));

        record(
            "auto", "后台巡检", "acc-1", "账号一", "workbuddy",
            OUTCOME_DEPARTED, None, None,
        );
        record(
            "auto", "后台巡检", "acc-2", "账号二", "workbuddy",
            OUTCOME_CLAIMED, Some("+12 积分".to_string()), None,
        );
        record(
            "manual", "手动派猫猫", "acc-3", "账号三", "workbuddy",
            OUTCOME_FAILED, None, Some("上游 500".to_string()),
        );

        let all = query(&Query::default(), 50, 0);
        assert_eq!(all["total"], 3, "三条都应落库：{all}");
        let rows = all["records"].as_array().unwrap();
        assert_eq!(rows.len(), 3);
        let claimed = rows
            .iter()
            .find(|row| row["outcome"] == OUTCOME_CLAIMED)
            .expect("应有领回行");
        assert_eq!(claimed["message"], "+12 积分");
        assert_eq!(claimed["ok"], true);
        let failed = rows
            .iter()
            .find(|row| row["outcome"] == OUTCOME_FAILED)
            .expect("应有失败行");
        assert_eq!(failed["error"], "上游 500");
        assert_eq!(failed["ok"], false);

        // 按结果过滤
        let only_failed = query(
            &Query { outcome: Some(OUTCOME_FAILED.to_string()), ..Default::default() },
            50,
            0,
        );
        assert_eq!(only_failed["total"], 1, "只应剩失败行：{only_failed}");

        // 关键词扫账号名
        let by_name = query(
            &Query { keyword: Some("账号二".to_string()), ..Default::default() },
            50,
            0,
        );
        assert_eq!(by_name["total"], 1, "关键词应命中账号二：{by_name}");

        // 清空全部
        assert_eq!(clear_all(), Some(3));
        assert_eq!(query(&Query::default(), 50, 0)["total"], 0);
    }
}
