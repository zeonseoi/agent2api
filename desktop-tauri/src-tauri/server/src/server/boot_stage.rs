//! 启动阶段计时（fork 定制）。
//!
//! ── 为什么需要它 ──────────────────────────────────────────
//! 进程内网关的启动日志原本只有「启动中…」与「服务已就绪」两条，中间任何一步
//! 卡住（打开数据库、预热模型目录、账号迁移）都只有一个无限转圈的「启动中」，
//! 排障时无法判断卡在哪个阶段。本模块在每个阶段边界记一行耗时，并在外壳判定
//! 启动超时时把**当前阶段名**带进失败文案。
//!
//! ── 日志库尚未就绪时先缓存 ─────────────────────────────────
//! `logging::init_store` 在 bootstrap 中段才执行，之前的时间点写不进日志库。
//! 所以 `stage` 在日志库初始化前只把结果缓存在内存，由 `flush` 在初始化后
//! 一次性补写；之后的时间点直接写库。控制台（stderr）在两种形态下都可能没有，
//! 不作为可靠通道。
//!
//! ── 取消语义 ──────────────────────────────────────────────
//! 外壳（`backend::ensure_ready`）对 bootstrap 设了硬超时。超时后调用 `cancel`，
//! 本模块在下一个阶段边界返回 Err，让 bootstrap 尽早收尾 —— 它无法打断正卡在
//! 某一步里的阻塞调用，只能阻止后续阶段继续产生副作用。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use crate::server::logging;

const TAG: &str = "[Startup]";

struct BootStage {
    started: Instant,
    last: Instant,
    current: String,
    /// 日志库就绪前缓冲的阶段结果：`(阶段名, 本阶段耗时 ms, 累计耗时 ms)`
    buffered: Vec<(String, u128, u128)>,
    flushed: bool,
}

static STATE: OnceLock<Mutex<BootStage>> = OnceLock::new();
static CANCELLED: AtomicBool = AtomicBool::new(false);

fn state() -> &'static Mutex<BootStage> {
    STATE.get_or_init(|| {
        let now = Instant::now();
        Mutex::new(BootStage {
            started: now,
            last: now,
            current: "初始化".to_string(),
            buffered: Vec::new(),
            flushed: false,
        })
    })
}

/// 开始一次启动计时（bootstrap 入口调用）。
pub fn begin() {
    CANCELLED.store(false, Ordering::SeqCst);
    let now = Instant::now();
    if let Ok(mut guard) = state().lock() {
        *guard = BootStage {
            started: now,
            last: now,
            current: "初始化".to_string(),
            buffered: Vec::new(),
            flushed: false,
        };
    }
}

/// 记录一个阶段完成。返回 Err 表示启动已被外壳判定超时，调用方应尽快返回。
pub fn stage(name: &str) -> Result<(), String> {
    let now = Instant::now();
    let line = {
        let Ok(mut guard) = state().lock() else {
            return Ok(());
        };
        let elapsed = now.duration_since(guard.last).as_millis();
        let total = now.duration_since(guard.started).as_millis();
        guard.last = now;
        guard.current = name.to_string();
        let line = format!("阶段「{name}」完成：用时 {elapsed} ms（累计 {total} ms）");
        if guard.flushed {
            Some(line)
        } else {
            guard.buffered.push((name.to_string(), elapsed, total));
            None
        }
    };
    // 日志写入放在锁外：写库可能很慢，不该让计时锁跟着一起被拖住
    if let Some(line) = line {
        logging::log(TAG, &line);
    }
    if CANCELLED.load(Ordering::SeqCst) {
        return Err("启动已被判定超时，停止后续初始化".to_string());
    }
    Ok(())
}

/// 日志库初始化后调用：把缓冲的阶段记录补写进库，并切换为直接写库。
pub fn flush() {
    let buffered = {
        let Ok(mut guard) = state().lock() else {
            return;
        };
        guard.flushed = true;
        std::mem::take(&mut guard.buffered)
    };
    for (name, elapsed, total) in buffered {
        logging::log(
            TAG,
            &format!("阶段「{name}」完成：用时 {elapsed} ms（累计 {total} ms）"),
        );
    }
}

/// 当前阶段名（超时文案用；取不到时返回「未知」）。
pub fn current() -> String {
    state()
        .lock()
        .map(|guard| guard.current.clone())
        .unwrap_or_else(|_| "未知".to_string())
}

/// 从 `begin` 到现在的总耗时（毫秒）。
pub fn total_ms() -> u128 {
    state()
        .lock()
        .map(|guard| guard.started.elapsed().as_millis())
        .unwrap_or(0)
}

/// 外壳判定启动超时：阻止 bootstrap 继续执行后续阶段。
pub fn cancel() {
    CANCELLED.store(true, Ordering::SeqCst);
}
