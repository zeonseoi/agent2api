//! 本 fork 对网关库的**增量结构**；官方 [`schema`] 除 `pub(crate)` 可见性外与上游逐字一致。
//!
//! ── 为什么不继续在 `schema.rs` 里加版本 ──────────────────────
//! 上游每次发版都往 `schema.rs` 追加版本常量与分支，而 fork 也曾在同一处插自己
//! 的版本（v10/v11 与 v12/v13/v14 都跟官方撞过号）。于是每次合并都要逐段重解
//! 冲突。现在 fork 不占用任何版本号：官方迁移照跑，fork 的对象每次打开库都按
//! **实际列 / 表**核对一遍（[`install`]，幂等），缺什么补什么。
//!
//! ── 撞号修复必须先于官方迁移（[`repair_legacy`]）──────────────
//! 老 fork 库的 `user_version` 已经越过官方 v10~v12，那三版的对象（`route`、
//! `checkin_records`、`client`）官方版本循环永远不会再跑。而官方 v18 要给
//! `requests(client, ts)` 建索引 —— 缺列的库会在那次迁移里报
//! `no such column: client`，整个库打不开。所以修复必须在官方迁移之前。
//!
//! 只补「`user_version` 已经越过的那几版」：没越过的留给官方循环自己加，
//! 先加会撞上 `duplicate column`。
//!
//! ── 硬约束 ────────────────────────────────────────────────────
//! 只补不删（多出来的列 / 表是用户数据，删不可逆）；[`migrate`] 在 `Db::open`
//! 与备份恢复两条路径上都会跑到，所以每一步都必须幂等。

use rusqlite::Connection;

use super::schema;

/// 旅行台账表（派猫猫旅行与成长任务领取的每账号明细）。
///
/// 与官方 `checkin_records` 同一形态：每执行一轮「真动作」，每个真动手的账号
/// 落一行。列的口径与写入方见 `core::travel_records`，读侧在 `core::checkin_ledger`。
/// `row_id` 用 AUTOINCREMENT 的理由同 `checkin_records`（翻页游标）。
const TRAVEL_RECORDS: &str = "
-- ── travel_records：派猫猫旅行每账号明细台账 ──────────────────
CREATE TABLE IF NOT EXISTS travel_records (
  row_id       INTEGER PRIMARY KEY AUTOINCREMENT,
  ts           INTEGER NOT NULL,
  date         TEXT NOT NULL DEFAULT '',
  source       TEXT NOT NULL DEFAULT '',
  reason       TEXT NOT NULL DEFAULT '',
  account_id   TEXT NOT NULL DEFAULT '',
  account_name TEXT NOT NULL DEFAULT '',
  provider     TEXT NOT NULL DEFAULT '',
  outcome      TEXT NOT NULL DEFAULT '',
  ok           INTEGER NOT NULL DEFAULT 0,
  message      TEXT,
  error        TEXT
);
CREATE INDEX IF NOT EXISTS idx_travel_records_ts ON travel_records(ts);
CREATE INDEX IF NOT EXISTS idx_travel_records_outcome ON travel_records(outcome);
CREATE INDEX IF NOT EXISTS idx_travel_records_provider ON travel_records(provider);
";

/// 官方迁移 + fork 增量的统一入口（`Db::open` 与备份恢复都走这里）。
pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    repair_legacy(conn)?;
    schema::migrate(conn)?;
    install(conn)
}

/// 补齐老 fork 库因版本撞号被跳过的官方对象。
///
/// 判据是「有没有 fork 的 `credit` 列」：官方血统的库从来没有它，不需要修。
/// 必须在 [`install`] 补 `credit` **之前**判断 —— 补完就分不清血统了。
fn repair_legacy(conn: &Connection) -> rusqlite::Result<()> {
    if !column_exists(conn, "requests", "credit")? {
        return Ok(());
    }
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    // 官方 v10：requests.route（选路决策快照）
    if version >= 10 {
        ensure_column(conn, "requests", "route", "TEXT")?;
    }
    // 官方 v11：checkin_records 签到明细台账（建表与索引都可重放）
    if version >= 11 {
        conn.execute_batch(schema::V11_SCHEMA)?;
    }
    // 官方 v12：requests.client（来源客户端）
    if version >= 12 {
        ensure_column(conn, "requests", "client", "TEXT NOT NULL DEFAULT ''")?;
    }
    Ok(())
}

/// 把 fork 独有的对象补齐（每次打开都核对，幂等）。
fn install(conn: &Connection) -> rusqlite::Result<()> {
    // 单次请求的上游扣费：REAL —— 上游给的是 0.07 / 5.3 这种小数。
    // 与 token 分列：它是「花费」不是「用量」，量纲不同；0 就是「没上报」。
    ensure_column(conn, "requests", "credit", "REAL NOT NULL DEFAULT 0")?;
    // 当天合计（报表只走聚合表、寿命与明细无关，所以聚合行上也要有落点）。
    ensure_column(conn, "request_daily", "credits", "REAL NOT NULL DEFAULT 0")?;
    conn.execute_batch(TRAVEL_RECORDS)
}

/// 列是否存在。
fn column_exists(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut statement = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        if row.get::<_, String>(1)? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 缺列才 ALTER。
///
/// `ALTER TABLE ... ADD COLUMN` 没有 `IF NOT EXISTS`，重复执行会以
/// `duplicate column name` 失败，所以先查 `PRAGMA`。表名与列名都是本文件的
/// 编译期常量，`format!` 拼进去没有注入面。
fn ensure_column(
    conn: &Connection,
    table: &str,
    column: &str,
    decl: &str,
) -> rusqlite::Result<()> {
    if column_exists(conn, table, column)? {
        return Ok(());
    }
    conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl};"))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    /// 用完删文件的临时库路径（本模块的用例各自独占一个标签）。
    fn temp_path(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "agent2api-schema-fork-{label}-{}.db",
            std::process::id()
        ));
        remove_temp(&path);
        path
    }

    fn remove_temp(path: &Path) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
        }
    }

    fn columns(conn: &Connection, table: &str) -> Vec<String> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})")).unwrap();
        let rows = stmt.query_map([], |row| row.get::<_, String>(1)).unwrap();
        rows.map(|row| row.unwrap()).collect()
    }

    fn table_exists(conn: &Connection, table: &str) -> bool {
        conn.query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |_| Ok(()),
        )
        .is_ok()
    }

    fn version(conn: &Connection) -> i64 {
        conn.pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap()
    }

    /// 造一个老 fork 库：官方 v1~v9 + fork 自己那几版（Credits 两列 + 旅行台账），
    /// `user_version` 直接标到 `through`。
    ///
    /// 老 fork 的版本号就是这么来的：它在官方 v9 之后拿 v10~v14 放自己的东西，
    /// 官方 v10~v12 的 `route` / `checkin_records` / `client` 因此从未被建过。
    /// 只能逐版跑官方 DDL 到 9：从完整 v20 库里删列删表来模拟需要把 v12 之后的
    /// 每一处改动都撤销一遍，官方一加版本就得跟着改。
    fn build_old_fork_db(path: &Path, through: i64) {
        let conn = Connection::open(path).unwrap();
        let mut cleaned = 0usize;
        for version in 1..=9 {
            schema::apply_version(&conn, version, &mut cleaned).unwrap();
        }
        conn.execute_batch(
            "ALTER TABLE requests ADD COLUMN credit REAL NOT NULL DEFAULT 0;
             ALTER TABLE request_daily ADD COLUMN credits REAL NOT NULL DEFAULT 0;
             CREATE TABLE IF NOT EXISTS travel_records (
               row_id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER NOT NULL,
               date TEXT NOT NULL DEFAULT '', source TEXT NOT NULL DEFAULT '',
               reason TEXT NOT NULL DEFAULT '', account_id TEXT NOT NULL DEFAULT '',
               account_name TEXT NOT NULL DEFAULT '', provider TEXT NOT NULL DEFAULT '',
               outcome TEXT NOT NULL DEFAULT '', ok INTEGER NOT NULL DEFAULT 0,
               message TEXT, error TEXT);",
        )
        .unwrap();
        conn.pragma_update(None, "user_version", through).unwrap();
    }

    /// 全新库：官方版本循环跑完后，fork 的三个对象由 `install` 补齐。
    #[test]
    fn fresh_db_gets_fork_objects() {
        let path = temp_path("fresh");
        let conn = Connection::open(&path).unwrap();
        migrate(&conn).unwrap();

        let requests = columns(&conn, "requests");
        for expected in ["credit", "route", "client"] {
            assert!(
                requests.iter().any(|column| column == expected),
                "requests.{expected} 应在：{requests:?}"
            );
        }
        let daily = columns(&conn, "request_daily");
        assert!(
            daily.iter().any(|column| column == "credits"),
            "request_daily.credits 应在：{daily:?}"
        );
        assert!(table_exists(&conn, "checkin_records"));
        assert!(table_exists(&conn, "travel_records"));
        assert_eq!(version(&conn), schema::SCHEMA_VERSION);
        drop(conn);
        remove_temp(&path);
    }

    /// 老 fork 停在 v11：官方 v10（`route`）与 v11（`checkin_records`）被跳过，
    /// 修复要补这两样；`client` 属于 v12，应当留给官方循环自己加。
    #[test]
    fn repairs_fork_v11_db() {
        let path = temp_path("repair11");
        build_old_fork_db(&path, 11);

        let conn = Connection::open(&path).unwrap();
        migrate(&conn).expect("老 fork v11 库应当能被修复升级");

        let requests = columns(&conn, "requests");
        for expected in ["credit", "route", "client"] {
            assert!(
                requests.iter().any(|column| column == expected),
                "requests.{expected} 应被补上：{requests:?}"
            );
        }
        let daily = columns(&conn, "request_daily");
        assert!(
            daily.iter().any(|column| column == "credits"),
            "request_daily.credits 应保留：{daily:?}"
        );
        assert!(
            table_exists(&conn, "checkin_records"),
            "checkin_records 表应被建出"
        );
        assert!(
            table_exists(&conn, "travel_records"),
            "travel_records 表应被建出"
        );
        assert_eq!(version(&conn), schema::SCHEMA_VERSION);
        drop(conn);
        remove_temp(&path);
    }

    /// 老 fork 停在 v14：官方 v10~v12 三样全被跳过。
    ///
    /// `client` 必须由修复**在官方迁移之前**补上 —— v18 要给
    /// `requests(client, ts)` 建索引，缺列会让整个迁移报 `no such column: client`。
    #[test]
    fn repairs_fork_v14_db() {
        let path = temp_path("repair14");
        build_old_fork_db(&path, 14);

        let conn = Connection::open(&path).unwrap();
        migrate(&conn).expect("老 fork v14 库应当能被修复升级");

        let requests = columns(&conn, "requests");
        for expected in ["credit", "route", "client"] {
            assert!(
                requests.iter().any(|column| column == expected),
                "requests.{expected} 应被补上：{requests:?}"
            );
        }
        assert!(
            table_exists(&conn, "checkin_records"),
            "checkin_records 表应被建出"
        );
        // v18 的索引依赖 client 列：它建得出来，就说明修复真的早于版本循环。
        let indexed: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'idx_requests_client_ts'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(indexed, 1, "v18 的 client 索引应在");
        assert_eq!(version(&conn), schema::SCHEMA_VERSION);
        drop(conn);
        remove_temp(&path);
    }

    /// 已是目标形态的库再迁移一次：不报错、不重复加列（两条路径都会跑到）。
    #[test]
    fn migrate_is_idempotent() {
        let path = temp_path("idem");
        let conn = Connection::open(&path).unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).expect("第二次迁移应当是无操作");
        drop(conn);
        remove_temp(&path);
    }

    /// 版本号比本二进制新时仍然**拒绝打开**（官方 v14 起反转的一条决定：
    /// 加密后的账号被旧程序读到会被静默删光，所以宁可在动手前先失败）。
    #[test]
    fn newer_db_is_rejected() {
        let path = temp_path("newer");
        let conn = Connection::open(&path).unwrap();
        migrate(&conn).unwrap();
        conn.pragma_update(None, "user_version", schema::SCHEMA_VERSION + 5)
            .unwrap();
        assert!(
            migrate(&conn).is_err(),
            "版本更高的库应当被拒绝打开，而不是静默动它"
        );
        assert_eq!(version(&conn), schema::SCHEMA_VERSION + 5);
        drop(conn);
        remove_temp(&path);
    }
}
