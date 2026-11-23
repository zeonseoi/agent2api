//! 网关无响应看门狗（就绪后启动）。
//!
//! 用 std::thread + 裸 TCP 而不是 tokio 任务：要监视的正是「运行时卡住」本身，
//! 建在同一运行时上的任务会与工作负载一起沉默。探活失败连续 3 次后把
//! `BackendHandle::unresponsive` 置位，界面据此提供重启程序入口。

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use tauri::{AppHandle, Manager};

use crate::server;
use crate::state::AppState;

/// 看门狗节奏：每 30 秒探一次 /health，连续 3 次失败（约 90 秒）判定无响应。
const WATCHDOG_INTERVAL: Duration = Duration::from_secs(30);
const WATCHDOG_TIMEOUT: Duration = Duration::from_secs(3);
const WATCHDOG_FAILURES: u32 = 3;

pub fn start_watchdog(app: AppHandle) {
    let _ = std::thread::Builder::new()
        .name("gateway-watchdog".to_string())
        .spawn(move || {
            let mut failures = 0u32;
            let mut reported = false;
            loop {
                std::thread::sleep(WATCHDOG_INTERVAL);
                let state = app.state::<AppState>();
                // 关闭流程一开始就退出：进程即将结束，继续探活会把「正在退出」误报成「无响应」。
                let Some(port) = super::lifecycle::running_port(&state.backend) else { return; };
                if health_probe(port) {
                    failures = 0;
                    if reported {
                        reported = false;
                        if let Ok(mut guard) = state.backend.lock() {
                            guard.unresponsive = false;
                        }
                        server::logging::console_line("[Watchdog]", "网关已恢复响应");
                    }
                    continue;
                }
                failures += 1;
                if failures >= WATCHDOG_FAILURES && !reported {
                    reported = true;
                    if let Ok(mut guard) = state.backend.lock() {
                        guard.unresponsive = true;
                    }
                    server::logging::console_line(
                        "[Watchdog]",
                        &format!(
                            "网关连续 {failures} 次健康检查失败（端口 {port}），判定无响应；可在侧栏重启程序"
                        ),
                    );
                }
            }
        });
}

/// 一次最小健康探针：只读 HTTP 状态行，不依赖任何第三方客户端。
fn health_probe(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, WATCHDOG_TIMEOUT) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(WATCHDOG_TIMEOUT));
    let _ = stream.set_write_timeout(Some(WATCHDOG_TIMEOUT));
    if stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut response = Vec::with_capacity(256);
    let mut chunk = [0u8; 128];
    while response.len() < 1024 {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => {
                response.extend_from_slice(&chunk[..read]);
                if response.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let head = String::from_utf8_lossy(&response);
    let Some(status) = head.lines().next() else {
        return false;
    };
    let mut parts = status.split_whitespace();
    let _protocol = parts.next();
    parts.next().is_some_and(|code| code.starts_with('2'))
}