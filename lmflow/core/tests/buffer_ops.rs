//! 张量预处理 BUFFER 算子(Cast / Affine / Clamp / Reduce)端到端验收。
//!
//! 用 Rust 的 `BufferData` 造输入缓冲、`payload()` 读回输出缓冲,穿过真实图。

#![cfg(feature = "builtin-kernels")] // 用内置 C++ 算子:纯 Rust 构建(--no-default-features)时整文件跳过

use lmflow::packet::Payload;
use lmflow::{BufferData, Builtin, Graph, Packet, Timestamp};

const DT_U8: i32 = 0;
const DT_F16: i32 = 6;

fn init() {
    lmflow::register_builtin_kernels();
}

/// 造一个一维 u8 缓冲包。
fn u8_buf(vals: &[u8]) -> Packet {
    let mut bd = BufferData::new(&[vals.len() as i64], DT_U8).unwrap();
    bd.bytes = vals.to_vec();
    Packet::from_builtin(Builtin::Buffer(bd)).at(Timestamp(0))
}

/// 造一个一维 F16 缓冲包。入参是**原始 binary16 位模式**(不是浮点值)——
/// 这样测试不需要在 Rust 侧再实现一份 half 转换,期望值也就无从「两边一起错」。
fn f16_buf(halves: &[u16]) -> Packet {
    let mut bd = BufferData::new(&[halves.len() as i64], DT_F16).unwrap();
    bd.bytes = halves.iter().flat_map(|h| h.to_ne_bytes()).collect();
    Packet::from_builtin(Builtin::Buffer(bd)).at(Timestamp(0))
}

/// 读回 F16 缓冲的原始位模式。
fn as_f16(bd: &BufferData) -> Vec<u16> {
    bd.bytes
        .chunks_exact(2)
        .map(|c| u16::from_ne_bytes([c[0], c[1]]))
        .collect()
}

/// 从收到的包里借出缓冲。
fn as_buf(p: &Packet) -> &BufferData {
    match p.payload() {
        Some(Payload::Builtin(Builtin::Buffer(bd))) => bd,
        _ => panic!("expected a buffer packet"),
    }
}
fn as_f32(bd: &BufferData) -> Vec<f32> {
    bd.bytes
        .chunks_exact(4)
        .map(|c| f32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}
fn as_f64(bd: &BufferData) -> Vec<f64> {
    bd.bytes
        .chunks_exact(8)
        .map(|c| f64::from_ne_bytes(c.try_into().unwrap()))
        .collect()
}

/// 送一个缓冲、取一个输出包(单节点图,喂一个即关流)。
fn run_one(cfg: &str, input: Packet) -> Option<Packet> {
    let graph = Graph::from_yaml(cfg).unwrap();
    let poller = graph.add_poller("out").unwrap();
    graph.start().unwrap();
    graph.input("in").unwrap().send(input).unwrap();
    graph.close_all_inputs();
    graph.wait_done().unwrap();
    poller.next()
}

#[test]
fn cast_u8_to_f32() {
    init();
    let cfg = r#"
nodes:
  - { name: c, kernel: CastKernel, input_ports: ["in"], output_ports: ["out"], options: { dtype: f32 } }
input_ports: ["in"]
output_ports: ["out"]
"#;
    let out = run_one(cfg, u8_buf(&[0, 128, 255])).expect("output");
    assert_eq!(as_f32(as_buf(&out)), vec![0.0, 128.0, 255.0]);
}

#[test]
fn affine_scale_shift() {
    init();
    let cfg = r#"
nodes:
  - { name: a, kernel: AffineKernel, input_ports: ["in"], output_ports: ["out"], options: { scale: 2.0, shift: 1.0, dtype: f64 } }
input_ports: ["in"]
output_ports: ["out"]
"#;
    let out = run_one(cfg, u8_buf(&[10, 20])).expect("output");
    assert_eq!(as_f64(as_buf(&out)), vec![21.0, 41.0]);
}

#[test]
fn clamp_min_max() {
    init();
    let cfg = r#"
nodes:
  - { name: k, kernel: ClampKernel, input_ports: ["in"], output_ports: ["out"], options: { min: 50, max: 200 } }
input_ports: ["in"]
output_ports: ["out"]
"#;
    let out = run_one(cfg, u8_buf(&[0, 128, 255])).expect("output");
    assert_eq!(as_buf(&out).bytes, vec![50u8, 128, 200]); // dtype 不变(u8)
}

#[test]
fn reduce_mean_emits_scalar() {
    init();
    let cfg = r#"
nodes:
  - { name: r, kernel: ReduceKernel, input_ports: ["in"], output_ports: ["out"], options: { op: mean } }
input_ports: ["in"]
output_ports: ["out"]
"#;
    let out = run_one(cfg, u8_buf(&[2, 4, 6])).expect("output");
    assert_eq!(out.as_f64(), Some(4.0)); // buffer → F64 标量
}

/// 真实前处理链:u8 图 → f32 → ×(1/255) → clamp(0,1)。
#[test]
fn preprocess_pipeline() {
    init();
    let cfg = r#"
nodes:
  - { name: cast,  kernel: CastKernel,  input_ports: ["in"],  output_ports: ["f"], options: { dtype: f32 } }
  - { name: norm,  kernel: AffineKernel, input_ports: ["f"],  output_ports: ["n"], options: { scale: 0.00392156862745098 } }
  - { name: clamp, kernel: ClampKernel, input_ports: ["n"],  output_ports: ["out"], options: { min: 0.0, max: 1.0 } }
input_ports: ["in"]
output_ports: ["out"]
"#;
    let out = run_one(cfg, u8_buf(&[0, 128, 255])).expect("output");
    let got = as_f32(as_buf(&out));
    let want = [0.0f32, 128.0 / 255.0, 1.0];
    for (g, w) in got.iter().zip(want.iter()) {
        assert!((g - w).abs() < 1e-4, "got {got:?} want {want:?}");
    }
}

/// F16 输入被正确读取(binary16 → f32 精确加宽)。
///
/// 期望值是**硬编码的 IEEE 754 binary16 位模式**,C++ 侧的转换是自己实现的
/// (不用 `_Float16`,它在 MSVC 上不可移植),转换本身的穷举验证在
/// `cpp/tests/buffer_util_test.cc`;这里只验「端到端穿过真实图后值还对」。
#[test]
fn cast_f16_to_f32() {
    init();
    let cfg = r#"
nodes:
  - { name: c, kernel: CastKernel, input_ports: ["in"], output_ports: ["out"], options: { dtype: f32 } }
input_ports: ["in"]
output_ports: ["out"]
"#;
    // 0.0 · 0.5 · -1.5 · 2.0 —— 都在 half 里精确可表示,故可用等号断言
    let out = run_one(cfg, f16_buf(&[0x0000, 0x3800, 0xBE00, 0x4000])).expect("output");
    assert_eq!(as_f32(as_buf(&out)), vec![0.0, 0.5, -1.5, 2.0]);
}

/// F16 作为**输出** dtype:写出的位模式必须与 IEEE binary16 逐位一致。
#[test]
fn cast_f32_to_f16() {
    init();
    let cfg = r#"
nodes:
  - { name: c, kernel: CastKernel, input_ports: ["in"], output_ports: ["out"], options: { dtype: f16 } }
input_ports: ["in"]
output_ports: ["out"]
"#;
    // u8 0/64/128/255 原样转 half(整数,精确)
    let out = run_one(cfg, u8_buf(&[0, 64, 128, 255])).expect("output");
    assert_eq!(as_f16(as_buf(&out)), vec![0x0000, 0x5400, 0x5800, 0x5BF8]);
}

/// 真实的移动端推理前处理链:u8 图 → f32 → ×(1/255) → **f16 张量**。
/// F16 之所以要支持,就是因为它是移动端推理的标准张量 dtype。
#[test]
fn preprocess_to_f16_tensor() {
    init();
    let cfg = r#"
nodes:
  - { name: cast, kernel: CastKernel,   input_ports: ["in"], output_ports: ["f"], options: { dtype: f32 } }
  - { name: norm, kernel: AffineKernel, input_ports: ["f"],  output_ports: ["n"], options: { scale: 0.00392156862745098 } }
  - { name: half, kernel: CastKernel,   input_ports: ["n"],  output_ports: ["out"], options: { dtype: f16 } }
input_ports: ["in"]
output_ports: ["out"]
"#;
    let out = run_one(cfg, u8_buf(&[0, 64, 128, 255])).expect("output");
    // 0 → 0x0000;64/255 → 0x3404;128/255 → 0x3804;255/255 = 1.0 → 0x3C00
    assert_eq!(as_f16(as_buf(&out)), vec![0x0000, 0x3404, 0x3804, 0x3C00]);
    assert_eq!(
        as_buf(&out).bytes.len(),
        8,
        "f16 每元素 2 字节 —— 相比 f32 省一半带宽,这正是用它的理由"
    );
}

/// F16 也能参与归约(读侧转 double,输出仍是 F64 标量)。
#[test]
fn reduce_mean_on_f16() {
    init();
    let cfg = r#"
nodes:
  - { name: r, kernel: ReduceKernel, input_ports: ["in"], output_ports: ["out"], options: { op: mean } }
input_ports: ["in"]
output_ports: ["out"]
"#;
    // 1.0 · 2.0 · 3.0 → 均值 2.0
    let out = run_one(cfg, f16_buf(&[0x3C00, 0x4000, 0x4200])).expect("output");
    assert_eq!(out.as_f64(), Some(2.0));
}

/// 未知 dtype 仍被拒(F16 放开了,但闸门本身还在 —— 别把「支持 F16」做成「什么都放过」)。
#[test]
fn unknown_dtype_is_still_rejected() {
    init();
    let cfg = r#"
nodes:
  - { name: c, kernel: CastKernel, input_ports: ["in"], output_ports: ["out"], options: { dtype: f32 } }
input_ports: ["in"]
output_ports: ["out"]
"#;
    let mut bd = BufferData::new(&[2], DT_U8).unwrap();
    bd.bytes = vec![0u8; 2];
    bd.dtype = 99; // 不存在的 dtype
    let graph = Graph::from_yaml(cfg).unwrap();
    graph.add_poller("out").unwrap();
    graph.start().unwrap();
    graph
        .input("in")
        .unwrap()
        .send(Packet::from_builtin(Builtin::Buffer(bd)).at(Timestamp(0)))
        .unwrap();
    graph.close_all_inputs();
    assert!(
        graph.wait_done().is_err(),
        "未知 dtype 应让算子失败、图报错"
    );
}

/// 非连续缓冲被拒(strides 不匹配 packed 行优先)。
#[test]
fn non_contiguous_is_rejected() {
    init();
    let cfg = r#"
nodes:
  - { name: c, kernel: CastKernel, input_ports: ["in"], output_ports: ["out"], options: { dtype: f32 } }
input_ports: ["in"]
output_ports: ["out"]
"#;
    let mut bd = BufferData::new(&[4], DT_U8).unwrap();
    bd.bytes = vec![1, 2, 3, 4];
    bd.strides[0] = 2; // 人为破坏:u8 连续应为 1
    let graph = Graph::from_yaml(cfg).unwrap();
    graph.add_poller("out").unwrap();
    graph.start().unwrap();
    graph
        .input("in")
        .unwrap()
        .send(Packet::from_builtin(Builtin::Buffer(bd)).at(Timestamp(0)))
        .unwrap();
    graph.close_all_inputs();
    assert!(graph.wait_done().is_err(), "非连续缓冲应被拒");
}
