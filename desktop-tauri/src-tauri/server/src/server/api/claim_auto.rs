//! 成长任务「到期前自动领取」的开关（WorkBuddy 国内版）。
//!
//! ```text
//!   GET  /api/claim-auto   读开关
//!   POST /api/claim-auto   存开关（body `{enabled}`），起停后台轮询
//! ```
//!
//! ── 为什么只剩这一条 ────────────────────────────────────────
//! 猫猫旅行的定时排程已由官方 `core::growth_scheduler` 提供
//! （`GET/POST /api/travel-schedule`），本 fork 不再另开 `/api/travel/*` ——
//! 同一件事留两套入口会让两个调度同时派猫。官方没有的是「按任务到期日提前
//! 领取」这一档，仍由 `core::growth_claim_auto` 的独立循环负责，这里只暴露
//! 它的开关。
//!
//! ── 响应形态 ─────────────────────────────────────────────────
//! 管理信封 `{success:true, data}`（`ok_json`），与签到那几条一致；参数错误
//! 走 `management_error`(400)，前端只看 message。

use axum::body::Bytes;
use axum::extract::State;
use axum::response::Response;
use serde_json::{json, Value};

use crate::server::core::growth_claim_auto;
use crate::server::errors::management_error;
use crate::server::http::ok_json;
use crate::server::ServerState;

/// GET /api/claim-auto —— 读「到期自动领取」开关。
pub async fn get_claim_auto() -> Response {
    ok_json(json!({ "enabled": growth_claim_auto::claim_auto_enabled() }))
}

/// POST /api/claim-auto —— 存开关并起停后台轮询。
pub async fn set_claim_auto(State(state): State<ServerState>, body: Bytes) -> Response {
    let payload = match serde_json::from_slice::<Value>(&body) {
        Ok(value) => value,
        Err(error) => return management_error(400, format!("请求体不是合法 JSON：{error}")),
    };
    let Some(enabled) = payload.get("enabled").and_then(Value::as_bool) else {
        return management_error(400, "需要 enabled（布尔值）");
    };
    match state.claim_auto() {
        Some(service) => service.set_enabled(enabled),
        // 全局句柄未装入（理论上启动时已装）：仍然写配置，只是本次不排调度
        None => growth_claim_auto::set_claim_auto(enabled),
    }
    ok_json(json!({ "enabled": growth_claim_auto::claim_auto_enabled() }))
}
