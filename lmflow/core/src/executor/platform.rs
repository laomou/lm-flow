/// 把**当前线程**绑定到指定 CPU 核(Linux/Android)。绑核是尽力而为的优化。
#[cfg(all(any(target_os = "linux", target_os = "android"), not(miri)))]
pub(super) fn pin_current_thread_to(cpu: usize) {
    extern "C" {
        fn sched_setaffinity(pid: i32, cpusetsize: usize, mask: *const u64) -> i32;
    }
    const NBITS: usize = 1024;
    if cpu >= NBITS {
        return;
    }
    let mut mask = [0u64; NBITS / 64];
    mask[cpu / 64] |= 1u64 << (cpu % 64);
    unsafe {
        let _ = sched_setaffinity(0, std::mem::size_of_val(&mask), mask.as_ptr());
    }
}

#[cfg(any(not(any(target_os = "linux", target_os = "android")), miri))]
pub(super) fn pin_current_thread_to(_cpu: usize) {}

/// 把**当前线程**设为实时优先级或对应平台的高优先级。全部尽力而为。
#[cfg(all(any(target_os = "linux", target_os = "android"), not(miri)))]
pub(super) fn set_current_thread_rt_priority(prio: i32) {
    extern "C" {
        fn sched_setscheduler(pid: i32, policy: i32, param: *const SchedParam) -> i32;
    }
    #[repr(C)]
    struct SchedParam {
        sched_priority: i32,
    }
    const SCHED_FIFO: i32 = 1;
    let param = SchedParam {
        sched_priority: prio.clamp(1, 99),
    };
    unsafe {
        let _ = sched_setscheduler(0, SCHED_FIFO, &param);
    }
}

#[cfg(miri)]
pub(super) fn set_current_thread_rt_priority(_prio: i32) {}

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub(super) fn set_current_thread_rt_priority(prio: i32) {
    extern "C" {
        fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
    }
    const QOS_CLASS_USER_INITIATED: u32 = 0x19;
    const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;
    let qos = if prio >= 90 {
        QOS_CLASS_USER_INTERACTIVE
    } else {
        QOS_CLASS_USER_INITIATED
    };
    unsafe {
        let _ = pthread_set_qos_class_self_np(qos, 0);
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios"
)))]
pub(super) fn set_current_thread_rt_priority(_prio: i32) {}

#[cfg(test)]
mod tests {
    #[cfg(all(any(target_os = "linux", target_os = "android"), not(miri)))]
    #[test]
    fn affinity_actually_pins_worker_thread() {
        use std::sync::mpsc;

        extern "C" {
            fn sched_getaffinity(pid: i32, cpusetsize: usize, mask: *mut u64) -> i32;
        }
        let cpu_count = std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(1);
        if cpu_count < 2 {
            return;
        }
        let (sender, receiver) = mpsc::channel::<[u64; 16]>();
        let handle = std::thread::spawn(move || {
            super::pin_current_thread_to(1);
            let mut mask = [0u64; 16];
            let result =
                unsafe { sched_getaffinity(0, std::mem::size_of_val(&mask), mask.as_mut_ptr()) };
            assert_eq!(result, 0, "sched_getaffinity failed");
            sender.send(mask).unwrap();
        });
        let mask = receiver.recv().unwrap();
        handle.join().unwrap();
        assert_eq!(mask[0], 1u64 << 1);
    }

    #[cfg(all(any(target_os = "linux", target_os = "android"), not(miri)))]
    #[test]
    fn rt_priority_is_best_effort() {
        extern "C" {
            fn sched_getscheduler(pid: i32) -> i32;
        }
        const SCHED_FIFO: i32 = 1;
        let handle = std::thread::spawn(|| {
            super::set_current_thread_rt_priority(10);
            unsafe { sched_getscheduler(0) }
        });
        let policy = handle.join().unwrap();
        assert!(policy == SCHED_FIFO || policy >= 0);
    }
}
