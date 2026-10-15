//! 派猫猫旅行的**自动调度**路由（WorkBuddy 国内版成长中心）。
//!
//! ```text
//!   GET  /api/travel/auto   读自动开关（旅行 + 到期领取）
//!   POST /api/travel/auto   存自动开关（body `{enabled}` 与/或 `{claimEnabled}`），
//!                           各自起停对应的后台轮询
//!   POST /api/travel/run    立即执行一轮（旅行 + 到期领取，各受自己的开关控制）
//! ```
//!
//! ── 上游调用与状态机在官方那侧 ────────────────────────────────
//! 旅行端点（猫档案 / 状态 / 派出 / 领奖 / 领养）与单账号「巡检一次」由官方
//! v3.0.3 的 `providers::workbuddy::growth::travel` + `api::growth` 提供，
//! 界面入口在签到中心的「成长任务」行里。本文件**只补官方没有的自动调度**：
//! 两个独立开关（各自的每 2 小时循环，见 `core::travel_auto` 与
//! `core::growth_claim_auto`）与一个「立即执行一轮」的手动触发。
//!
//! ── 为什么不重复提供 per-account 的派发 / 领取 ───────────────
//! 那两件事官方已经各有一个入口（成长任务行里的「巡检一次」按状态自动决定派
//! 还是领），再开一条同义端点只会让「手动动作」有两个事实源。自动轮询内部的
//! 单账号状态机仍复用官方实现（`growth::travel::run_once` / `growth::process_task`），
//! 没有另写一份。
//!
//! ── 响应形态 ─────────────────────────────────────────────────
//! 全部走管理信封 `{success:true, data}`（`ok_json`），与签到那几条一致；
//! 失败用 `management_error`（400），前端只看 message。

use axum::body::Bytes;
use axum::extract::State;
use axum::response::Response;
use serde_json::{json, Value};

use crate::server::core::growth_claim_auto;
use crate::server::core::travel_auto::{self, TravelMode};
use crate::server::errors::management_error;
use crate::server::http::ok_json;
use crate::server::ServerState;

/// GET /api/travel/auto —— 读自动开关（旅行 + 成长任务领取各一项）。
pub async fn get_travel_auto() -> Response {
    ok_json(json!({
        "enabled": travel_auto::auto_enabled(),
        "claimEnabled": growth_claim_auto::claim_auto_enabled(),
    }))
}

/// POST /api/travel/auto —— 存自动开关（body `{enabled}` 与/或 `{claimEnabled}`），
/// 各自起停对应的后台调度。两项可分别提交（只传一项时另一项不动）。
pub async fn set_travel_auto(State(state): State<ServerState>, body: Bytes) -> Response {
    let payload = match serde_json::from_slice::<Value>(&body) {
        Ok(value) => value,
        Err(error) => return management_error(400, format!("请求体不是合法 JSON：{error}")),
    };
    let enabled = payload.get("enabled").and_then(Value::as_bool);
    let claim_enabled = payload.get("claimEnabled").and_then(Value::as_bool);
    if enabled.is_none() && claim_enabled.is_none() {
        return management_error(400, "需要 enabled 或 claimEnabled（布尔值）");
    }
    // 旅行开关：起停它自己那条轮询
    if let Some(enabled) = enabled {
        match state.travel_auto() {
            Some(service) => service.set_enabled(enabled),
            // 全局句柄未装入（理论上启动时已装）：仍然写配置，只是本次不排调度
            None => travel_auto::set_auto(enabled),
        }
    }
    // 领取开关：起停**另一条**独立轮询（与旅行互不影响，见 core::growth_claim_auto）
    if let Some(claim_enabled) = claim_enabled {
        match state.claim_auto() {
            Some(service) => service.set_enabled(claim_enabled),
            None => growth_claim_auto::set_claim_auto(claim_enabled),
        }
    }
    ok_json(json!({
        "enabled": travel_auto::auto_enabled(),
        "claimEnabled": growth_claim_auto::claim_auto_enabled(),
    }))
}

/// POST /api/travel/run —— 立即执行一轮（旅行一轮 + 到期领取一轮，各受自己的
/// 开关控制：关着的那个不跑，与后台循环同口径）。
pub async fn run_travel(State(state): State<ServerState>) -> Response {
    let data = match state.travel_auto() {
        Some(service) => service.run_now().await,
        None => travel_auto::collect(state.store(), TravelMode::Act, "手动触发").await,
    };
    match state.claim_auto() {
        Some(service) => {
            let _ = service.run_now().await;
        }
        None => {
            let _ = growth_claim_auto::claim_due_tasks(state.store(), "手动触发").await;
        }
    }
    ok_json(data)
}
