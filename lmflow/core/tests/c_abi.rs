//! C ABI 冒烟测试:**完全按 C 调用方的方式**驱动引擎。
//!
//! 这是 `docs/design.md` 里「边界①」的覆盖。刻意只用 `extern "C"` 函数与
//! `LMFlowPacket` 裸结构体,不碰任何 Rust 侧便利 API —— 因为外部 C/C++/Python
//! 宿主看到的就只有这些。`examples/cpp/hello_world/hello_world_host.cc` 的逻辑与此一致。

mod common;

use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::Once;

use lmflow::ffi::*;
use lmflow::packet::type_id;
use lmflow::{register_kernel, Kernel, KernelContract, KernelCtx};

#[derive(Default)]
struct CAbiI64Pass;

impl Kernel for CAbiI64Pass {
    fn get_contract(contract: &mut KernelContract) {
        contract.input_type(0, type_id::I64);
        contract.output_type(0, type_id::I64);
    }

    fn process(&mut self, context: &mut KernelCtx) -> lmflow::Result<()> {
        context.forward(0, 0)
    }
}

fn register_test_kernels() {
    common::register_test_kernels();
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        register_kernel::<CAbiI64Pass>("CAbiI64Pass").unwrap();
    });
}

fn cs(s: &str) -> CString {
    CString::new(s).unwrap()
}

unsafe fn last_error() -> String {
    CStr::from_ptr(lmflow_last_error())
        .to_string_lossy()
        .into_owned()
}

/// C 调用方自建包:owner=NULL + 自备 drop_fn(所有权在提交时移交引擎)。
unsafe extern "C" fn drop_boxed_i32(p: *mut c_void) {
    drop(Box::from_raw(p as *mut i32));
}

unsafe extern "C" fn c_contract_error(_factory: *mut c_void, contract: *mut LMFlowContract) {
    lmflow_contract_set_error(contract, c"C GetContract marker".as_ptr());
}

unsafe extern "C" fn c_contract_process(_self: *mut c_void, _context: *mut LMFlowContext) -> i32 {
    0
}

fn make_int_packet(v: i32, ts: i64) -> LMFlowPacket {
    LMFlowPacket {
        payload: Box::into_raw(Box::new(v)) as *mut c_void,
        type_id: 0, // 不声明类型 —— 与 PassThrough 的 SetAny 契约相容
        timestamp: ts,
        owner: std::ptr::null_mut(),
        drop_fn: Some(drop_boxed_i32),
    }
}

// 不写 executors —— 走**默认执行器**(按 CPU 核数开线程的线程池),也就是绝大多数
// 宿主的实际配置。委托执行器另有专门用例(见 tests/concurrency.rs)。
const CONFIG: &str = r#"
nodes:
  - name: "n1"
    kernel: "PassThrough"
    input_ports: ["in"]
    output_ports: ["mid"]
  - name: "n2"
    kernel: "PassThrough"
    input_ports: ["mid"]
    output_ports: ["out"]
input_ports: ["in"]
output_ports: ["out"]
"#;

#[test]
fn c_get_contract_failure_rejects_graph_build() {
    unsafe {
        let vtable = LMFlowKernelVTable {
            create: None,
            get_contract: Some(c_contract_error),
            open: None,
            process: Some(c_contract_process),
            close: None,
            destroy: None,
        };
        assert_eq!(
            lmflow_register_kernel(c"CFailingContract".as_ptr(), &vtable, std::ptr::null_mut(),),
            0
        );
    }
    let error = lmflow::Graph::from_yaml(
        r#"
nodes:
  - { name: bad, kernel: CFailingContract, input_ports: [in], output_ports: [] }
input_ports: [in]
"#,
    )
    .unwrap_err();
    assert!(error.to_string().contains("C GetContract marker"));
}

#[test]
fn abi_version_and_handshake() {
    assert_eq!(
        lmflow_abi_version(),
        4,
        "matches LMFLOW_ABI_VERSION in include/flow.h"
    );
}

#[test]
fn custom_type_descriptor_registration_is_strict_and_queryable() {
    let name = cs("lmflow.test.CAbiDescriptor");
    let conflicting_name = cs("lmflow.test.CAbiDescriptorConflict");
    let type_id = unsafe { lmflow_type_id(name.as_ptr()) };

    unsafe {
        assert_ne!(type_id, 0, "{}", last_error());
        assert_eq!(
            lmflow_register_type_descriptor(type_id, name.as_ptr(), 24, 8),
            0,
            "{}",
            last_error()
        );
        assert_eq!(
            lmflow_register_type_descriptor(type_id, name.as_ptr(), 24, 8),
            0,
            "identical registration must be idempotent"
        );
        assert_eq!(lmflow_type_size(type_id), 24);
        assert_eq!(lmflow_type_align(type_id), 8);
        assert_eq!(
            CStr::from_ptr(lmflow_type_name(type_id)).to_str().unwrap(),
            "lmflow.test.CAbiDescriptor"
        );

        assert_ne!(
            lmflow_register_type_descriptor(type_id, name.as_ptr(), 32, 8),
            0,
            "same id/name with a different layout must fail"
        );
        assert!(last_error().contains("already registered"));

        assert_ne!(
            lmflow_register_type_descriptor(type_id, conflicting_name.as_ptr(), 24, 8),
            0,
            "same id with a different name must fail"
        );
        assert!(last_error().contains("stable-name id"));
    }
}

#[test]
fn custom_type_descriptor_rejects_noncanonical_id() {
    let name = cs("lmflow.test.CAbiNonCanonical");
    let type_id = unsafe { lmflow_type_id(name.as_ptr()) };
    unsafe {
        assert_ne!(
            lmflow_register_type_descriptor(type_id + 1, name.as_ptr(), 16, 8),
            0
        );
    }
    let message = unsafe { last_error() };
    assert!(message.contains("stable-name id"), "{message}");
    assert!(message.contains(&type_id.to_string()), "{message}");
}

#[test]
fn bounded_poller_exposes_drop_policy_and_watermark_accounting() {
    common::register_test_kernels();
    unsafe {
        let graph = lmflow_graph_new();
        let yaml = cs(r#"
nodes:
  - { name: pass, kernel: PassThrough, input_ports: [in], output_ports: [out] }
input_ports: [in]
output_ports: [out]
"#);
        assert_eq!(lmflow_graph_init_from_yaml(graph, yaml.as_ptr()), 0);
        let output = cs("out");
        let poller =
            lmflow_graph_add_poller_bounded(graph, output.as_ptr(), 1, LMFLOW_POLLER_DROP_OLDEST);
        assert!(!poller.is_null(), "{}", last_error());
        assert_eq!(lmflow_graph_start(graph), 0);
        let input_name = cs("in");
        let input = lmflow_graph_input(graph, input_name.as_ptr());

        assert_eq!(lmflow_input_send(input, make_int_packet(1, 1)), 0);
        assert_eq!(lmflow_graph_wait_until_idle(graph), 0);
        assert_eq!(lmflow_input_send(input, make_int_packet(2, 2)), 0);
        assert_eq!(lmflow_graph_wait_until_idle(graph), 0);
        assert_eq!(lmflow_graph_total_queued(graph), 1);
        assert_eq!(lmflow_poller_dropped_count(poller), 1);
        let mut packet = LMFlowPacket::default();
        assert!(lmflow_poller_next(poller, &mut packet));
        assert_eq!(*(packet.payload as *const i32), 2);
        lmflow_packet_drop(&mut packet);
        assert_eq!(lmflow_graph_total_queued(graph), 0);

        lmflow_graph_close_all_inputs(graph);
        assert_eq!(lmflow_graph_wait_done(graph), 0);
        lmflow_input_free(input);
        lmflow_poller_free(poller);
        lmflow_graph_free(graph);
    }
}

/// 与 examples/cpp/hello_world/hello_world_host.cc 等价的完整流程。
#[test]
fn full_pipeline_through_c_abi() {
    common::register_test_kernels();
    unsafe {
        let g = lmflow_graph_new();
        assert!(!g.is_null());

        let yaml = cs(CONFIG);
        assert_eq!(
            lmflow_graph_init_from_yaml(g, yaml.as_ptr()),
            0,
            "init failed: {}",
            last_error()
        );

        let port_out = cs("out");
        let poller = lmflow_graph_add_poller(g, port_out.as_ptr());
        assert!(!poller.is_null(), "add_poller failed: {}", last_error());

        assert_eq!(lmflow_graph_start(g), 0, "start failed: {}", last_error());
        assert_eq!(lmflow_graph_state(g), 2, "should be Running(2) after start");

        let port_in = cs("in");
        let input = lmflow_graph_input(g, port_in.as_ptr());
        assert!(!input.is_null(), "graph_input failed: {}", last_error());

        for i in 0..5i32 {
            assert_eq!(
                lmflow_input_send(input, make_int_packet(i, i as i64)),
                0,
                "send failed: {}",
                last_error()
            );
            let mut out = LMFlowPacket {
                payload: std::ptr::null_mut(),
                type_id: 0,
                timestamp: 0,
                owner: std::ptr::null_mut(),
                drop_fn: None,
            };
            assert!(
                lmflow_poller_next(poller, &mut out),
                "packet #{i} should be retrievable"
            );
            assert!(!out.payload.is_null());
            assert_eq!(
                *(out.payload as *const i32),
                i,
                "value should pass through the pipeline unchanged"
            );
            assert_eq!(out.timestamp, i as i64);
            // 语义 3:poller 移交所有权,调用方必须释放
            lmflow_packet_drop(&mut out);
            assert!(out.payload.is_null(), "fields should be zeroed after drop");
            lmflow_packet_drop(&mut out); // 重复调用必须安全
        }

        lmflow_graph_close_all_inputs(g);
        assert_eq!(
            lmflow_graph_wait_done(g),
            0,
            "wait_done failed: {}",
            last_error()
        );
        assert_eq!(
            lmflow_graph_state(g),
            4,
            "should be Terminated(4) after finishing"
        );

        lmflow_input_free(input);
        lmflow_poller_free(poller);
        lmflow_graph_free(g);
    }
}

#[test]
fn c_abi_can_pump_a_delegating_executor() {
    common::register_test_kernels();
    unsafe {
        let graph = lmflow_graph_new();
        let yaml = cs(r#"
executors:
  - { name: host, type: DelegatingExecutor }
nodes:
  - { name: pass, kernel: PassThrough, executor: host, input_ports: [in], output_ports: [out] }
input_ports: [in]
output_ports: [out]
"#);
        assert_eq!(lmflow_graph_init_from_yaml(graph, yaml.as_ptr()), 0);
        let output = cs("out");
        let poller = lmflow_graph_add_poller(graph, output.as_ptr());
        assert!(!poller.is_null());
        assert_eq!(lmflow_graph_start(graph), 0);
        let input_name = cs("in");
        let input = lmflow_graph_input(graph, input_name.as_ptr());
        assert_eq!(lmflow_input_send(input, make_int_packet(7, 0)), 0);

        let mut packet = LMFlowPacket::default();
        assert!(!lmflow_poller_try_next(poller, &mut packet));
        assert!(lmflow_graph_pump_step(graph));
        assert!(lmflow_poller_try_next(poller, &mut packet));
        assert_eq!(*(packet.payload as *const i32), 7);
        lmflow_packet_drop(&mut packet);

        lmflow_graph_close_all_inputs(graph);
        assert_eq!(lmflow_graph_wait_done(graph), 0);
        lmflow_input_free(input);
        lmflow_poller_free(poller);
        lmflow_graph_free(graph);
    }
}

#[test]
fn c_abi_wakeup_callback_coalesces_delegated_tasks() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static WAKES: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "C" fn wake(_user: *mut c_void) {
        WAKES.fetch_add(1, Ordering::SeqCst);
    }

    common::register_test_kernels();
    WAKES.store(0, Ordering::SeqCst);
    unsafe {
        let graph = lmflow_graph_new();
        let yaml = cs(r#"
executors:
  - { name: host, type: DelegatingExecutor }
nodes:
  - { name: pass, kernel: PassThrough, executor: host, input_ports: [in], output_ports: [out] }
input_ports: [in]
output_ports: [out]
"#);
        assert_eq!(lmflow_graph_init_from_yaml(graph, yaml.as_ptr()), 0);
        assert_eq!(
            lmflow_graph_set_wakeup_callback(graph, Some(wake), std::ptr::null_mut()),
            0
        );
        assert_eq!(lmflow_graph_start(graph), 0);
        let input_name = cs("in");
        let input = lmflow_graph_input(graph, input_name.as_ptr());
        assert_eq!(lmflow_input_send(input, make_int_packet(1, 0)), 0);
        assert_eq!(lmflow_input_send(input, make_int_packet(2, 1)), 0);
        assert_eq!(
            WAKES.load(Ordering::SeqCst),
            1,
            "multiple queued delegated tasks should share one wakeup"
        );
        while lmflow_graph_pump_step(graph) {}
        assert_eq!(lmflow_input_send(input, make_int_packet(3, 2)), 0);
        assert_eq!(
            WAKES.load(Ordering::SeqCst),
            2,
            "draining to false should re-arm the next wakeup"
        );
        lmflow_graph_set_wakeup_callback(graph, None, std::ptr::null_mut());
        lmflow_graph_cancel(graph);
        let _ = lmflow_graph_wait_done(graph);
        lmflow_input_free(input);
        lmflow_graph_free(graph);
    }
}

#[test]
fn builtin_packet_types_roundtrip() {
    unsafe {
        // 整数
        let mut p = lmflow_packet_from_i64(-7, 3);
        assert_eq!(p.type_id, 2, "LMFLOW_TYPE_I64");
        let mut v = 0i64;
        assert!(lmflow_packet_as_i64(&p, &mut v));
        assert_eq!(v, -7);
        assert_eq!(p.timestamp, 3);
        // 类型不符必须返回 false 而不是乱读内存
        let mut d = 0f64;
        assert!(
            !lmflow_packet_as_f64(&p, &mut d),
            "an I64 packet should not be read as F64"
        );
        lmflow_packet_drop(&mut p);

        // 浮点 / 布尔
        let mut p = lmflow_packet_from_f64(1.5, 0);
        assert!(lmflow_packet_as_f64(&p, &mut d) && d == 1.5);
        lmflow_packet_drop(&mut p);
        let mut b = false;
        let mut p = lmflow_packet_from_bool(true, 0);
        assert!(lmflow_packet_as_bool(&p, &mut b) && b);
        lmflow_packet_drop(&mut p);

        // 字符串
        let s = cs("你好");
        let mut p = lmflow_packet_from_str(s.as_ptr(), 0);
        let mut out: *const c_char = std::ptr::null();
        assert!(lmflow_packet_as_str(&p, &mut out));
        assert_eq!(CStr::from_ptr(out).to_str().unwrap(), "你好");
        lmflow_packet_drop(&mut p);

        // 字节块(引擎内部拷贝一份,源可立即失效)
        let data = [1u8, 2, 3, 4];
        let mut p = lmflow_packet_from_bytes(data.as_ptr() as *const c_void, 4, 0);
        let mut ptr: *const c_void = std::ptr::null();
        let mut len = 0usize;
        assert!(lmflow_packet_as_bytes(&p, &mut ptr, &mut len));
        assert_eq!(len, 4);
        assert_eq!(std::slice::from_raw_parts(ptr as *const u8, 4), &data);
        lmflow_packet_drop(&mut p);
    }
}

#[test]
fn buffer_alloc_view_and_cow() {
    unsafe {
        let shape = [2i64, 3, 4];
        let mut buf = LMFlowBuffer::default();
        let mut p = lmflow_packet_new_buffer(3, shape.as_ptr(), 7 /*F32*/, 0, &mut buf);
        assert!(!p.payload.is_null(), "allocation failed: {}", last_error());
        assert_eq!(p.type_id, 6, "LMFLOW_TYPE_BUFFER");
        assert_eq!(buf.ndim, 3);
        assert_eq!(&buf.shape[..3], &[2, 3, 4]);
        // 行优先连续:最后一维步长 = 元素大小
        assert_eq!(&buf.strides[..3], &[3 * 4 * 4, 4 * 4, 4]);
        assert!(!buf.data.is_null());

        // 引擎分配的缓冲可就地写入
        *(buf.data as *mut f32) = 1.25;

        // 只读视图
        let mut view = LMFlowBuffer::default();
        assert!(lmflow_packet_as_buffer(&p, &mut view));
        assert_eq!(*(view.data as *const f32), 1.25);
        assert_eq!(
            view.flags & 1,
            1,
            "the view from as_buffer should be marked READONLY"
        );

        // CoW:独占时零拷贝(指针不变)
        let before = buf.data;
        let mut m = LMFlowBuffer::default();
        assert_eq!(lmflow_packet_make_mutable_buffer(&mut p, &mut m), 0);
        assert_eq!(m.data, before, "CoW should not copy when exclusive");

        lmflow_packet_drop(&mut p);
    }
}

#[test]
fn cow_copies_when_shared() {
    unsafe {
        let shape = [4i64];
        let mut b = LMFlowBuffer::default();
        let mut p1 = lmflow_packet_new_buffer(1, shape.as_ptr(), 0 /*U8*/, 0, &mut b);
        *(b.data as *mut u8) = 0x11;

        // 显式再持一份引用 → 共享
        let mut p2 = lmflow_packet_clone(&p1);
        assert!(!p2.owner.is_null());
        assert_eq!(
            p2.payload, p1.payload,
            "clone only bumps the refcount, does not copy data"
        );

        // 此时 make_mutable 必须复制,且不污染另一份
        let mut m = LMFlowBuffer::default();
        assert_eq!(lmflow_packet_make_mutable_buffer(&mut p2, &mut m), 0);
        assert_ne!(m.data, b.data, "CoW must copy when shared");
        *(m.data as *mut u8) = 0x22;
        assert_eq!(
            *(b.data as *const u8),
            0x11,
            "the original branch must not be polluted"
        );

        lmflow_packet_drop(&mut p1);
        lmflow_packet_drop(&mut p2);
    }
}

#[test]
fn from_buffer_handles_non_contiguous_strides() {
    unsafe {
        // 源:行优先连续的 2x3 i32 = [[0,1,2],[3,4,5]]
        let data: [i32; 6] = [0, 1, 2, 3, 4, 5];
        let esz = 4i64;
        // 按「转置视图」3x2 来描述它:shape=[3,2],strides=[esz, 3*esz](非连续)
        let mut shape = [0i64; 8];
        shape[0] = 3;
        shape[1] = 2;
        let mut strides = [0i64; 8];
        strides[0] = esz;
        strides[1] = 3 * esz;
        let src = LMFlowBuffer {
            data: data.as_ptr() as *mut c_void,
            ndim: 2,
            shape,
            strides,
            dtype: 4, // I32
            ..Default::default()
        };

        let mut p = lmflow_packet_from_buffer(&src, 0);
        assert!(!p.payload.is_null(), "{}", last_error());

        // 取回:应被展平成行优先连续的 3x2 = [[0,3],[1,4],[2,5]]
        let mut view = LMFlowBuffer::default();
        assert!(lmflow_packet_as_buffer(&p, &mut view));
        assert_eq!(&view.shape[..2], &[3, 2]);
        let out = std::slice::from_raw_parts(view.data as *const i32, 6);
        assert_eq!(
            out,
            &[0, 3, 1, 4, 2, 5],
            "transposed (non-contiguous) strides must be flattened element-by-element correctly, not mis-copied row by row"
        );
        lmflow_packet_drop(&mut p);
    }
}

/// 整块连续的 HWC 缓冲必须走一次性整块拷贝,而不是按「最后一维 = 一行」逐行搬。
///
/// 通用路径以最后一维为一行,而 HWC 图像的最后一维是通道数(2/3/4)—— 那会退化成
/// 几百万次几字节的拷贝(实测 1920x1080x2 要 6.7ms,约 600MB/s)。本测试同时钉住
/// **正确性**(内容逐字节相同)与**不变量**(连续布局下 strides 恰为紧密行优先),
/// 后者正是快路径的判据;判据写错就会静默走回慢路或拷错。
#[test]
fn from_buffer_copies_contiguous_hwc_in_one_shot() {
    unsafe {
        // 32x16x3 u8 的 HWC 图:最后一维 3,正是会触发退化的形状
        const H: usize = 32;
        const W: usize = 16;
        const C: usize = 3;
        let data: Vec<u8> = (0..(H * W * C)).map(|i| (i % 251) as u8).collect();
        let mut shape = [0i64; 8];
        shape[0] = H as i64;
        shape[1] = W as i64;
        shape[2] = C as i64;
        let mut strides = [0i64; 8];
        strides[0] = (W * C) as i64; // 紧密行优先
        strides[1] = C as i64;
        strides[2] = 1;
        let src = LMFlowBuffer {
            data: data.as_ptr() as *mut c_void,
            ndim: 3,
            shape,
            strides,
            dtype: 0, // U8
            ..Default::default()
        };

        let mut p = lmflow_packet_from_buffer(&src, 0);
        assert!(!p.payload.is_null(), "{}", last_error());

        let mut view = LMFlowBuffer::default();
        assert!(lmflow_packet_as_buffer(&p, &mut view));
        assert_eq!(&view.shape[..3], &[H as i64, W as i64, C as i64]);
        let out = std::slice::from_raw_parts(view.data as *const u8, H * W * C);
        assert_eq!(
            out,
            &data[..],
            "a contiguous HWC buffer must be copied byte-for-byte"
        );
        // 取回的视图必须仍是紧密行优先 —— 这是快路径判据成立的前提。
        assert_eq!(
            &view.strides[..3],
            &[(W * C) as i64, C as i64, 1],
            "engine-side buffers are packed row-major; the fast path keys off exactly this"
        );
        lmflow_packet_drop(&mut p);
    }
}

/// 负步长(numpy 的 `arr[::-1]`)必须逐元素拷对 —— 它绝不能被误判成连续而整块拷。
#[test]
fn from_buffer_handles_negative_stride() {
    unsafe {
        // 源:连续的 4 个 i32 = [10,20,30,40];按「反向视图」描述它
        let data: [i32; 4] = [10, 20, 30, 40];
        let esz = 4i64;
        let mut shape = [0i64; 8];
        shape[0] = 4;
        let mut strides = [0i64; 8];
        strides[0] = -esz;
        let src = LMFlowBuffer {
            // 反向视图的 data 指向最后一个元素
            data: data.as_ptr().add(3) as *mut c_void,
            ndim: 1,
            shape,
            strides,
            dtype: 4, // I32
            ..Default::default()
        };

        let mut p = lmflow_packet_from_buffer(&src, 0);
        assert!(!p.payload.is_null(), "{}", last_error());
        let mut view = LMFlowBuffer::default();
        assert!(lmflow_packet_as_buffer(&p, &mut view));
        let out = std::slice::from_raw_parts(view.data as *const i32, 4);
        assert_eq!(
            out,
            &[40, 30, 20, 10],
            "a negative stride must be flattened in reverse, never taken for a contiguous block"
        );
        lmflow_packet_drop(&mut p);
    }
}

#[test]
fn from_buffer_rejects_invalid_descriptors_before_dereferencing() {
    unsafe {
        let data = [1u8, 2];
        let valid = LMFlowBuffer {
            data: data.as_ptr() as *mut c_void,
            shape: [2, 0, 0, 0, 0, 0, 0, 0],
            strides: [1, 0, 0, 0, 0, 0, 0, 0],
            ndim: 1,
            dtype: 0,
            ..Default::default()
        };

        let rejected = |buffer: &LMFlowBuffer, expected: &str| {
            let packet = lmflow_packet_from_buffer(buffer, 0);
            assert!(
                packet.payload.is_null(),
                "invalid descriptor unexpectedly produced a packet"
            );
            let message = last_error();
            assert!(message.contains(expected), "{message}");
        };

        let mut buffer = valid;
        buffer.ndim = 0;
        rejected(&buffer, "ndim");

        let mut buffer = valid;
        buffer.ndim = 9;
        rejected(&buffer, "ndim");

        let mut buffer = valid;
        buffer.dtype = 99;
        rejected(&buffer, "dtype");

        let mut buffer = valid;
        buffer.shape[0] = -1;
        rejected(&buffer, "must not be negative");

        let mut buffer = valid;
        buffer.device = 1;
        buffer.data = std::ptr::dangling_mut::<c_void>();
        rejected(&buffer, "only LMFLOW_DEVICE_CPU");

        let mut buffer = valid;
        buffer.flags = 2;
        rejected(&buffer, "unknown bits");

        let mut buffer = valid;
        buffer.reserved[0] = 1;
        rejected(&buffer, "reserved fields must be zero");

        let mut buffer = valid;
        buffer.shape[1] = 1;
        rejected(&buffer, "shape entries after ndim");

        let mut buffer = valid;
        buffer.strides[1] = 1;
        rejected(&buffer, "stride entries after ndim");

        let mut buffer = valid;
        buffer.data = std::ptr::null_mut();
        rejected(&buffer, "data must be non-null");

        let mut buffer = valid;
        buffer.data = std::ptr::dangling_mut::<c_void>();
        buffer.shape[0] = 3;
        buffer.strides[0] = i64::MAX;
        rejected(&buffer, "exceeds platform pointer offsets");

        let mut buffer = valid;
        buffer.data = std::ptr::dangling_mut::<c_void>();
        buffer.shape[0] = 3;
        buffer.strides[0] = i64::MIN;
        rejected(&buffer, "exceeds platform pointer offsets");

        let mut readonly = valid;
        readonly.flags = 1;
        let mut packet = lmflow_packet_from_buffer(&readonly, 0);
        assert!(!packet.payload.is_null(), "{}", last_error());
        lmflow_packet_drop(&mut packet);

        let broadcast_source = [9u8];
        let broadcast = LMFlowBuffer {
            data: broadcast_source.as_ptr() as *mut c_void,
            shape: [3, 0, 0, 0, 0, 0, 0, 0],
            strides: [0, 0, 0, 0, 0, 0, 0, 0],
            ndim: 1,
            dtype: 0,
            ..Default::default()
        };
        let mut packet = lmflow_packet_from_buffer(&broadcast, 0);
        assert!(!packet.payload.is_null(), "{}", last_error());
        let mut view = LMFlowBuffer::default();
        assert!(lmflow_packet_as_buffer(&packet, &mut view));
        assert_eq!(
            std::slice::from_raw_parts(view.data as *const u8, 3),
            &[9, 9, 9],
            "zero strides are valid broadcast views"
        );
        lmflow_packet_drop(&mut packet);
    }
}

#[test]
fn new_buffer_rejects_ndim_above_the_fixed_descriptor_limit() {
    unsafe {
        let shape = [1i64; 9];
        let packet = lmflow_packet_new_buffer(9, shape.as_ptr(), 0, 0, std::ptr::null_mut());
        assert!(packet.payload.is_null());
        let message = last_error();
        assert!(message.contains("1..=8"), "{message}");
    }
}

#[test]
fn new_buffer_rejects_unallocatable_sizes_without_aborting() {
    unsafe {
        let shape = [i64::MAX];
        let packet = lmflow_packet_new_buffer(1, shape.as_ptr(), 0, 0, std::ptr::null_mut());
        assert!(packet.payload.is_null());
        let message = last_error();
        assert!(
            message.contains("cannot allocate") || message.contains("overflow"),
            "{message}"
        );
    }
}

#[test]
fn make_mutable_rejects_borrowed_packet() {
    unsafe {
        // owner==NULL 的自建包不属于引擎,不能 CoW
        let mut p = make_int_packet(1, 0);
        let mut m = LMFlowBuffer::default();
        assert_ne!(
            lmflow_packet_make_mutable_buffer(&mut p, &mut m),
            0,
            "a self-built packet should be rejected with a readable reason"
        );
        assert!(last_error().contains("owner"), "{}", last_error());
        lmflow_packet_drop(&mut p);
    }
}

#[test]
fn null_pointers_do_not_crash() {
    unsafe {
        // 所有导出函数都应对空指针返回错误/默认值,而不是崩溃
        assert!(lmflow_graph_init_from_yaml(std::ptr::null_mut(), std::ptr::null()) != 0);
        assert!(lmflow_graph_start(std::ptr::null_mut()) != 0);
        assert!(!lmflow_graph_pump_step(std::ptr::null_mut()));
        assert!(lmflow_graph_input(std::ptr::null_mut(), std::ptr::null()).is_null());
        assert!(lmflow_graph_add_poller(std::ptr::null_mut(), std::ptr::null()).is_null());
        assert!(!lmflow_poller_next(
            std::ptr::null_mut(),
            std::ptr::null_mut()
        ));
        assert_eq!(lmflow_graph_num_nodes(std::ptr::null_mut()), 0);
        assert_eq!(lmflow_ctx_num_inputs(std::ptr::null()), 0);
        assert_eq!(lmflow_ctx_input_timestamp(std::ptr::null()), i64::MIN);
        assert!(!lmflow_packet_as_i64(
            std::ptr::null(),
            std::ptr::null_mut()
        ));
        lmflow_packet_drop(std::ptr::null_mut());
        lmflow_graph_free(std::ptr::null_mut());
        lmflow_graph_cancel(std::ptr::null_mut());
        lmflow_graph_close_all_inputs(std::ptr::null_mut());
        lmflow_input_free(std::ptr::null_mut());
        lmflow_poller_free(std::ptr::null_mut());
    }
}

/// 句柄由调用方拥有:即使先 free 了图,句柄仍安全 —— 之后再用只得到「图已结束」,
/// 绝不 use-after-free。这守卫的是 Python/C++ 宿主先销毁 graph、后用 input/poller 的场景。
#[test]
fn handles_stay_safe_after_graph_free() {
    common::register_test_kernels();
    unsafe {
        let g = lmflow_graph_new();
        let yaml = cs(CONFIG);
        assert_eq!(lmflow_graph_init_from_yaml(g, yaml.as_ptr()), 0);
        let port_out = cs("out");
        let poller = lmflow_graph_add_poller(g, port_out.as_ptr());
        assert_eq!(lmflow_graph_start(g), 0);
        let port_in = cs("in");
        let input = lmflow_graph_input(g, port_in.as_ptr());
        assert!(!input.is_null() && !poller.is_null());

        // 先把图 free 掉(cancel + wait_done + 释放图槽),句柄却仍在手上
        lmflow_graph_free(g);

        // 往已结束的图发送:必须安全地返回错误(而不是崩溃/挂死)
        let rc = lmflow_input_send(input, make_int_packet(1, 0));
        assert_ne!(
            rc, 0,
            "graph has finished, send should error rather than UAF"
        );

        // poller 取包也安全:图已结束,返回 false
        let mut out = LMFlowPacket {
            payload: std::ptr::null_mut(),
            type_id: 0,
            timestamp: 0,
            owner: std::ptr::null_mut(),
            drop_fn: None,
        };
        assert!(
            !lmflow_poller_next(poller, &mut out),
            "a finished graph's poller should return false"
        );

        // 归还句柄(此刻才真正释放引擎)
        lmflow_input_free(input);
        lmflow_poller_free(poller);
    }
}

#[test]
fn errors_are_reported_with_readable_text() {
    unsafe {
        let g = lmflow_graph_new();
        // 未 init 就 start
        assert_ne!(lmflow_graph_start(g), 0);
        assert!(last_error().contains("initialized"), "{}", last_error());

        // 坏 YAML
        let bad = cs("nodes: [ { kernel: X, typo: 1 } ]");
        assert_ne!(lmflow_graph_init_from_yaml(g, bad.as_ptr()), 0);
        assert!(!last_error().is_empty(), "must give a readable reason");

        lmflow_graph_free(g);
    }
}

#[test]
fn unknown_subgraph_is_rejected_not_ignored() {
    unsafe {
        let g = lmflow_graph_new();
        // 引用不存在的子图 —— 必须报错,而不是静默忽略(子图现已支持,但名字要存在)
        let y = cs("nodes: [ { type: SomeSubgraph } ]");
        let rc = lmflow_graph_init_from_yaml(g, y.as_ptr());
        assert_ne!(
            rc, 0,
            "unknown subgraph must be rejected, not silently ignored"
        );
        assert!(last_error().contains("subgraph"), "{}", last_error());
        lmflow_graph_free(g);
    }
}

#[test]
fn to_dot_on_uninitialized_graph_is_valid_empty_digraph() {
    unsafe {
        let g = lmflow_graph_new();
        // 没 init 也不能崩:返回合法的空 digraph,可直接喂 graphviz
        let dot = CStr::from_ptr(lmflow_graph_to_dot_view(g, LMFLOW_DOT_TOPOLOGY))
            .to_string_lossy()
            .into_owned();
        assert!(dot.contains("digraph"), "{dot}");
        lmflow_graph_free(g);
    }
}

#[test]
fn introspection_through_c_abi() {
    common::register_test_kernels();
    unsafe {
        let g = lmflow_graph_new();
        let yaml = cs(CONFIG);
        assert_eq!(lmflow_graph_init_from_yaml(g, yaml.as_ptr()), 0);

        assert_eq!(lmflow_graph_num_nodes(g), 2);
        assert_eq!(lmflow_graph_num_input_ports(g), 1);
        assert_eq!(lmflow_graph_num_output_ports(g), 1);
        assert_eq!(
            CStr::from_ptr(lmflow_graph_node_name(g, 0))
                .to_str()
                .unwrap(),
            "n1"
        );
        assert_eq!(lmflow_graph_node_num_input_ports(g, 0), 1);
        assert_eq!(
            CStr::from_ptr(lmflow_graph_node_input_port_name(g, 0, 0))
                .to_str()
                .unwrap(),
            "in"
        );
        assert_eq!(
            CStr::from_ptr(lmflow_graph_input_port_name(g, 0))
                .to_str()
                .unwrap(),
            "in"
        );
        // 越界索引返回空串而不是崩溃
        assert_eq!(
            CStr::from_ptr(lmflow_graph_node_name(g, 99))
                .to_str()
                .unwrap(),
            ""
        );

        let dump = CStr::from_ptr(lmflow_graph_dump(g))
            .to_string_lossy()
            .into_owned();
        assert!(dump.contains("n1") && dump.contains("n2"), "{dump}");

        let dot = CStr::from_ptr(lmflow_graph_to_dot_view(g, LMFLOW_DOT_TOPOLOGY))
            .to_string_lossy()
            .into_owned();
        assert!(
            dot.contains("digraph lmflow") && dot.contains("n1") && dot.contains("n2"),
            "{dot}"
        );

        // struct_size 太小必须被拒(前向兼容契约)
        let mut st = LMFlowNodeStats {
            struct_size: 4,
            reserved0: 0,
            node_name: std::ptr::null(),
            kernel_name: std::ptr::null(),
            running: false,
            running_for_us: 0,
            processed: 0,
            errors: 0,
            total_process_us: 0,
            max_process_us: 0,
            packets_in: 0,
            packets_out: 0,
            peak_queue_depth: 0,
            queued: 0,
        };
        assert!(
            !lmflow_graph_node_stats(g, 0, &mut st),
            "struct_size too small should be rejected"
        );
        st.struct_size = std::mem::size_of::<LMFlowNodeStats>() as u32;
        assert!(lmflow_graph_node_stats(g, 0, &mut st));
        // 统计模式的 DOT:标注 + 热力图,经 C ABI 也要能出
        let dot_stats = CStr::from_ptr(lmflow_graph_to_dot_view(g, LMFLOW_DOT_DIAGNOSTICS))
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            dot_stats.contains("digraph lmflow") && dot_stats.contains("pkts"),
            "diagnostics 应标出统计:{dot_stats}"
        );
        let dot_compact = CStr::from_ptr(lmflow_graph_to_dot_view(g, LMFLOW_DOT_COMPACT))
            .to_str()
            .unwrap()
            .to_string();
        assert!(dot_compact.contains("@default\\nCREATED"));
        assert!(!dot_compact.contains("CREATED · 0 pkts"));
        assert!(!dot_compact.contains("ports:"));
        assert!(CStr::from_ptr(lmflow_graph_to_dot_view(g, 99))
            .to_bytes()
            .is_empty());
        assert!(last_error().contains("invalid DOT view"));
        assert_eq!(
            CStr::from_ptr(st.kernel_name).to_str().unwrap(),
            "PassThrough"
        );

        let mut queue_stats = LMFlowInputQueueStats {
            struct_size: 4,
            reserved0: 0,
            node_name: std::ptr::null(),
            port_name: std::ptr::null(),
            producer_name: std::ptr::null(),
            packet_capacity: 0,
            queued_packets: 0,
            queued_bytes: 0,
            reserved_packets: 0,
            peak_queued_packets: 0,
            peak_queued_bytes: 0,
            blocked: false,
            reserved1: [0; 7],
            blocked_for_us: 0,
            block_events: 0,
            total_blocked_us: 0,
        };
        assert!(!lmflow_graph_input_queue_stats(g, 0, 0, &mut queue_stats));
        queue_stats.struct_size = std::mem::size_of::<LMFlowInputQueueStats>() as u32;
        assert!(lmflow_graph_input_queue_stats(g, 0, 0, &mut queue_stats));
        assert_eq!(
            CStr::from_ptr(queue_stats.node_name).to_str().unwrap(),
            "n1"
        );
        assert_eq!(
            CStr::from_ptr(queue_stats.port_name).to_str().unwrap(),
            "in"
        );
        assert_eq!(queue_stats.packet_capacity, 0);
        assert!(!queue_stats.blocked);

        lmflow_graph_free(g);
    }
}

#[test]
fn observer_receives_packets() {
    common::register_test_kernels();

    // 用 Mutex 而非 static mut:后者创建共享引用本身就是坏实践
    static SEEN: std::sync::Mutex<Vec<i32>> = std::sync::Mutex::new(Vec::new());
    unsafe extern "C" fn on_packet(_user: *mut c_void, pkt: LMFlowPacket) {
        if !pkt.payload.is_null() {
            let v = unsafe { *(pkt.payload as *const i32) };
            SEEN.lock().expect("lock poisoned").push(v);
        }
    }

    unsafe {
        let g = lmflow_graph_new();
        let yaml = cs(CONFIG);
        assert_eq!(lmflow_graph_init_from_yaml(g, yaml.as_ptr()), 0);
        let port = cs("out");
        assert_eq!(
            lmflow_graph_observe(g, port.as_ptr(), Some(on_packet), std::ptr::null_mut()),
            0,
            "{}",
            last_error()
        );
        assert_eq!(lmflow_graph_start(g), 0);
        let pin = cs("in");
        let input = lmflow_graph_input(g, pin.as_ptr());
        for i in 0..3i32 {
            assert_eq!(
                lmflow_input_send(input, make_int_packet(i * 10, i as i64)),
                0
            );
            // 每送一个就排干一次,故 observer 收到的顺序是确定的
            assert_eq!(lmflow_graph_wait_until_idle(g), 0);
        }
        lmflow_graph_close_all_inputs(g);
        assert_eq!(lmflow_graph_wait_done(g), 0);
        assert_eq!(
            *SEEN.lock().expect("lock poisoned"),
            vec![0, 10, 20],
            "observer should receive each packet in order"
        );
        lmflow_input_free(input);
        lmflow_graph_free(g);
    }
}

#[test]
fn log_callback_receives_engine_messages() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNT: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "C" fn sink(_u: *mut c_void, _lv: i32, msg: *const c_char) {
        assert!(!msg.is_null());
        COUNT.fetch_add(1, Ordering::SeqCst);
    }
    common::register_test_kernels();
    register_test_kernels();
    unsafe {
        lmflow_set_log_callback(Some(sink), std::ptr::null_mut());
        let g = lmflow_graph_new();
        // 让算子报错,从而触发引擎记录错误日志
        let y = cs(r#"
nodes:
  - { name: "s", kernel: "CAbiI64Pass", input_ports: ["in"], output_ports: ["out"] }
input_ports: ["in"]
output_ports: ["out"]
"#);
        assert_eq!(lmflow_graph_init_from_yaml(g, y.as_ptr()), 0);
        assert_eq!(lmflow_graph_start(g), 0);
        let pin = cs("in");
        let input = lmflow_graph_input(g, pin.as_ptr());
        // type_id 与契约声明的 int 不符 → 引擎记录错误
        let mut bad = make_int_packet(1, 0);
        bad.type_id = 999;
        let _ = lmflow_input_send(input, bad);
        let _ = lmflow_graph_wait_until_idle(g);
        assert!(
            COUNT.load(Ordering::SeqCst) > 0,
            "errors should surface through the log callback"
        );
        // 图级错误文本必须能拿到(lmflow_last_error 是线程局部的,拿不到工作线程的)
        let ge = CStr::from_ptr(lmflow_graph_last_error(g))
            .to_string_lossy()
            .into_owned();
        assert!(ge.contains("type mismatch"), "graph-level error: {ge}");
        lmflow_input_free(input);
        lmflow_graph_free(g);
        lmflow_set_log_callback(None, std::ptr::null_mut());
    }
}
