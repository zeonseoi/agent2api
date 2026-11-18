use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock};

use serde::Serialize;
use serde_json::{json, Value};

use crate::server::config::{self, AccountSortRule, AccountSortSettings, AddedSortDirection, BalanceSortDirection, CreditSortDirection};
use super::{account_order_balance::{self, AccountSortBalance, BalanceSortState}, routing, usage_records::UsageRecord};

struct UsageFacts {
    credit_expiries: Vec<i64>,
    balance: AccountSortBalance,
    query_config: String,
}

type AccountFacts = HashMap<String, Arc<UsageFacts>>;

fn usage_slot() -> &'static Mutex<AccountFacts> {
    static FACTS: OnceLock<Mutex<AccountFacts>> = OnceLock::new();
    FACTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn number(value: &Value) -> Option<f64> {
    value.as_f64().or_else(|| value.as_str()?.trim().parse().ok())
        .filter(|value| value.is_finite())
}

/// 积分包的**可用截止时刻**：`expireAt` 与 `refreshAt` 取早者。
///
/// ── 为什么不能只看 `expireAt` ──────────────────────────────
/// WorkBuddy 的订阅基础包（「CodeBuddy个人体验版」）`expireAt` 是账号寿命
/// （领取后 8 年多），`refreshAt` 才是本计费周期结束（下月 1 号零点）。按
/// `expireAt` 排序会让所有这类账号被判成「八年后才到期」，到期近→远这条规则
/// 在它们之间完全失效。
///
/// ── 为什么取 min 而不是直接换字段 ───────────────────────────
/// 同一个账号下其余包（裂变包 / 拉新权益包）的 `refreshAt` 是 `CycleEndTime + 1s`，
/// 比 `expireAt` **晚** 1 秒 —— 直接换成 `refreshAt` 会给这 264 个包平白推后 1 秒。
/// 取 min 后它们仍取 `expireAt`，只有基础包那种「周期明显早于有效期」的才变。
/// 没有 `refreshAt` 的提供商（Trae / Qoder）走原路径，行为不变。
fn pack_expiry(pack: &Value) -> Option<f64> {
    let expiry = pack.get("expireAt").and_then(number)?;
    let cycle = pack.get("refreshAt").and_then(number).filter(|cycle| *cycle > 0.0);
    Some(cycle.map_or(expiry, |cycle| expiry.min(cycle)))
}

fn credit_expiries(usage: &Value) -> Vec<i64> {
    let mut expiries: Vec<i64> = usage.get("credits").and_then(Value::as_array)
        .into_iter().flatten().filter_map(|pack| {
            if ["snapshot", "record", "daily", "unlimited"].iter().any(|key| pack.get(key).and_then(Value::as_bool) == Some(true)) {
                return None;
            }
            if pack.get("remain").and_then(number).is_some_and(|remain| remain <= 0.0) { return None; }
            let expiry = pack_expiry(pack)?;
            (expiry > 0.0).then_some(expiry as i64)
        }).collect();
    expiries.sort_unstable();
    expiries.dedup();
    expiries
}

fn usage_facts(usage: &Value, query_config: &str) -> Arc<UsageFacts> {
    Arc::new(UsageFacts { credit_expiries: credit_expiries(usage), balance: AccountSortBalance::from_usage(usage), query_config: query_config.to_string() })
}

pub fn note_usage(id: &str, usage: &Value, query_config: &str) {
    if let Ok(mut facts) = usage_slot().lock() { facts.insert(id.to_string(), usage_facts(usage, query_config)); }
}

pub fn reload_usage(records: &HashMap<String, UsageRecord>) {
    let fresh = records.iter().filter_map(|(id, record)| {
        record.usage.as_ref().map(|usage| (id.clone(), usage_facts(usage, &record.query_config)))
    }).collect();
    if let Ok(mut facts) = usage_slot().lock() { *facts = fresh; }
}

pub fn supports_credit_expiry(provider: &str) -> bool {
    super::plugin_host::account_features_of(provider).map(|features| features.credit_expiry)
        .unwrap_or_else(|| matches!(provider, "workbuddy" | "workbuddy-intl" | "trae" | "officeace"))
}

pub fn rule_enabled(provider: &str, rule: &AccountSortRule) -> bool {
    match rule {
        AccountSortRule::Credit { on, .. } => *on && supports_credit_expiry(provider),
        AccountSortRule::Balance { on, .. } => *on && super::usage_query::supports_usage_provider(provider),
        AccountSortRule::Added { on, .. } => *on,
    }
}

pub fn rules_for(provider: &str, settings: &AccountSortSettings) -> Vec<AccountSortRule> {
    let defaults = vec![
        AccountSortRule::Credit { on: supports_credit_expiry(provider), dir: CreditSortDirection::Soon },
        AccountSortRule::Balance { on: false, dir: BalanceSortDirection::High },
        AccountSortRule::Added { on: false, dir: AddedSortDirection::New },
    ];
    let Some(configured) = settings.get(provider) else { return defaults; };
    let mut rules = configured.clone();
    for mut rule in defaults {
        if rules.iter().any(|current| current.key() == rule.key()) { continue; }
        match &mut rule {
            AccountSortRule::Credit { on, .. } | AccountSortRule::Balance { on, .. } | AccountSortRule::Added { on, .. } => *on = false,
        }
        rules.push(rule);
    }
    rules
}

/// 一家内部排序的产物（`sorting` 的公开快照与路由选路共用这一份实现）。
struct ProviderBlockSort {
    rules: Vec<AccountSortRule>,
    balance_state: BalanceSortState,
    /// 逐账号读数：`(id, 最早有效到期, 余额)`。快照要用，选路只要排序结果。
    readings: Vec<(String, Option<i64>, AccountSortBalance)>,
}

/// 一家内部按规则排序（就地改 `members` 的顺序）。
///
/// 这条逻辑被两处使用：账号页 / 队列拖拽看的 [`sorting`]，以及路由选路时
/// 组外账号的块。**必须是同一份** —— 另写一份的下场是「建了一个组，组外账号的
/// 顺序也悄悄变了」（两处对「余额单位不一致」的处理本来就不同口径）。
fn sort_provider_block(
    members: &mut [Value], provider: &str,
    configured: &AccountSortSettings, facts: &AccountFacts, now: i64,
) -> ProviderBlockSort {
    let credit_supported = supports_credit_expiry(provider);
    let rules = rules_for(provider, configured);
    let mut readings = Vec::with_capacity(members.len());
    let mut expiries: HashMap<String, Option<i64>> = HashMap::new();
    let mut balances: HashMap<String, AccountSortBalance> = HashMap::new();
    for account in members.iter() {
        let Some(id) = routing::account_id(account) else { continue };
        // 查询配置改过之后旧读数不再代表它现在指向的额度 —— 当「没有数据」处理。
        let fact = facts.get(id).filter(|fact| fact.query_config.is_empty()
            || account.get("usageConfigKey").and_then(Value::as_str).is_none_or(|key| key == fact.query_config));
        let expiry = fact.and_then(|fact| fact.credit_expiries.iter().find(|expiry| **expiry > now)).copied();
        let balance = if account.get("isGuest") == Some(&Value::Bool(true)) { AccountSortBalance::default() }
            else { fact.map(|fact| fact.balance.clone()).unwrap_or_default() };
        expiries.insert(id.to_string(), expiry);
        balances.insert(id.to_string(), balance.clone());
        readings.push((id.to_string(), expiry, balance));
    }
    let balance_state = account_order_balance::state(super::usage_query::supports_usage_provider(provider),
        readings.iter().map(|(_, _, balance)| balance));
    members.sort_by(|left, right| {
        for rule in &rules {
            let order = match rule {
                AccountSortRule::Credit { on: true, dir } if credit_supported => {
                    let expiry = |account: &Value| routing::account_id(account)
                        .and_then(|id| expiries.get(id)).copied().flatten();
                    compare_expiry(expiry(left), expiry(right), *dir)
                }
                AccountSortRule::Balance { on: true, dir } if balance_state == BalanceSortState::Ready => {
                    let balance = |account: &Value| routing::account_id(account)
                        .and_then(|id| balances.get(id)).and_then(AccountSortBalance::comparable);
                    account_order_balance::compare(balance(left), balance(right), *dir)
                }
                AccountSortRule::Added { on: true, dir } => {
                    let added = |account: &Value| account.get("addedAt").and_then(number).unwrap_or(0.0);
                    let order = added(left).total_cmp(&added(right));
                    match dir { AddedSortDirection::Old => order, AddedSortDirection::New => order.reverse() }
                }
                _ => Ordering::Equal,
            };
            if order != Ordering::Equal { return order; }
        }
        Ordering::Equal
    });
    ProviderBlockSort { rules, balance_state, readings }
}

pub fn manual_groups(accounts: &[Value]) -> Vec<(String, Vec<Value>)> {
    let mut accounts = accounts.to_vec();
    accounts.sort_by_key(|account| (
        account.get("priority").and_then(Value::as_i64).unwrap_or(i64::MAX),
        account.get("addedAt").and_then(Value::as_i64).unwrap_or(0),
    ));
    let mut groups: Vec<(String, Vec<Value>)> = Vec::new();
    for account in accounts {
        let provider = routing::provider_of(&account).to_string();
        match groups.iter_mut().find(|(id, _)| *id == provider) {
            Some((_, members)) => members.push(account),
            None => groups.push((provider, vec![account])),
        }
    }
    groups
}

fn compare_expiry(left: Option<i64>, right: Option<i64>, direction: CreditSortDirection) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => match direction {
            CreditSortDirection::Soon => left.cmp(&right),
            CreditSortDirection::Late => right.cmp(&left),
        },
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountSorting {
    pub updated_at: i64,
    pub configured: AccountSortSettings,
    pub rules: AccountSortSettings,
    pub provider_order: Vec<String>,
    pub account_order: BTreeMap<String, Vec<String>>,
    pub credit_expires_at: BTreeMap<String, Option<i64>>,
    pub balances: BTreeMap<String, AccountSortBalance>,
    pub balance_sort_states: BTreeMap<String, BalanceSortState>,
}

pub fn sorting(accounts: &[Value], now: i64) -> AccountSorting {
    let configured = config::account_sort_settings();
    let mut result = AccountSorting {
        updated_at: now,
        configured: (*configured).clone(), rules: BTreeMap::new(), provider_order: Vec::new(),
        account_order: BTreeMap::new(), credit_expires_at: BTreeMap::new(),
        balances: BTreeMap::new(), balance_sort_states: BTreeMap::new(),
    };
    let facts = usage_slot().lock().map(|facts| facts.clone()).unwrap_or_default();
    for (provider, mut members) in manual_groups(accounts) {
        let block = sort_provider_block(&mut members, &provider, &configured, &facts, now);
        for (id, expiry, balance) in block.readings {
            result.credit_expires_at.insert(id.clone(), expiry);
            result.balances.insert(id, balance);
        }
        result.balance_sort_states.insert(provider.clone(), block.balance_state);
        result.provider_order.push(provider.clone());
        result.account_order.insert(provider.clone(), members.iter().filter_map(|account| routing::account_id(account).map(str::to_string)).collect());
        result.rules.insert(provider, block.rules);
    }
    result
}

pub fn ordered_accounts(accounts: &[Value], sorting: &AccountSorting) -> Vec<Value> {
    let by_id: HashMap<&str, &Value> = accounts.iter().filter_map(|account| routing::account_id(account).map(|id| (id, account))).collect();
    sorting.provider_order.iter().flat_map(|provider| sorting.account_order.get(provider).into_iter().flatten())
        .filter_map(|id| by_id.get(id.as_str()).map(|account| (*account).clone())).collect()
}

/// **路由专用**的账号顺序：统一排序组内的账号跨家排在一起。
///
/// ── 与 [`sorting`] 的关系 ────────────────────────────────────
/// 两者都按「块内规则」排序，差别只在**怎么分块**：`sorting` 一律按真实
/// provider 分块（账号页 / 队列拖拽用的就是它），这里让「统一排序组」里的
/// 账号先按组 id 归块，于是同一组的各家账号进同一块、共用一套规则，
/// 「积分到期近→远、余额大优先」才能真正跨家生效。
///
/// ── 为什么不能靠改 `priority` 实现 ───────────────────────────
/// `priority` 是**每家内部**的序号，队列按它升序后仍按家切块（见
/// [`manual_groups`]）—— 只要 A 家的队首比 B 家的队首优先，A 家整块就永远
/// 在 B 家前面。交错只能是「分块那一步」的产物。
///
/// ── 边界：只读计算，绝不落库 ─────────────────────────────────
/// 本函数只在**转发选路读取时**调用，不写 `priority` 列、不改 `accountSort`。
/// 于是账号页的组顺位、队列拖拽、`apply_priorities` 全部保持原样；后台任务
/// （签到 / 旅行 / 成长 / 连登）读的是账号集合，从头到尾没碰过这里 ——
/// 统一组对它们完全不可见。
///
/// 没有配置任何组时走 [`sorting`] + [`ordered_accounts`] 的原路径，
/// 行为与改造前逐字相同。
pub fn routing_ordered_accounts(accounts: &[Value], now: i64) -> Vec<Value> {
    let groups = config::account_groups::groups();
    if groups.is_empty() {
        return ordered_accounts(accounts, &sorting(accounts, now));
    }
    let facts = usage_slot().lock().map(|facts| facts.clone()).unwrap_or_default();
    ordered_with_groups(accounts, &groups, &config::account_sort_settings(), &facts, now)
}

/// [`routing_ordered_accounts`] 的可注入形态（测试用同一份逻辑，不碰全局配置）。
fn ordered_with_groups(
    accounts: &[Value], groups: &config::account_groups::AccountGroups,
    configured: &AccountSortSettings, facts: &AccountFacts, now: i64,
) -> Vec<Value> {
    let group_of = config::account_groups::group_of(groups);

    // 块序沿用 `manual_groups` 的口径：按账号优先级首次出现的先后。
    let mut by_priority = accounts.to_vec();
    by_priority.sort_by_key(|account| (
        account.get("priority").and_then(Value::as_i64).unwrap_or(i64::MAX),
        account.get("addedAt").and_then(Value::as_i64).unwrap_or(0),
    ));
    let mut blocks: Vec<(String, bool, Vec<Value>)> = Vec::new();
    for account in by_priority {
        // 组内账号归组块，其余仍按真实家归块 —— 每个账号只出现一次。
        let group = routing::account_id(&account).and_then(|id| group_of.get(id).cloned());
        let is_group = group.is_some();
        let key = group.unwrap_or_else(|| routing::provider_of(&account).to_string());
        match blocks.iter_mut().find(|(name, _, _)| *name == key) {
            Some((_, _, members)) => members.push(account),
            None => blocks.push((key, is_group, vec![account])),
        }
    }
    blocks.into_iter().flat_map(|(key, is_group, mut members)| {
        sort_block(&mut members, &key, is_group, configured, facts, now);
        members
    }).collect()
}

/// 按 `key` 排一块。`is_group` = 这一块是统一排序组（跨家），否则是单独一家。
///
/// 单独一家的块**直接复用** [`sort_provider_block`] —— 与账号页看到的顺序是
/// 同一份实现。另写一份的代价很具体：混合单位时 `sorting` 会把整条余额规则
/// 关掉、而组块按主单位比，两边口径不同，于是「建了一个组」会连带改变
/// 组外账号的排序。
///
/// 组块（跨家）与单家只有两处必须不同：
///   1. **能力按账号自己的真实 provider 判**（组内是跨家的，传组 id 会让
///      `supports_credit_expiry` / `supports_usage_provider` 恒假、规则静默失效）；
///   2. **余额按块内主单位比较**（跨家时不同单位不可比，非主单位的账号落到
///      下一条规则）。
fn sort_block(
    members: &mut [Value], key: &str, is_group: bool, configured: &AccountSortSettings,
    facts: &AccountFacts, now: i64,
) {
    if !is_group {
        sort_provider_block(members, key, configured, facts, now);
        return;
    }
    // 组块没有自己的 provider，默认规则按「块内有没有任何账号支持这项能力」定，
    // 否则 `rules_for` 拿组 id 去问能力会恒假、到期规则整条消失。
    let any_credit = members.iter().any(|account| supports_credit_expiry(routing::provider_of(account)));
    let rules = if configured.contains_key(key) {
        rules_for(key, configured)
    } else {
        vec![
            AccountSortRule::Credit { on: any_credit, dir: CreditSortDirection::Soon },
            AccountSortRule::Balance { on: false, dir: BalanceSortDirection::High },
            AccountSortRule::Added { on: false, dir: AddedSortDirection::New },
        ]
    };
    // 与 [`sorting`] 同一口径：账号的查询配置改过之后，旧读数不再代表它现在
    // 指向的额度，必须当「没有数据」处理 —— 否则组内会按一份过期余额排序。
    let fact_of = |account: &Value| -> Option<&Arc<UsageFacts>> {
        let fact = facts.get(routing::account_id(account)?)?;
        let fresh = fact.query_config.is_empty()
            || account.get("usageConfigKey").and_then(Value::as_str).is_none_or(|key| key == fact.query_config);
        fresh.then_some(fact)
    };
    let expiry_of = |account: &Value| -> Option<i64> {
        if !supports_credit_expiry(routing::provider_of(account)) { return None; }
        fact_of(account)?.credit_expiries.iter().find(|expiry| **expiry > now).copied()
    };
    let balance_of = |account: &Value| -> Option<AccountSortBalance> {
        if !super::usage_query::supports_usage_provider(routing::provider_of(account)) { return None; }
        if account.get("isGuest") == Some(&Value::Bool(true)) { return None; }
        fact_of(account).map(|fact| fact.balance.clone())
    };
    // 跨家时余额单位可能不同（积分 / ¥ / 次），而「大优先」只在同单位内可比。
    // 取块内**出现最多的单位**当主单位，只有主单位的账号参与余额比较，其余
    // 落到下一条规则 —— 不能按对判断「同单位才比」：那会破坏全序
    // （A≈B、B≈C 却 A≠C），Rust 的 `sort_by` 会直接 panic。
    let sort_unit: Option<String> = {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for account in members.iter() {
            if let Some(balance) = balance_of(account) {
                if balance.comparable().is_some() {
                    *counts.entry(balance.unit.clone()).or_default() += 1;
                }
            }
        }
        counts.into_iter().max_by(|left, right| left.1.cmp(&right.1).then_with(|| right.0.cmp(&left.0))).map(|(unit, _)| unit)
    };
    members.sort_by(|left, right| {
        for rule in &rules {
            let order = match rule {
                AccountSortRule::Credit { on: true, dir } => {
                    compare_expiry(expiry_of(left), expiry_of(right), *dir)
                }
                AccountSortRule::Balance { on: true, dir } => {
                    let value = |account: &Value| -> Option<f64> {
                        let balance = balance_of(account)?;
                        (Some(&balance.unit) == sort_unit.as_ref()).then(|| balance.comparable()).flatten()
                    };
                    account_order_balance::compare(value(left), value(right), *dir)
                }
                AccountSortRule::Added { on: true, dir } => {
                    let added = |account: &Value| account.get("addedAt").and_then(number).unwrap_or(0.0);
                    let order = added(left).total_cmp(&added(right));
                    match dir { AddedSortDirection::Old => order, AddedSortDirection::New => order.reverse() }
                }
                _ => Ordering::Equal,
            };
            if order != Ordering::Equal { return order; }
        }
        Ordering::Equal
    });
}

pub fn enrich_snapshot(snapshot: &mut Value) {
    let accounts = routing::accounts_of(snapshot);
    let sorting = sorting(&accounts, crate::server::logging::now_ms());
    snapshot["sorting"] = json!(sorting);
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// 订阅基础包以「本计费周期结束」为到期时刻，而不是账号寿命（8 年后）。
    ///
    /// 实测数据：体验版 `expireAt` 是领取 + 8 年多、`refreshAt` 是下月 1 号；
    /// 其余包（裂变包 / 拉新权益包）的 `refreshAt` 恰好比 `expireAt` 晚 1 秒。
    /// 取 min 后只有前者变化，后者仍取 `expireAt`。
    #[test]
    fn a_subscription_base_pack_expires_at_its_billing_cycle() {
        let now = 1_700_000_000_000;
        let base = json!({ "remain": 500, "expireAt": now + 300_000_000_000i64, "refreshAt": now + 1_000_000 });
        assert_eq!(pack_expiry(&base), Some((now + 1_000_000) as f64));

        // 裂变包：refreshAt 比 expireAt 晚 1 秒（CycleEndTime + 1s），必须仍取 expireAt。
        let bonus = json!({ "remain": 100, "expireAt": now + 5_000, "refreshAt": now + 6_000 });
        assert_eq!(pack_expiry(&bonus), Some((now + 5_000) as f64));

        // 没有 refreshAt 的提供商（Trae / Qoder）行为不变。
        let trae = json!({ "remain": 500, "expireAt": now + 7_000 });
        assert_eq!(pack_expiry(&trae), Some((now + 7_000) as f64));

        // refreshAt 为 0 / 缺失 / 非数字都按「没有周期」处理。
        assert_eq!(pack_expiry(&json!({ "expireAt": now + 8_000, "refreshAt": 0 })), Some((now + 8_000) as f64));
        assert_eq!(pack_expiry(&json!({ "expireAt": now + 8_000, "refreshAt": "" })), Some((now + 8_000) as f64));
        assert_eq!(pack_expiry(&json!({ "refreshAt": now + 8_000 })), None);
    }

    /// 路由排序要真的用上「周期结束」：两个基础包同一天发放时，按周期结束先后排。
    #[test]
    fn routing_orders_base_packs_by_cycle_end() {
        let now = 1_700_000_000_000;
        let accounts = vec![
            json!({ "id": "wb1", "provider": "workbuddy", "priority": 0, "addedAt": 1 }),
            json!({ "id": "wb2", "provider": "workbuddy", "priority": 1, "addedAt": 2 }),
        ];
        let mut facts: AccountFacts = HashMap::new();
        // 两个包的 expireAt 都是八年后，只有 refreshAt 能分出先后。
        facts.insert("wb1".into(), usage_facts(&json!({ "credits": [
            { "remain": 500, "expireAt": now + 300_000_000_000i64, "refreshAt": now + 9_000 },
        ] }), ""));
        facts.insert("wb2".into(), usage_facts(&json!({ "credits": [
            { "remain": 500, "expireAt": now + 300_000_000_000i64, "refreshAt": now + 3_000 },
        ] }), ""));
        // 空组表 = 全部账号走单家块，正好覆盖路由的常规路径（不碰全局读数槽）。
        let groups = crate::server::config::account_groups::AccountGroups::new();
        let order: Vec<String> = ordered_with_groups(&accounts, &groups, &AccountSortSettings::new(), &facts, now)
            .into_iter().filter_map(|account| routing::account_id(&account).map(str::to_string)).collect();
        assert_eq!(order, vec!["wb2", "wb1"]);
    }

    /// 统一排序组的核心行为：同一组内的各家账号进同一块，规则跨家生效。
    ///
    /// 这里直接测「归块」这一步 —— 它才是交错能否成立的地方。`priority`
    /// 只决定块与块之间的先后，块内顺序完全由规则决定。
    #[test]
    fn accounts_of_one_group_share_a_block() {
        let groups: crate::server::config::account_groups::AccountGroups = [
            ("g1".to_string(), crate::server::config::account_groups::AccountGroup {
                id: "g1".into(), name: "主力池".into(),
                accounts: vec!["wb1".into(), "tr1".into()],
            }),
        ].into_iter().collect();
        let group_of = crate::server::config::account_groups::group_of(&groups);
        assert_eq!(group_of.get("wb1").map(String::as_str), Some("g1"));
        assert_eq!(group_of.get("tr1").map(String::as_str), Some("g1"));
        // 不在组里的账号没有归属，仍按真实 provider 归块。
        assert_eq!(group_of.get("wb9"), None);
    }

    #[test]
    fn a_group_rejects_an_account_listed_twice() {
        let groups: crate::server::config::account_groups::AccountGroups = [
            ("g1".to_string(), crate::server::config::account_groups::AccountGroup {
                id: "g1".into(), name: "A".into(), accounts: vec!["x".into()],
            }),
            ("g2".to_string(), crate::server::config::account_groups::AccountGroup {
                id: "g2".into(), name: "B".into(), accounts: vec!["x".into()],
            }),
        ].into_iter().collect();
        // 一个账号落在两个块里，一次请求可能对它试两次 —— 必须拒绝。
        assert!(crate::server::config::account_groups::validate(&groups).is_err());
    }

    #[test]
    fn routing_order_is_unchanged_without_groups() {
        // 没配组时 `routing_ordered_accounts` 必须与 `sorting` + `ordered_accounts`
        // 逐字等价，否则等于悄悄改了所有人的路由行为。
        let accounts = vec![
            json!({ "id": "wb1", "provider": "workbuddy", "priority": 0, "addedAt": 1 }),
            json!({ "id": "tr1", "provider": "trae", "priority": 5, "addedAt": 2 }),
            json!({ "id": "wb2", "provider": "workbuddy", "priority": 1, "addedAt": 3 }),
        ];
        let now = crate::server::logging::now_ms();
        let plain: Vec<String> = ordered_accounts(&accounts, &sorting(&accounts, now))
            .into_iter().filter_map(|account| routing::account_id(&account).map(str::to_string)).collect();
        let routed: Vec<String> = routing_ordered_accounts(&accounts, now)
            .into_iter().filter_map(|account| routing::account_id(&account).map(str::to_string)).collect();
        assert_eq!(plain, routed);
    }

    #[test]
    fn a_group_interleaves_providers_by_expiry() {
        // 用户要的正是这件事：workbuddy 与 trae 放进同一组后，按「到期近→远」
        // 交错排列，而不是把 workbuddy 用完才轮到 trae。
        let now = 1_700_000_000_000;
        let accounts = vec![
            // 优先级：workbuddy 全部排在 trae 前面（旧行为下 trae 永远轮不到）。
            json!({ "id": "wb1", "provider": "workbuddy", "priority": 0, "addedAt": 1 }),
            json!({ "id": "wb2", "provider": "workbuddy", "priority": 1, "addedAt": 2 }),
            json!({ "id": "tr1", "provider": "trae", "priority": 5, "addedAt": 3 }),
            json!({ "id": "tr2", "provider": "trae", "priority": 6, "addedAt": 4 }),
        ];
        let mut facts: AccountFacts = HashMap::new();
        // trae 的积分更早到期 → 应该排在最前。
        facts.insert("wb1".into(), usage_facts(&json!({ "credits": [
            { "remain": 10, "expireAt": now + 5_000 },
        ] }), ""));
        facts.insert("wb2".into(), usage_facts(&json!({ "credits": [
            { "remain": 10, "expireAt": now + 9_000 },
        ] }), ""));
        facts.insert("tr1".into(), usage_facts(&json!({ "credits": [
            { "remain": 10, "expireAt": now + 1_000 },
        ] }), ""));
        facts.insert("tr2".into(), usage_facts(&json!({ "credits": [
            { "remain": 10, "expireAt": now + 7_000 },
        ] }), ""));
        let groups: crate::server::config::account_groups::AccountGroups = [
            ("g1".to_string(), crate::server::config::account_groups::AccountGroup {
                id: "g1".into(), name: "主力池".into(),
                accounts: vec!["wb1".into(), "wb2".into(), "tr1".into(), "tr2".into()],
            }),
        ].into_iter().collect();
        let order: Vec<String> = ordered_with_groups(&accounts, &groups, &AccountSortSettings::new(), &facts, now)
            .into_iter().filter_map(|account| routing::account_id(&account).map(str::to_string)).collect();
        assert_eq!(order, vec!["tr1", "wb1", "tr2", "wb2"]);
    }

    #[test]
    fn a_group_leaves_accounts_outside_it_in_the_old_order() {
        // 建组只该改变**组内**的顺序。组外账号走的仍是「单家块」，必须与
        // `sorting` 的口径逐字一致 —— 混合单位时尤其容易漂移：那边整条余额
        // 规则关掉、这边若按主单位比，组外账号的先后会跟着变。
        let now = 1_700_000_000_000;
        let accounts = vec![
            json!({ "id": "wb1", "provider": "workbuddy", "priority": 0, "addedAt": 1 }),
            json!({ "id": "wb2", "provider": "workbuddy", "priority": 1, "addedAt": 2 }),
            // qoder 不在组里：¥ 与积分混在这家的两个账号上，整条余额规则应被关掉。
            json!({ "id": "qd1", "provider": "qoder", "priority": 5, "addedAt": 3 }),
            json!({ "id": "qd2", "provider": "qoder", "priority": 6, "addedAt": 4 }),
        ];
        let mut facts: AccountFacts = HashMap::new();
        facts.insert("wb1".into(), usage_facts(&json!({ "totalLeft": 10 }), ""));
        facts.insert("wb2".into(), usage_facts(&json!({ "totalLeft": 90 }), ""));
        facts.insert("qd1".into(), usage_facts(&json!({ "totalLeft": 10 }), ""));
        facts.insert("qd2".into(), usage_facts(&json!({ "unit": "¥", "available": 90 }), ""));
        let mut configured = AccountSortSettings::new();
        configured.insert("qoder".into(), vec![AccountSortRule::Balance { on: true, dir: BalanceSortDirection::High }]);
        let groups: crate::server::config::account_groups::AccountGroups = [
            ("g1".to_string(), crate::server::config::account_groups::AccountGroup {
                id: "g1".into(), name: "主力池".into(), accounts: vec!["wb1".into()],
            }),
        ].into_iter().collect();
        let order: Vec<String> = ordered_with_groups(&accounts, &groups, &configured, &facts, now)
            .into_iter().filter_map(|account| routing::account_id(&account).map(str::to_string)).collect();
        // qoder 两个账号的余额单位不同 → 余额规则整条不生效 → 保持手动顺序 qd1、qd2。
        let qoder: Vec<&String> = order.iter().filter(|id| id.starts_with("qd")).collect();
        assert_eq!(qoder, vec!["qd1", "qd2"]);
    }

    #[test]
    fn a_group_accepts_accounts_from_any_provider() {
        // 组不按家筛选：内置家、插件家、自定义家都能进同一块。这里刻意放一家
        // 「不支持到期」的（qoder）和一家插件家，验证它们不会被悄悄排除。
        let now = 1_700_000_000_000;
        let accounts = vec![
            json!({ "id": "wb1", "provider": "workbuddy", "priority": 0, "addedAt": 1 }),
            json!({ "id": "tr1", "provider": "trae", "priority": 5, "addedAt": 2 }),
            json!({ "id": "qd1", "provider": "qoder", "priority": 9, "addedAt": 3 }),
            json!({ "id": "cu1", "provider": "custom-abc", "priority": 12, "addedAt": 4 }),
        ];
        let mut facts: AccountFacts = HashMap::new();
        facts.insert("wb1".into(), usage_facts(&json!({ "credits": [{ "remain": 10, "expireAt": now + 9_000 }] }), ""));
        facts.insert("tr1".into(), usage_facts(&json!({ "credits": [{ "remain": 10, "expireAt": now + 1_000 }] }), ""));
        let groups: crate::server::config::account_groups::AccountGroups = [
            ("g1".to_string(), crate::server::config::account_groups::AccountGroup {
                id: "g1".into(), name: "全量池".into(),
                accounts: vec!["wb1".into(), "tr1".into(), "qd1".into(), "cu1".into()],
            }),
        ].into_iter().collect();
        let order: Vec<String> = ordered_with_groups(&accounts, &groups, &AccountSortSettings::new(), &facts, now)
            .into_iter().filter_map(|account| routing::account_id(&account).map(str::to_string)).collect();
        assert_eq!(order.len(), 4, "四家的账号都必须留在组内");
        // 有到期读数的按到期近→远在前；没有读数的（qoder / custom）落在后面，
        // 且彼此保持手动顺序 —— 它们不是被排除，只是没有可比较的值。
        assert_eq!(order, vec!["tr1", "wb1", "qd1", "cu1"]);
    }

    #[test]
    fn groups_never_remove_or_duplicate_accounts() {
        // 组的边界：它只改**顺序**。后台任务（签到 / 旅行 / 成长）读的是账号集合，
        // 只要集合不变，它们就不受任何影响。这里把「集合相同、每个只出现一次」
        // 钉成断言 —— 以后谁在归块时漏了兜底分支，这条会红。
        let now = crate::server::logging::now_ms();
        let accounts = vec![
            json!({ "id": "wb1", "provider": "workbuddy", "priority": 0, "addedAt": 1 }),
            json!({ "id": "wb2", "provider": "workbuddy", "priority": 1, "addedAt": 2 }),
            json!({ "id": "tr1", "provider": "trae", "priority": 5, "addedAt": 3 }),
            json!({ "id": "qd1", "provider": "qoder", "priority": 7, "addedAt": 4 }),
        ];
        let groups: crate::server::config::account_groups::AccountGroups = [
            // 只把 wb1 与 tr1 放进组，wb2 留在 workbuddy 块、qd1 留在 qoder 块。
            ("g1".to_string(), crate::server::config::account_groups::AccountGroup {
                id: "g1".into(), name: "主力池".into(),
                accounts: vec!["wb1".into(), "tr1".into()],
            }),
        ].into_iter().collect();
        let facts: AccountFacts = HashMap::new();
        let order: Vec<String> = ordered_with_groups(&accounts, &groups, &AccountSortSettings::new(), &facts, now)
            .into_iter().filter_map(|account| routing::account_id(&account).map(str::to_string)).collect();
        assert_eq!(order.len(), accounts.len(), "账号数量必须不变");
        let mut sorted = order.clone();
        sorted.sort();
        assert_eq!(sorted, vec!["qd1", "tr1", "wb1", "wb2"], "集合必须与入参相同");
    }
}
