//! 成长任务「到期前自动领取」的开关与执行时刻（WorkBuddy 国内版）。
//!
//! ```text
//!   GET  /api/claim-auto   读开关与执行时刻
//!   POST /api/claim-auto   存开关/时刻（body `{enabled?, times?}`），起停后台轮询
//! ```
//!
//! ── 为什么只剩这一条 ────────────────────────────────────────
//! 猫猫旅行的定时排程已由官方 `core::growth_scheduler` 提供
//! （`GET/POST /api/travel-schedule`），本 fork 不再另开 `/api/travel/*` ——
//! 同一件事留两套入口会让两个调度同时派猫。官方没有的是「按任务到期日提前
//! 领取」这一档，仍由 `core::growth_claim_auto` 的独立循环负责，这里只暴露
//! 它的开关与执行时刻（北京时间每天多个 HH:MM，与插件任务表单同一口径）。
//!
//! ── 响应形态 ─────────────────────────────────────────────────
//! 管理信封 `{success:true, data}`（`ok_json`），与签到那几条一致；参数错误
//! 走 `management_error`(400)，前端只看 message。

use axum::body::Bytes;
use axum::response::Response;
use serde_json::Value;

use crate::server::core::growth_claim_auto;
use crate::server::errors::management_error;
use crate::server::http::ok_json;

/// GET /api/claim-auto —— 读「到期自动领取」开关与执行时刻。
pub async fn get_claim_auto() -> Response {
    ok_json(growth_claim_auto::snapshot())
}

/// POST /api/claim-auto —— 存开关/时刻并起停后台轮询。
pub async fn set_claim_auto(body: Bytes) -> Response {
    let payload = match serde_json::from_slice::<Value>(&body) {
        Ok(value) => value,
        Err(error) => return management_error(400, format!("请求体不是合法 JSON：{error}")),
    };
    let Some(fields) = payload.as_object() else {
        return management_error(400, "请求体必须是 JSON 对象");
    };
    if fields.keys().any(|key| !matches!(key.as_str(), "enabled" | "times")) {
        return management_error(400, "只接受 enabled 与 times 两项");
    }
    let enabled = match fields.get("enabled") {
        None => None,
        Some(value) => match value.as_bool() {
            Some(enabled) => Some(enabled),
            None => return management_error(400, "enabled 必须是布尔值"),
        },
    };
    let times = match fields.get("times") {
        None => None,
        Some(value) => match value.as_array() {
            Some(items) => {
                let mut times = Vec::with_capacity(items.len());
                for item in items {
                    let Some(text) = item.as_str() else {
                        return management_error(400, "执行时刻必须是字符串数组");
                    };
                    times.push(text.to_string());
                }
                Some(times)
            }
            None => return management_error(400, "执行时刻必须是字符串数组"),
        },
    };
    if enabled.is_none() && times.is_none() {
        return management_error(400, "需要 enabled 或 times");
    }
    match growth_claim_auto::save_settings(enabled, times) {
        Ok(snapshot) => ok_json(snapshot),
        Err(error) => management_error(400, error),
    }
}
