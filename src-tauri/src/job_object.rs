//! Windows Job Object：让 PTY 里派生出的整棵进程树跟着会话一起死。
//!
//! 背景：ConPTY 起的 shell（比如 `pwsh`/`cmd`）自己又会 spawn 子进程（比如用户在
//! 终端里跑的 `python xxx.py`）。`portable_pty::Child::kill()` 只 TerminateProcess
//! 顶层的 shell，不会波及它的子孙进程——Windows 没有 POSIX 那种"进程组"概念，
//! 子进程杀死父进程后会被 init 收养，在 Windows 上则是直接被孤立，继续跑。
//! 结果就是关掉 Code Bar 之后，Task Manager 里还挂着几个孤儿 python.exe。
//!
//! 解法：每个 PTY 会话开一个 Job Object，把 shell 进程扔进去，并设置
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`。这样只要满足以下任一条件，
//! 整棵进程树（shell 及其派生的所有子进程，只要没自己脱离 Job）会被内核一次杀光：
//!   1. 显式调用 `TerminateJobObject`（用户在前端点击关闭某个终端标签）
//!   2. Job Object 的最后一个句柄被关闭（我们把句柄保存在 AppHandle 状态里，
//!      应用退出、状态被清空时 Rust 自动 drop，触发这条)
//!
//! 仅 Windows 有效；macOS/Linux 上 PTY 本身通过 setsid() 建立了独立进程组，
//! `pty.rs` 里对应平台的 kill 逻辑已经用 killpg 处理了整组，不需要这个模块。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};

macro_rules! job_log {
    ($($arg:tt)*) => {{
        if std::env::var_os("CODE_BAR_POPUP_LOG").is_some() {
            eprintln!($($arg)*);
        }
    }};
}

/// 包一层让裸指针能跨线程存放。Job handle 只在这里创建/关闭，不做并发写，
/// SAFETY 由外部的 Mutex<HashMap<..>> 保证互斥访问。
///
/// pub(crate) 而非私有：`JobObjectMap`（HashMap 的值类型）要通过 tauri 的
/// `app.manage()` / `app.state::<T>()` 暴露到 lib.rs、pty.rs 里，
/// 这些调用点必须能对完整类型（含 JobHandle）具名，否则报 "private type"。
pub(crate) struct JobHandle(HANDLE);

// HANDLE 是内核对象句柄，本身没有线程亲和性，跨线程使用是 Win32 API 的常规用法。
unsafe impl Send for JobHandle {}

impl Drop for JobHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                // 不主动 TerminateJobObject：句柄计数归零时，
                // 若已设置 KILL_ON_JOB_CLOSE，内核会自动杀光 Job 里的进程。
                CloseHandle(self.0);
            }
        }
    }
}

/// session_id → Job Object 句柄
pub type JobObjectMap = Arc<Mutex<HashMap<String, JobHandle>>>;

pub fn new_map() -> JobObjectMap {
    Arc::new(Mutex::new(HashMap::new()))
}

/// 为一个 PTY 会话创建 Job Object，并把给定 pid 的进程塞进去。
/// 失败时只记录日志、不返回 Err：Job Object 是"锦上添花"的兜底清理，
/// 缺失时不应该阻止 PTY 会话本身正常工作。
pub fn attach(map: &JobObjectMap, session_id: &str, pid: u32) {
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            job_log!(
                "[job_object] CreateJobObjectW 失败 session={session_id} pid={pid}"
            );
            return;
        }

        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if ok == 0 {
            job_log!(
                "[job_object] SetInformationJobObject 失败 session={session_id} pid={pid}"
            );
            CloseHandle(job);
            return;
        }

        // 打开目标进程句柄：需要 PROCESS_SET_QUOTA（加入 Job 用）+ PROCESS_TERMINATE
        // （KILL_ON_JOB_CLOSE 触发时内核需要这个权限来结束进程）。
        let proc_handle = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
        if proc_handle.is_null() {
            job_log!(
                "[job_object] OpenProcess 失败 session={session_id} pid={pid}"
            );
            CloseHandle(job);
            return;
        }

        let assigned = AssignProcessToJobObject(job, proc_handle);
        CloseHandle(proc_handle);
        if assigned == 0 {
            // 常见原因：目标进程已经在另一个不兼容的 Job 里（比如被某些沙箱/
            // 容器工具包裹）。这种情况下放弃兜底清理，但不影响会话本身。
            job_log!(
                "[job_object] AssignProcessToJobObject 失败 session={session_id} pid={pid}"
            );
            CloseHandle(job);
            return;
        }

        let mut guard = map.lock().unwrap_or_else(|e| e.into_inner());
        guard.insert(session_id.to_string(), JobHandle(job));
    }
}

/// 立即终结某个会话 Job 里的整棵进程树（用户主动关闭该终端标签时调用）。
/// 幂等：会话不存在时什么也不做。
pub fn terminate(map: &JobObjectMap, session_id: &str) {
    let handle = {
        let mut guard = map.lock().unwrap_or_else(|e| e.into_inner());
        guard.remove(session_id)
    };
    if let Some(h) = handle {
        unsafe {
            TerminateJobObject(h.0, 1);
        }
        // h 在这里 drop，CloseHandle 会被调用（TerminateJobObject 之后关闭是安全的）。
    }
}

/// 应用退出时调用：终结所有仍存活会话的进程树。
/// 逐个 TerminateJobObject 而不是依赖 Drop 顺序，确保在进程真正退出前
/// 所有子进程都已经被杀死（否则若 AppHandle 状态被提前 drop 的时机不确定，
/// 可能出现应用主进程已退出、清理还没跑完的竞态)。
pub fn terminate_all(map: &JobObjectMap) {
    let mut guard = map.lock().unwrap_or_else(|e| e.into_inner());
    for (session_id, handle) in guard.drain() {
        unsafe {
            TerminateJobObject(handle.0, 1);
        }
        job_log!("[job_object] terminate_all 清理 session={session_id}");
        // handle 在这里 drop → CloseHandle。
    }
}
