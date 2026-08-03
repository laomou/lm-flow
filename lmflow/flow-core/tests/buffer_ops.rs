//! 张量预处理 BUFFER 算子(Cast / Affine / Clamp / Reduce)端到端验收。
//!
//! 用 Rust 的 `BufferData` 造输入缓冲、`payload()` 读回输出缓冲,穿过真实图。

use flow_core::packet::Payload;
use flow_core::{BufferData, Builtin, Graph, Packet, Timestamp};

const DT_U8: i32 = 0;
const DT_F16: i32 = 6;

fn init() {
    flow_core::register_builtin_kernels();
}

/// 造一个一维 u8 缓冲包。
fn u8_buf(vals: &[u8]) -> Packet {
    let mut bd = BufferData::new(&[vals.len() as i64], DT_U8).unwrap();
    bd.bytes = vals.to_vec();
    Packet::from_builtin(Builtin::Buffer(bd)).at(Timestamp(0))
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

/// F16 输入不被这些数值算子支持 → 清晰报错(不静默算错)。
#[test]
fn f16_input_is_rejected() {
    init();
    let cfg = r#"
nodes:
  - { name: c, kernel: CastKernel, input_ports: ["in"], output_ports: ["out"], options: { dtype: f32 } }
input_ports: ["in"]
output_ports: ["out"]
"#;
    let mut bd = BufferData::new(&[2], DT_F16).unwrap(); // 2 个 f16 零
    bd.bytes = vec![0u8; 4];
    let graph = Graph::from_yaml(cfg).unwrap();
    graph.add_poller("out").unwrap();
    graph.start().unwrap();
    graph
        .input("in")
        .unwrap()
        .send(Packet::from_builtin(Builtin::Buffer(bd)).at(Timestamp(0)))
        .unwrap();
    graph.close_all_inputs();
    assert!(graph.wait_done().is_err(), "F16 应让算子失败、图报错");
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
