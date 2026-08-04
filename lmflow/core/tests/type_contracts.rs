//! 端口类型契约的两级校验:
//! * producer 与 consumer 都声明具体类型时,建图期即可证明不兼容并拒绝;
//! * 任一侧声明 ANY 时保留动态图能力,实际包在运行期校验;
//! * 算子必须兑现自己的输出契约,包括直接连图输出的路径。

use lmflow::packet::type_id;
use lmflow::{register_kernel, Graph, Kernel, KernelContract, KernelCtx, Packet, Timestamp};

#[derive(Default)]
struct I64Identity;
impl Kernel for I64Identity {
    fn get_contract(c: &mut KernelContract) {
        c.input_type(0, type_id::I64);
        c.output_type(0, type_id::I64);
    }

    fn process(&mut self, cc: &mut KernelCtx) -> lmflow::Result<()> {
        cc.forward(0, 0)
    }
}

#[derive(Default)]
struct F64Sink;
impl Kernel for F64Sink {
    fn get_contract(c: &mut KernelContract) {
        c.input_type(0, type_id::F64);
    }

    fn process(&mut self, _cc: &mut KernelCtx) -> lmflow::Result<()> {
        Ok(())
    }
}

#[derive(Default)]
struct AnyIdentity;
impl Kernel for AnyIdentity {
    fn get_contract(c: &mut KernelContract) {
        c.input_any(0);
        c.output_any(0);
    }

    fn process(&mut self, cc: &mut KernelCtx) -> lmflow::Result<()> {
        cc.forward(0, 0)
    }
}

#[derive(Default)]
struct LiesAboutOutput;
impl Kernel for LiesAboutOutput {
    fn get_contract(c: &mut KernelContract) {
        c.input_any(0);
        c.output_type(0, type_id::I64);
    }

    fn process(&mut self, cc: &mut KernelCtx) -> lmflow::Result<()> {
        cc.emit(0, Packet::from_f64(1.5))
    }
}

fn register_test_kernels() {
    let _ = register_kernel::<I64Identity>("TypeTestI64Identity");
    let _ = register_kernel::<F64Sink>("TypeTestF64Sink");
    let _ = register_kernel::<AnyIdentity>("TypeTestAnyIdentity");
    let _ = register_kernel::<LiesAboutOutput>("TypeTestLiesAboutOutput");
}

#[test]
fn concrete_contract_mismatch_is_rejected_at_build() {
    register_test_kernels();
    let err = Graph::from_yaml(
        r#"
nodes:
  - { name: produce_i64, kernel: TypeTestI64Identity, input_ports: [in], output_ports: [mid] }
  - { name: consume_f64, kernel: TypeTestF64Sink, input_ports: [mid], output_ports: [] }
input_ports: [in]
"#,
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("edge `mid`")
            && msg.contains("produce_i64")
            && msg.contains("consume_f64")
            && msg.contains("I64")
            && msg.contains("F64"),
        "应在建图期给出完整连接诊断: {msg}"
    );
}

#[test]
fn matching_concrete_contracts_build_and_run() {
    register_test_kernels();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - { name: first, kernel: TypeTestI64Identity, input_ports: [in], output_ports: [mid] }
  - { name: second, kernel: TypeTestI64Identity, input_ports: [mid], output_ports: [out] }
input_ports: [in]
output_ports: [out]
"#,
    )
    .unwrap();
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    graph
        .input("in")
        .unwrap()
        .send(Packet::from_i64(7).at(Timestamp(0)))
        .unwrap();
    graph.close_all_inputs();
    graph.wait_done().unwrap();
    assert_eq!(poller.next().and_then(|p| p.as_i64()), Some(7));
}

#[test]
fn typed_producer_to_any_consumer_is_allowed() {
    register_test_kernels();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - { name: typed, kernel: TypeTestI64Identity, input_ports: [in], output_ports: [mid] }
  - { name: dynamic, kernel: TypeTestAnyIdentity, input_ports: [mid], output_ports: [out] }
input_ports: [in]
output_ports: [out]
"#,
    )
    .expect("typed → ANY 应静态兼容");
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    graph
        .input("in")
        .unwrap()
        .send(Packet::from_i64(8).at(Timestamp(0)))
        .unwrap();
    graph.close_all_inputs();
    graph.wait_done().unwrap();
    assert_eq!(poller.next().and_then(|p| p.as_i64()), Some(8));
}

#[test]
fn any_producer_to_typed_consumer_remains_runtime_checked() {
    register_test_kernels();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - { name: dynamic, kernel: TypeTestAnyIdentity, input_ports: [in], output_ports: [mid] }
  - { name: consume_f64, kernel: TypeTestF64Sink, input_ports: [mid], output_ports: [] }
input_ports: [in]
"#,
    )
    .expect("ANY → typed 无法静态证明不兼容,应允许建图");
    graph.start().unwrap();
    graph
        .input("in")
        .unwrap()
        .send(Packet::from_i64(1).at(Timestamp(0)))
        .unwrap();
    graph.close_all_inputs();
    let msg = graph.wait_done().unwrap_err().to_string();
    assert!(
        msg.contains("input port `mid`") && msg.contains("I64") && msg.contains("F64"),
        "实际包仍应在 typed consumer 入口失败: {msg}"
    );
}

#[test]
fn kernel_output_must_satisfy_its_declared_contract() {
    register_test_kernels();
    let graph = Graph::from_yaml(
        r#"
nodes:
  - { name: liar, kernel: TypeTestLiesAboutOutput, input_ports: [in], output_ports: [out] }
input_ports: [in]
output_ports: [out]
"#,
    )
    .unwrap();
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
        msg.contains("output port `out`") && msg.contains("I64") && msg.contains("F64"),
        "应指出算子违反自己的输出契约: {msg}"
    );
    assert!(
        poller.try_next().is_none(),
        "违反输出契约的包不得先到达图输出"
    );
}
