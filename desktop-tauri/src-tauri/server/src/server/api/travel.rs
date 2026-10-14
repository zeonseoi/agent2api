//! 派猫猫旅行的**自动调度**路由（WorkBuddy 国内版成长中心）。
//!
//! ```text
//!   GET  /api/travel/auto   读自动开关
//!   POST /api/travel/auto   存自动开关（body `{enabled}`），并起停后端轮询
//!   POST /api/travel/run    立即执行一轮（该派的派、该领的领）
//! ```
//!
//! ── 上游调用与状态机在官方那侧 ────────────────────────────────
//! 旅行端点（猫档案 / 状态 / 派出 / 领奖 / 领养）与单账号「巡检一次」由官方
//! v3.0.3 的 `providers::workbuddy::growth::travel` + `api::growth` 提供，
//! 界面入口在签到中心的「成长任务」行里。本文件**只补官方没有的自动调度**：
//! 一个开关（起停后台每 2 小时一轮，见 `core::travel_auto`）与一个「立即执行
//! 一轮」的手动触发（否则台账要等后台那一轮才有新条目）。
//!
//! ── 为什么不重复提供 per-account 的派发 / 领取 ───────────────
//! 那两件事官方已经各有一个入口（成长任务行里的「巡检一次」按状态自动决定派
//! 还是领），再开一条同义端点只会让「手动动作」有两个事实源。自动轮询内部的
//! 单账号状态机仍复用官方实现（`growth::travel::run_once`），没有另写一份。
//!
//! ── 响应形态 ─────────────────────────────────────────────────
//! 全部走管理信封 `{success:true, data}`（`ok_json`），与签到那几条一致；
//! 失败用 `management_error`（400），前端只看 message。

use axum::body::Bytes;
use axum::extract::State;
use axum::response::Response;
use serde_json::{json, Value};

use crate::server::core::travel_auto::{self, TravelMode};
use crate::server::errors::management_error;
use crate::server::http::ok_json;
use crate::server::ServerState;

/// GET /api/travel/auto —— 读自动开关。
pub async fn get_travel_auto() -> Response {
    ok_json(json!({ "enabled": travel_auto::auto_enabled() }))
}

/// POST /api/travel/auto —— 存自动开关（body `{enabled}`），并起停调度。
pub async fn set_travel_auto(State(state): State<ServerState>, body: Bytes) -> Response {
    let payload = match serde_json::from_slice::<Value>(&body) {
        Ok(value) => value,
        Err(error) => return management_error(400, format!("请求体不是合法 JSON：{error}")),
    };
    let Some(enabled) = payload.get("enabled").and_then(Value::as_bool) else {
        return management_error(400, "enabled 必须是布尔值");
    };
    match state.travel_auto() {
        Some(service) => service.set_enabled(enabled),
        // 全局句柄未装入（理论上启动时已装）：仍然写配置，只是本次不排调度
        None => travel_auto::set_auto(enabled),
    }
    ok_json(json!({ "enabled": travel_auto::auto_enabled() }))
}

/// POST /api/travel/run —— 立即执行一轮（自动派发 + 领取）。
pub async fn run_travel(State(state): State<ServerState>) -> Response {
    let data = match state.travel_auto() {
        Some(service) => service.run_now().await,
        None => travel_auto::collect(state.store(), TravelMode::Act, "手动触发").await,
    };
    ok_json(data)
}
