//! 旅行记录明细台账的路由（签到中心「旅行记录」页的数据源）。
//!
//! ```text
//!   GET    /api/travel-records   查询（?limit=&offset=&outcome=&provider=&keyword=&start=&end=）
//!   DELETE /api/travel-records   删除（带筛选 = 只删命中的；全清必须显式 all=1）
//! ```
//!
//! 与 `api::checkin_records` 同形（签到记录页那条）：响应形状、
//! 参数解析口径、DELETE 护栏全部照搬 —— 两页在界面上是并排的两个标签页，
//! 后端契约刻意保持一致。
//!
//! ── 响应形状 ────────────────────────────────────────────────
//! GET → `{records: [...], total}`：total 是**同条件**的总数（分页器用它算页数），
//! records 按时间倒序。DELETE → `{success:true, removed}`（removed = 删除条数）。
//!
//! ── DELETE 的护栏（照抄 logs 的规则）────────────────────────
//! 带任一筛选参数时只删命中的条目（与 GET 同一份过滤链 —— 界面上筛出什么
//! 就删什么）；清空全部必须**显式**带 `all=1`，不带的空请求给 400。

use std::collections::HashMap;

use axum::extract::{Query, State};
use axum::response::Response;
use serde_json::json;

use crate::server::core::travel_records;
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
fn query_of(params: &Params) -> travel_records::Query {
    travel_records::Query {
        outcome: params.get("outcome").cloned(),
        provider: params.get("provider").cloned(),
        keyword: params.get("keyword").cloned(),
        start: parse_query_ms(params.get("start")),
        end: parse_query_ms(params.get("end")),
    }
}

/// GET /api/travel-records —— 一页明细 + 同条件总数
pub async fn query_records(
    State(_state): State<ServerState>,
    Query(params): Query<Params>,
) -> Response {
    let limit = parse_usize(params.get("limit")).unwrap_or(200);
    let offset = parse_usize(params.get("offset")).unwrap_or(0);
    ok_json(travel_records::query(&query_of(&params), limit, offset))
}

/// DELETE /api/travel-records —— 按筛选删除 / 显式全清（护栏见模块头）
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
                "未指定筛选条件：要清空全部旅行记录请显式带 all=1",
            );
        }
        let removed = travel_records::clear_all().unwrap_or(0);
        crate::server::logging::log("[Travel]", &format!("旅行记录已全部清空（{removed} 条）"));
        return ok_json(json!({ "success": true, "removed": removed }));
    }
    match travel_records::clear_where(&query) {
        Some(removed) => ok_json(json!({ "success": true, "removed": removed })),
        // 库不可用：删没删得掉说不清，如实报错（与 checkin_records 同一取向）
        None => crate::server::errors::management_error(500, "旅行记录删除失败（库不可用）"),
    }
}
