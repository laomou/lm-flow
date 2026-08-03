//! 图重跑(`reset`)验收 —— **纯 Rust,零 C++**。
//!
//! reset 的价值:Terminated 后复位重跑,**保留已 open 的算子实例**(省重载模型)。
//! 高风险点是「复位不彻底」—— 漏清某个运行态字段会让下一轮跑出隐蔽错误。本文件专门
//! 钉几个最易漏的:时间戳单调性(Edge::last_sent)、旧错误残留(GraphShared)、
//! input_bounds、统计归零;并反向钉「open 不该重跑」。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use lmflow::{register_kernel, Graph, Kernel, KernelCtx, Packet, State, Timestamp};

/// 记录 open 被调用了几次(全局)—— 用来证明 reset **不**重跑 open。
static OPENS: AtomicUsize = AtomicUsize::new(0);

/// 直通,但在 open 时累加全局计数。
#[derive(Default)]
struct CountOpen;
impl Kernel for CountOpen {
    fn open(&mut self, _cc: &mut KernelCtx) -> lmflow::Result<()> {
        OPENS.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn process(&mut self, cc: &mut KernelCtx) -> lmflow::Result<()> {
        cc.forward(0, 0)
    }
}

/// 奇数报错 —— 用来制造「错误终止」,再验证 reset 清掉了旧错误。
#[derive(Default)]
struct FailOnFive;
impl Kernel for FailOnFive {
    fn process(&mut self, cc: &mut KernelCtx) -> lmflow::Result<()> {
        let v = cc.input(0).and_then(|p| p.as_i64()).unwrap_or(0);
        if v == 5 {
            return Err(cc.fail("boom on 5"));
        }
        cc.forward(0, 0)
    }
}

fn reg() {
    let _ = register_kernel::<CountOpen>("CountOpenK");
    let _ = register_kernel::<FailOnFive>("FailOnFiveK");
}

fn one_node(kernel: &str) -> String {
    format!(
        "nodes:\n  - {{ name: k, kernel: {kernel}, input_ports: [\"in\"], output_ports: [\"out\"] }}\n\
         input_ports: [\"in\"]\noutput_ports: [\"out\"]\n"
    )
}

/// 跑一轮:喂 0..n(时间戳从 0 起),收齐输出。
fn run_round(g: &Graph, out: &lmflow::Poller, n: i64) -> Vec<i64> {
    g.start().unwrap();
    let inp = g.input("in").unwrap();
    for i in 0..n {
        inp.send(Packet::from_i64(i).at(Timestamp(i))).unwrap();
    }
    g.close_all_inputs();
    let mut got = Vec::new();
    while (got.len() as i64) < n {
        match out.next() {
            Some(p) => got.push(p.as_i64().unwrap()),
            None => break,
        }
    }
    g.wait_done_timeout(Duration::from_secs(5)).unwrap();
    got
}

/// 正常跑 → reset → 再跑。第二轮时间戳又从 0 起(最易漏的 last_sent 若没清,
/// 单调性校验会拒掉它);poller 复用;统计归零;open 只在首轮跑一次。
#[test]
fn reset_allows_clean_rerun() {
    reg();
    OPENS.store(0, Ordering::SeqCst);
    let g = Graph::from_yaml(&one_node("CountOpenK")).unwrap();
    let out = g.add_poller("out").unwrap(); // 复用同一个 poller 句柄跨两轮

    let r1 = run_round(&g, &out, 5);
    assert_eq!(r1, vec![0, 1, 2, 3, 4], "第一轮");
    assert_eq!(g.state(), State::Terminated);
    let opens_after_r1 = OPENS.load(Ordering::SeqCst);
    assert_eq!(
        opens_after_r1, 1,
        "open 应只在首轮调一次(实际 {opens_after_r1})"
    );
    // 统计:第一轮处理了 5 个
    assert_eq!(g.node_stats(0).unwrap().processed, 5);

    g.reset().expect("Terminated 后应能 reset");
    assert_eq!(g.state(), State::Initialized, "reset 后回到可 start");
    assert_eq!(g.node_stats(0).unwrap().processed, 0, "reset 必须清零统计");

    // 第二轮:时间戳又从 0 起 —— last_sent 没清的话这里会因单调性校验失败。
    let r2 = run_round(&g, &out, 5);
    assert_eq!(r2, vec![0, 1, 2, 3, 4], "第二轮(时间戳复用,last_sent 已清)");
    assert_eq!(
        OPENS.load(Ordering::SeqCst),
        1,
        "reset **不该**重跑 open —— 算子实例被保留(这正是 reset 的价值)"
    );
    assert_eq!(g.node_stats(0).unwrap().processed, 5, "第二轮又处理 5 个");
}

/// 错误终止 → reset → 干净重跑。钉「旧 error 不残留」(GraphShared 无现成清除路径,最易漏)。
#[test]
fn reset_clears_prior_error() {
    reg();
    let g = Graph::from_yaml(&one_node("FailOnFiveK")).unwrap();
    let out = g.add_poller("out").unwrap();

    // 第一轮:喂到 5 触发错误
    g.start().unwrap();
    let inp = g.input("in").unwrap();
    for i in 0..8i64 {
        let _ = inp.send(Packet::from_i64(i).at(Timestamp(i)));
    }
    let _ = out.next();
    g.close_all_inputs();
    let err = g.wait_done_timeout(Duration::from_secs(5));
    assert!(err.is_err(), "第一轮应因 5 而出错");

    // reset 后旧错误必须消失,第二轮只喂不触发错误的值,应干净跑通
    g.reset().expect("错误终止后也应能 reset");
    let out2 = g.add_poller("out").unwrap();
    g.start().unwrap();
    let inp = g.input("in").unwrap();
    for i in [0i64, 1, 2, 3] {
        inp.send(Packet::from_i64(i).at(Timestamp(i))).unwrap();
    }
    g.close_all_inputs();
    let mut got = Vec::new();
    while got.len() < 4 {
        match out2.next() {
            Some(p) => got.push(p.as_i64().unwrap()),
            None => break,
        }
    }
    g.wait_done_timeout(Duration::from_secs(5))
        .expect("reset 后第二轮不该带着上一轮的错误");
    assert_eq!(got, vec![0, 1, 2, 3]);
}

/// 在 Running(未终止)上 reset 必须被拒 —— 复位运行中的图是灾难。
#[test]
fn reset_on_running_is_rejected() {
    reg();
    let g = Graph::from_yaml(&one_node("PassThrough")).unwrap();
    g.add_poller("out").unwrap();
    g.start().unwrap();
    // 此刻 Running,未 wait_done
    let err = g.reset().expect_err("Running 上 reset 必须报错");
    assert!(err.to_string().contains("Terminated"), "{err}");
    // 收尾
    g.close_all_inputs();
    let _ = g.wait_done_timeout(Duration::from_secs(5));
}
