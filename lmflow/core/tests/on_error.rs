//! 节点级错误策略(`on_error`)验收。
//!
//! 设计取舍见 §7.6:**能在算子内处理的错误就在算子内处理** —— 返回 `Ok` 而不产出,
//! 引擎会自动推进下游时间戳边界(本文件第一条测试就钉这个)。`on_error: skip` 是给
//! 你**管不到**的失败用的:引擎侧的契约类型校验失败、以及算子的 panic / C++ 异常
//! (那些没法「返回 Ok」)。

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use lmflow::{register_kernel, Graph, Kernel, KernelCtx, Packet, State, Timestamp};

/// 偶数放过、奇数**返回 Ok 但不产出**(算子自己吞掉)。
#[derive(Default)]
struct DropOdd;
impl Kernel for DropOdd {
    fn process(&mut self, cc: &mut KernelCtx) -> lmflow::Result<()> {
        let v = cc.input(0).and_then(|p| p.as_i64()).unwrap_or(0);
        if v % 2 == 0 {
            cc.forward(0, 0)
        } else {
            Ok(()) // 不产出 —— 引擎应自动推进边界,下游不该卡
        }
    }
}

/// 奇数直接 `Err`。配 `on_error` 用。
#[derive(Default)]
struct FailOdd;
impl Kernel for FailOdd {
    fn process(&mut self, cc: &mut KernelCtx) -> lmflow::Result<()> {
        let v = cc.input(0).and_then(|p| p.as_i64()).unwrap_or(0);
        if v % 2 == 0 {
            cc.forward(0, 0)
        } else {
            Err(cc.fail("odd packet rejected"))
        }
    }
}

/// 奇数 panic —— 算子**无法**自己「返回 Ok」的那类失败。
#[derive(Default)]
struct PanicOdd;
impl Kernel for PanicOdd {
    fn process(&mut self, cc: &mut KernelCtx) -> lmflow::Result<()> {
        let v = cc.input(0).and_then(|p| p.as_i64()).unwrap_or(0);
        if v % 2 != 0 {
            panic!("odd packet blows up");
        }
        cc.forward(0, 0)
    }
}

static ONCE: AtomicI64 = AtomicI64::new(0);
fn reg() {
    if ONCE.fetch_add(1, Ordering::SeqCst) == 0 {
        register_kernel::<DropOdd>("DropOdd").unwrap();
        register_kernel::<FailOdd>("FailOdd").unwrap();
        register_kernel::<PanicOdd>("PanicOdd").unwrap();
    }
}

fn graph(kernel: &str, on_error: &str) -> Graph {
    let oe = if on_error.is_empty() {
        String::new()
    } else {
        format!(", on_error: \"{on_error}\"")
    };
    Graph::from_yaml(&format!(
        "nodes:\n  - {{ name: k, kernel: {kernel}, input_ports: [\"in\"], output_ports: [\"mid\"]{oe} }}\n\
         \x20 - {{ name: t, kernel: PassThrough, input_ports: [\"mid\"], output_ports: [\"out\"] }}\n\
         input_ports: [\"in\"]\noutput_ports: [\"out\"]\n"
    ))
    .unwrap()
}

/// 喂 0..6,收 `want` 个输出。返回 (收到的值, 图最终状态, wait_done 是否出错)
fn drive(g: &Graph, want: usize) -> (Vec<i64>, State, Option<String>) {
    let out = g.add_poller("out").unwrap();
    g.start().unwrap();
    let inp = g.input("in").unwrap();
    for i in 0..6i64 {
        inp.send(Packet::from_i64(i).at(Timestamp(i))).unwrap();
    }
    g.close_all_inputs();
    let mut got = Vec::new();
    while got.len() < want {
        match out.next() {
            Some(p) => got.push(p.as_i64().unwrap()),
            None => break,
        }
    }
    let err = g
        .wait_done_timeout(Duration::from_secs(5))
        .err()
        .map(|e| e.to_string());
    (got, g.state(), err)
}

/// **前提验证**:算子返回 Ok 但不产出时,引擎自动推进下游边界 —— 下游不卡。
/// 这是「能自己处理的就自己处理」这条建议成立的基础。
#[test]
fn kernel_can_skip_by_not_emitting() {
    reg();
    let g = graph("DropOdd", "");
    let (got, state, err) = drive(&g, 3);
    assert_eq!(got, vec![0, 2, 4], "只放过偶数");
    assert_eq!(err, None, "算子自己吞掉,图不该出错");
    assert_eq!(
        state,
        State::Terminated,
        "不产出也必须能正常终止(边界已推进)"
    );
}

/// 默认 `abort`:一个包失败即终止全图(历史行为,不能变)。
#[test]
fn default_abort_fails_whole_graph() {
    reg();
    let g = graph("FailOdd", "");
    let (_got, _state, err) = drive(&g, 6);
    let err = err.expect("默认 abort 应让 wait_done 报错");
    assert!(err.contains("odd packet rejected"), "应带上原因:{err}");
}

/// `on_error: skip`:丢掉出错的包,**其余照常流过**,图正常终止。
#[test]
fn skip_survives_failing_packets() {
    reg();
    let g = graph("FailOdd", "skip");
    let (got, state, err) = drive(&g, 3);
    assert_eq!(err, None, "skip 不该让图出错:{err:?}");
    assert_eq!(got, vec![0, 2, 4], "偶数照常流过,奇数被丢");
    assert_eq!(
        state,
        State::Terminated,
        "必须正常终止 —— 说明边界推进了,下游没卡"
    );
    // 有损行为必须可观测
    let st = g.node_stats(0).unwrap();
    assert_eq!(st.errors, 3, "3 个奇数各记一次错");
    assert_eq!(st.processed, 3, "成功的只算 3 个");
}

/// `skip` 也要能兜住 panic —— 那是算子**无法**自己「返回 Ok」的失败。
#[test]
fn skip_also_survives_panics() {
    reg();
    let g = graph("PanicOdd", "skip");
    let (got, state, err) = drive(&g, 3);
    assert_eq!(err, None, "panic 也应被 skip 兜住:{err:?}");
    assert_eq!(got, vec![0, 2, 4]);
    assert_eq!(state, State::Terminated);
    assert_eq!(g.node_stats(0).unwrap().errors, 3);
}

/// 未知 `on_error` 值必须建图期明确报错,而不是静默当默认。
#[test]
fn unknown_on_error_is_rejected() {
    reg();
    let err = Graph::from_yaml(
        "nodes:\n  - { name: k, kernel: PassThrough, input_ports: [\"in\"], output_ports: [\"out\"], on_error: \"ignore\" }\ninput_ports: [\"in\"]\noutput_ports: [\"out\"]\n",
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("on_error"), "应指出是 on_error 的问题:{err}");
    assert!(err.contains("ignore"), "应回显非法值:{err}");
}
