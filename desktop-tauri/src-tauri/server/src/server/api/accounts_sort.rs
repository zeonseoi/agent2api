use axum::body::Bytes;
use axum::response::Response;
use serde_json::Value;

use crate::server::{config, errors, ServerState};
use crate::server::core::{account_order, routing};
use crate::server::http::{ok_json, parse_body};

pub fn get_sorting(state: &ServerState) -> Response {
    let accounts = routing::accounts_of(&state.store().list_visible_accounts());
    ok_json(serde_json::json!(account_order::sorting(&accounts, crate::server::logging::now_ms())))
}

pub fn save_sorting(state: &ServerState, body: &Bytes) -> Response {
    let payload = match parse_body(body) {
        Ok(value) => value,
        Err(error) => return errors::management_error(400, error.message),
    };
    let Some(provider) = payload.get("provider").and_then(Value::as_str) else {
        return errors::management_error(400, "缺少 provider 字段");
    };
    // 键既可以是真实提供商，也可以是统一排序组 id —— 组内跨家共用的那套规则
    // 存在同一张 `accountSort` 表里，以组 id 为键（见 `config::account_groups`）。
    if !crate::server::core::providers::is_known_provider_id(provider)
        && !crate::server::core::custom_providers::is_custom_provider_id(provider)
        && !config::account_groups::groups().contains_key(provider)
    {
        return errors::management_error(400, format!("未知提供商「{provider}」"));
    }
    let rules = match payload.get("rules") {
        Some(Value::Null) => None,
        Some(value) => match serde_json::from_value::<Vec<config::AccountSortRule>>(value.clone()) {
            Ok(rules) => Some(rules),
            Err(error) => return errors::management_error(400, format!("账号排序规则格式错误: {error}")),
        },
        None => return errors::management_error(400, "缺少 rules 字段，恢复默认请传 null"),
    };
    if let Err(error) = config::validate_account_sort(provider, rules.as_deref().unwrap_or_default()) {
        return errors::management_error(400, error);
    }
    if let Err(error) = config::save_account_sort(provider, rules, payload.get("onlyIfUnset") == Some(&Value::Bool(true))) {
        return errors::management_error(500, error);
    }
    get_sorting(state)
}

/// 统一排序组（跨家交错）：`{ groups: [{id, name, accounts:[...]}] }`。
///
/// 只影响**全局默认路由**的顺序，不影响签到 / 旅行 / 成长等后台任务 ——
/// 见 `config::account_groups` 模块头。
pub fn get_groups() -> Response {
    let groups = config::account_groups::groups();
    ok_json(serde_json::json!({ "groups": groups.values().collect::<Vec<_>>() }))
}

pub fn save_groups(state: &ServerState, body: &Bytes) -> Response {
    let payload = match parse_body(body) {
        Ok(value) => value,
        Err(error) => return errors::management_error(400, error.message),
    };
    let Some(items) = payload.get("groups") else {
        return errors::management_error(400, "缺少 groups 字段（清空请传空数组）");
    };
    let parsed = match serde_json::from_value::<Vec<config::account_groups::AccountGroup>>(items.clone()) {
        Ok(groups) => groups,
        Err(error) => return errors::management_error(400, format!("统一排序组格式错误: {error}")),
    };
    let groups: config::account_groups::AccountGroups = parsed.into_iter()
        .map(|group| (group.id.clone(), group)).collect();
    if let Err(error) = config::account_groups::validate(&groups) {
        return errors::management_error(400, error);
    }
    // 组 id 与真实 provider id 撞名会让两块**合并**（归块键相同）——
    // 一个账号可能因此在同一次请求里被试两次。这类冲突必须挡在落库之前。
    if let Some(clash) = groups.keys().find(|id| {
        crate::server::core::providers::is_known_provider_id(id)
            || crate::server::core::custom_providers::is_custom_provider_id(id)
    }) {
        return errors::management_error(400, format!("组标识「{clash}」与提供商同名，请换一个"));
    }
    // 账号必须真实存在：组里写一个不存在的 id 只会在选路时静默失效。
    let known: std::collections::HashSet<String> = routing::accounts_of(&state.store().list_visible_accounts())
        .into_iter().filter_map(|account| routing::account_id(&account).map(str::to_string)).collect();
    if let Some(missing) = groups.values().flat_map(|group| group.accounts.iter())
        .find(|id| !known.contains(*id))
    {
        return errors::management_error(400, format!("账号「{missing}」不存在，请刷新账号页后重试"));
    }
    if let Err(error) = config::account_groups::save(groups) {
        return errors::management_error(500, error);
    }
    get_groups()
}
