//! Rust 算子编写糖(`trait Kernel` + `register_kernel`)端到端验收。
//!
//! 用纯 Rust 写算子、注册、建图跑通;并验证 `process` 返 `Err` / panic 都被兜住成图错误(不崩)。

use lmflow::{register_kernel, Graph, Kernel, KernelContract, KernelCtx, Packet, State, Timestamp};

// ---- 一个读 option 的 Rust 算子:out = in * factor ----
#[derive(Default)]
struct RustDouble {
    factor: i64,
}
impl Kernel for RustDouble {
    fn get_contract(c: &mut KernelContract) {
        c.input_type(0, lmflow::packet::type_id::I64);
        c.output_type(0, lmflow::packet::type_id::I64);
    }
    fn open(&mut self, cc: &mut KernelCtx) -> lmflow::Result<()> {
        self.factor = cc.option_i64("factor", 2);
        Ok(())
    }
    fn process(&mut self, cc: &mut KernelCtx) -> lmflow::Result<()> {
        let v = cc
            .input(0)
            .and_then(|p| p.as_i64())
            .ok_or_else(|| cc.fail("input is not an i64"))?;
        cc.emit(0, Packet::from_i64(v * self.factor))
    }
}

#[test]
fn rust_kernel_runs() {
    register_kernel::<RustDouble>("RustDouble").unwrap();
    let g = Graph::from_yaml(
        r#"
nodes:
  - { name: d, kernel: RustDouble, input_ports: ["in"], output_ports: ["out"], options: { factor: 3 } }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    let out = g.add_poller("out").unwrap();
    g.start().unwrap();
    let inp = g.input("in").unwrap();
    let mut got = Vec::new();
    for i in 0..4i64 {
        inp.send(Packet::from_i64(i).at(Timestamp(i))).unwrap();
        got.push(out.next().unwrap().as_i64().unwrap());
    }
    g.close_all_inputs();
    g.wait_done().unwrap();
    assert_eq!(got, vec![0, 3, 6, 9], "out = in * factor(3)");
    assert_eq!(g.state(), State::Terminated);
}

// ---- process 返回 Err → 图报错(带上原因)----
#[derive(Default)]
struct RustBoom;
impl Kernel for RustBoom {
    fn process(&mut self, cc: &mut KernelCtx) -> lmflow::Result<()> {
        Err(cc.fail("deliberate failure"))
    }
}

#[test]
fn rust_kernel_error_fails_graph() {
    register_kernel::<RustBoom>("RustBoom").unwrap();
    let g = Graph::from_yaml(
        r#"
nodes:
  - { name: b, kernel: RustBoom, input_ports: ["in"], output_ports: ["out"] }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    g.add_poller("out").unwrap();
    g.start().unwrap();
    g.input("in")
        .unwrap()
        .send(Packet::from_i64(1).at(Timestamp(0)))
        .unwrap();
    g.close_all_inputs();
    let err = g.wait_done().unwrap_err();
    assert!(err.to_string().contains("deliberate failure"), "{err}");
}

// ---- process panic → catch_unwind 兜住,图报错、进程不崩 ----
#[derive(Default)]
struct RustPanic;
impl Kernel for RustPanic {
    fn process(&mut self, _cc: &mut KernelCtx) -> lmflow::Result<()> {
        panic!("kernel goes boom");
    }
}

#[test]
fn rust_kernel_panic_is_caught() {
    register_kernel::<RustPanic>("RustPanic").unwrap();
    let g = Graph::from_yaml(
        r#"
nodes:
  - { name: p, kernel: RustPanic, input_ports: ["in"], output_ports: ["out"] }
input_ports: ["in"]
output_ports: ["out"]
"#,
    )
    .unwrap();
    g.add_poller("out").unwrap();
    g.start().unwrap();
    g.input("in")
        .unwrap()
        .send(Packet::from_i64(1).at(Timestamp(0)))
        .unwrap();
    g.close_all_inputs();
    let err = g.wait_done().unwrap_err();
    assert!(
        err.to_string().contains("panic"),
        "panic must surface as a graph error: {err}"
    );
}
