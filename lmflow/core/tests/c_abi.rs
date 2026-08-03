//! C ABI 冒烟测试:**完全按 C 调用方的方式**驱动引擎。
//!
//! 这是 `docs/design.md` 里「边界①」的覆盖。刻意只用 `extern "C"` 函数与
//! `LMFlowPacket` 裸结构体,不碰任何 Rust 侧便利 API —— 因为外部 C/C++/Python
//! 宿主看到的就只有这些。`examples/cpp/hello_world/hello_world_host.cc` 的逻辑与此一致。

#![cfg(feature = "builtin-kernels")] // 用内置 C++ 算子:纯 Rust 构建(--no-default-features)时整文件跳过

use std::ffi::{c_char, c_void, CStr, CString};

use lmflow::ffi::*;

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

fn make_int_packet(v: i32, ts: i64) -> LMFlowPacket {
    LMFlowPacket {
        payload: Box::into_raw(Box::new(v)) as *mut c_void,
        type_id: 0, // 不声明类型 —— 与 PassThroughKernel 的 SetAny 契约相容
        timestamp: ts,
        owner: std::ptr::null_mut(),
        drop_fn: Some(drop_boxed_i32),
    }
}

const CONFIG: &str = r#"
nodes:
  - name: "n1"
    kernel: "PassThroughKernel"
    input_ports: ["in"]
    output_ports: ["mid"]
  - name: "n2"
    kernel: "PassThroughKernel"
    input_ports: ["mid"]
    output_ports: ["out"]
input_ports: ["in"]
output_ports: ["out"]
"#;

#[test]
fn abi_version_and_handshake() {
    assert_eq!(
        lmflow_abi_version(),
        1,
        "matches LMFLOW_ABI_VERSION in include/flow.h"
    );
}

/// 与 examples/cpp/hello_world/hello_world_host.cc 等价的完整流程。
#[test]
fn full_pipeline_through_c_abi() {
    lmflow::register_builtin_kernels();
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
    lmflow::register_builtin_kernels();
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
        let dot = CStr::from_ptr(lmflow_graph_to_dot(g, false))
            .to_string_lossy()
            .into_owned();
        assert!(dot.contains("digraph"), "{dot}");
        lmflow_graph_free(g);
    }
}

#[test]
fn introspection_through_c_abi() {
    lmflow::register_builtin_kernels();
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

        let dot = CStr::from_ptr(lmflow_graph_to_dot(g, false))
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
        let dot_stats = CStr::from_ptr(lmflow_graph_to_dot(g, true))
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            dot_stats.contains("digraph lmflow") && dot_stats.contains("pkts"),
            "with_stats 应标出统计:{dot_stats}"
        );
        assert_eq!(
            CStr::from_ptr(st.kernel_name).to_str().unwrap(),
            "PassThroughKernel"
        );

        lmflow_graph_free(g);
    }
}

#[test]
fn observer_receives_packets() {
    lmflow::register_builtin_kernels();

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
            // 主线程执行器:需要进入引擎才会推进(见 docs/design.md §7.9)
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
    lmflow::register_builtin_kernels();
    unsafe {
        lmflow_set_log_callback(Some(sink), std::ptr::null_mut());
        let g = lmflow_graph_new();
        // 让算子报错,从而触发引擎记录错误日志
        let y = cs(r#"
nodes:
  - { name: "s", kernel: "ScaleKernel", input_ports: ["in"], output_ports: ["out"], options: { factor: 2 } }
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
