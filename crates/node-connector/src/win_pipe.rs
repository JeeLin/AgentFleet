//! Windows 匿名管道可读性探测的**唯一**实现。
//!
//! `connector::exec_windows` 与 `pty::windows` 都要在 `ReadFile` 之前判
//! 「这根管道现在能不能读」。两侧共用这一份判据：任何一侧单独增删错误码，
//! 都会让写端全关时的 EOF 收敛失效（`any_open()` 不收敛 / `ReadFile`
//! 永久阻塞，v0.7.2 的 53m51s 挂死即由此而来）。
//!
//! 单独成文件、且整块 `#[cfg(windows)]`，是为了不把真 Win32 FFI 塞进
//! `win_command` 那段「平台无关、零 `unsafe`、Linux 上能跑单测」的纯逻辑。

use std::ptr;

use windows_sys::Win32::Foundation::{
    GetLastError, ERROR_BROKEN_PIPE, ERROR_PIPE_NOT_CONNECTED, HANDLE,
};
use windows_sys::Win32::System::Pipes::PeekNamedPipe;

/// 零阻塞地判「这根匿名管道现在能不能读」。
///
/// `PeekNamedPipe` 查到有字节才算可读；它返回 FALSE 时只有
/// `ERROR_BROKEN_PIPE` / `ERROR_PIPE_NOT_CONNECTED`（所有写端已关）也算可读，
/// 让调用方随后的 `ReadFile` 拿到同一条错误码并摘掉读端。其余失败按
/// 「暂时没字节」处理，由调用方的轮询节奏决定下一次探测。
pub(crate) fn pipe_is_readable(handle: HANDLE) -> bool {
    let mut available: u32 = 0;
    let ok = unsafe {
        PeekNamedPipe(
            handle,
            ptr::null_mut(),
            0,
            ptr::null_mut(),
            &mut available,
            ptr::null_mut(),
        )
    };
    if ok != 0 {
        return available > 0;
    }
    matches!(
        unsafe { GetLastError() },
        ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED
    )
}
