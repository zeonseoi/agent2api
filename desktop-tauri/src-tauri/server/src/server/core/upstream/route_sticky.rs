//! 会话粘滞。内存表是运行期权威；库里的 `route_sticky` 只负责「重启恢复」——
//! 每次成功转发写一行、解除时删行、启动时读回未过期的行。管理读取不续期，
//! 清除后在途请求不得把旧绑定写回来（内存 generation 闸，落盘同受它约束）。

use std::collections::HashMap;
use std::sync::Mutex;

use rusqlite::Connection;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::server::{db::Db, logging};

#[derive(Clone)]
struct StickyEntry {
    id: String,
    account_id: String,
    model: String,
    source: String,
    created_at: i64,
    last_seen: i64,
    ttl_ms: i64,
    hits: u64,
}

#[derive(Default)]
struct StickyTable {
    entries: HashMap<String, StickyEntry>,
    generation: u64,
}

#[derive(Default)]
pub(super) struct StickySessions {
    table: Mutex<StickyTable>,
    /// 落盘句柄（与账号存储共用同一个连接）。`None` = 库不可用或单测，
    /// 退化为纯内存，语义与落盘前一致。
    db: Option<Db>,
}

pub(super) fn entry_id(key: &str, model: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(model.trim().to_ascii_lowercase().as_bytes());
    hasher.update(b"\0");
    hasher.update(key.as_bytes());
    format!("{:x}", hasher.finalize())
}

impl StickySessions {
    /// 启动构造：把库里未过期的绑定读回内存，过期的行顺手清掉。
    ///
    /// 过期口径与运行期一致（`last_seen + ttl_ms`）；配置随后把 TTL 调小时，
    /// 列表/查找仍按 `min(当前配置, 行内 ttl)` 收口。
    pub(super) fn load(db: Option<Db>, now: i64) -> Self {
        let sessions = Self { table: Mutex::new(StickyTable::default()), db };
        let Some(db) = sessions.db.as_ref() else { return sessions };
        let rows = db.with(|conn| prune_expired(conn, now).and_then(|_| read_rows(conn)));
        let rows = match rows {
            Some(Ok(rows)) => rows,
            Some(Err(error)) => {
                logging::log("[Sticky]", &format!("会话粘滞记录读取失败，按空表启动: {error}"));
                return sessions;
            }
            None => {
                logging::log("[Sticky]", "会话粘滞记录读取失败：数据库不可用，按空表启动");
                return sessions;
            }
        };
        {
            let Ok(mut table) = sessions.table.lock() else { return sessions };
            for entry in rows {
                table.entries.insert(entry.id.clone(), entry);
            }
            // 与运行期同一上限：异常大的表只留最近使用的 1 万条。
            if table.entries.len() > 10_000 {
                let mut kept: Vec<StickyEntry> = table.entries.values().cloned().collect();
                kept.sort_by_key(|entry| std::cmp::Reverse(entry.last_seen));
                let keep: std::collections::HashSet<String> =
                    kept.into_iter().take(10_000).map(|entry| entry.id).collect();
                table.entries.retain(|id, _| keep.contains(id));
            }
        }
        sessions
    }

    pub(super) fn generation(&self) -> u64 {
        self.table.lock().map(|table| table.generation).unwrap_or(0)
    }

    pub(super) fn lookup(&self, key: &str, model: &str, now: i64, ttl_ms: i64) -> Option<String> {
        if key.is_empty() || ttl_ms <= 0 { return None; }
        let id = entry_id(key, model);
        let mut table = self.table.lock().ok()?;
        table.entries.retain(|_, entry| now.saturating_sub(entry.last_seen) <= entry.ttl_ms);
        let entry = table.entries.get_mut(&id)?;
        if now.saturating_sub(entry.last_seen) > ttl_ms {
            table.entries.remove(&id);
            return None;
        }
        entry.last_seen = now;
        entry.ttl_ms = ttl_ms;
        entry.hits = entry.hits.saturating_add(1);
        Some(entry.account_id.clone())
    }

    pub(super) fn bind(
        &self, key: &str, model: &str, account_id: &str, source: &str,
        now: i64, ttl_ms: i64, generation: u64,
    ) {
        if key.is_empty() || account_id.is_empty() || ttl_ms <= 0 { return; }
        // 内存先行（写库放在释放锁之后：不让 SQLite 的等待挡住选路）。
        let (saved, dropped) = {
            let Ok(mut table) = self.table.lock() else { return };
            if table.generation != generation { return; }
            let mut dropped = Vec::new();
            table.entries.retain(|id, entry| {
                let keep = now.saturating_sub(entry.last_seen) <= entry.ttl_ms;
                if !keep { dropped.push(id.clone()); }
                keep
            });
            let id = entry_id(key, model);
            if table.entries.len() >= 10_000 && !table.entries.contains_key(&id) {
                if let Some(oldest) = table.entries.iter().min_by_key(|(_, entry)| entry.last_seen)
                    .map(|(id, _)| id.clone())
                {
                    table.entries.remove(&oldest);
                    dropped.push(oldest);
                }
            }
            let entry = table.entries.entry(id.clone()).or_insert_with(|| StickyEntry {
                id, account_id: account_id.to_string(), model: model.to_string(),
                source: source.to_string(), created_at: now, last_seen: now, ttl_ms, hits: 0,
            });
            entry.account_id = account_id.to_string();
            entry.last_seen = now;
            entry.ttl_ms = ttl_ms;
            (entry.clone(), dropped)
        };
        let Some(db) = self.db.as_ref() else { return };
        for id in &dropped {
            log_failure(db.with(|conn| remove_by_id(conn, id)), "清理过期/淘汰行");
        }
        log_failure(db.with(|conn| write_row(conn, &saved)), "写入");
    }

    pub(super) fn list(&self, model: Option<&str>, now: i64, ttl_for: impl Fn(&str) -> i64) -> Vec<Value> {
        let (values, expired) = {
            let Ok(mut table) = self.table.lock() else { return Vec::new() };
            let mut expired = Vec::new();
            table.entries.retain(|id, entry| {
                let ttl = ttl_for(&entry.model).min(entry.ttl_ms);
                let keep = ttl > 0 && now.saturating_sub(entry.last_seen) <= ttl;
                if !keep { expired.push(id.clone()); }
                keep
            });
            let mut entries: Vec<&StickyEntry> = table.entries.values()
                .filter(|entry| model.map_or(true, |model| entry.model.eq_ignore_ascii_case(model)))
                .collect();
            entries.sort_by_key(|entry| std::cmp::Reverse(entry.last_seen));
            let values: Vec<Value> = entries.into_iter().map(|entry| json!({
                "id": entry.id,
                "keyPreview": format!("{}…", &entry.id[..12]),
                "accountId": crate::server::core::routing_groups::account_key(&entry.account_id),
                "candidateId": entry.account_id,
                "memberId": crate::server::core::routing_groups::member_key(&entry.account_id),
                "model": entry.model,
                "source": entry.source,
                "createdAt": entry.created_at,
                "lastSeen": entry.last_seen,
                "expiresAt": entry.last_seen + ttl_for(&entry.model).min(entry.ttl_ms),
                "remainingMs": (entry.last_seen + ttl_for(&entry.model).min(entry.ttl_ms) - now).max(0),
                "hits": entry.hits,
            })).collect();
            (values, expired)
        };
        if let Some(db) = self.db.as_ref() {
            for id in &expired {
                log_failure(db.with(|conn| remove_by_id(conn, id)), "清理过期行");
            }
        }
        values
    }

    /// 某个候选失败后解除该会话的绑定（仅当绑定的正是这个候选且世代未变）。
    pub(super) fn forget(&self, key: &str, model: &str, candidate: &str, generation: u64) {
        if key.is_empty() || candidate.is_empty() { return; }
        let Ok(mut table) = self.table.lock() else { return };
        if table.generation != generation { return; }
        let id = entry_id(key, model);
        if table.entries.get(&id).is_some_and(|entry| entry.account_id == candidate) {
            table.entries.remove(&id);
        }
    }

    pub(super) fn clear(&self, id: Option<&str>, model: Option<&str>) -> usize {
        let cleared = {
            let Ok(mut table) = self.table.lock() else { return 0 };
            let before = table.entries.len();
            table.entries.retain(|key, entry| {
                !(id.map_or(true, |id| id == key)
                    && model.map_or(true, |model| entry.model.eq_ignore_ascii_case(model)))
            });
            table.generation = table.generation.wrapping_add(1);
            before - table.entries.len()
        };
        // 库里可能还留着已过期待清的行，解除时一并删掉（不只删内存里数到的）。
        if let Some(db) = self.db.as_ref() {
            log_failure(db.with(|conn| remove_matching(conn, id, model)), "解除");
        }
        cleared
    }
}

fn prune_expired(conn: &Connection, now: i64) -> rusqlite::Result<usize> {
    conn.execute("DELETE FROM route_sticky WHERE last_seen + ttl_ms <= ?1", [now])
}

fn read_rows(conn: &Connection) -> rusqlite::Result<Vec<StickyEntry>> {
    let mut statement = conn.prepare(
        "SELECT id, account_id, model, source, created_at, last_seen, ttl_ms, hits FROM route_sticky",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(StickyEntry {
            id: row.get(0)?,
            account_id: row.get(1)?,
            model: row.get(2)?,
            source: row.get(3)?,
            created_at: row.get(4)?,
            last_seen: row.get(5)?,
            ttl_ms: row.get(6)?,
            hits: row.get::<_, i64>(7)?.max(0) as u64,
        })
    })?;
    rows.collect()
}

fn write_row(conn: &Connection, entry: &StickyEntry) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO route_sticky (id, account_id, model, source, created_at, last_seen, ttl_ms, hits)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(id) DO UPDATE SET account_id = excluded.account_id, model = excluded.model,
             source = excluded.source, last_seen = excluded.last_seen, ttl_ms = excluded.ttl_ms,
             hits = excluded.hits",
        rusqlite::params![
            entry.id, entry.account_id, entry.model, entry.source, entry.created_at,
            entry.last_seen, entry.ttl_ms, entry.hits as i64,
        ],
    )
}

fn remove_by_id(conn: &Connection, id: &str) -> rusqlite::Result<usize> {
    conn.execute("DELETE FROM route_sticky WHERE id = ?1", [id])
}

/// 与内存 `clear` 的 retain 同一口径：id / 模型（不区分大小写）都为给定时取交集。
fn remove_matching(conn: &Connection, id: Option<&str>, model: Option<&str>) -> rusqlite::Result<usize> {
    match (id, model) {
        (Some(id), Some(model)) => conn.execute(
            "DELETE FROM route_sticky WHERE id = ?1 AND model = ?2 COLLATE NOCASE",
            rusqlite::params![id, model],
        ),
        (Some(id), None) => conn.execute("DELETE FROM route_sticky WHERE id = ?1", [id]),
        (None, Some(model)) => conn.execute(
            "DELETE FROM route_sticky WHERE model = ?1 COLLATE NOCASE",
            [model],
        ),
        (None, None) => conn.execute("DELETE FROM route_sticky", []),
    }
}

fn log_failure(result: Option<rusqlite::Result<usize>>, action: &str) {
    match result {
        None => logging::log("[Sticky]", &format!("会话粘滞{action}落盘失败：数据库不可用（本进程内仍生效）")),
        Some(Err(error)) => logging::log("[Sticky]", &format!("会话粘滞{action}落盘失败: {error}")),
        Some(Ok(_)) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::server::db::schema_fork::ROUTE_STICKY).unwrap();
        conn
    }

    fn entry(id: &str, model: &str, account_id: &str, last_seen: i64) -> StickyEntry {
        StickyEntry {
            id: id.to_string(),
            account_id: account_id.to_string(),
            model: model.to_string(),
            source: "content-hash".to_string(),
            created_at: last_seen,
            last_seen,
            ttl_ms: 60_000,
            hits: 2,
        }
    }

    /// UPSERT：同一 id 再写一次是更新而不是新行，created_at 保留首次值。
    #[test]
    fn writing_the_same_id_updates_the_row() {
        let conn = memory_conn();
        write_row(&conn, &entry("id-a", "m1", "trae:a1", 100)).unwrap();
        let mut newer = entry("id-a", "m1", "trae:a2", 200);
        newer.created_at = 100;
        write_row(&conn, &newer).unwrap();

        let rows = read_rows(&conn).unwrap();
        assert_eq!(rows.len(), 1, "同一 id 应当只有一行");
        assert_eq!(rows[0].account_id, "trae:a2");
        assert_eq!(rows[0].last_seen, 200);
        assert_eq!(rows[0].created_at, 100, "重绑不覆盖创建时刻");
    }

    /// 启动清理只删过期行（last_seen + ttl_ms 之后的才算过期）。
    #[test]
    fn pruning_drops_only_expired_rows() {
        let conn = memory_conn();
        write_row(&conn, &entry("fresh", "m1", "trae:a1", 100_000)).unwrap();
        write_row(&conn, &entry("expired", "m1", "trae:a2", 0)).unwrap();

        prune_expired(&conn, 120_000).unwrap();
        let rows = read_rows(&conn).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "fresh");
    }

    /// 解除与内存 retain 同口径：id 与模型（不区分大小写）同时给定时取交集。
    #[test]
    fn removal_mirrors_the_memory_clear() {
        let conn = memory_conn();
        write_row(&conn, &entry("id-a", "DeepSeek-Flash", "trae:a1", 1)).unwrap();
        write_row(&conn, &entry("id-b", "deepseek-flash", "trae:a2", 1)).unwrap();
        write_row(&conn, &entry("id-c", "other", "trae:a3", 1)).unwrap();

        assert_eq!(remove_matching(&conn, Some("id-a"), None).unwrap(), 1);
        assert_eq!(remove_matching(&conn, None, Some("deepseek-flash")).unwrap(), 1, "模型名不区分大小写");
        assert_eq!(remove_matching(&conn, None, None).unwrap(), 1);

        // id 与模型不匹配时（交集为空）不误删
        write_row(&conn, &entry("id-d", "other", "trae:a4", 1)).unwrap();
        assert_eq!(remove_matching(&conn, Some("id-d"), Some("missing")).unwrap(), 0);
        assert_eq!(read_rows(&conn).unwrap().len(), 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forget_removes_only_matching_candidate_in_current_generation() {
        let sticky = StickySessions::default();
        let gen = sticky.generation();
        sticky.bind("k1", "m1", "a1", "test", 1000, 60_000, gen);
        // 候选不匹配时不解绑（避免覆盖新成功绑定的候选）
        sticky.forget("k1", "m1", "a2", gen);
        assert_eq!(sticky.lookup("k1", "m1", 1001, 60_000).as_deref(), Some("a1"));
        // 世代过期时不解绑
        sticky.forget("k1", "m1", "a1", gen.wrapping_add(1));
        assert_eq!(sticky.lookup("k1", "m1", 1002, 60_000).as_deref(), Some("a1"));
        // 匹配候选且同世代时立即解绑
        sticky.forget("k1", "m1", "a1", gen);
        assert!(sticky.lookup("k1", "m1", 1003, 60_000).is_none());
    }
}
