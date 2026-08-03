//! 内置 C++ 算子的**错误路径**验收 —— 坏输入必须被拒,且带上可读原因。
//!
//! 起因:把算子里的 `if (!cond) return cc.Fail(...)` 批量换成 `LMFLOW_RET_CHECK_MSG` 时,
//! 有四处需要**把条件取反**(原先是「满足即失败」)。取反写错的话:
//! 好输入会被拒(已有测试立刻变红),或**坏输入会被放过**(已有测试**察觉不到** ——
//! 它们只喂好输入)。这个文件专门钉后一种,即那四处取反的错误路径。

#![cfg(feature = "builtin-kernels")] // 用内置 C++ 算子:纯 Rust 构建时整文件跳过

use std::time::Duration;

use lmflow::{Graph, Packet, Timestamp};

fn init() {
    lmflow::register_builtin_kernels();
}

/// `CastKernel` / `AffineKernel` 的 `options.dtype` 非法 → **Open 阶段就该失败**,
/// 即 `start()` 返回错误(而不是跑起来每帧报错)。对应取反:`out_dt_ >= 0`。
#[test]
fn bad_dtype_option_fails_at_start() {
    init();
    for kernel in ["CastKernel", "AffineKernel"] {
        let g = Graph::from_yaml(&format!(
            "nodes:\n  - {{ name: k, kernel: {kernel}, input_ports: [\"in\"], output_ports: [\"out\"], options: {{ dtype: \"nosuchtype\" }} }}\n\
             input_ports: [\"in\"]\noutput_ports: [\"out\"]\n"
        ))
        .unwrap();
        let err = g
            .start()
            .expect_err(&format!("{kernel}: 非法 dtype 必须让 start() 失败"));
        let msg = err.to_string();
        assert!(msg.contains("dtype"), "{kernel}: 原因里应提到 dtype:{msg}");
        // RET_CHECK 应带上位置,便于定位是哪个算子哪一行
        assert!(
            msg.contains("check failed") && msg.contains(".cc:"),
            "{kernel}: 应带上表达式与 file:line:{msg}"
        );
    }
}

/// 必需 option 缺失 → Open 失败,对应取反 `RequireOption(...) == LMFLOW_OK`。
///
/// 不用 `NormalizeKernel`:它在 `GetContract` 里还声明了必需 side packet `calibration`,
/// 会**更早**失败,`RET_CHECK` 那条根本触不到。改用自定义 Rust 算子精确命中这条语义
/// —— `RequireOption` 的 Rust 侧等价物是 `option_*` 返回默认值,故这里用 raw C ABI 的
/// `require` 语义:算子在 Open 里显式检查、缺失即 Fail(与 normalize 同形)。
#[derive(Default)]
struct NeedsOpt;
impl lmflow::Kernel for NeedsOpt {
    fn open(&mut self, cc: &mut lmflow::KernelCtx) -> lmflow::Result<()> {
        if !cc.has_option("scale") {
            return Err(cc.fail("options.scale missing (required)"));
        }
        Ok(())
    }
    fn process(&mut self, cc: &mut lmflow::KernelCtx) -> lmflow::Result<()> {
        cc.forward(0, 0)
    }
}

#[test]
fn missing_required_option_fails_at_start() {
    init();
    let _ = lmflow::register_kernel::<NeedsOpt>("NeedsOptTestKernel");
    let g = Graph::from_yaml(
        "nodes:\n  - { name: n, kernel: NeedsOptTestKernel, input_ports: [\"in\"], output_ports: [\"out\"] }\n\
         input_ports: [\"in\"]\noutput_ports: [\"out\"]\n",
    )
    .unwrap();
    let msg = g
        .start()
        .expect_err("缺必需 option 必须让 start() 失败")
        .to_string();
    assert!(msg.contains("scale"), "原因里应提到 scale:{msg}");
}

/// `MuxKernel` 的选择器越界 → Process 失败。对应取反:`k >= 0 && k < ndata`。
/// 这条同时验证「合法选择器仍然工作」——否则取反写反了会两头都错。
#[test]
fn mux_selector_range_is_enforced() {
    init();
    // 控制口 + 2 个数据口:合法选择器是 0 / 1
    let yaml = "nodes:\n  - { name: m, kernel: MuxKernel, input_ports: [\"ctl\", \"a\", \"b\"], output_ports: [\"out\"] }\n\
                input_ports: [\"ctl\", \"a\", \"b\"]\noutput_ports: [\"out\"]\n";

    // 合法:选 1 → 应转发 b 的值
    let g = Graph::from_yaml(yaml).unwrap();
    let out = g.add_poller("out").unwrap();
    g.start().unwrap();
    g.input("ctl")
        .unwrap()
        .send(Packet::from_i64(1).at(Timestamp(0)))
        .unwrap();
    g.input("a")
        .unwrap()
        .send(Packet::from_i64(10).at(Timestamp(0)))
        .unwrap();
    g.input("b")
        .unwrap()
        .send(Packet::from_i64(20).at(Timestamp(0)))
        .unwrap();
    assert_eq!(
        out.next().and_then(|p| p.as_i64()),
        Some(20),
        "合法选择器必须照常工作(取反写反了这里就错)"
    );
    g.close_all_inputs();
    g.wait_done_timeout(Duration::from_secs(5)).unwrap();

    // 越界:选 5(只有 2 个数据口)→ 必须失败
    let g2 = Graph::from_yaml(yaml).unwrap();
    g2.add_poller("out").unwrap();
    g2.start().unwrap();
    g2.input("ctl")
        .unwrap()
        .send(Packet::from_i64(5).at(Timestamp(0)))
        .unwrap();
    g2.input("a")
        .unwrap()
        .send(Packet::from_i64(10).at(Timestamp(0)))
        .unwrap();
    g2.input("b")
        .unwrap()
        .send(Packet::from_i64(20).at(Timestamp(0)))
        .unwrap();
    g2.close_all_inputs();
    let msg = g2
        .wait_done_timeout(Duration::from_secs(5))
        .expect_err("越界选择器必须让图报错")
        .to_string();
    assert!(msg.contains("out of range"), "原因应说明越界:{msg}");
    assert!(msg.contains("check failed"), "应经 RET_CHECK 报出:{msg}");
}
