//! 签到中心「签到记录」页的**合并台账**（`checkin_records` + `travel_records`）。
//!
//! ── 为什么要合并 ────────────────────────────────────────────
//! 两张表记的是同一件事的两半，同属「签到中心的动作明细」：
//!   · `checkin_records`（schema v11）：每账号一次签到的结果（成功 / 已领 /
//!     等窗口 / 失败），写入方 `core::checkin_records`；
//!   · `travel_records`（schema v14，本 fork）：猫猫旅行的真动作（派出 / 领回 /
//!     领养）与成长任务的到期自动领取（领任务 / 领取失败），写入方
//!     `core::travel_auto` 与 `core::growth_claim_auto`。
//! 改造前它们是「签到记录」「成长记录」两个并排的标签页，列与筛选逐字相同 ——
//! 用户要的是**一处看全**，于是合并成一张表、一个标签页（后端合并，见下）。
//!
//! ── 为什么在后端合并 ────────────────────────────────────────
//! 这页是**服务端真分页**（`limit` / `offset` + `total`）：两个来源各有自己的
//! 时间序与总数，前端把两页拼起来做不出正确的翻页与读数。所以合并点只能在这里：
//! `UNION ALL` 两表 → 统一 `ORDER BY ts DESC, row_id DESC` → 再切 LIMIT / OFFSET。
//!
//! ── rowId 的形态（前端拿它当 React key）──────────────────────
//! 两表的 `row_id` 都从 1 自增，直接透出会在合并结果里撞号。这里带来源前缀：
//! `c123` = checkin_records 的 123，`g45` = travel_records 的 45。
//!
//! ── kept_active 只有签到行有 ────────────────────────────────
//! 「国际版保活」是签到特有的标记（见 `core::checkin_records::kept_active_of`）；
//! 旅行行恒 `0`（`FALSE`），前端那枚「保活」徽章因此只出现在签到行上。
//!
//! ── 筛选链与删除护栏 ────────────────────────────────────────
//! 筛选字段两表同名（outcome / provider / keyword / start / end），一份条件同时
//! 作用于两表 —— 「筛出什么就删什么」的口径与原来两页各自实现时一致。无筛选的
//! 删除仍必须显式 `all=1`（API 层护栏，见 `api::checkin_ledger`）。

use std::sync::OnceLock;

use serde_json::{json, Value};

use crate::server::db::Db;

static DB: OnceLock<Option<Db>> = OnceLock::new();

pub fn install(db: Option<Db>) {
    let _ = DB.set(db);
}

fn database() -> Option<&'static Db> {
    DB.get().and_then(Option::as_ref)
}

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

    /// 拼 WHERE 子句与参数。两表列名一致，同一份条件两边通用；
    /// 查询与删除共用同一份过滤链（「筛出什么就删什么」）。
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

/// 一页明细的查询（`offset` 真分页，`limit` 缺省由 API 层给、上限 1000）。
/// 响应形状 `{records: [...], total}`：total 是**同条件**的两表之和（分页器用），
/// records 按时间倒序（新的在前）。库不可用给空表（界面显示空态，不报错）。
pub fn query(query: &Query, limit: usize, offset: usize) -> Value {
    let limit = limit.clamp(1, 1000);
    let Some(db) = database() else {
        return json!({ "records": [], "total": 0 });
    };
    let (where_clause, filter_params) = query.clause();
    // 两表列集合对齐后 UNION ALL：旅行行没有 kept_active，补常量 0（前端那枚
    // 「保活」徽章因此只可能出现在签到行上）；rowId 带来源前缀避免两表撞号。
    let sql_rows = format!(
        "SELECT * FROM (
             SELECT 'c' || row_id AS uid, ts, date, source, reason, account_id,
                    account_name, provider, outcome, ok, message, error, kept_active
               FROM checkin_records{where_clause}
             UNION ALL
             SELECT 'g' || row_id AS uid, ts, date, source, reason, account_id,
                    account_name, provider, outcome, ok, message, error, 0
               FROM travel_records{where_clause}
         )
         ORDER BY ts DESC, uid DESC
         LIMIT {limit} OFFSET {offset}"
    );
    let sql_count = format!(
        "SELECT (SELECT COUNT(*) FROM checkin_records{where_clause})
              + (SELECT COUNT(*) FROM travel_records{where_clause})"
    );
    db.with(|conn| -> Result<Value, rusqlite::Error> {
        let params_from_json = |values: &[Value]| -> Vec<String> {
            values.iter().map(value_to_text).collect()
        };
        // UNION 子查询与 COUNT 各出现一次 WHERE，参数要按出现次数重复铺开
        let row_params: Vec<String> = params_from_json(&filter_params)
            .into_iter()
            .cycle()
            .take(filter_params.len() * 2)
            .collect();
        let count_params: Vec<String> = params_from_json(&filter_params)
            .into_iter()
            .cycle()
            .take(filter_params.len() * 2)
            .collect();
        let total: i64 = conn.query_row(
            &sql_count,
            rusqlite::params_from_iter(count_params),
            |row| row.get(0),
        )?;
        let mut statement = conn.prepare(&sql_rows)?;
        let rows = statement.query_map(
            rusqlite::params_from_iter(row_params),
            |row| {
                Ok(json!({
                    "rowId": row.get::<_, String>("uid")?,
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
                    "keptActive": row.get::<_, i64>("kept_active").unwrap_or(0) != 0,
                }))
            },
        )?;
        let records: Vec<Value> = rows.filter_map(|item| item.ok()).collect();
        Ok(json!({ "records": records, "total": total }))
    })
    .and_then(|result| result.ok())
    .unwrap_or_else(|| json!({ "records": [], "total": 0 }))
}

/// 按筛选条件删除（「清空筛选结果」），两表各删一遍；无筛选条件的调用**必须**
/// 由 API 层先过显式 `all=1` 护栏（照抄 logs 的规则，理由见 api::checkin_ledger）。
/// 返回删除条数之和；库不可用给 None（API 层给 500 信封）。
pub fn clear_where(query: &Query) -> Option<usize> {
    let db = database()?;
    let (where_clause, filter_params) = query.clause();
    let params_from_json = |values: &[Value]| -> Vec<String> {
        values.iter().map(value_to_text).collect()
    };
    db.with_mut(|conn| {
        let mut removed = 0usize;
        for table in ["checkin_records", "travel_records"] {
            let sql = format!("DELETE FROM {table}{where_clause}");
            removed += conn
                .execute(&sql, rusqlite::params_from_iter(params_from_json(&filter_params)))
                .unwrap_or(0);
        }
        removed
    })
}

/// 清空全部（API 层护栏已过），两表各清一遍。返回删除条数之和。
pub fn clear_all() -> Option<usize> {
    let db = database()?;
    db.with_mut(|conn| {
        let mut removed = 0usize;
        for table in ["checkin_records", "travel_records"] {
            removed += conn
                .execute(&format!("DELETE FROM {table}"), [])
                .unwrap_or(0);
        }
        removed
    })
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

    /// 两表的行合并后：按时间倒序混排、total 是两表之和、rowId 带来源前缀、
    /// 筛选与清空同时作用于两表。
    ///
    /// 行**直接 INSERT**，不走 `checkin_records::record_row` / `travel_records::record`
    /// —— 那两个模块的句柄是 `OnceLock`（进程内只装一次），借它们装库会让本用例
    /// 跟同进程内别的用例抢全局句柄（谁先跑谁赢），是一条自找的偶发失败。
    /// 这里只装本模块自己的句柄，数据面直接用 SQL 铺。
    #[test]
    fn union_query_filter_and_clear() {
        let (db, _guard) = TempDb::open("checkin-ledger");
        install(Some(db.clone()));

        let seed = |conn: &rusqlite::Connection, table: &str, rows: &[(i64, &str, &str, &str)]| {
            for (ts, account, outcome, detail) in rows {
                let message = Some(detail.to_string()).filter(|text| !text.is_empty());
                conn.execute(
                    &format!(
                        "INSERT INTO {table}
                           (ts, date, source, reason, account_id, account_name, provider,
                            outcome, ok, message, error{extra})
                         VALUES (?1, '2026-10-16', 'auto', '后台巡检', ?2, ?2, 'workbuddy',
                                 ?3, 0, ?4, NULL{extra_val})",
                        extra = if table == "checkin_records" { ", kept_active" } else { "" },
                        extra_val = if table == "checkin_records" { ", 0" } else { "" },
                    ),
                    rusqlite::params![ts, account, outcome, message],
                )
                .expect("铺测试数据");
            }
        };
        db.with_mut(|conn| {
            seed(
                conn,
                "checkin_records",
                &[(1_000, "账号一", "success", "今日积分 +100")],
            );
            seed(
                conn,
                "travel_records",
                &[
                    (2_000, "账号二", "departed", ""),
                    (3_000, "账号三", "task_failed", ""),
                ],
            );
        })
        .expect("库不可用");

        let all = query(&Query::default(), 50, 0);
        assert_eq!(all["total"], 3, "签到 1 条 + 旅行 2 条都该被数到：{all}");
        let rows = all["records"].as_array().unwrap();
        assert_eq!(rows.len(), 3);
        // 时间倒序：最新的 task_failed 那条在最前
        assert_eq!(rows[0]["outcome"], "task_failed", "应按 ts 倒序：{all}");
        let ids: Vec<&str> = rows
            .iter()
            .map(|row| row["rowId"].as_str().unwrap())
            .collect();
        assert!(
            ids.iter().any(|id| id.starts_with('c')) && ids.iter().any(|id| id.starts_with('g')),
            "两来源的行都要在，且 rowId 带来源前缀：{ids:?}"
        );
        // 签到行有 keptActive 字段、旅行行恒 false
        let travel = rows
            .iter()
            .find(|row| row["rowId"].as_str().unwrap().starts_with('g'))
            .expect("应有旅行行");
        assert_eq!(travel["keptActive"], false);

        // 按结果筛选：只留成长任务失败那一条
        let only_task_failed = query(
            &Query {
                outcome: Some(crate::server::core::travel_records::OUTCOME_TASK_FAILED.to_string()),
                ..Default::default()
            },
            50,
            0,
        );
        assert_eq!(only_task_failed["total"], 1, "只应剩任务失败行：{only_task_failed}");

        // 关键词扫账号名（两表同一份过滤链）
        let by_name = query(
            &Query { keyword: Some("账号二".to_string()), ..Default::default() },
            50,
            0,
        );
        assert_eq!(by_name["total"], 1, "关键词应命中旅行表的账号二：{by_name}");

        // 带筛选删除只删命中；两表都要被作用到
        assert_eq!(
            clear_where(&Query { keyword: Some("账号二".to_string()), ..Default::default() }),
            Some(1)
        );
        assert_eq!(query(&Query::default(), 50, 0)["total"], 2);

        // 清空全部（两表各一遍）
        assert_eq!(clear_all(), Some(2));
        assert_eq!(query(&Query::default(), 50, 0)["total"], 0);
    }
}
