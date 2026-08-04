//! `LMFLOW_TYPE_HOST_OBJECT`(7)是**预留未启用**的 payload 类型(ADR #26)。
//!
//! 此前引擎对它**没有任何校验**:契约声明 7、包也带 7,`check_input_types` 按数值相等
//! 就放行了。也就是说「未启用」实际只靠「没有构造函数生产它」维持 —— 一旦有人用
//! C 侧手填 `type_id = 7` 或 Rust unsafe `from_foreign` 绕过,宿主语言原生对象就静默进入了
//! 数据流,而 ADR #9 拒绝它的两条理由都还成立:
//!
//!   1. 图是 YAML 描述的,看不出节点是什么语言写的 → 出现两级类型系统,某些包只能在
//!      同语言子图里流动,接到异语言算子上只会拿到无法解读的不透明指针;
//!   2. 这类对象的引用归零**可能发生在引擎工作线程上**,而释放它需要抢 GIL —— 死锁隐患。
//!
//! 本文件钉住两条入口都被明确拒绝。纯 Rust 算子,故两种 feature 配置下都跑。

use lmflow::packet::type_id;
use lmflow::{register_kernel, Graph, Kernel, KernelContract, KernelCtx, Packet, Timestamp};

/// 契约里声明 HOST_OBJECT 的算子 —— 建图期就该被拒。
#[derive(Default)]
struct DeclaresHostObject;
impl Kernel for DeclaresHostObject {
    fn get_contract(c: &mut KernelContract) {
        c.input_type(0, type_id::HOST_OBJECT);
    }
    fn process(&mut self, cc: &mut KernelCtx) -> lmflow::Result<()> {
        cc.forward(0, 0)
    }
}

/// 输出口声明 HOST_OBJECT —— 同样该被拒(两个方向都要堵)。
#[derive(Default)]
struct EmitsHostObject;
impl Kernel for EmitsHostObject {
    fn get_contract(c: &mut KernelContract) {
        c.input_any(0);
        c.output_type(0, type_id::HOST_OBJECT);
    }
    fn process(&mut self, cc: &mut KernelCtx) -> lmflow::Result<()> {
        cc.forward(0, 0)
    }
}

/// 契约声明 any、但实际产出 HOST_OBJECT 的源 —— 用来钉住「无下游节点、直接到图输出」
/// 这条路径。输入类型校验看不到源产出,必须在 flush 前检查暂存输出。
#[derive(Default)]
struct HostObjectSource;
impl Kernel for HostObjectSource {
    fn process(&mut self, cc: &mut KernelCtx) -> lmflow::Result<()> {
        let ptr = Box::into_raw(Box::new(1i64)) as *mut std::ffi::c_void;
        unsafe extern "C" fn drop_i64(p: *mut std::ffi::c_void) {
            drop(unsafe { Box::from_raw(p as *mut i64) });
        }
        cc.emit(0, unsafe {
            Packet::from_foreign(ptr, type_id::HOST_OBJECT, Some(drop_i64))
        })?;
        cc.source_done();
        Ok(())
    }
}

/// 在 close 阶段产出 HOST_OBJECT,用来覆盖独立于 process 的刷新分支。
#[derive(Default)]
struct HostObjectOnClose;
impl Kernel for HostObjectOnClose {
    fn process(&mut self, _cc: &mut KernelCtx) -> lmflow::Result<()> {
        Ok(())
    }

    fn close(&mut self, cc: &mut KernelCtx) -> lmflow::Result<()> {
        let ptr = Box::into_raw(Box::new(1i64)) as *mut std::ffi::c_void;
        unsafe extern "C" fn drop_i64(p: *mut std::ffi::c_void) {
            drop(unsafe { Box::from_raw(p as *mut i64) });
        }
        cc.emit(0, unsafe {
            Packet::from_foreign(ptr, type_id::HOST_OBJECT, Some(drop_i64))
        })
    }
}

fn one_node(kernel: &str) -> String {
    format!(
        r#"
nodes:
  - {{ name: n, kernel: {kernel}, input_ports: ["in"], output_ports: ["out"] }}
input_ports: ["in"]
output_ports: ["out"]
"#
    )
}

/// 入口一:**契约**声明 HOST_OBJECT → 建图期报错。
///
/// 建图期而非运行期,是因为这属于配置/契约错误 —— 与本项目「配置用到未实现特性就在
/// 建图期报错、绝不静默忽略」一致(§0.2)。
#[test]
fn contract_declaring_host_object_is_rejected_at_build() {
    let _ = register_kernel::<DeclaresHostObject>("DeclaresHostObject");
    let err = Graph::from_yaml(&one_node("DeclaresHostObject")).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("HOST_OBJECT") && msg.contains("not enabled"),
        "应指明该类型未启用: {msg}"
    );
    assert!(
        msg.contains("input port 0"),
        "应指出是哪个方向的哪个口: {msg}"
    );
    // 报错必须给出替代方案,否则用户只知道「不行」不知道「那用什么」。
    assert!(
        msg.contains("LMFLOW_TYPE_BUFFER") && msg.contains("LMFLOW_TYPE_STR"),
        "应给出替代方案(BUFFER / STR+JSON): {msg}"
    );
}

/// 输出口方向同样被拒。
#[test]
fn output_contract_declaring_host_object_is_rejected() {
    let _ = register_kernel::<EmitsHostObject>("EmitsHostObject");
    let msg = Graph::from_yaml(&one_node("EmitsHostObject"))
        .unwrap_err()
        .to_string();
    assert!(
        msg.contains("HOST_OBJECT") && msg.contains("output port 0"),
        "输出口方向也要拒: {msg}"
    );
}

/// 入口二:契约声明 `any`,但**包自己**带 type_id = 7 → 运行期报错。
///
/// 这条是真正容易漏的那个:`any` 端口的数值相等检查根本不会看包的类型,所以校验必须
/// 排在 `want == 0` 的短路**之前**。把 `HOST_OBJECT` 的判断放到短路之后,这条测试就会失败。
#[test]
fn packet_carrying_host_object_is_rejected_at_runtime() {
    unsafe extern "C" fn drop_i64(p: *mut std::ffi::c_void) {
        drop(unsafe { Box::from_raw(p as *mut i64) });
    }
    // PassThrough 声明的是 any —— 正是漏网最可能发生的配置
    let graph = Graph::from_yaml(&one_node("PassThrough")).unwrap();
    graph.add_poller("out").unwrap();
    graph.start().unwrap();

    let ptr = Box::into_raw(Box::new(1i64)) as *mut std::ffi::c_void;
    let pkt =
        unsafe { Packet::from_foreign(ptr, type_id::HOST_OBJECT, Some(drop_i64)) }.at(Timestamp(0));
    graph.input("in").unwrap().send(pkt).unwrap();
    graph.close_all_inputs();

    let msg = graph.wait_done().unwrap_err().to_string();
    assert!(
        msg.contains("HOST_OBJECT") && msg.contains("not enabled"),
        "any 端口也必须拒掉带 HOST_OBJECT 的包: {msg}"
    );
    assert!(
        msg.contains("LMFLOW_TYPE_BUFFER"),
        "同样要给替代方案: {msg}"
    );
}

/// 对照:同一张 any 端口的图,换成正常的内建类型就该通过 ——
/// 证明上面拒的是 HOST_OBJECT 本身,而不是把 `any` 端口整个弄坏了。
#[test]
fn any_port_still_accepts_normal_packets() {
    let graph = Graph::from_yaml(&one_node("PassThrough")).unwrap();
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    graph
        .input("in")
        .unwrap()
        .send(Packet::from_i64(9).at(Timestamp(0)))
        .unwrap();
    graph.close_all_inputs();
    graph.wait_done().unwrap();
    assert_eq!(poller.next().and_then(|p| p.as_i64()), Some(9));
}

/// 入口三:**算子输出**自己带 HOST_OBJECT,且边直接连图输出 → 必须在交给
/// poller / observer 前拒绝。否则源节点没有输入可检查,包会完整绕过运行期门禁。
#[test]
fn source_output_carrying_host_object_is_rejected_before_dispatch() {
    let _ = register_kernel::<HostObjectSource>("HostObjectSource");
    let graph = Graph::from_yaml(
        r#"
executors:
  - { name: pool, type: ThreadPoolExecutor, num_threads: 1 }
nodes:
  - { name: source, kernel: HostObjectSource, input_ports: [], output_ports: [out], executor: pool }
output_ports: [out]
"#,
    )
    .unwrap();
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();

    let msg = graph.wait_done().unwrap_err().to_string();
    assert!(
        msg.contains("HOST_OBJECT") && msg.contains("output port `out`"),
        "应在输出派发前指出非法类型和端口: {msg}"
    );
    assert!(
        poller.try_next().is_none(),
        "非法 HOST_OBJECT 不得到达图输出"
    );
}

/// close 有独立的调用与刷新路径,同样不得把 HOST_OBJECT 交给图输出。
#[test]
fn close_output_carrying_host_object_is_rejected_before_dispatch() {
    let _ = register_kernel::<HostObjectOnClose>("HostObjectOnClose");
    let graph = Graph::from_yaml(&one_node("HostObjectOnClose")).unwrap();
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    graph
        .input("in")
        .unwrap()
        .send(Packet::from_i64(1).at(Timestamp(0)))
        .unwrap();
    graph.close_all_inputs();

    let msg = graph.wait_done().unwrap_err().to_string();
    assert!(
        msg.contains("HOST_OBJECT") && msg.contains("output port `out`"),
        "close 产出也应在派发前被拒: {msg}"
    );
    assert!(
        poller.try_next().is_none(),
        "close 产出的非法 HOST_OBJECT 不得到达图输出"
    );
}

/// 入口四:side packet 不走普通输入队列,也必须在注入时拒绝,否则算子能从 Context
/// 直接取得这个未启用类型。
#[test]
fn side_packet_carrying_host_object_is_rejected() {
    let graph = Graph::from_yaml(&one_node("PassThrough")).unwrap();
    let ptr = Box::into_raw(Box::new(1i64)) as *mut std::ffi::c_void;
    unsafe extern "C" fn drop_i64(p: *mut std::ffi::c_void) {
        drop(unsafe { Box::from_raw(p as *mut i64) });
    }
    let err = graph
        .set_side_packet("native", unsafe {
            Packet::from_foreign(ptr, type_id::HOST_OBJECT, Some(drop_i64))
        })
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("side packet `native`") && msg.contains("HOST_OBJECT"),
        "应指出非法 side packet: {msg}"
    );
}
