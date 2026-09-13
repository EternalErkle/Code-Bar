use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use tauri::{Emitter, Manager};

use crate::{
    cli_detect::resolve_command_path,
    state::{PtyKillerMap, PtyMasterMap, PtySessionMeta, PtySessionMetaMap, PtyWriterMap},
    util::{expand_path, home_dir, resolve_windows_pty_command},
};

/// PTY 启动代次计数器。全局唯一且只增不减，因此代次绝不会被复用。
static PTY_GENERATION: AtomicU64 = AtomicU64::new(1);

/// 获取锁并在中毒时恢复。
///
/// PTY 命令运行在主线程上：任何一次 panic 都会毒化这些 Mutex，
/// 之后每一条 PTY 命令都会跟着 panic，最终整个进程被终止。
/// 这些 map 里没有需要保护的跨字段不变量，恢复内部值是安全的。
fn lock_or_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 每个会话独立的数据事件名。
///
/// Tauri 的 emit 是广播：用统一事件名时，每条输出都会投递给所有已挂载的终端，
/// 开销随会话数线性放大。事件名只保留 Tauri 允许的字符集。
pub fn pty_data_event(session_id: &str) -> String {
    let safe: String = session_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("pty-data:{safe}")
}

// ── 辅助：从 AppHandle 获取 PTY 状态 ─────────────────────────────

fn pty_writer_map(app: &tauri::AppHandle) -> PtyWriterMap {
    app.state::<PtyWriterMap>().inner().clone()
}

fn pty_killer_map(app: &tauri::AppHandle) -> PtyKillerMap {
    app.state::<PtyKillerMap>().inner().clone()
}

fn pty_master_map(app: &tauri::AppHandle) -> PtyMasterMap {
    app.state::<PtyMasterMap>().inner().clone()
}

fn pty_session_meta_map(app: &tauri::AppHandle) -> PtySessionMetaMap {
    app.state::<PtySessionMetaMap>().inner().clone()
}

// ── PTY 输出状态机 ────────────────────────────────────────────────

/// PTY 输出的可见文字状态，用于检测 CLI 等待/运行状态
struct AnsiStripper {
    state: u8, // 0=normal, 1=ESC, 2=CSI
    window: Vec<u8>,
}

impl AnsiStripper {
    fn new() -> Self {
        Self {
            state: 0,
            window: Vec::with_capacity(256),
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            match self.state {
                0 => {
                    if byte == 0x1b {
                        self.state = 1;
                    } else if byte >= 0x20 || byte == b'\r' || byte == b'\n' {
                        self.window.push(byte);
                        if self.window.len() > 256 {
                            self.window.drain(..128);
                        }
                    }
                }
                1 => {
                    self.state = if byte == b'[' { 2 } else { 0 };
                }
                2 => {
                    if byte >= 0x40 && byte <= 0x7e {
                        self.state = 0;
                    }
                }
                _ => {
                    self.state = 0;
                }
            }
        }
    }

    fn visible(&self) -> &[u8] {
        &self.window
    }

    fn clear(&mut self) {
        self.window.clear();
    }
}

// ── Tauri Commands ────────────────────────────────────────────────

/// 启动 PTY 会话
#[tauri::command]
pub async fn start_pty_session(
    app: tauri::AppHandle,
    session_id: String,
    workdir: String,
    command: String,
    args: Vec<String>,
    cols: u16,
    rows: u16,
    env: Option<Vec<(String, String)>>,
) -> Result<(), String> {
    use base64::Engine;
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};

    let expanded = expand_path(&workdir);
    let runner_type = env
        .as_ref()
        .and_then(|pairs| {
            pairs
                .iter()
                .find(|(k, _)| k == "CODE_BAR_RUNNER_TYPE")
                .map(|(_, v)| v.clone())
        })
        .unwrap_or_else(|| {
            std::path::Path::new(&command)
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| match name {
                    "claude" => "claude-code",
                    "codex" => "codex",
                    other => other,
                })
                .unwrap_or_default()
                .to_string()
        });

    // 先停掉同 session 的旧 PTY。
    // kill 之后必须 wait 才能回收进程，但 wait 可能阻塞，
    // 因此丢给独立线程，不占用命令线程也不持锁。
    {
        // 同样遵守 meta → killer 的锁序
        let meta_map_arc = pty_session_meta_map(&app);
        let old = {
            let _meta_guard = lock_or_recover(&meta_map_arc);
            let km = pty_killer_map(&app);
            let mut km = lock_or_recover(&km);
            km.remove(&session_id)
        };
        if let Some(mut old) = old {
            std::thread::spawn(move || {
                let _ = old.kill();
                let _ = old.wait();
            });
        }
    }

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("openpty 失败: {e}"))?;

    let resolved_command = resolve_command_path(&command);
    let (launch_command, launch_args) = resolve_windows_pty_command(&resolved_command, &args);

    let mut cmd = if cfg!(windows)
        && std::path::Path::new(&launch_command)
            .extension()
            .and_then(|s| s.to_str())
            .map(|ext| matches!(ext.to_ascii_lowercase().as_str(), "cmd" | "bat"))
            .unwrap_or(false)
    {
        let mut builder = CommandBuilder::new("cmd.exe");
        builder.arg("/d");
        builder.arg("/c");
        builder.arg(&launch_command);
        for arg in &launch_args {
            builder.arg(arg);
        }
        builder
    } else {
        let mut builder = CommandBuilder::new(&launch_command);
        for arg in &launch_args {
            builder.arg(arg);
        }
        builder
    };
    cmd.cwd(&expanded);

    // 继承基础环境变量
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    let codebar_tmp = crate::util::codebar_tmp_dir()
        .to_string_lossy()
        .to_string();
    cmd.env("TMPDIR", &codebar_tmp);
    cmd.env("TEMP", &codebar_tmp);
    cmd.env("TMP", &codebar_tmp);
    if let Some(home) = home_dir() {
        cmd.env("HOME", home.to_string_lossy().to_string());
        #[cfg(windows)]
        cmd.env("USERPROFILE", home.to_string_lossy().to_string());
    }

    // 构建子进程 PATH（补充 node 所在目录，供 claude/codex 等 Node.js 脚本使用）
    {
        let base_path = std::env::var("PATH").unwrap_or_default();
        let node_path = resolve_command_path("node");
        let node_dir = if std::path::Path::new(&node_path).parent().is_some() {
            std::path::Path::new(&node_path)
                .parent()
                .map(|d| d.to_string_lossy().to_string())
        } else {
            None
        };
        let sep = if cfg!(windows) { ';' } else { ':' };
        let enriched_path = match node_dir {
            Some(dir) if !base_path.split(sep).any(|s| s == dir) => {
                format!("{dir}{sep}{base_path}")
            }
            _ => base_path,
        };
        cmd.env("PATH", enriched_path);
    }

    // 注入调用方传入的额外环境变量
    if let Some(extra_env) = env {
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
    }

    // hook 鉴权令牌。必须排在调用方环境变量之后注入，这样前端即使传入同名变量
    // 也无法覆盖真实令牌；前端自始至终不接触该值，避免 webview 被注入后泄露。
    // 生成失败时不注入：监听端会因此拒绝全部请求（fail closed）。
    if let Some(token) = crate::hook_auth::run_token() {
        cmd.env(crate::hook_auth::HOOK_TOKEN_ENV, token);
    }

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("spawn 失败: {e}"))?;

    let mut master_reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| format!("clone_reader 失败: {e}"))?;

    let master_writer = pair
        .master
        .take_writer()
        .map_err(|e| format!("take_writer 失败: {e}"))?;

    // 本次启动的代次，全局唯一
    let generation = PTY_GENERATION.fetch_add(1, Ordering::SeqCst);

    // 注册四张表时全程持有 meta 锁。
    //
    // meta 锁在这里充当「会话注册表」的总闸：退出清理同样先拿 meta 再动其它表，
    // 于是「校验代次 + 摘除条目」和「注册新会话」互斥。
    // 否则旧读取线程可能在校验之后、注册完成之前把新会话的 writer/master 摘掉。
    //
    // 锁序固定为 meta → killer → writer → master，所有多锁路径都必须遵守。
    {
        let meta_map_arc = pty_session_meta_map(&app);
        let mut meta_map = lock_or_recover(&meta_map_arc);
        meta_map.insert(
            session_id.clone(),
            PtySessionMeta {
                runner_type,
                workdir: expanded.clone(),
                generation,
            },
        );

        {
            let km = pty_killer_map(&app);
            let mut km = lock_or_recover(&km);
            km.insert(session_id.clone(), child);
        }
        {
            let wm = pty_writer_map(&app);
            let mut wm = lock_or_recover(&wm);
            wm.insert(session_id.clone(), master_writer);
        }
        {
            let mm = pty_master_map(&app);
            let mut mm = lock_or_recover(&mm);
            mm.insert(session_id.clone(), pair.master);
        }
    }

    // 输出管线分成两级：
    //   读取线程 —— 只做阻塞 read，尽快把数据交给通道，避免 PTY 缓冲区回压；
    //   派发线程 —— 在一个短窗口内合并多次读取，再一次性 emit。
    // 合并是关键：CLI 重绘满屏时每秒产生上百次小块读取，
    // 逐块 emit 会让事件分发和 webview 重绘成为瓶颈。
    let (chunk_tx, chunk_rx) = std::sync::mpsc::channel::<Vec<u8>>();

    std::thread::spawn(move || {
        let mut buf = [0u8; 65536];
        loop {
            match master_reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if chunk_tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let app_r = app.clone();
    let sid_r = session_id.clone();
    let data_event = pty_data_event(&session_id);
    let writer_map_r = pty_writer_map(&app);
    let master_map_r = pty_master_map(&app);
    let killer_map_r = pty_killer_map(&app);
    let session_meta_map_r = pty_session_meta_map(&app);
    std::thread::spawn(move || {
        const FLUSH_WINDOW: std::time::Duration = std::time::Duration::from_millis(8);
        const MAX_BATCH: usize = 512 * 1024;

        let mut stripper = AnsiStripper::new();
        // 0 = unknown, 1 = running, 2 = waiting
        let mut last_status: u8 = 0;

        loop {
            let Ok(first) = chunk_rx.recv() else {
                break;
            };
            let mut batch = first;

            let deadline = std::time::Instant::now() + FLUSH_WINDOW;
            while batch.len() < MAX_BATCH {
                let now = std::time::Instant::now();
                if now >= deadline {
                    break;
                }
                match chunk_rx.recv_timeout(deadline - now) {
                    Ok(next) => batch.extend_from_slice(&next),
                    Err(_) => break,
                }
            }

            let b64 = base64::engine::general_purpose::STANDARD.encode(&batch);
            let _ = app_r.emit(
                &data_event,
                serde_json::json!({ "session_id": sid_r, "data": b64 }),
            );

            // 检测 CLI 状态特征
            stripper.feed(&batch);
            let win_str = String::from_utf8_lossy(stripper.visible());
            let new_status = if win_str.contains("? for shortcuts") {
                2u8 // waiting
            } else if win_str.contains("esc to interrupt") {
                1u8 // running
            } else {
                0u8
            };

            if new_status != 0 && new_status != last_status {
                last_status = new_status;
                stripper.clear();
                let event = if new_status == 2 {
                    "pty-waiting"
                } else {
                    "pty-running"
                };
                let _ = app_r.emit(event, serde_json::json!({ "session_id": sid_r }));
            }
        }

        // 只有仍是当前代次才清理并上报退出。
        // 否则说明同 id 的新会话已经接管，这些条目不属于本线程 ——
        // 旧代码在这里会 reap 掉新会话的子进程，并在持锁状态下 wait()，
        // 结果是所有 PTY 命令一起卡住。
        //
        // 校验和摘除必须在同一个 meta 锁区间内完成，否则两者之间
        // 仍可能插进一次新会话注册，导致新会话的条目被误删。
        let child = {
            let mut meta_map = lock_or_recover(&session_meta_map_r);
            let is_current = meta_map
                .get(&sid_r)
                .map(|meta| meta.generation == generation)
                .unwrap_or(false);
            if !is_current {
                return;
            }
            meta_map.remove(&sid_r);

            let child = {
                let mut km = lock_or_recover(&killer_map_r);
                km.remove(&sid_r)
            };
            // writer / master 过去从不清理，会随会话数持续泄漏
            {
                let mut wm = lock_or_recover(&writer_map_r);
                wm.remove(&sid_r);
            }
            {
                let mut mm = lock_or_recover(&master_map_r);
                mm.remove(&sid_r);
            }
            child
        };

        // 锁已释放后才 wait()：持锁 wait 会把所有 PTY 命令堵死
        if let Some(mut child) = child {
            let _ = child.wait();
        }

        let _ = app_r.emit("pty-exit", serde_json::json!({ "session_id": sid_r }));
    });

    Ok(())
}

/// 向 PTY 写入数据（键盘输入，base64 编码）
///
/// 必须是 async + spawn_blocking：CLI 忙于处理、没有读取 stdin 时，
/// write_all 会阻塞。同步命令跑在主线程上，一旦阻塞界面就会「未响应」。
#[tauri::command]
pub async fn write_pty(
    app: tauri::AppHandle,
    session_id: String,
    data: String,
) -> Result<(), String> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&data)
        .map_err(|e| format!("base64 decode 失败: {e}"))?;
    let wm = pty_writer_map(&app);
    tokio::task::spawn_blocking(move || {
        let mut wm = lock_or_recover(&wm);
        if let Some(writer) = wm.get_mut(&session_id) {
            writer
                .write_all(&bytes)
                .map_err(|e| format!("write 失败: {e}"))?;
        }
        Ok::<(), String>(())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 向 PTY 写入一行文本（附加换行触发执行）
#[tauri::command]
pub async fn send_pty_query(
    app: tauri::AppHandle,
    session_id: String,
    query: String,
) -> Result<(), String> {
    let wm = pty_writer_map(&app);
    tokio::task::spawn_blocking(move || {
        let mut wm = lock_or_recover(&wm);
        if let Some(writer) = wm.get_mut(&session_id) {
            let mut data = query.into_bytes();
            data.push(if cfg!(windows) { b'\r' } else { b'\n' });
            writer
                .write_all(&data)
                .map_err(|e| format!("send_pty_query write 失败: {e}"))?;
            writer
                .flush()
                .map_err(|e| format!("send_pty_query flush 失败: {e}"))?;
            Ok(())
        } else {
            Err(format!("PTY session '{session_id}' 不存在或尚未就绪"))
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 调整 PTY 大小（cols/rows 至少为 20/5，防止 SIGWINCH 异常）
#[tauri::command]
pub async fn resize_pty(
    app: tauri::AppHandle,
    session_id: String,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    use portable_pty::PtySize;
    let cols = cols.max(20);
    let rows = rows.max(5);
    let mm = pty_master_map(&app);
    tokio::task::spawn_blocking(move || {
        let mm = lock_or_recover(&mm);
        if let Some(master) = mm.get(&session_id) {
            master
                .resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(|e| format!("resize_pty 失败: {e}"))?;
        }
        Ok::<(), String>(())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 停止 PTY 会话
#[tauri::command]
pub fn stop_pty_session(app: tauri::AppHandle, session_id: String) -> Result<(), String> {
    let mut had_session = false;

    // 锁序与注册/清理路径保持一致：meta → killer → writer → master
    let child = {
        let meta_map_arc = pty_session_meta_map(&app);
        let mut meta_map = lock_or_recover(&meta_map_arc);
        if meta_map.remove(&session_id).is_some() {
            had_session = true;
        }

        let child = {
            let km = pty_killer_map(&app);
            let mut km = lock_or_recover(&km);
            km.remove(&session_id)
        };
        if child.is_some() {
            had_session = true;
        }
        {
            let wm = pty_writer_map(&app);
            let mut wm = lock_or_recover(&wm);
            if wm.remove(&session_id).is_some() {
                had_session = true;
            }
        }
        {
            let mm = pty_master_map(&app);
            let mut mm = lock_or_recover(&mm);
            if mm.remove(&session_id).is_some() {
                had_session = true;
            }
        }
        child
    };

    // kill 之后必须 wait 才能回收，但两者都可能阻塞：交给独立线程
    if let Some(mut child) = child {
        std::thread::spawn(move || {
            let _ = child.kill();
            let _ = child.wait();
        });
    }
    if had_session {
        let _ = app.emit("pty-exit", serde_json::json!({ "session_id": session_id }));
    }
    Ok(())
}
