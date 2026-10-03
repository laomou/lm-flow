use lmflow::{register_kernel, Graph, Kernel, KernelCtx, KernelRunner};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

static DESTROYS: AtomicUsize = AtomicUsize::new(0);
static FIELDS: AtomicUsize = AtomicUsize::new(0);
static CLOSES: AtomicUsize = AtomicUsize::new(0);
static WARNINGS: AtomicUsize = AtomicUsize::new(0);
#[derive(Default)]
struct Resource;
impl Drop for Resource {
    fn drop(&mut self) {
        FIELDS.fetch_add(1, Ordering::SeqCst);
    }
}
#[derive(Default)]
struct PanicDrop {
    _resource: Resource,
}
impl Kernel for PanicDrop {
    fn process(&mut self, _: &mut KernelCtx) -> lmflow::Result<()> {
        Ok(())
    }
    fn close(&mut self, _: &mut KernelCtx) -> lmflow::Result<()> {
        CLOSES.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
impl Drop for PanicDrop {
    fn drop(&mut self) {
        DESTROYS.fetch_add(1, Ordering::SeqCst);
        panic!("kernel destructor failed");
    }
}
unsafe extern "C" fn log(_: *mut std::ffi::c_void, level: i32, message: *const std::ffi::c_char) {
    let message = std::ffi::CStr::from_ptr(message).to_string_lossy();
    if level == lmflow::runtime::LOG_WARN
        && message.contains("PanicDrop")
        && message.contains("destruction")
    {
        WARNINGS.fetch_add(1, Ordering::SeqCst);
    }
}
#[test]
fn rust_kernel_destructor_panic_does_not_abort_host() {
    const CHILD: &str = "LMFLOW_DESTROY_PANIC_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "rust_kernel_destructor_panic_does_not_abort_host",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if child.try_wait().unwrap().is_some() {
                let output = child.wait_with_output().unwrap();
                assert!(
                    output.status.success(),
                    "child failed: {}\n{}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr)
                );
                return;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("kernel destruction stalled");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    register_kernel::<PanicDrop>("DestroyPanicProbe").unwrap();
    lmflow::runtime::set_log_callback(Some(log), std::ptr::null_mut());
    // Repeat across ownership paths to ensure the host remains usable after each panic.
    for mode in 0..4 {
        match mode {
            0 => {
                let mut runner = KernelRunner::new("DestroyPanicProbe", 0, 0).unwrap();
                runner.open().unwrap();
                drop(runner);
            }
            1 => {
                let graph = Graph::from_yaml(
                    "nodes: [{kernel: DestroyPanicProbe, input_ports: [in]}]\ninput_ports: [in]",
                )
                .unwrap();
                graph.start().unwrap();
                graph.close_all_inputs();
                graph.wait_done().unwrap();
                drop(graph);
            }
            2 => unsafe {
                use lmflow::ffi::*;
                let name = std::ffi::CString::new("DestroyPanicProbe").unwrap();
                let runner = lmflow_kernel_runner_new(name.as_ptr(), 0, 0);
                assert!(!runner.is_null());
                assert_eq!(lmflow_kernel_runner_start(runner), 0);
                lmflow_kernel_runner_free(runner);
            },
            _ => {
                let result = std::panic::catch_unwind(|| {
                    let mut runner = KernelRunner::new("DestroyPanicProbe", 0, 0).unwrap();
                    runner.open().unwrap();
                    panic!("host failure before runner destruction");
                });
                assert!(result.is_err());
            }
        }
        assert_eq!(DESTROYS.load(Ordering::SeqCst), mode + 1);
        assert_eq!(FIELDS.load(Ordering::SeqCst), mode + 1);
        assert_eq!(CLOSES.load(Ordering::SeqCst), mode + 1);
        assert_eq!(WARNINGS.load(Ordering::SeqCst), mode + 1);
    }
    lmflow::runtime::set_log_callback(None, std::ptr::null_mut());
}
