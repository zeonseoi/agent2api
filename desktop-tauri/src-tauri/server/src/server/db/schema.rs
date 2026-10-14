//! 建表 DDL 与 schema 版本管理（`PRAGMA user_version` 逐版本升级）。
//!
//! ── 为什么自己写十几行而不用 rusqlite_migration ─────────────
//! 迁移库解决的通用问题是「一堆版本文件 + 校验和 + 回滚 + 断点续跑」。本项目
//! 的表**一共 6 张、只增不改**（旧列不删、新列靠可空的 `data` JSON 兜底），
//! 版本推进就是「比大小 → 跑 DDL → 写版本号」三步。引入第三方迁移框架会带来：
//! 一个新依赖、一套它自己的版本表（与 `user_version` 二选一，反而多一份状态）、
//! 以及「框架怎么理解我们的幂等语义」这类需要读源码才能确认的事。十几行确定性
//! 代码可以逐行看懂、出错时能直接对着库文件查，符合本项目一贯取向。
//!
//! ── 为什么全部 DDL 集中在本文件 ─────────────────────────────
//! 表结构是**跨 store 的公共契约**：写侧（各 store 的接线切片）与读侧（API）
//! 分属不同文件、不同任务，两边的字段口径必须有一处唯一的事实来源。字段散在
//! 各 store 的 `CREATE TABLE` 里，加一列就会出现「谁先建谁生效」的库文件差异
//! （同一个版本，A 机器建的库有这列、B 机器没有）。集中之后「库长什么样」只
//! 用读这一个文件。
//!
//! ── 版本推进方式 ────────────────────────────────────────────
//! 读 `user_version`（默认 0 = 全新库），对它到 [`SCHEMA_VERSION`] 之间的
//! 每个版本按序调用 [`apply_version`]，最后把 `user_version` 写成
//! [`SCHEMA_VERSION`]。**每推一个版本都是一个独立事务**（DDL 与版本号一起
//! 提交）：中途失败时已完成的那几个版本保留、最后一个整体回滚，下次启动从
//! 落库的版本号继续 —— 不会出现「表建了一半但版本号已经写新」的错位。
//!
//! ── 为什么所有语句都带 IF NOT EXISTS ────────────────────────
//! 版本号是**唯一**的推进依据，但现实里存在「表已经在了、版本号却是 0」的
//! 情形：用户手工拷过库文件、或者从旧版本二进制回退再前进。DDL 幂等之后这种
//! 库能直接跑过去（缺的补上、有的跳过），而不是在启动时报一句
//! 「table already exists」把用户挡在门外。
//!
//! ── kv 表的键命名规范（后续切片按此约定往里写）──────────────
//! `kv` 是「不属于某张实体表的零散持久状态」的落点，键分两类：
//!   - **网关配置（原 `config.json`）的顶层键直接用键名**，一字不改：
//!     `apiKeys`、`modelRules`、`logRetentionDays`、`debugMode`……
//!     为什么保留原名而不是改成 snake_case：这些名字同时是 **HTTP API 的
//!     契约**（`GET /api/config` 的响应体、前端 `config.X` 的读法），改名会
//!     让前端、账号迁移、模型规则三处同时受影响；数据库的列名与 JSON 键
//!     一一对应，排障时能直接把库里的值贴进配置文件比对。
//!     值的形态也与配置文件一致：**值就是那段 JSON**（标量存 `"30"`、
//!     对象存 `{...}`、数组存 `[...]`），所以 `config.json` → `kv` 的迁移
//!     是「顶层键逐个搬」，不需要为每种类型单独定编码。
//!   - **其它零散状态用固定名**（驼峰，与前面同类）：见 [`RESERVED_KV_KEYS`]。
//!     注意 `desktopSettings` 存的是**整份**设置对象而不是拆成多个键 ——
//!     桌面设置是壳侧 `settings.rs` 的整体读写单元（前端一次 `PUT` 整份），
//!     拆键会让「一次写入」变成多行更新，徒增事务与冲突面。
//!
//! ── 两类键名为什么绝不能相撞（T7 起这是一条硬约束）──────────
//! `config.json` 进了 `kv` 之后，「哪些行属于配置」不再由**表的边界**表达
//! （改造前配置独占一个文件），而是由键名是否落在 [`RESERVED_KV_KEYS`] 里
//! 区分。`config::save_raw` 的「删除已不存在的键」要按这个集合做排除 ——
//! 配置项的名字若与某个固定键撞了，写一次配置就会把那份状态**删掉**
//! （比如配置里出现 `desktopSettings`，用户改一次 API Key 就丢了桌面设置）。
//! 当前配置项（`apiKeys` / `modelRules` / `logRetentionDays` / `debugMode` /
//! `sanitizeBlacklistFingerprints` / `promptMode` / `promptFile` /
//! `scheduledTasks` / `autoCheckin` / `locale` / `lastRequestModel` /
//! `providerRoute` / `updateProxy` / 三个 `*Dir` / 三个 `*RetentionDays` /
//! 三个 `retry*` / 六条 `scheduledTasks.*` 子键 / 三个 `*Imported` 标记 /
//! 旧字段 `apiKey`）与固定键**无冲突**，逐项核对过（全仓 `update_raw_field` /
//! `raw.insert` / `KEY_*` 常量的取值集合 vs 下面的 `RESERVED_KV_KEYS`）。
//! 后加固定键名时**必须**回来核对一次：撞名的代价是用户的配置或状态被静默
//! 覆盖，而两处代码离得很远（一个在本文件，一个在使用方）。

use rusqlite::Connection;

/// `kv` 表里**不属于网关配置**的固定键名（模块头「两类键名绝不能相撞」讲的
/// 就是它与配置顶层键的关系）。
///
/// 为什么要有一份集中清单：`config::save_raw` 必须知道「哪些行不归我管」，
/// 才能在写配置时把「配置里已删掉的键」精确删掉而不误伤别人的状态。没有这份
/// 清单，那个删除只能放宽成「删掉所有不在新配置里的键」—— 桌面设置、账号
/// 优先级作用域、日志下一个 id 会在用户改一次 API Key 时全部消失。
///
/// 它是**事实来源**：各使用方自己的常量（`logs_store::sql::NEXT_ID_KEY`）
/// 仍是它们各自读写时用的名字，本清单只服务
/// 「配置写侧要排除哪些键」这一个判断。两处若不一致，症状是配置写入误删状态
/// —— 所以新增固定键时**两处都要加**（使用方常量 + 本清单），
/// [`is_reserved`] 是唯一的判断入口。
///
/// 清单里可能有**没有使用方常量**的项（如遗留的 `desensitize`）：那些是
/// 「功能已删、数据留着」的键，登记它们的唯一目的就是让配置写入别去动它。
pub const RESERVED_KV_KEYS: &[&str] = &[
    // 账号优先级的全局作用域（core::account_store::sql）
    "priorityScope",
    // 事件日志的下一个 id 水位（logs_store::sql）
    "logsNextId",
    // 整份桌面设置 JSON（壳侧 settings）—— 一个键而不是拆成三个：
    // 桌面设置是整体读写单元（前端一次 PUT 整份），拆键会让一次写入变成
    // 多行更新，徒增事务与冲突面。
    "desktopSettings",
    // **遗留**：旧「敏感词脱敏」的整份状态（词表 / 开关 / 作用角色 / 作用提供商）。
    //
    // 那个功能已随规则集换成硬编码指纹脱敏（`core::sanitize`）而整体删除，
    // 这个键**不再有任何读写方**。仍然登记在这里是有意的：它意味着配置写入
    // 不会顺手把这行删掉 —— 老用户库里那份自定义词表原样留着（想查还能
    // `sqlite3` 看），而不是因为改一次 API Key 就静默消失。真要清理时手工
    // 删这一行即可。
    "desensitize",
    // 面板管理员（server::access）：{username, hash} 整份 JSON 一个键 ——
    // 首次部署在面板上注册产生（环境变量预置时优先于它），属于「其它零散
    // 状态」一类：不是配置项，配置写入绝不能动它。
    "panelAdmin",
    // 面板刷新令牌（server::access）：整份记录数组一个键（条目是个位数：
    // 每设备一条活链），轮换/撤销都是整键重写。刷新令牌在库里只存 sha256，
    // 重启后 access 短效令牌虽在内存丢失，浏览器用它静默换新。
    "panelTokens",
    // ALTCHA 机器人校验的 HMAC 密钥（server::altcha）：首次签发 challenge
    // 时生成并落库，只经 HMAC 使用、从不外发。属于「其它零散状态」，
    // 配置写入绝不能动它。
    "panelAltchaSecret",
    // config.json 迁移的完成标记（db::migrate::config）。为什么配置这一项
    // **需要**标记键而其余项不需要：配置项是开放集合，没法问「配置迁过了
    // 没有」（理由与完整论证见 db::migrate::config 的模块头）。
    "configMigrated",
    // 请求明细 / 聚合的迁移完成标记（request_stats::legacy）。
    //
    // 这两项用标记键，而 `logs` / `accounts` / `debug_traffic` 三项不用，
    // 原因是**那三张表有可用的业务主键**（历史行号 / 记录 uuid / 请求关联 id），
    // 「这条已经导过了」能直接由主键判断（`INSERT OR IGNORE`）；而
    // `requests` 的主键是自增 `row_id`（与内容无关）、`id` 列在旧行里可能是
    // 空串，`request_daily` 的 `date` 又会被运行期 UPSERT 改写 —— 都当不了
    // 「这条已经导过了」的判据。完整论证见
    // `request_stats::legacy::MARKER_REQUESTS` 的文档注释。
    //
    // 登记在这里是**必须**的：`config::save_raw` 写配置时要靠本清单排除
    // 「不归我管」的键，漏登记会让用户改一次 API Key 就把这两个标记删掉
    // —— 下次点「升级」会重复导入一遍历史明细。
    "requestsMigrated",
    "dailyMigrated",
    // 模型用量口径订正的完成标记（request_stats::aggregate 的
    // `MODEL_UPSTREAM_MARKER`）—— #139 的根因就是这个键漏登记：
    // 它在 `remap_model_days` 里**直接写库**（与重算同一事务，有意的：
    // 中断后标记必然没写、下次整批重跑），而 `config::init` 把全库读进内存
    // 快照发生在那之前 —— 不登记的话，用户改任意一项配置时 `save_raw` 会把
    // 它当「配置里已删掉的键」清掉，下次启动同一批历史被重算第二遍
    // （日期键未变时是幂等覆盖、静默；变过日期键就留下重复的聚合行）。
    "modelUsageUpstreamRemapped",
    // 远程模型清单的持久化缓存（core::providers::catalog_cache）：整份
    // 「各家上次成功拉到的清单」一个键（十份清单挤一个键的理由见那个模块头）。
    // 属于「其它零散状态」——它不是配置项，配置写入绝不能动它。
    "modelCatalogCache",
    // 后台任务的持久化排期与执行状态（core::task_state）：整份「每条任务的
    // 上次尝试 / 上次成功 / 下次执行 / 失败冷却 / 缓存值」一个键。它替代了
    // 改造前「排期只在内存里、重启即从头」的形态 —— 于是重启不再让
    // GitHub 检查、余额查询、模型刷新、凭证维护各多打一轮上游请求。
    // 属于「其它零散状态」：配置写入绝不能动它（否则排期归零、重启又立刻重跑）。
    "backgroundTaskState",
    // 旧「定时查询积分」的结果快照（全局任务已退役）：键保留在保留清单里是
    // 刻意的 —— `core::usage_records` 启动时把它的行导入 account_usage_records
    // 表后删除本键；若那次迁移失败，这里不让配置写入把唯一的迁移来源误清掉。
    // 迁移成功后这个键不再出现，条目留在清单里只是防御。
    "usageQuerySnapshot",
    // 出网代理池（core::proxy_pool）：整份 `{items: [...]}` 一个键 ——
    // 「网络代理」页维护的命名代理，账号可按 id 引用。属于「其它零散状态」：
    // 它不是配置项（配置写入绝不能动它，否则用户配好的代理会被改一次 API Key
    // 清空）。条目不落独立表、只整份读写，理由见那个模块头。
    "proxyPool",
    // 订阅（core::subscriptions）：整份 `{items: [...]}` 一个键 —— 「网络代理」
    // 页「订阅」区块维护的订阅链接，内核（sub_kernel）拉起时从 api 层拿启用
    // 清单。与 proxyPool 同类零散状态：不是配置项，配置写入绝不能动它。
    "subscriptions",
    // OfficeAce 模型可用性探测的结论（core::providers::officeace::probe）：
    // 整份 `{unavailable:[…], probedAt, account}` 一个键。探测「目录里列着、这个
    // 号实际打不通」的模型（上游回 81004/81009），结论由 `advertise_models` 用来
    // 默认隐藏它们。属于「其它零散状态」——不是配置项，配置写入绝不能动它
    // （否则每次改配置都让隐藏集合清零、客户端又照着一个一个试）。
    "officeaceModelProbe",
    // models.dev 模型元数据的刷新结果（core::model_meta::snapshot）：整份
    // 「上次成功拉到的精简索引」一个键（文件形态与内嵌快照逐字相同，生成口径见
    // `scripts/sync-modelsdev.mjs`）。**不是**配置项 —— 配置写入绝不能动它，
    // 否则用户改一次 API Key 就退回发版时那份更旧的内嵌快照。
    // 与上面的 `modelCatalogCache` 是两码事：那个存各家上游的**模型清单**，
    // 这个存模型的**元数据**（能力位 / 思考档位），刷新链路与失效语义各不同。
    "modelsDevCatalog",
];

/// 这个键是否属于「其它零散状态」（即不归网关配置管）。
///
/// 唯一入口：`config::save_raw` 用它决定哪些行不参与「删除已不存在的键」。
/// 做成函数而不是让调用方自己 `.contains()`：将来若固定键改成带前缀的命名，
/// 改一处即可（与 `db::migrate` 各迁移项包一层 `sql::has_key` 同一手法）。
pub fn is_reserved(key: &str) -> bool {
    RESERVED_KV_KEYS.contains(&key)
}

/// 当前 schema 版本。加表 / 加列时 +1，并在 [`apply_version`] 里补一个分支。
///
/// 为什么从 1 而不是 0 起步：0 是全新库的初始值，用它当「已是最新版」会让
/// 全新库跳过建表。1 起步后「库里什么都没有」与「schema 版本 1」是两件事。
///
/// ── 版本 2 / 5 / 6 / 7 / 10 / 12 / 13：既有表**加列** ────────────
/// 见 [`ADDED_COLUMNS`]（版本号 → 表 → 列 → 声明，每列的语义写在那一份表上）。
/// v2 给 `requests` 加两个 JSON 文本列，v5 加思考等级两列，v6 加阶段与计时
/// 两列，v7 加「测试来源」列，v10 加选路决策快照列，v12 加请求级 Credits，
/// v13 给 `request_daily` 加当天 Credits 合计。
///
/// 加列是本文件唯一**非幂等**的 DDL（`ALTER ... ADD COLUMN` 不支持
/// `IF NOT EXISTS`）。在单条版本线上幂等由版本号保证就够了，但本 fork 与
/// 官方上游曾各占一份 v10/v11（本地是 Credits 两列、官方是 `requests.route`
/// 与 `checkin_records`）—— 撞号之后出现「版本号已经越过某版、它引入的列却
/// 缺失」的库，按版本号跑 DDL 永远补不上。所以加列改为按**实际列**核对
/// （[`ensure_column`]），并在每次打开时用 [`reconcile`] 兜一遍最终形态。
///
/// ── 版本 3：request_raw 表（原始报文，预览对话功能的地基）───────
/// 见 [`V3_SCHEMA`]。与加列类的差别：这是**新表**，走
/// `CREATE TABLE IF NOT EXISTS`，天然幂等，不需要按列核对。
///
/// ── 版本 4：把 v3 的建表语句**再幂等地跑一遍**（存量库修复）──────
/// 见 [`V4_SCHEMA`]。这不是结构变更，而是对「版本号写着 3、表却不在」
/// 这类存量库的修复：实测有库 `user_version=3` 却没有 `request_raw`（早期
/// 二进制曾在这个版本号下放过别的 DDL，撞号了）。后果是记账收尾事务里那句
/// `DELETE FROM request_raw` 报「no such table」→ **整个事务回滚** → 明细的
/// 进行中行永远收不了尾（界面上表现为「只有进行中、没有结束」）。
/// 这是本项目第一次遇到版本号撞车 —— v4 的补法是「换一个新版本号把 DDL 重放」，
/// 只在**单条线**上够用；等到两条线撞号（见上面加列那段）就需要 [`reconcile`]。
///
/// ── 版本 8：account_usage_records 表（每账号的余额查询记录）─────
/// 见 [`V8_SCHEMA`]。新表，`CREATE TABLE IF NOT EXISTS`。它取代旧 kv 快照
/// `usageQuerySnapshot` 成为「每账号最近一次余额查询结果」的权威存放处：
/// 全局「定时查询积分」退役后查询按账号各自到期触发，一行一账号才能逐条
/// 更新、逐条被选路读取（kv 整份读改写撑不住这个粒度）。旧快照的数据由
/// `core::usage_records` 启动时导入，升级后余额列不会变空白。
///
/// ── 版本 9：requests 补「按账号 + 时间」索引（限制器的 Token 周期聚合）──
/// 见 [`V9_SCHEMA`]。`CREATE INDEX IF NOT EXISTS`，同样天然幂等。
///
/// ── 版本 11：checkin_records 表（签到记录的每账号明细台账）───────
/// 见 [`V11_SCHEMA`]。新表，`CREATE TABLE IF NOT EXISTS`。它是签到中心
/// 「签到记录」页的数据源：每执行一次签到，每个账号一行（领成功 / 已领过 /
/// 失败及其错误原文 / 收益文案），补上改造前「只有 config.json 里一条聚合
/// 同 lastResult」看不见的明细。写入方是 `core::checkin_records`（记账点唯一：
/// `core::billing::checkin::run_checkin`）。
///
/// ── 版本 14：travel_records 表（旅行记录的每账号明细台账）───────
/// 见 [`V14_SCHEMA`]。与 v11 同一形态（新表）。它是签到中心「旅行记录」页的
/// 数据源：每执行一轮「真动作」，每个真动手的账号一行（派出 / 领回 / 失败）。
/// 写入方是 `core::travel_records`（记账点在 `core::travel_auto`）。它与
/// v11 的 checkin_records 是**并列**的两张表，不共用列。
pub const SCHEMA_VERSION: i64 = 14;

/// 版本 1 的全部表与索引：改造前所有 JSON / JSONL 文件的对应形态。
///
/// 为什么把六张表写成一个 batch 而不是每张一个常量再拼：它们共同构成「版本 1
/// 的 schema」这一个整体，分开的常量只会让「v1 到底包含什么」需要跨常量拼接
/// 才能看清。单张表的定界用注释块表达（下方按表分块），语句本身用
/// `execute_batch` 一次提交。
const V1_SCHEMA: &str = "
-- ── kv：通用键值表 ────────────────────────────────────────────
-- 存什么：网关配置的每个顶层键、桌面设置整份、各存储的元数据、零散状态。
-- 为什么要有它：这些数据的共同点是「没有固定结构、读写都是整份进出」——
-- 为它们各建一张表，表数量会随配置项增长；塞进一个 TEXT 列则失去按 key 精确
-- 读写的能力（那是它们唯一需要的查询能力）。一列值 + 主键键名正好。
-- 为什么值是 TEXT 而不是 BLOB / JSONB：SQLite 没有原生 JSON 类型，JSON 就是
-- 文本；存 TEXT 时 `json_extract` 等 JSON 函数可以直接用来做条件查询，
-- 而 BLOB 会强制每次比较都做编码转换。值里存什么形态由写入方决定（见模块头
-- 的键命名规范），本表不做校验。
CREATE TABLE IF NOT EXISTS kv (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

-- ── accounts：账号表（原 accounts.json）────────────────────────
-- 存什么：一条账号记录一行。`data` 列存**整条账号记录的 JSON 原文**。
-- 为什么保留 JSON 原文而不是把每个字段都拆成列：「用户升级绝不能丢账号里
-- 任何字段」是本项目的硬约束（含未知字段、用户手工加的备注、旧版本遗留的
-- currentAccountId），而账号里有 `rateLimits` 这类嵌套结构、各家 provider
-- 的字段集还在演进。拆列意味着每加一个字段都要一次 schema 迁移 + 一次数据
-- 重写，且**漏掉一个字段就等于升级时把那列的数据静默丢掉** —— 这是
-- `core::account_store` 模块头第 1 条不变量，JSON 原文是唯一能自然满足它的形态。
-- 那一列的例外：需要**排序 / 筛选 / 校验唯一性**的字段额外提取成列，
-- 它们的值从 JSON 里复制一份（权威副本仍是 `data`，提取列是查询用的投影）。
--   id       主键，账号唯一标识（与 JSON 里的 id 同值）
--   provider 提供商 id（workbuddy / raccoon / cline-free…），按家分组查
--   priority 优先级数值，越小越先用（全局唯一，见账号存储不变量 2）
--   enabled  是否启用，0/1。SQLite 没有布尔类型，按惯例用 INTEGER
--   added_at 加入时间（毫秒 Unix 时间戳），同优先级时的稳定排序依据
--   data     整条记录的 JSON 原文（**权威副本**）
-- 「已启用且有凭证的第一个账号」这个派生（pick_current）走下面的复合索引：
-- 先按 provider 分片、再按 priority 升序，added_at 保证同优先级顺序稳定。
CREATE TABLE IF NOT EXISTS accounts (
  id       TEXT PRIMARY KEY,
  provider TEXT NOT NULL,
  priority INTEGER NOT NULL,
  enabled  INTEGER NOT NULL DEFAULT 1,
  added_at INTEGER NOT NULL DEFAULT 0,
  data     TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_accounts_order ON accounts(provider, priority, added_at);

-- ── logs：事件日志（原 logs.jsonl）─────────────────────────────
-- 存什么：日志页展示的运行日志，一行一条。
-- 为什么 id 是 INTEGER PRIMARY KEY 而不另建自增列：SQLite 里
-- 「INTEGER PRIMARY KEY」这一列**就是** rowid 的别名，天然自增、天然唯一，
-- 且是物理存储顺序（按 id 查是 O(log n) 的 B 树查找）。这正是原来内存里
-- 那条「自增 id」的等价物，前端按 id 增量拉取（`since_id`）因此直接可用。
-- 为什么 data 可空：结构化事件的附加字段（429 切换的 from/to 等）只有部分
-- 日志有；空串与「没有 data」语义不同，所以用 NULL 而不是默认 '{}'。
-- 为什么 ts 存 INTEGER 毫秒：原文件里就是 `Date.now()` 的毫秒数，且范围
-- 与负数场景都在 i64 内，不需要 TEXT 的日期格式（排序即数值排序才是重点）。
-- 三个索引（ts / level / category）对应日志页固定的三种过滤维度：
-- 按时间倒序翻页、按「级别及以上」筛选、按分类筛选。刻意**不做**复合索引：
-- 三个维度是独立下拉、组合方式多，复合索引只能覆盖其中一种组合，反而不如
-- 让 SQLite 自己挑（数据量级是几百到几万行，单列索引足够）。
CREATE TABLE IF NOT EXISTS logs (
  id       INTEGER PRIMARY KEY,
  ts       INTEGER NOT NULL,
  level    TEXT NOT NULL,
  category TEXT NOT NULL,
  message  TEXT NOT NULL,
  data     TEXT
);
CREATE INDEX IF NOT EXISTS idx_logs_ts ON logs(ts);
CREATE INDEX IF NOT EXISTS idx_logs_level ON logs(level);
CREATE INDEX IF NOT EXISTS idx_logs_category ON logs(category);

-- ── requests：请求明细（原 requests.jsonl）─────────────────────
-- 存什么：每次对话转发的记账明细（模型、账号、状态、耗时、token 用量）。
-- 为什么是 AUTOINCREMENT 的 row_id 而不是让 id 当主键：
--   row_id 是**自增物理行号**（内部用，保证稳定顺序、可作游标）；
--   id 是请求的**关联键**（与调试报文 debug_traffic.id 同值，靠它把明细与
--   原始报文对上），旧数据里可能是空串，**且不保证唯一**（重试、去重排队
--   的若干路径会产生同 id 的记账）。两个字段职责不同，混用会让「空串 id
--   被当成主键」直接撞唯一约束而丢掉整条记录。
-- 为什么显式写 AUTOINCREMENT：不加它时 rowid 会被复用（删掉最大行后新行
-- 会拿到同一个号），而这里 row_id 会被当作翻页游标，复用会让「翻页漏行 /
-- 重行」。代价是多一张 sqlite_sequence 表，可忽略。
-- 为什么 token 四列各自 DEFAULT 0 而不是可空：原 JSON 里缺失就是 0
-- （前端直接做加法与求和），可空会让每个读点都要处理 NULL；这四个数的
-- 「没有」与「0」在语义上也没有区别。
-- 其余 DEFAULT 同理：都是「旧数据没有这个字段」时的缺省值，含义与文件版
-- 的读取兜底（`Number(x) || 0`）逐字对应 —— 导入旧数据时不必预先补齐每个
-- 字段，缺的列走 DEFAULT 即可。
CREATE TABLE IF NOT EXISTS requests (
  row_id            INTEGER PRIMARY KEY AUTOINCREMENT,
  id                TEXT NOT NULL DEFAULT '',
  ts                INTEGER NOT NULL,
  model             TEXT NOT NULL,
  account_id        TEXT NOT NULL DEFAULT '',
  account_name      TEXT NOT NULL DEFAULT '',
  status            INTEGER NOT NULL DEFAULT 0,
  duration_ms       INTEGER NOT NULL DEFAULT 0,
  first_response_ms INTEGER,
  attempts          INTEGER NOT NULL DEFAULT 1,
  error             TEXT,
  prompt_tokens     INTEGER NOT NULL DEFAULT 0,
  completion_tokens INTEGER NOT NULL DEFAULT 0,
  total_tokens      INTEGER NOT NULL DEFAULT 0,
  cache_read_tokens INTEGER NOT NULL DEFAULT 0,
  provider          TEXT NOT NULL DEFAULT '',
  client_model      TEXT NOT NULL DEFAULT '',
  upstream_model    TEXT NOT NULL DEFAULT ''
);
-- ts：报表按区间聚合（近 24 小时 / 7 天 / 30 天）与明细按时间倒序翻页。
-- id：由请求 id 反查明细（与调试报文对照、按 id 看一次请求的完整记录）。
CREATE INDEX IF NOT EXISTS idx_requests_ts ON requests(ts);
CREATE INDEX IF NOT EXISTS idx_requests_id ON requests(id);

-- ── request_daily：按天聚合（原 request-daily.jsonl）──────────
-- 存什么：一天一行（date 是**本地时区**的 `YYYY-MM-DD`，与文件名版逐字相同；
-- 时区处理在 `request_stats::clock`，本表只存字符串、不做时区推断）。
-- 为什么 date 直接当主键：一天一行是这张表的全部语义，主键即唯一约束，
-- 天然让「同一天重复记账」变成 UPSERT 而不是插入两行。
-- 为什么三个 *Stats 列是 JSON 数组文本：它们是「当天按模型 / 提供商 / 账号
-- 的累计」，条目数随用户配置变化（模型目录是远程刷新的）、且**整体读写**
-- （报表要整份拿出来算），拆表会让「取一天的三个维度」变成三次 join。
-- 与文件版的行内嵌数组一一对应，导入时原样搬过来即可。
-- 默认 `'[]'` 而不是 NULL：空数组与「这一列没有数据」在这张表里是同一件事，
-- 统一成 `'[]'` 让读取方少一个分支。
-- requests/successful/tokens 这几列是「当天总量」，保留成独立列而不是也从
-- JSON 里算：热力图与汇总行只读它们，拆出来才走得上主键索引。
CREATE TABLE IF NOT EXISTS request_daily (
  date               TEXT PRIMARY KEY,
  requests           INTEGER NOT NULL DEFAULT 0,
  successful         INTEGER NOT NULL DEFAULT 0,
  tokens             INTEGER NOT NULL DEFAULT 0,
  cache_hit_tokens   INTEGER NOT NULL DEFAULT 0,
  cache_input_tokens INTEGER NOT NULL DEFAULT 0,
  model_tokens       TEXT NOT NULL DEFAULT '[]',
  provider_stats     TEXT NOT NULL DEFAULT '[]',
  account_stats      TEXT NOT NULL DEFAULT '[]'
);

-- ── debug_traffic：调试报文（原 debug-traffic.jsonl）──────────
-- 存什么：调试模式下的**上游侧**原始请求/响应（头部已脱敏，脱敏不可关闭）。
-- 为什么 id 是主键：它与 requests.id 同值、是跨表关联键；一条请求只有一份
-- 报文，所以天然唯一（与 requests 不同 —— 那边同 id 可以有多行记账）。
-- 为什么 request/response 的 body 列可空、而 header 列 NOT NULL 带默认值：
-- 请求侧一定存在（我们至少发出去过），响应侧可能因为上游断连而没有；
-- 而头部哪怕没有也应该是 `'{}'`（前端按对象读）。
-- 为什么体用 TEXT 而不用 BLOB：里面是 SSE 文本 / JSON 文本，存 TEXT 才能在
-- `sqlite3` 命令行里直接看（排障场景这是主要用法）。
-- truncated 标记「体被单条上限截断过」（见 debug_traffic::MAX_ENTRY_BYTES），
-- 缺了它前端会把半截响应当完整响应展示。
-- size 是该条**落库后实际占用的字节数**（截断后的三份 JSON 文本 + 两份可空
-- 文本 + 身份字段的长度合计，见 `debug_traffic::sql::payload_bytes`），供两道闸
-- 的字节统计用（`SUM(size)` 与 MAX_TOTAL_BYTES 比较）。
-- 它**不是**「未截断前的原始大小」：闸门守的是「这张表实际占多大」，用截断后的
-- 体积才对得上。这一行原先按「将来可能存原始大小」的设想写成「未截断前」，与
-- 实现不符 —— 改造前的 entry_size 也是截断后体积（原版 record 先 truncate 再算
-- size），所以现在的口径是承接既有实现、不是本次改造新定的。将来真需要原始
-- 大小就**另加一列**（在截断前记一笔），不要复用 size。
-- 索引只有 ts：调试报文按时间倒序翻页是唯一的批量读法，其余都按主键 id 取。
CREATE TABLE IF NOT EXISTS debug_traffic (
  id               TEXT PRIMARY KEY,
  ts               INTEGER NOT NULL,
  url              TEXT NOT NULL DEFAULT '',
  provider         TEXT NOT NULL DEFAULT '',
  request_headers  TEXT NOT NULL DEFAULT '{}',
  request_body     TEXT NOT NULL DEFAULT '',
  status           INTEGER,
  response_headers TEXT,
  response_body    TEXT,
  truncated        INTEGER NOT NULL DEFAULT 0,
  size             INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_debug_traffic_ts ON debug_traffic(ts);
";

/// 「加列」类版本的全部内容：`(版本号, 表, 列, 声明)`。
///
/// ── 为什么把加列集中成一份表，而不是每版一个 DDL 常量 ──────────────
/// 加列是唯一非幂等的 DDL（`ALTER TABLE ... ADD COLUMN` 不支持
/// `IF NOT EXISTS`，写上去是语法错）。在**单条**版本线上，「一个版本跑一次、
/// 跑完把 `user_version` 写进同一事务」已经够了。但本 fork 与官方上游各占了
/// 一份 v10/v11（本地是 Credits 两列、官方是 `requests.route` 与
/// `checkin_records`）—— 撞号之后会出现「版本号已经越过某版、它引入的列却
/// 缺失」的库（升级路径详见 [`migrate`] 与 [`reconcile`]）。对这种库按版本号
/// 跑 DDL 永远补不上那一列；而按**实际列**核对则天然幂等。集中成这一份表，
/// 就是为了让「推进版本」（[`apply_version`]）与「核对修复」（[`reconcile`]）
/// 读同一份事实来源，不会两处漂移。
///
/// 第 4 列（声明）是 `ADD COLUMN` 后面的类型与约束，逐字来自原先各版本的常量。
/// 是否已存在由 [`ensure_column`] 用 `PRAGMA table_info` 判断。
///
/// ── 为什么是「可空的 JSON 文本」而不是拆表（v2）─────────────────
/// `attempt_details` / `sensitive_hits` 是**一条明细的附属历史**，读写都是
/// 整份进出、没有按单次尝试查询的需求（要排障就在 `sqlite3` 里对 JSON 列用
/// `json_each`）。默认 `'[]'` 而不是 NULL：空数组与「这一列没有数据」是同一
/// 件事，统一成 `'[]'` 让读取方少一个分支（写入侧 `encode_json_list` 也把
/// 序列化失败兜成 `'[]'`，两边形态一致）。
///
/// 逐列语义（按版本）：
///   - v2 `attempt_details`：每次上游尝试的明细 `[{provider,status,error}]`，
///     按发生顺序。条数 <= attempts，另受 `usage::MAX_ATTEMPT_DETAILS`（24）闸。
///   - v2 `sensitive_hits`：本次命中的脱敏规则 `[{word,count}]`，按次数降序。
///     列名与结构未随「敏感词 → 硬编码规则」的改造而变，前端「敏」标签照常。
///   - v5 `client_reasoning`：下游请求体里客户端显式指定的思考等级；空串 =
///     没指定（或该行来自还没有此列的旧版本）。
///   - v5 `upstream_reasoning`：实际随上游请求发出的思考等级；空串 = 没有
///     等级随行（客户端没指定且映射没绑、承载家不接等级、或一次都没发出去）。
///   - v6 `phase`：在途阶段（connecting/waiting/streaming/retrying/queued），
///     空串 = 不在途；终态行收尾时清空，与 status=0 同进同退。
///   - v6 `phase_started_at`：进入当前阶段的时刻（毫秒，与 ts 同口径），阶段
///     计时由它现算；不在途的行是 NULL（「真的可能没有」用 NULL 而非 0）。
///   - v7 `is_test`：这条明细是不是模型测试发起的（1/0）。测试走真实转发链路，
///     与真实流量长得一样，唯一能分开的就是「谁发起的」——报表聚合据此排除
///     （测试是人工反复发起的样本，混进去会把真实流量读歪），请求日志照常
///     显示并标出来。
///   - v10 `route`：选路决策快照 JSON 原文（`{strategy,sticky,stickyHit,
///     picked,ordered[],blocked[]}`，采集侧已截断）。DEFAULT NULL：旧行与
///     「没走到选路」都是「没有决策」，与 `attempt_details` 的 `'[]'` 不同
///     （那是有意的空表，这里不必造一种空形态）。
///   - v12 `credit`：本次请求的上游扣费 Credits（REAL，小数如 0.07 / 5.3）。
///     不是所有家都给 ——WorkBuddy 国内版实测非零，国际版与其余家多为 0；
///     旧行 DEFAULT 0。
///   - v13 `credits`：`request_daily` 当天 Credits 合计（REAL）。报表页只走
///     聚合表（寿命独立于明细、`all` 可跨年），所以聚合行上也要有落点，否则
///     跨年区间会少一大段。列名用复数（与 `DailyEntry::credits` 同名）：单天
///     合计是个汇总读数，明细那列是**单次请求**的扣费，用单数 `credit`；
///     两者语义不同、不共用列名。
const ADDED_COLUMNS: &[(i64, &str, &str, &str)] = &[
    (2, "requests", "attempt_details", "TEXT NOT NULL DEFAULT '[]'"),
    (2, "requests", "sensitive_hits", "TEXT NOT NULL DEFAULT '[]'"),
    (5, "requests", "client_reasoning", "TEXT NOT NULL DEFAULT ''"),
    (5, "requests", "upstream_reasoning", "TEXT NOT NULL DEFAULT ''"),
    (6, "requests", "phase", "TEXT NOT NULL DEFAULT ''"),
    (6, "requests", "phase_started_at", "INTEGER"),
    (7, "requests", "is_test", "INTEGER NOT NULL DEFAULT 0"),
    (10, "requests", "route", "TEXT"),
    (12, "requests", "credit", "REAL NOT NULL DEFAULT 0"),
    (13, "request_daily", "credits", "REAL NOT NULL DEFAULT 0"),
];

/// 版本 3：`request_raw` 表 —— 请求/响应的**下游侧**原始正文（预览对话功能的地基）。
///
/// ── 它和 debug_traffic 表是什么关系 ──────────────────────────
/// `debug_traffic`（调试报文）是**调试模式专用**的上游侧报文（头部脱敏、
/// 有全局字节闸、开关关闭时完全不采集）；本表是**始终采集**的下游侧正文
/// （客户端发来的请求体 + 网关下发的响应体），供请求日志的「预览对话」弹窗
/// 用。两表按同一个 `id`（请求关联 id）各存各的，谁也不依赖谁 ——
/// `core::debug_traffic` 的现有行为一字未动，本表是纯粹的并行补充。
///
/// ── 为什么独立成表而不是给 requests 加两列 ────────────────────
/// 正文是大字段（两侧各 128 KiB 上限），明细列表的每次分页都要取
/// `REQUEST_COLUMNS` —— 混进主表会让「只看列表」的请求反复搬运几百 KB 的
/// TEXT。独立表后主查询零开销，按 id 取详情时才 join 进来；请求日志列表页
/// 也不需要改任何 SQL（OmniProxy 的 request_log_raw 副表同此取舍）。
///
/// ── 为什么 id 直接当主键 ────────────────────────────────────
/// 一条请求只有一份「下游视角」的正文，天然一对一（与 requests 表的 id
/// 不同 —— 那边同 id 可以多行，所以不敢当主键）。主键即唯一约束，
/// 「同 id 再写」天然是 UPSERT 覆盖，不需要额外判重。
///
/// ── 为什么体用 TEXT 而不用 BLOB ─────────────────────────────
/// 与 debug_traffic 同一取向：里面是 JSON / SSE 文本，存 TEXT 才能在
/// `sqlite3` 命令行里直接看（排障的主要用法）。
///
/// ── 为什么没有索引（连 ts 都不建）────────────────────────────
/// 本表只有两种读法：按主键 id 取单行（主键自带）、按「保留最新 N 行」裁剪
/// （2000 行的表全排序是微秒级）。唯一会按 ts 范围查它的是清理预览的计数，
/// 那条走的是 `id IN (SELECT id FROM requests ...)` 主键查找。表有行数硬闸，
/// 不会长到大到需要索引。
const V3_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS request_raw (
  id            TEXT PRIMARY KEY,
  ts            INTEGER NOT NULL,
  request_body  TEXT NOT NULL DEFAULT '',
  response_body TEXT NOT NULL DEFAULT '',
  size          INTEGER NOT NULL DEFAULT 0
);
";

/// 版本 4：v3 建表语句的幂等重放（存量库修复，完整背景见 [`SCHEMA_VERSION`]）。
///
/// 直接复用 [`V3_SCHEMA`] 而不是复制一份：两处若各写一份，将来 request_raw
/// 加列时必然只改一处，而「表定义只有一处事实来源」正是 v2 那条纪律。
/// `CREATE TABLE IF NOT EXISTS` 保证对正常库（表已在）是无操作。
const V4_SCHEMA: &str = V3_SCHEMA;

/// 版本 8：account_usage_records 表 —— 每账号一条余额查询记录（一账号一行，
/// 主键即账号 id，重复写入天然是 UPSERT）。
///
/// 存什么：`usage` 是适配器返回的**归一化余额 JSON 原文**（与 `/api/accounts/usage`
/// 行里的 `usage` 同一份形状，前端按它渲染余额列），`error` / `code` 是失败行的
/// 原因与机器标记（与快照行同语义）。`remaining` 是从 usage 里提出的**数字**投影
/// （归一化家 = `available`，workbuddy 既有形状 = `totalLeft`）：转发选路的
/// 「余额不足跳过」每条请求都要比一次，从 JSON 里现场解析太奢侈，主键查一列即可；
/// `unlimited` 单独一列，workbuddy 的 ∞ 账号不参与阈值判定。
///
/// ── 为什么 `remaining` 只在成功时覆写 ────────────────────────
/// 失败行保留上次成功的数值：欠费判定不因「这次查询失败」而放行（避免
/// 「查询失败 → 放行 → 402」的窗口期），恢复需要查询成功且数值回到阈值之上。
/// 失败本身的展示信息在 `error` / `code` 里，二者互不覆盖。
///
/// 为什么时间是 INTEGER 毫秒而不是 DATETIME：调度判定（到期 = 上次尝试 +
/// 间隔）要的是可直接比较的毫秒数，与 `logging::now_ms` 同一口径，读出来即用。
const V8_SCHEMA: &str = "
-- ── account_usage_records：每账号的余额查询记录（schema v8）──
CREATE TABLE IF NOT EXISTS account_usage_records (
  account_id      TEXT PRIMARY KEY,
  usage           TEXT,
  error           TEXT,
  code            TEXT,
  remaining       REAL,
  unlimited       INTEGER NOT NULL DEFAULT 0 CHECK (unlimited IN (0, 1)),
  last_success_at INTEGER NOT NULL DEFAULT 0,
  last_attempt_at INTEGER NOT NULL DEFAULT 0,
  updated_at      INTEGER NOT NULL DEFAULT 0
);
";

/// 版本 9：给 `requests` 补「按账号 + 时间」的聚合索引。
///
/// ── 它服务什么 ──────────────────────────────────────────────
/// 账号限制器的 **Token 消耗规则**：每条规则问的是「这个账号在当前重置窗口
/// （对齐自然时间的固定窗口，30 分钟 ~ 24 小时）内一共消耗了多少 Token」——
/// 一次 `SELECT account_id, SUM(total_tokens) FROM requests
/// WHERE account_id = ? AND ts >= ?` 的聚合。没有这个索引它只能走
/// `idx_requests_ts` 全表扫到过滤条件上，限制器的心跳刷新（每 10 秒一轮）
/// 会把请求明细表整个翻一遍。
///
/// 为什么是复合索引 `(account_id, ts)` 而不是只补 `account_id`：窗口过滤
/// （`ts >= 窗口起点`）要在**同一账号内**做范围扫描，复合索引让聚合变成
/// 一次有序的范围读；单独的 account_id 索引拿到行号后还得回表逐行比 ts。
///
/// 聚合查询的全部调用方：`core::limiter` 的窗口刷新（按 distinct 周期分组
/// 查）与单账号即时复查（自动禁用判定）。
const V9_SCHEMA: &str = "
-- ── requests 补按账号聚合的索引（schema v9）──────────────────
CREATE INDEX IF NOT EXISTS idx_requests_account_ts ON requests(account_id, ts);
";

/// 版本 11：checkin_records 表 —— 签到记录的**每账号明细台账**。
///
/// 存什么：每执行一次签到（定时条目轮次 / 启动补签 / 手动批量 / 单账号 /
/// 国际版活动领取），每个账号落一行。列的口径：
///   - `ts`：记账时刻（毫秒，`logging::now_ms` 同口径；排序与翻页游标）；
///   - `date`：北京时间 `YYYY-MM-DD`（上游自然日，`core::beijing` 的日期键 ——
///     「哪天的签到」按它算，跟机器时区无关，与调度判定同一份口径）；
///   - `source`：触发来源 `schedule / startup / manual / single / activity`
///     （见 `core::checkin_records` 的模块头），界面显示为「来源」列；
///   - `reason`：给日志看的那句（「到点触发（08:00）」「手动签到」…），原样存；
///   - `account_id` / `account_name` / `provider`：账号三件套（结果行里就有，
///     原样复制；账号删除后历史行仍然可读）；
///   - `outcome`：`success / already / pending / failed`（判定复用签到执行体的
///     同一对判据，判据只此一份，见 `core::checkin_records::classify`）；
///   - `ok`：`success|already → 1` 的布尔投影（「今日已完成」口径的 SQL 过滤列，
///     免得每个读点都写 `(outcome='success' OR outcome='already')`）；
///   - `message`：claim.msg 原文（「今日积分 +100」等**收益就在这里**）；
///   - `error`：执行错误原文（失败行的原因）；
///   - `kept_active`：国际版保活成功（`activity.pokeSucceeded`）标记。
///
/// 为什么 row_id 用 AUTOINCREMENT：与 `requests` 同一理由 —— 它当翻页游标，
/// 复用 rowid 会让「翻页漏行 / 重行」。
///
/// 索引三个（ts / outcome / provider），对应记录页固定的三种过滤维度，
/// 刻意不做复合索引（与 logs 表同一取舍：组合多、量级小，让 SQLite 自己挑）。
const V11_SCHEMA: &str = "
-- ── checkin_records：签到记录的每账号明细台账（schema v11）────
CREATE TABLE IF NOT EXISTS checkin_records (
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
  error        TEXT,
  kept_active  INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_checkin_records_ts ON checkin_records(ts);
CREATE INDEX IF NOT EXISTS idx_checkin_records_outcome ON checkin_records(outcome);
CREATE INDEX IF NOT EXISTS idx_checkin_records_provider ON checkin_records(provider);
";

/// 版本 14：travel_records 表 —— 派猫猫旅行的**每账号明细台账**。
///
/// 与 [`V11_SCHEMA`]（checkin_records）同一形态：每执行一次「真动作」的轮次
/// （后台巡检 / 启动补跑 / 手动触发）里，每个**真动手的**账号落一行。列的口径：
///   - `ts` / `date`：记账时刻与北京时间日期键（口径同 checkin_records）；
///   - `source`：触发来源 `auto / startup / manual`（后台循环 / 启动补跑 /
///     手动「立即执行」或单账号「派猫猫 / 领回」）—— 见 `core::travel_records`；
///   - `reason`：给日志看的那句（轮次触发原因，如「后台巡检」「手动触发」）；
///   - `account_id` / `account_name` / `provider`：账号三件套；
///   - `outcome`：`departed`（派出旅途）/ `claimed`（领回奖励）/
///     `failed`（派出或领取失败）；
///   - `ok`：`departed|claimed → 1` 的布尔投影（SQL 过滤列）；
///   - `message`：领取收益文案（如 `+12 积分`），派出行为空；
///   - `error`：失败原因原文（失败行）。
///
/// 为什么**只记真动作**：旅行每 2 小时一轮、跨小时才领，多数轮次对多数账号
/// 是「在途等待」或「当天已领满跳过」—— 那些不是事件。只把「真派了 / 真领了 /
/// 真失败」记下来，台账才可读（与 `core::travel_history` 的旧取舍一致）。
///
/// 为什么 row_id 用 AUTOINCREMENT：与 [`V11_SCHEMA`] 同理由（翻页游标）。
/// 它取代了 config.json 的 `travelHistory`（每轮一行的轮次汇总）—— 那张卡与
/// 后端 `travel_history` 模块随本版一并删除（见 `core::travel_auto::collect`）。
///
/// 索引三个（ts / outcome / provider），与 checkin_records 同一取舍。
const V14_SCHEMA: &str = "
-- ── travel_records：派猫猫旅行每账号明细台账（schema v14）────
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

/// 把库升到 [`SCHEMA_VERSION`]（幂等：已是最新版时只做一次核对修复）。
///
/// 返回 `rusqlite::Result` 而不是本模块自造的字符串错误：调用方 `Db::open`
/// 是唯一消费点，它把错误包成一句人能读的话（`format!`）就够了；而这里需要
/// 区分「读版本号失败 / 跑 DDL 失败 / 写版本号失败」三种，保留 rusqlite 的
/// 错误类型（含 SQLite 的错误码与消息）才说得清是哪一步坏的。
///
/// ── 为什么最后一定要跑一遍 [`reconcile`] ────────────────────
/// 版本号是**推进**的依据，但不是唯一事实：本 fork 与官方上游曾各占一份
/// v10/v11（本地是 Credits 两列、官方是 `requests.route` 与 `checkin_records`），
/// 撞号之后会出现「版本号已经越过某版、它引入的列/表却缺失」的库 —— 对这类库
/// 按版本号跑 DDL 永远补不上。`reconcile` 按**实际对象**核对最终形态，缺什么
/// 补什么；它幂等，所以每次打开都跑，顺带成为「上一次修复中途失败、下次自动
/// 重试」的兜底（即使版本号已经写成 SCHEMA_VERSION）。
pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let current: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if current > SCHEMA_VERSION {
        // 版本比本二进制新（用户从新版回退到旧版）：**不报错、也不动库**。
        // 报错会让旧版直接起不来（用户只是想回退一次），而「降级 schema」
        // 意味着删列删表，那是不可逆的数据丢失 —— 两者都不该自动发生。
        // （只有**严格**大于才跳过：等于时仍要走一遍 reconcile，见上面。）
        return Ok(());
    }
    for version in (current + 1)..=SCHEMA_VERSION {
        // 每个版本一个独立事务：DDL 与版本号必须一起生效或一起不生效，
        // 否则中断后会出现「表已建、版本号还是旧的」（下次启动重跑 DDL，
        // 靠 IF NOT EXISTS 能过）与「版本号已新、表没建全」（**跑不过去**）
        // 两种错位，后者更糟，所以宁可牺牲「一次提交全部版本」的原子性。
        //
        // 用 `unchecked_transaction()` 而不是 `transaction()`：后者要
        // `&mut Connection`，而本函数的签名是 `&Connection`（它在 `Db::open`
        // 里被调用，那时 `Db` 还没构造出来，拿不到 `with_mut`）。「同一连接上
        // 不能有嵌套事务」这条约束由调用点保证 —— `Db::open` 持有唯一那把锁，
        // 迁移期间不会有第二个访问者。
        //
        // 注意 `tx` 不需要 `mut`：`pragma_update` / `commit` 都只取 `&self`，
        // 写事务的独占性由 SQLite 自己保证（同一连接上不允许第二条语句并发），
        // 不是靠 Rust 的独占借用。
        let tx = conn.unchecked_transaction()?;
        apply_version(&tx, version)?;
        tx.pragma_update(None, "user_version", version)?;
        tx.commit()?;
    }
    reconcile(conn)
}

/// 按**实际对象**核对最终 schema：该有的表 / 索引不在就建、该有的列不在就补。
///
/// ── 与「按版本号推进」是什么关系 ─────────────────────────────
/// 不是替代，而是兜底。`migrate` 的版本循环仍按版本号推进（新库、单线老库都
/// 走它），本函数只补版本号**补不到的洞**：两条版本线撞号（见 [`migrate`] 与
/// [`ADDED_COLUMNS`]）会让某个版本的 DDL 永久跳过。
///
/// ── 为什么可以每次都跑 ──────────────────────────────────────
/// 它做的全是幂等操作：`CREATE ... IF NOT EXISTS`（表 / 索引在就跳过）与
/// [`ensure_column`]（列在就跳过）。代价是每次打开多几条 `PRAGMA table_info`
/// 与一段零改动的 DDL，相对一次启动可忽略。
///
/// ── 为什么是「缺才补」而不是「多就删」────────────────────────
/// 只补不删：多出来的列 / 表（比如官方比本 fork 多带的）是数据，删列删表都
/// 不可逆。本函数的职责是保证「本二进制能跑」，不是把库修剪成某个形态。
fn reconcile(conn: &Connection) -> rusqlite::Result<()> {
    // 先保证所有「表 / 索引」在（都是 CREATE IF NOT EXISTS，天然幂等）：
    // V1 是全部基础表 + 其索引，V3 / V8 / V11 / V14 是后续新增的表，V9 是一个索引。
    // （V4 复用 V3，不重复列。）
    for ddl in [V1_SCHEMA, V3_SCHEMA, V8_SCHEMA, V9_SCHEMA, V11_SCHEMA, V14_SCHEMA] {
        conn.execute_batch(ddl)?;
    }
    // 再逐列补齐「加列」类版本的列（表里已存在的跳过）。
    for &(_, table, column, decl) in ADDED_COLUMNS {
        ensure_column(conn, table, column, decl)?;
    }
    Ok(())
}

/// `table` 是否已经有 `column` 列。
///
/// 用 `PRAGMA table_info` 读列清单再按名比对（而不是 `select ... limit 0`
/// 之类的试探）：后者靠报错判断，会把「列不存在」与「其它 SQL 错误」混成一类。
/// `table` 是本文件的编译期常量（[`ADDED_COLUMNS`]），不是外部输入，所以直接
/// 拼进 SQL —— 何况 `PRAGMA` 本就不支持参数绑定（`table_info(?)` 会被当成一个
/// 字面量列名）。
fn column_exists(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 幂等加列：列已在就跳过，不在就 `ALTER TABLE ... ADD COLUMN`。
///
/// 这是本文件唯一「绕过版本号」的写操作，理由见 [`ADDED_COLUMNS`] 的说明
/// （两条版本线撞号会让某些列永远补不上）。`table` / `column` / `decl` 都来自
/// 编译期常量，不拼外部输入。
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

/// 执行某个版本的 DDL。**新增版本时在这里加一个分支**，并在上面写好该版本的
/// 建表/改表语句常量。
///
/// 加列类版本（2/5/6/7/10/12/13）统一走 [`apply_added_columns`]：DDL 在
/// [`ADDED_COLUMNS`] 里，执行时按列核对（[`ensure_column`]）—— 撞号库里这些列
/// 可能已经在了，裸 ALTER 会以 `duplicate column name` 失败（这正是本次修的
/// 问题：fork 的 v11 已经带了 `credit`/`credits`，而官方 v10/v11 的列还没补）。
///
/// `_` 分支返回 Ok 而不是 unreachable!：版本号是外部输入（库文件可能来自
/// 更新的二进制，被旧版读到），越过 [`SCHEMA_VERSION`] 的版本不该 panic。
/// `migrate` 的主循环也不可能走到这里（循环上界就是 SCHEMA_VERSION），
/// 所以这个分支实际是「调用方传了越界的 version」的防御。
fn apply_version(conn: &Connection, version: i64) -> rusqlite::Result<()> {
    match version {
        1 => conn.execute_batch(V1_SCHEMA),
        // v3：request_raw 原始报文表（新表用 CREATE IF NOT EXISTS，见 V3_SCHEMA）
        3 => conn.execute_batch(V3_SCHEMA),
        // v4：重放 v3 建表（修复「版本号 3、表却不在」的存量库，见 SCHEMA_VERSION）
        4 => conn.execute_batch(V4_SCHEMA),
        // v8：account_usage_records 每账号余额记录表（取代旧 kv 快照，见 V8_SCHEMA）
        8 => conn.execute_batch(V8_SCHEMA),
        // v9：requests 补按账号聚合的索引（限制器的 Token 周期消耗聚合，见 V9_SCHEMA）
        9 => conn.execute_batch(V9_SCHEMA),
        // v11：checkin_records 签到记录明细台账（签到记录页的数据源，见 V11_SCHEMA）
        11 => conn.execute_batch(V11_SCHEMA),
        // v14：travel_records 旅行记录明细台账（旅行记录页的数据源，见 V14_SCHEMA）
        14 => conn.execute_batch(V14_SCHEMA),
        // v2 / v5 / v6 / v7 / v10 / v12 / v13：既有表加列，见 ADDED_COLUMNS
        2 | 5 | 6 | 7 | 10 | 12 | 13 => apply_added_columns(conn, version),
        _ => Ok(()),
    }
}

/// 执行「加列」类版本：把 [`ADDED_COLUMNS`] 里该版本的全部列按名补齐（幂等）。
fn apply_added_columns(conn: &Connection, version: i64) -> rusqlite::Result<()> {
    for &(v, table, column, decl) in ADDED_COLUMNS {
        if v == version {
            ensure_column(conn, table, column, decl)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    /// 建一个「fork 的 v11」库：v1 基础表 + fork 自己的 v10/v11（`requests.credit`
    /// 与 `request_daily.credits`），user_version = 11 —— 但它**缺**官方 v10/v11 的
    /// `requests.route` 与 `checkin_records` 表。
    ///
    /// 这正是线上那个库的形态：官方二进制读到 11 会跳过迁移（`route` 永远补不上，
    /// 报表 / 日志查询报 `no such column: route` 而整块空白），而合并版二进制会去
    /// 跑 v12 的 `ALTER ADD COLUMN credit` → duplicate column → 打开失败。
    fn build_fork_v11(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(V1_SCHEMA).unwrap();
        conn.execute_batch(
            "ALTER TABLE requests ADD COLUMN credit REAL NOT NULL DEFAULT 0;\
             ALTER TABLE request_daily ADD COLUMN credits REAL NOT NULL DEFAULT 0;",
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 11i64).unwrap();
    }

    fn columns(conn: &Connection, table: &str) -> Vec<String> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})")).unwrap();
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    fn table_exists(conn: &Connection, table: &str) -> bool {
        conn.query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |_| Ok(()),
        )
        .is_ok()
    }

    /// 用完删文件的临时库路径（本模块的用例各自独占一个标签）。
    fn temp_path(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "agent2api-schema-{label}-{}.db",
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

    #[test]
    fn repairs_fork_v11_db() {
        let path = temp_path("repair");
        build_fork_v11(&path);

        let conn = Connection::open(&path).unwrap();
        migrate(&conn).expect("fork v11 库应当能被修复升级");

        let cols = columns(&conn, "requests");
        assert!(cols.iter().any(|c| c == "route"), "route 应被补上：{cols:?}");
        assert!(cols.iter().any(|c| c == "credit"), "credit 应保留：{cols:?}");
        assert!(
            cols.iter().any(|c| c == "attempt_details"),
            "v2 列也应在：{cols:?}"
        );
        assert!(
            cols.iter().any(|c| c == "phase_started_at"),
            "v6 列也应在：{cols:?}"
        );
        assert!(
            table_exists(&conn, "checkin_records"),
            "checkin_records 表应被建出"
        );
        assert!(
            table_exists(&conn, "travel_records"),
            "travel_records 表应被建出"
        );
        let daily = columns(&conn, "request_daily");
        assert!(
            daily.iter().any(|c| c == "credits"),
            "credits 应保留：{daily:?}"
        );
        let uv: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(uv, SCHEMA_VERSION);
        drop(conn);
        remove_temp(&path);
    }

    /// 全新库按版本顺序推进仍得到同一份形态（顺序路径没被改坏）。
    #[test]
    fn fresh_db_reaches_target() {
        let path = temp_path("fresh");
        let conn = Connection::open(&path).unwrap();
        migrate(&conn).unwrap();

        let cols = columns(&conn, "requests");
        for expected in [
            "attempt_details",
            "sensitive_hits",
            "client_reasoning",
            "upstream_reasoning",
            "phase",
            "phase_started_at",
            "is_test",
            "route",
            "credit",
        ] {
            assert!(cols.iter().any(|c| c == expected), "{expected} 应在：{cols:?}");
        }
        assert!(columns(&conn, "request_daily").iter().any(|c| c == "credits"));
        assert!(table_exists(&conn, "checkin_records"));
        assert!(table_exists(&conn, "travel_records"));
        let uv: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(uv, SCHEMA_VERSION);
        drop(conn);
        remove_temp(&path);
    }

    /// 已是目标形态的库：再迁移一次不报错（reconcile 幂等）。
    #[test]
    fn migrate_is_idempotent() {
        let path = temp_path("idem");
        let conn = Connection::open(&path).unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).expect("第二次迁移应当是无操作");
        drop(conn);
        remove_temp(&path);
    }

    /// 版本号比本二进制新时不改库（回退场景），也不因缺列而报错。
    #[test]
    fn newer_db_is_left_untouched() {
        let path = temp_path("newer");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(V1_SCHEMA).unwrap();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 5)
            .unwrap();
        migrate(&conn).expect("更新的库应当被原样放过");

        // 版本号不变，且没有把本二进制的列塞进去（说明确实没动库）。
        let uv: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(uv, SCHEMA_VERSION + 5);
        assert!(!columns(&conn, "requests").iter().any(|c| c == "route"));
        drop(conn);
        remove_temp(&path);
    }
}
