//! 账号统一排序组：把多家的账号放进同一块，让「积分到期近→远 / 余额大优先」
//! 这类规则跨家生效。
//!
//! ── 它只影响路由，不影响任何后台任务 ─────────────────────────
//! 组只是一份**账号 id 的引用列表**：账号记录、`provider` 字段、凭证、签到、
//! 余额查询、旅行、成长任务全都不动。本模块产出的顺序只在**转发选路读取时**
//! 计算一次（见 `account_order::routing_sorting`），**不写回 `priority` 列** ——
//! 所以账号页的组顺位、队列拖拽、`apply_priorities` 全部照旧。
//!
//! 这条边界不是靠约定，而是数据上就没有：后台任务读的是账号集合
//! （`list_accounts` / `provider_account_ids`），从来没有读过这里。
//!
//! ── 为什么一个账号只能属于一个组 ─────────────────────────────
//! 归块必须是唯一的：同一个账号落在两个块里，一次请求可能对它试两次。
//! 校验见 [`validate`]。

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::{RuntimeConfig, CONFIG};

pub const KEY_ACCOUNT_GROUPS: &str = "accountGroups";
pub const MAX_GROUPS: usize = 64;
pub const MAX_GROUP_ACCOUNTS: usize = 2_000;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AccountGroup {
    /// 组的稳定标识（也是 `accountSort` 里规则集的键）。
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub accounts: Vec<String>,
}

pub type AccountGroups = BTreeMap<String, AccountGroup>;

pub fn validate(groups: &AccountGroups) -> Result<(), String> {
    if groups.len() > MAX_GROUPS {
        return Err(format!("统一排序组最多允许 {MAX_GROUPS} 个"));
    }
    let mut seen: HashSet<&str> = HashSet::new();
    for (id, group) in groups {
        if id.is_empty() || id.len() > 128 || !id.bytes().all(|ch| ch.is_ascii_alphanumeric() || b"-_.".contains(&ch)) {
            return Err("统一排序组标识无效".to_string());
        }
        if group.id != *id {
            return Err("统一排序组标识不一致".to_string());
        }
        if group.name.trim().is_empty() || group.name.chars().count() > 64
            || group.name.chars().any(char::is_control)
        {
            return Err("组名不能为空或超过 64 个字符".to_string());
        }
        if group.accounts.len() > MAX_GROUP_ACCOUNTS {
            return Err(format!("一个统一排序组最多允许 {MAX_GROUP_ACCOUNTS} 个账号"));
        }
        for account in &group.accounts {
            if account.is_empty() || account.trim() != account || account.len() > 256
                || account.chars().any(char::is_control)
            {
                return Err("账号标识包含空白或非法字符".to_string());
            }
            if !seen.insert(account) {
                return Err(format!("账号「{account}」同时属于多个统一排序组"));
            }
        }
    }
    Ok(())
}

pub(crate) fn from_raw(raw: &Map<String, Value>) -> Arc<AccountGroups> {
    let groups = raw.get(KEY_ACCOUNT_GROUPS).and_then(Value::as_array).map(|items| {
        items.iter().take(MAX_GROUPS).filter_map(|item| {
            let group = serde_json::from_value::<AccountGroup>(item.clone()).ok()?;
            (!group.id.is_empty()).then(|| (group.id.clone(), group))
        }).collect::<AccountGroups>()
    }).unwrap_or_default();
    Arc::new(groups)
}

pub fn groups() -> Arc<AccountGroups> {
    CONFIG.read().ok()
        .and_then(|guard| guard.as_ref().map(|config| config.account_groups.clone()))
        .unwrap_or_default()
}

/// 账号 id → 所属组 id。选路时按它归块。
pub fn group_of(groups: &AccountGroups) -> BTreeMap<String, String> {
    groups.values().flat_map(|group| {
        group.accounts.iter().map(move |account| (account.clone(), group.id.clone()))
    }).collect()
}

/// 整组写入（`groups` 为空数组 = 清空）。校验在落盘前完成。
pub fn save(groups: AccountGroups) -> Result<(), String> {
    validate(&groups)?;
    let mut error = None;
    let saved = super::update(|config: &mut RuntimeConfig| {
        let items: Vec<Value> = groups.values()
            .map(|group| serde_json::to_value(group).unwrap_or(Value::Null))
            .collect();
        let value = Value::Array(items);
        config.raw.insert(KEY_ACCOUNT_GROUPS.to_string(), value);
        config.account_groups = Arc::new(groups.clone());
        if serde_json::to_value(&groups).is_err() {
            error = Some("统一排序组序列化失败".to_string());
        }
    });
    if let Some(error) = error { return Err(error); }
    if !saved { return Err("统一排序组落盘失败，本次修改仅在当前进程生效".to_string()); }
    Ok(())
}
