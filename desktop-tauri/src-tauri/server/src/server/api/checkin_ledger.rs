//! 签到中心「签到记录」页的路由（合并台账 `checkin_records` + `travel_records`）。
//!
//! ```text
//!   GET    /api/checkin-ledger   查询（?limit=&offset=&outcome=&provider=&keyword=&start=&end=）
//!   DELETE /api/checkin-ledger   删除（带筛选 = 只删命中的；全清必须显式 all=1）
//! ```
//!
//! ── 响应形状 ────────────────────────────────────────────────
//! GET → `{records: [...], total}`：total 是**同条件**的两表之和（分页器用它算
//! 页数），records 按时间倒序、两来源混排。rowId 带来源前缀（`c123` / `g45`），
//! 因为两张表的 row_id 各自自增、直接透出会在合并结果里撞号。
//! DELETE → `{success:true, removed}`（removed = 两表删除条数之和）。
//!
//! ── DELETE 的护栏（照抄 logs 的规则）────────────────────────
//! 带任一筛选参数时只删命中的条目（与 GET 同一份过滤链 —— 界面上筛出什么
//! 就删什么，两表都作用）；清空全部必须**显式**带 `all=1`，不带的空请求给 400。
//! 护栏的完整理由见 `api::logs_api::clear_logs`：筛选清空漏传参数曾经把全部
//! 日志删掉，用多一个参数把这类事故变成一条 400。
//!
//! ── 参数解析 ────────────────────────────────────────────────
//! 与日志/报表同一套口径（`api::logs_api` 的解析函数语义）：查询串里的值都是
//! 字符串，数字项非法（空串 / 非数字）**忽略而不是报错**；limit 夹在
//! 1..1000、缺省 200。start / end 是毫秒时间戳（`http::parse_query_ms`）。

use std::collections::HashMap;

use axum::extract::{Query, State};
use axum::response::Response;
use serde_json::json;

use crate::server::core::checkin_ledger;
use crate::server::http::{ok_json, parse_query_ms};
use crate::server::ServerState;

type Params = HashMap<String, String>;

/// 查询串里的字符串转 usize（空串 / 非数字 → None）
fn parse_usize(value: Option<&String>) -> Option<usize> {
    let text = value?.trim();
    if text.is_empty() {
        return None;
    }
    text.parse::<usize>().ok()
}

/// 查询串 → 筛选条件（ outcome / provider / keyword / start / end ）
fn query_of(params: &Params) -> checkin_ledger::Query {
    checkin_ledger::Query {
        outcome: params.get("outcome").cloned(),
        provider: params.get("provider").cloned(),
        keyword: params.get("keyword").cloned(),
        start: parse_query_ms(params.get("start")),
        end: parse_query_ms(params.get("end")),
    }
}

/// GET /api/checkin-ledger —— 一页明细 + 同条件总数
pub async fn query_records(
    State(_state): State<ServerState>,
    Query(params): Query<Params>,
) -> Response {
    let limit = parse_usize(params.get("limit")).unwrap_or(200);
    let offset = parse_usize(params.get("offset")).unwrap_or(0);
    ok_json(checkin_ledger::query(&query_of(&params), limit, offset))
}

/// DELETE /api/checkin-ledger —— 按筛选删除 / 显式全清（护栏见模块头）
pub async fn clear_records(
    State(_state): State<ServerState>,
    Query(params): Query<Params>,
) -> Response {
    let query = query_of(&params);
    if !query.has_filters() {
        let explicit = params
            .get("all")
            .map(|value| value.trim() == "1" || value.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        if !explicit {
            return crate::server::errors::management_error(
                400,
                "未指定筛选条件：要清空全部签到记录请显式带 all=1",
            );
        }
        let removed = checkin_ledger::clear_all().unwrap_or(0);
        crate::server::logging::log("[Checkin]", &format!("签到记录已全部清空（{removed} 条）"));
        return ok_json(json!({ "success": true, "removed": removed }));
    }
    match checkin_ledger::clear_where(&query) {
        Some(removed) => ok_json(json!({ "success": true, "removed": removed })),
        // 库不可用：删没删得掉说不清，如实报错（查询侧降级为空表是安全的，
        // 删除侧降级会谎报成功）
        None => crate::server::errors::management_error(500, "签到记录删除失败（库不可用）"),
    }
}
