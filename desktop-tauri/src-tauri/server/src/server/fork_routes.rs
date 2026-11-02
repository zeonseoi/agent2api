//! fork 追加的管理路由。
//!
//! 与官方路由表分开维护：`http.rs` 是官方重构最频繁的文件之一，把 fork 的
//! 两条路由收在这里，`panel_router` 里只留一行 `merge(fork_routes())` ——
//! 合并上游时冲突面从「两条路由 + 若干注释」压到一行。
//!
//! · `/api/claim-auto` —— 成长任务「到期前自动领取」的开关
//!   （`core::growth_claim_auto`，官方没有这一档）；
//! · `/api/checkin-ledger` —— 合并台账（`checkin_records` + `travel_records`），
//!   与 `/api/checkin-records` 同形（GET 查询 / DELETE 清空，全清护栏 `all=1`）。
//!
//! 两条都挂在 `protected` 组里（经 `panel_router` 的 `merge`），与相邻的官方
//! 管理路由同一鉴权口径。

use axum::routing::{any, get};
use axum::Router;

use crate::server::api;
use crate::server::ServerState;

pub fn fork_routes() -> Router<ServerState> {
    Router::new()
        .route(
            "/api/claim-auto",
            get(api::claim_auto::get_claim_auto).post(api::claim_auto::set_claim_auto),
        )
        .route(
            "/api/checkin-ledger",
            get(api::checkin_ledger::query_records).delete(api::checkin_ledger::clear_records),
        )
        .route("/api/checkin-ledger/", any(api::logs_api::not_found))
        .route("/api/checkin-ledger/{*rest}", any(api::logs_api::not_found))
}
