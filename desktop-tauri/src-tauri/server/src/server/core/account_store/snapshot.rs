use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use super::{read_rows, AccountStore};

#[derive(Default)]
pub(super) struct SnapshotCache {
    build: Mutex<()>,
    state: RwLock<Option<CachedSnapshot>>,
}

struct CachedSnapshot {
    revision: u64,
    metadata: u64,
    data_version: i64,
    checked_at: Instant,
    rows: Arc<BTreeMap<i64, Arc<Value>>>,
    snapshot: Arc<RoutingSnapshot>,
}

pub(crate) struct RoutingSnapshot {
    pub accounts: Vec<Arc<Value>>,
    pub by_id: HashMap<String, Arc<Value>>,
    pub by_provider: HashMap<String, Vec<Arc<Value>>>,
    pub public_accounts: Vec<Arc<Value>>,
    pub public_by_id: HashMap<String, Arc<Value>>,
    pub available_providers: HashSet<String>,
    pub forwarding_providers: HashSet<String>,
}

impl RoutingSnapshot {
    fn from_rows(rows: &BTreeMap<i64, Arc<Value>>) -> Self {
        let public_accounts: Vec<_> = rows.values().cloned().collect();
        let public_by_id = public_accounts.iter().filter_map(|account| {
            Some((account.get("id")?.as_str()?.to_string(), account.clone()))
        }).collect();
        let accounts: Vec<_> = public_accounts.iter().map(|account| Arc::new(route_account(account))).collect();
        let by_id = accounts.iter().filter_map(|account| {
            Some((account.get("id")?.as_str()?.to_string(), account.clone()))
        }).collect();
        let available_providers = accounts.iter().filter(|account| {
            account.get("enabled").and_then(Value::as_bool) != Some(false)
                && account.get("hasCredentials").and_then(Value::as_bool) == Some(true)
        }).filter_map(|account| account.get("provider")?.as_str().map(str::to_string)).collect();
        let forwarding_providers = accounts.iter().filter_map(|account| {
            account.get("provider")?.as_str().map(str::to_string)
        }).collect::<HashSet<_>>().into_iter()
            .filter(|provider| super::store::provider_forwards_requests(provider)).collect();
        let mut by_provider: HashMap<String, Vec<Arc<Value>>> = HashMap::new();
        for account in &accounts {
            let provider = crate::server::core::routing::provider_of(account);
            by_provider.entry(provider.to_string()).or_default().push(account.clone());
        }
        Self { accounts, by_id, by_provider, public_accounts, public_by_id, available_providers, forwarding_providers }
    }
}

impl AccountStore {
    pub(crate) fn routing_snapshot(&self) -> Arc<RoutingSnapshot> {
        let Some(db) = self.database() else { return empty() };
        let cache = self.snapshot_cache();
        let metadata = crate::server::core::providers::catalog::generation();
        if let Some(snapshot) = fresh(cache, db.account_revision(), metadata) { return snapshot; }
        crate::server::core::blocking::in_place(|| self.build_routing_snapshot(db))
    }

    fn build_routing_snapshot(&self, db: &crate::server::db::Db) -> Arc<RoutingSnapshot> {
        let cache = self.snapshot_cache();
        let _build = cache.build.lock().unwrap_or_else(|error| error.into_inner());
        let metadata = crate::server::core::providers::catalog::generation();
        if let Some(snapshot) = fresh(cache, db.account_revision(), metadata) { return snapshot; }
        let previous = cache.state.read().ok().and_then(|state| state.as_ref().map(|cached| {
            (cached.revision, cached.metadata, cached.data_version, cached.rows.clone())
        }));
        // 连接锁只复制密文行；解密、规则推导和轻量投影全部在锁外执行。
        let batch = db.with(|conn| -> rusqlite::Result<_> {
            let data_version: i64 = conn.pragma_query_value(None, "data_version", |row| row.get(0))?;
            let since = previous.as_ref().filter(|(_, old_metadata, old_version, _)| {
                *old_metadata == metadata && *old_version == data_version
            }).map(|(revision, _, _, _)| *revision);
            let changes = db.account_changes(since);
            let (replace, rows, unchanged) = match changes.rows {
                Some(ids) => {
                    let unchanged = ids.is_empty();
                    (false, read_rows::changed(conn, &ids)?, unchanged)
                }
                None => (true, read_rows::all(conn)?.into_iter().map(|row| (row.rowid, Some(row))).collect(), false),
            };
            Ok((changes.revision, data_version, replace, rows, unchanged))
        }).and_then(Result::ok);
        let Some((revision, data_version, replace, changes, unchanged)) = batch else { return empty() };
        if unchanged && db.account_revision() == revision
            && crate::server::core::providers::catalog::generation() == metadata {
            if let Ok(mut state) = cache.state.write() {
                if let Some(cached) = state.as_mut() {
                    cached.checked_at = Instant::now();
                    return cached.snapshot.clone();
                }
            }
        }
        let mut rows = if replace { BTreeMap::new() } else {
            previous.map(|(_, _, _, rows)| (*rows).clone()).unwrap_or_default()
        };
        let mut failed = 0;
        for (rowid, raw) in changes {
            rows.remove(&rowid);
            if let Some(raw) = raw {
                match read_rows::decode(raw) {
                    Ok((_, record)) => { rows.insert(rowid, Arc::new(self.public_account(&record))); }
                    Err(_) => failed += 1,
                }
            }
        }
        if failed > 0 {
            super::sql::warn_load_failure("读取选路账号", &format!("{failed} 条账号记录解密或解析失败，已跳过；原始数据未修改"));
        }
        let snapshot = Arc::new(RoutingSnapshot::from_rows(&rows));
        if db.account_revision() == revision && crate::server::core::providers::catalog::generation() == metadata {
            if let Ok(mut state) = cache.state.write() {
                *state = Some(CachedSnapshot { revision, metadata, data_version,
                    checked_at: Instant::now(), rows: Arc::new(rows), snapshot: snapshot.clone() });
            }
        }
        snapshot
    }

    pub(crate) fn route_account_ids_for_provider(&self, provider: &str) -> Vec<String> {
        self.provider_account_ids(provider, true)
    }

    /// 一个 provider 名下的账号 id（`store.list` 回调的取材）。
    ///
    /// ── 为什么需要一个不看 `enabled` 的版本 ─────────────────────
    /// 插件的 `store.list` 契约是「列出一个 provider 的**全部**账号」
    /// （`provider-sdk` 的 `ctx::store_list` 注释逐字如此），插件自己再按
    /// 场景决定要不要跳过禁用的：模型目录刷新在 `catalog.rs` / `catalog_state.rs`
    /// 里各自过滤一遍 `enabled`，而签到、猫猫旅行、成长任务这些后台任务
    /// **按设计不看启用状态**（与网关 `auto_checkin` 同一口径，见
    /// `auto_checkin::retry` 模块头「不看 available / enabled」）。
    ///
    /// 从前这里只有转发视角（滤掉禁用的），于是用户把账号全关掉之后，
    /// 插件侧的后台任务一个账号都列不到，`travel_records` 空表、
    /// 成长任务 `accounts: 0`，而签到（走网关自己的 `list_accounts`）照常跑
    /// —— 同一条「账号被禁用」在两条链路上结论相反。
    ///
    /// 凭证仍然要查：没有凭证的账号插件连不上，列出去只会制造失败行。
    pub(crate) fn provider_account_ids(&self, provider: &str, routing_only: bool) -> Vec<String> {
        let snapshot = self.routing_snapshot();
        let mut accounts: Vec<_> = snapshot.by_provider.get(provider).into_iter().flatten()
            .filter(|account| provider_account_match(account, provider, routing_only))
            .collect();
        accounts.sort_by_key(|account| super::store::route_order_key(account));
        accounts.into_iter().filter_map(|account| account.get("id")?.as_str().map(str::to_string)).collect()
    }

    pub(crate) fn route_account(&self, id: &str) -> Option<Arc<Value>> {
        self.routing_snapshot().by_id.get(id).cloned()
    }
}

/// 一条账号是否属于 `store.list` 这次要列的范围。
///
/// `routing_only` = 转发视角（滤掉禁用的）。插件侧的后台任务必须传 `false`，
/// 否则用户把账号关掉之后旅行 / 成长 / 连登全都扫不到人（见调用处说明）。
fn provider_account_match(account: &Value, provider: &str, routing_only: bool) -> bool {
    account.get("provider").and_then(Value::as_str) == Some(provider)
        && (!routing_only || account.get("enabled").and_then(Value::as_bool) != Some(false))
        && account.get("hasCredentials").and_then(Value::as_bool) == Some(true)
}

fn fresh(cache: &SnapshotCache, revision: u64, metadata: u64) -> Option<Arc<RoutingSnapshot>> {
    let state = cache.state.read().ok()?;
    let cached = state.as_ref()?;
    (cached.revision == revision && cached.metadata == metadata
        && cached.checked_at.elapsed() < Duration::from_secs(1)).then(|| cached.snapshot.clone())
}

fn empty() -> Arc<RoutingSnapshot> {
    Arc::new(RoutingSnapshot::from_rows(&BTreeMap::new()))
}

// 选路只消费所需字段；完整公开投影复用同一份按账号增量更新的缓存。
fn route_account(account: &Value) -> Value {
    let mut public = Map::new();
    for key in [
        "id", "provider", "name", "nickname", "email", "userId", "uid", "account",
        "displayName", "priority", "addedAt", "enabled", "hasCredentials", "nameCustom",
        "needsRelogin", "allowRouteSkip", "maxConcurrent", "rateLimits", "routingCooldowns", "limiterGroups",
        "upstreamCooldownUntil", "zcodePlan", "pool", "quotaChannel", "pluginModelPermissions", "usageConfigKey",
    ] {
        if let Some(value) = account.get(key) { public.insert(key.to_string(), value.clone()); }
    }
    let source = account.pointer("/proxy/config/source").and_then(Value::as_str).unwrap_or("");
    public.insert("proxy".to_string(), json!({ "config": { "source": source } }));
    Value::Object(public)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::provider_account_match;

    #[test]
    fn disabled_accounts_stay_listed_for_background_tasks() {
        // 插件的 store.list 契约是「这一家的全部账号」：后台任务（猫猫旅行 /
        // 成长任务 / 连登）按设计不看启用状态，禁用账号必须列得出来，否则
        // 用户关掉账号后它们会静默扫到 0 个账号。
        let disabled = json!({
            "id": "a1", "provider": "workbuddy", "enabled": false, "hasCredentials": true,
        });
        assert!(!provider_account_match(&disabled, "workbuddy", true));
        assert!(provider_account_match(&disabled, "workbuddy", false));
    }

    #[test]
    fn accounts_without_credentials_are_never_listed() {
        // 没凭证的账号插件连不上，列出去只会制造失败行。
        let missing = json!({
            "id": "a2", "provider": "workbuddy", "enabled": true, "hasCredentials": false,
        });
        assert!(!provider_account_match(&missing, "workbuddy", false));
        assert!(!provider_account_match(&missing, "workbuddy", true));
    }

    #[test]
    fn other_providers_are_excluded() {
        let other = json!({
            "id": "a3", "provider": "trae", "enabled": true, "hasCredentials": true,
        });
        assert!(!provider_account_match(&other, "workbuddy", false));
    }
}
