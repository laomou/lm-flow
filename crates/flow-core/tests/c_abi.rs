//! C ABI 冒烟测试:**完全按 C 调用方的方式**驱动引擎。
//!
//! 这是 `docs/design.md` 里「边界①」的覆盖。刻意只用 `extern "C"` 函数与
//! `FlowPacket` 裸结构体,不碰任何 Rust 侧便利 API —— 因为外部 C/C++/Python
//! 宿主看到的就只有这些。`examples/cpp/hello_world_host.cc` 的逻辑与此一致。

use std::ffi::{c_char, c_void, CStr, CString};

use flow_core::ffi::*;

fn cs(s: &str) -> CString {
    CString::new(s).unwrap()
}

unsafe fn last_error() -> String {
    CStr::from_ptr(flow_last_error())
        .to_string_lossy()
        .into_owned()
}

/// C 调用方自建包:owner=NULL + 自备 drop_fn(所有权在提交时移交引擎)。
unsafe extern "C" fn drop_boxed_i32(p: *mut c_void) {
    drop(Box::from_raw(p as *mut i32));
}

fn make_int_packet(v: i32, ts: i64) -> FlowPacket {
    FlowPacket {
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
        flow_abi_version(),
        1,
        "与 include/flow.h 的 FLOW_ABI_VERSION 一致"
    );
}

/// 与 examples/cpp/hello_world_host.cc 等价的完整流程。
#[test]
fn full_pipeline_through_c_abi() {
    flow_core::register_builtin_kernels();
    unsafe {
        let g = flow_graph_new();
        assert!(!g.is_null());

        let yaml = cs(CONFIG);
        assert_eq!(
            flow_graph_init_from_yaml(g, yaml.as_ptr()),
            0,
            "init 失败: {}",
            last_error()
        );

        let port_out = cs("out");
        let poller = flow_graph_add_poller(g, port_out.as_ptr());
        assert!(!poller.is_null(), "add_poller 失败: {}", last_error());

        assert_eq!(flow_graph_start(g), 0, "start 失败: {}", last_error());
        assert_eq!(flow_graph_state(g), 2, "start 后应为 Running(2)");

        let port_in = cs("in");
        let input = flow_graph_input(g, port_in.as_ptr());
        assert!(!input.is_null(), "graph_input 失败: {}", last_error());

        for i in 0..5i32 {
            assert_eq!(
                flow_input_send(input, make_int_packet(i, i as i64)),
                0,
                "send 失败: {}",
                last_error()
            );
            let mut out = FlowPacket {
                payload: std::ptr::null_mut(),
                type_id: 0,
                timestamp: 0,
                owner: std::ptr::null_mut(),
                drop_fn: None,
            };
            assert!(flow_poller_next(poller, &mut out), "第 {i} 个包应能取到");
            assert!(!out.payload.is_null());
            assert_eq!(*(out.payload as *const i32), i, "值应原样穿过管线");
            assert_eq!(out.timestamp, i as i64);
            // 语义 3:poller 移交所有权,调用方必须释放
            flow_packet_drop(&mut out);
            assert!(out.payload.is_null(), "drop 后字段应被清零");
            flow_packet_drop(&mut out); // 重复调用必须安全
        }

        flow_graph_close_all_inputs(g);
        assert_eq!(
            flow_graph_wait_done(g),
            0,
            "wait_done 失败: {}",
            last_error()
        );
        assert_eq!(flow_graph_state(g), 4, "结束后应为 Terminated(4)");

        flow_input_free(input);
        flow_poller_free(poller);
        flow_graph_free(g);
    }
}

#[test]
fn builtin_packet_types_roundtrip() {
    unsafe {
        // 整数
        let mut p = flow_packet_from_i64(-7, 3);
        assert_eq!(p.type_id, 2, "FLOW_TYPE_I64");
        let mut v = 0i64;
        assert!(flow_packet_as_i64(&p, &mut v));
        assert_eq!(v, -7);
        assert_eq!(p.timestamp, 3);
        // 类型不符必须返回 false 而不是乱读内存
        let mut d = 0f64;
        assert!(!flow_packet_as_f64(&p, &mut d), "I64 包不该被当成 F64 读出");
        flow_packet_drop(&mut p);

        // 浮点 / 布尔
        let mut p = flow_packet_from_f64(1.5, 0);
        assert!(flow_packet_as_f64(&p, &mut d) && d == 1.5);
        flow_packet_drop(&mut p);
        let mut b = false;
        let mut p = flow_packet_from_bool(true, 0);
        assert!(flow_packet_as_bool(&p, &mut b) && b);
        flow_packet_drop(&mut p);

        // 字符串
        let s = cs("你好");
        let mut p = flow_packet_from_str(s.as_ptr(), 0);
        let mut out: *const c_char = std::ptr::null();
        assert!(flow_packet_as_str(&p, &mut out));
        assert_eq!(CStr::from_ptr(out).to_str().unwrap(), "你好");
        flow_packet_drop(&mut p);

        // 字节块(引擎内部拷贝一份,源可立即失效)
        let data = [1u8, 2, 3, 4];
        let mut p = flow_packet_from_bytes(data.as_ptr() as *const c_void, 4, 0);
        let mut ptr: *const c_void = std::ptr::null();
        let mut len = 0usize;
        assert!(flow_packet_as_bytes(&p, &mut ptr, &mut len));
        assert_eq!(len, 4);
        assert_eq!(std::slice::from_raw_parts(ptr as *const u8, 4), &data);
        flow_packet_drop(&mut p);
    }
}

#[test]
fn buffer_alloc_view_and_cow() {
    unsafe {
        let shape = [2i64, 3, 4];
        let mut buf = FlowBuffer::default();
        let mut p = flow_packet_new_buffer(3, shape.as_ptr(), 7 /*F32*/, 0, &mut buf);
        assert!(!p.payload.is_null(), "分配失败: {}", last_error());
        assert_eq!(p.type_id, 6, "FLOW_TYPE_BUFFER");
        assert_eq!(buf.ndim, 3);
        assert_eq!(&buf.shape[..3], &[2, 3, 4]);
        // 行优先连续:最后一维步长 = 元素大小
        assert_eq!(&buf.strides[..3], &[3 * 4 * 4, 4 * 4, 4]);
        assert!(!buf.data.is_null());

        // 引擎分配的缓冲可就地写入
        *(buf.data as *mut f32) = 1.25;

        // 只读视图
        let mut view = FlowBuffer::default();
        assert!(flow_packet_as_buffer(&p, &mut view));
        assert_eq!(*(view.data as *const f32), 1.25);
        assert_eq!(view.flags & 1, 1, "as_buffer 取得的视图应标记 READONLY");

        // CoW:独占时零拷贝(指针不变)
        let before = buf.data;
        let mut m = FlowBuffer::default();
        assert_eq!(flow_packet_make_mutable_buffer(&mut p, &mut m), 0);
        assert_eq!(m.data, before, "独占时 CoW 不应复制");

        flow_packet_drop(&mut p);
    }
}

#[test]
fn cow_copies_when_shared() {
    unsafe {
        let shape = [4i64];
        let mut b = FlowBuffer::default();
        let mut p1 = flow_packet_new_buffer(1, shape.as_ptr(), 0 /*U8*/, 0, &mut b);
        *(b.data as *mut u8) = 0x11;

        // 显式再持一份引用 → 共享
        let mut p2 = flow_packet_clone(&p1);
        assert!(!p2.owner.is_null());
        assert_eq!(p2.payload, p1.payload, "clone 只增引用,不复制数据");

        // 此时 make_mutable 必须复制,且不污染另一份
        let mut m = FlowBuffer::default();
        assert_eq!(flow_packet_make_mutable_buffer(&mut p2, &mut m), 0);
        assert_ne!(m.data, b.data, "被共享时 CoW 必须复制");
        *(m.data as *mut u8) = 0x22;
        assert_eq!(*(b.data as *const u8), 0x11, "原分支不得被污染");

        flow_packet_drop(&mut p1);
        flow_packet_drop(&mut p2);
    }
}

#[test]
fn make_mutable_rejects_borrowed_packet() {
    unsafe {
        // owner==NULL 的自建包不属于引擎,不能 CoW
        let mut p = make_int_packet(1, 0);
        let mut m = FlowBuffer::default();
        assert_ne!(
            flow_packet_make_mutable_buffer(&mut p, &mut m),
            0,
            "自建包应被拒绝并给出可读原因"
        );
        assert!(last_error().contains("owner"), "{}", last_error());
        flow_packet_drop(&mut p);
    }
}

#[test]
fn null_pointers_do_not_crash() {
    unsafe {
        // 所有导出函数都应对空指针返回错误/默认值,而不是崩溃
        assert!(flow_graph_init_from_yaml(std::ptr::null_mut(), std::ptr::null()) != 0);
        assert!(flow_graph_start(std::ptr::null_mut()) != 0);
        assert!(flow_graph_input(std::ptr::null_mut(), std::ptr::null()).is_null());
        assert!(flow_graph_add_poller(std::ptr::null_mut(), std::ptr::null()).is_null());
        assert!(!flow_poller_next(
            std::ptr::null_mut(),
            std::ptr::null_mut()
        ));
        assert_eq!(flow_graph_num_nodes(std::ptr::null_mut()), 0);
        assert_eq!(flow_ctx_num_inputs(std::ptr::null()), 0);
        assert_eq!(flow_ctx_input_timestamp(std::ptr::null()), i64::MIN);
        assert!(!flow_packet_as_i64(std::ptr::null(), std::ptr::null_mut()));
        flow_packet_drop(std::ptr::null_mut());
        flow_graph_free(std::ptr::null_mut());
        flow_graph_cancel(std::ptr::null_mut());
        flow_graph_close_all_inputs(std::ptr::null_mut());
        flow_input_free(std::ptr::null_mut());
        flow_poller_free(std::ptr::null_mut());
    }
}

/// 句柄由调用方拥有:即使先 free 了图,句柄仍安全 —— 之后再用只得到「图已结束」,
/// 绝不 use-after-free。这守卫的是 Python/C++ 宿主先销毁 graph、后用 input/poller 的场景。
#[test]
fn handles_stay_safe_after_graph_free() {
    flow_core::register_builtin_kernels();
    unsafe {
        let g = flow_graph_new();
        let yaml = cs(CONFIG);
        assert_eq!(flow_graph_init_from_yaml(g, yaml.as_ptr()), 0);
        let port_out = cs("out");
        let poller = flow_graph_add_poller(g, port_out.as_ptr());
        assert_eq!(flow_graph_start(g), 0);
        let port_in = cs("in");
        let input = flow_graph_input(g, port_in.as_ptr());
        assert!(!input.is_null() && !poller.is_null());

        // 先把图 free 掉(cancel + wait_done + 释放图槽),句柄却仍在手上
        flow_graph_free(g);

        // 往已结束的图发送:必须安全地返回错误(而不是崩溃/挂死)
        let rc = flow_input_send(input, make_int_packet(1, 0));
        assert_ne!(rc, 0, "图已结束,send 应报错而不是 UAF");

        // poller 取包也安全:图已结束,返回 false
        let mut out = FlowPacket {
            payload: std::ptr::null_mut(),
            type_id: 0,
            timestamp: 0,
            owner: std::ptr::null_mut(),
            drop_fn: None,
        };
        assert!(
            !flow_poller_next(poller, &mut out),
            "已结束的图 poller 应返回 false"
        );

        // 归还句柄(此刻才真正释放引擎)
        flow_input_free(input);
        flow_poller_free(poller);
    }
}

#[test]
fn errors_are_reported_with_readable_text() {
    unsafe {
        let g = flow_graph_new();
        // 未 init 就 start
        assert_ne!(flow_graph_start(g), 0);
        assert!(last_error().contains("初始化"), "{}", last_error());

        // 坏 YAML
        let bad = cs("nodes: [ { kernel: X, typo: 1 } ]");
        assert_ne!(flow_graph_init_from_yaml(g, bad.as_ptr()), 0);
        assert!(!last_error().is_empty(), "必须给出可读原因");

        flow_graph_free(g);
    }
}

#[test]
fn unsupported_config_is_rejected_not_ignored() {
    unsafe {
        let g = flow_graph_new();
        // 子图(node 的 type 字段)尚未实现 —— 必须报 UNSUPPORTED,而不是静默忽略
        let y = cs("nodes: [ { kernel: PassThroughKernel, type: SomeSubgraph } ]");
        let rc = flow_graph_init_from_yaml(g, y.as_ptr());
        assert_eq!(rc, 10, "应为 FLOW_ERR_UNSUPPORTED,而不是静默忽略");
        assert!(last_error().contains("子图"), "{}", last_error());
        flow_graph_free(g);
    }
}

#[test]
fn introspection_through_c_abi() {
    flow_core::register_builtin_kernels();
    unsafe {
        let g = flow_graph_new();
        let yaml = cs(CONFIG);
        assert_eq!(flow_graph_init_from_yaml(g, yaml.as_ptr()), 0);

        assert_eq!(flow_graph_num_nodes(g), 2);
        assert_eq!(flow_graph_num_input_ports(g), 1);
        assert_eq!(flow_graph_num_output_ports(g), 1);
        assert_eq!(
            CStr::from_ptr(flow_graph_node_name(g, 0)).to_str().unwrap(),
            "n1"
        );
        assert_eq!(
            CStr::from_ptr(flow_graph_input_port_name(g, 0))
                .to_str()
                .unwrap(),
            "in"
        );
        // 越界索引返回空串而不是崩溃
        assert_eq!(
            CStr::from_ptr(flow_graph_node_name(g, 99))
                .to_str()
                .unwrap(),
            ""
        );

        let dump = CStr::from_ptr(flow_graph_dump(g))
            .to_string_lossy()
            .into_owned();
        assert!(dump.contains("n1") && dump.contains("n2"), "{dump}");

        // struct_size 太小必须被拒(前向兼容契约)
        let mut st = FlowNodeStats {
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
            queued: 0,
        };
        assert!(
            !flow_graph_node_stats(g, 0, &mut st),
            "struct_size 过小应被拒"
        );
        st.struct_size = std::mem::size_of::<FlowNodeStats>() as u32;
        assert!(flow_graph_node_stats(g, 0, &mut st));
        assert_eq!(
            CStr::from_ptr(st.kernel_name).to_str().unwrap(),
            "PassThroughKernel"
        );

        flow_graph_free(g);
    }
}

#[test]
fn observer_receives_packets() {
    flow_core::register_builtin_kernels();

    // 用 Mutex 而非 static mut:后者创建共享引用本身就是坏实践
    static SEEN: std::sync::Mutex<Vec<i32>> = std::sync::Mutex::new(Vec::new());
    unsafe extern "C" fn on_packet(_user: *mut c_void, pkt: FlowPacket) {
        if !pkt.payload.is_null() {
            let v = unsafe { *(pkt.payload as *const i32) };
            SEEN.lock().expect("锁中毒").push(v);
        }
    }

    unsafe {
        let g = flow_graph_new();
        let yaml = cs(CONFIG);
        assert_eq!(flow_graph_init_from_yaml(g, yaml.as_ptr()), 0);
        let port = cs("out");
        assert_eq!(
            flow_graph_observe(g, port.as_ptr(), Some(on_packet), std::ptr::null_mut()),
            0,
            "{}",
            last_error()
        );
        assert_eq!(flow_graph_start(g), 0);
        let pin = cs("in");
        let input = flow_graph_input(g, pin.as_ptr());
        for i in 0..3i32 {
            assert_eq!(flow_input_send(input, make_int_packet(i * 10, i as i64)), 0);
            // 主线程执行器:需要进入引擎才会推进(见 docs/design.md §7.9)
            assert_eq!(flow_graph_wait_until_idle(g), 0);
        }
        flow_graph_close_all_inputs(g);
        assert_eq!(flow_graph_wait_done(g), 0);
        assert_eq!(
            *SEEN.lock().expect("锁中毒"),
            vec![0, 10, 20],
            "observer 应按序收到每个包"
        );
        flow_input_free(input);
        flow_graph_free(g);
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
    flow_core::register_builtin_kernels();
    unsafe {
        flow_set_log_callback(Some(sink), std::ptr::null_mut());
        let g = flow_graph_new();
        // 让算子报错,从而触发引擎记录错误日志
        let y = cs(r#"
nodes:
  - { name: "s", kernel: "ScaleKernel", input_ports: ["in"], output_ports: ["out"], options: { factor: 2 } }
input_ports: ["in"]
output_ports: ["out"]
"#);
        assert_eq!(flow_graph_init_from_yaml(g, y.as_ptr()), 0);
        assert_eq!(flow_graph_start(g), 0);
        let pin = cs("in");
        let input = flow_graph_input(g, pin.as_ptr());
        // type_id 与契约声明的 int 不符 → 引擎记录错误
        let mut bad = make_int_packet(1, 0);
        bad.type_id = 999;
        let _ = flow_input_send(input, bad);
        let _ = flow_graph_wait_until_idle(g);
        assert!(COUNT.load(Ordering::SeqCst) > 0, "错误应经日志回调透出");
        // 图级错误文本必须能拿到(flow_last_error 是线程局部的,拿不到工作线程的)
        let ge = CStr::from_ptr(flow_graph_last_error(g))
            .to_string_lossy()
            .into_owned();
        assert!(ge.contains("类型不符"), "图级错误: {ge}");
        flow_input_free(input);
        flow_graph_free(g);
        flow_set_log_callback(None, std::ptr::null_mut());
    }
}
