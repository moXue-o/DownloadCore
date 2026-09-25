//! 系统层面的辅助：提升进程优先级、关闭 Windows 的省电节流（EcoQoS）。
//!
//! 目的：抢网时让本进程在 CPU 调度上更早被服务（及时收数据、回 ACK、建连接），
//! 并避免被系统按"后台/省电"降速。用 HIGH（不是 REALTIME），安全、无需管理员。

/// 提升当前进程优先级并关闭执行速度节流。返回是否至少成功提升优先级。
#[cfg(windows)]
pub fn raise_process_priority() -> bool {
    use core::ffi::c_void;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn SetPriorityClass(h: *mut c_void, class: u32) -> i32;
        fn SetProcessInformation(h: *mut c_void, class: i32, info: *mut c_void, size: u32) -> i32;
    }

    const HIGH_PRIORITY_CLASS: u32 = 0x0000_0080;
    const PROCESS_POWER_THROTTLING: i32 = 4;
    const PROCESS_POWER_THROTTLING_EXECUTION_SPEED: u32 = 0x1;

    #[repr(C)]
    struct PowerThrottlingState {
        version: u32,
        control_mask: u32,
        state_mask: u32,
    }

    let mut raised = false;
    unsafe {
        let h = GetCurrentProcess();
        if SetPriorityClass(h, HIGH_PRIORITY_CLASS) != 0 {
            raised = true;
        }
        // StateMask 的 EXECUTION_SPEED 位为 0 = 不节流
        let mut st = PowerThrottlingState {
            version: 1,
            control_mask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            state_mask: 0,
        };
        let _ = SetProcessInformation(
            h,
            PROCESS_POWER_THROTTLING,
            &mut st as *mut _ as *mut c_void,
            core::mem::size_of::<PowerThrottlingState>() as u32,
        );
    }
    raised
}

#[cfg(not(windows))]
pub fn raise_process_priority() -> bool {
    false
}

/// 把当前线程提到"最高"优先级（在进程级基础上再加一点）。
#[cfg(windows)]
pub fn promote_current_thread() {
    use core::ffi::c_void;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentThread() -> *mut c_void;
        fn SetThreadPriority(h: *mut c_void, prio: i32) -> i32;
    }
    const THREAD_PRIORITY_HIGHEST: i32 = 2;
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
    }
}

#[cfg(not(windows))]
pub fn promote_current_thread() {}
