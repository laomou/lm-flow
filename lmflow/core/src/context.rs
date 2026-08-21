//! 算子上下文:本次调用的输入、输出暂存、参数、side packet、自我信息。
//!
//! 关键设计:`emit`/`forward` **不直接写下游边**,而是写进本上下文的 `staging`;
//! `process` 返回后由引擎统一分发。这样调用算子期间无需持任何边锁
//! (docs/design.md §7.0 规则 R1)。

use std::collections::BTreeMap;
use std::ffi::c_char;
use std::sync::Arc;

use crate::kernel::PortTable;
use crate::packet::Packet;
use crate::runtime::{CStrArena, GraphShared};
use crate::status::{Error, Result};
use crate::timestamp::Timestamp;

/// 节点参数(YAML 的 `options` 子树)。支持点号路径与数组。
pub struct Options {
    root: serde_yaml::Value,
    strings: CStrArena,
    json: String,
}

impl Options {
    pub fn new(root: serde_yaml::Value) -> Self {
        let json = serde_json::to_string(&root).unwrap_or_else(|_| "{}".to_string());
        let json = if json == "null" {
            "{}".to_string()
        } else {
            json
        };
        Self {
            root,
            strings: CStrArena::default(),
            json,
        }
    }

    /// 按点号路径取值:`"roi.x"` → `root["roi"]["x"]`。
    pub fn get(&self, path: &str) -> Option<&serde_yaml::Value> {
        let mut cur = &self.root;
        for seg in path.split('.') {
            cur = cur.get(seg)?;
        }
        Some(cur)
    }

    pub fn has(&self, path: &str) -> bool {
        self.get(path).is_some()
    }

    pub fn i64(&self, path: &str) -> Option<i64> {
        let v = self.get(path)?;
        v.as_i64().or_else(|| v.as_u64().map(|u| u as i64))
    }
    pub fn f64(&self, path: &str) -> Option<f64> {
        let v = self.get(path)?;
        // 整数字面量也应能读成浮点(YAML 里 1 和 1.0 用户不会区分)
        v.as_f64().or_else(|| v.as_i64().map(|i| i as f64))
    }
    pub fn bool(&self, path: &str) -> Option<bool> {
        self.get(path)?.as_bool()
    }
    pub fn str(&self, path: &str) -> Option<&str> {
        self.get(path)?.as_str()
    }

    /// 字符串参数的 C 指针(生命周期随 graph)。
    pub fn str_cstr(&self, path: &str) -> Option<*const c_char> {
        self.str(path).map(|s| self.strings.intern(s))
    }

    pub fn seq(&self, path: &str) -> Option<&Vec<serde_yaml::Value>> {
        self.get(path)?.as_sequence()
    }

    pub fn count(&self, path: &str) -> usize {
        self.seq(path).map_or(0, |s| s.len())
    }

    pub fn i64_array(&self, path: &str, out: &mut [i64]) -> usize {
        let Some(seq) = self.seq(path) else { return 0 };
        for (slot, v) in out.iter_mut().zip(seq.iter()) {
            *slot = v
                .as_i64()
                .or_else(|| v.as_u64().map(|u| u as i64))
                .unwrap_or(0);
        }
        seq.len()
    }

    pub fn f64_array(&self, path: &str, out: &mut [f64]) -> usize {
        let Some(seq) = self.seq(path) else { return 0 };
        for (slot, v) in out.iter_mut().zip(seq.iter()) {
            *slot = v
                .as_f64()
                .or_else(|| v.as_i64().map(|i| i as f64))
                .unwrap_or(0.0);
        }
        seq.len()
    }

    pub fn str_array(&self, path: &str, out: &mut [*const c_char]) -> usize {
        let Some(seq) = self.seq(path) else { return 0 };
        // 先收集,避免在持有 seq 借用时调用 intern(&self 方法,借用检查允许,但保持清晰)
        let items: Vec<Option<&str>> = seq.iter().map(|v| v.as_str()).collect();
        for (slot, item) in out.iter_mut().zip(items.iter()) {
            *slot = match item {
                Some(s) => self.strings.intern(s),
                None => self.strings.intern(""),
            };
        }
        seq.len()
    }

    pub fn json(&self) -> &str {
        &self.json
    }
    pub fn json_cstr(&self) -> *const c_char {
        self.strings.intern(&self.json)
    }
}

/// 一次算子调用的上下文。
pub struct Context {
    /// 本次的输入。`None` 表示已被 `take_input` 取走(CoW 省拷贝的前提)。
    pub inputs: Vec<Option<Packet>>,
    /// 批处理(`batch` 策略)时本口的多个包;单包策略下恒空、走 `inputs`。见 `input_count`/`input_at`。
    pub input_batches: Vec<Vec<Packet>>,
    /// 每个输出口的暂存队列;`process` 返回后由引擎分发。
    pub staging: Vec<Vec<Packet>>,
    /// 算子显式推进的时间戳边界(不产出时用它告知下游)。
    pub next_bounds: Vec<Option<Timestamp>>,
    pub input_ts: Timestamp,

    pub node_name: String,
    pub kernel_name: String,
    pub in_ports: Arc<PortTable>,
    pub out_ports: Arc<PortTable>,
    pub options: Arc<Options>,
    pub side_packets: Arc<BTreeMap<String, Packet>>,
    pub shared: Arc<GraphShared>,
    /// 输入口是否已终结(上游关闭且排空)。
    pub inputs_done: Vec<bool>,
    /// 算子经 `set_error` 提供的失败原因。
    pub error_msg: Option<String>,
    /// 供 `lmflow_ctx_close_reason` 返回;进入 close 前由引擎写入。
    pub close_reason: i32,
    /// 源算子经 `source_done()` 自报「已产完」;引擎在 process 返回后读取。
    pub source_done: bool,
    /// 源算子协作式让出 worker，并请求引擎在指定延迟后再次调度。
    pub source_yield: Option<std::time::Duration>,
    names: CStrArena,
}

impl Context {
    pub fn new(
        node_name: String,
        kernel_name: String,
        in_ports: Arc<PortTable>,
        out_ports: Arc<PortTable>,
        options: Arc<Options>,
        side_packets: Arc<BTreeMap<String, Packet>>,
        shared: Arc<GraphShared>,
    ) -> Self {
        let ni = in_ports.len();
        let no = out_ports.len();
        Self {
            inputs: vec![None; ni],
            input_batches: (0..ni).map(|_| Vec::new()).collect(),
            staging: (0..no).map(|_| Vec::new()).collect(),
            next_bounds: vec![None; no],
            input_ts: Timestamp::unset(),
            node_name,
            kernel_name,
            in_ports,
            out_ports,
            options,
            side_packets,
            shared,
            inputs_done: vec![false; ni],
            error_msg: None,
            close_reason: crate::runtime::CLOSE_NORMAL,
            source_done: false,
            source_yield: None,
            names: CStrArena::default(),
        }
    }

    /// 每次调用前复位(输入由调用方填充)。
    pub fn reset(&mut self) {
        for slot in &mut self.inputs {
            *slot = None;
        }
        for b in &mut self.input_batches {
            b.clear();
        }
        for s in &mut self.staging {
            s.clear();
        }
        self.next_bounds.fill(None);
        self.input_ts = Timestamp::unset();
        self.error_msg = None;
        self.source_done = false;
        self.source_yield = None;
        // 下面两项在正常流程里使用前会被重写(claim 时写 inputs_done、进 close 前写
        // close_reason),故单次调用不清也不出错;但 `reset` 要能用于「彻底静态复位」
        // (图 reset 重跑),所以这里一并归位,不留任何上一轮残留。
        self.inputs_done.fill(false);
        self.close_reason = crate::runtime::CLOSE_NORMAL;
    }

    /// 处理结束后**立即释放**本次输入的引用。
    ///
    /// 这条至关重要:若把输入留到下次 `reset` 才清,上游节点就会一直持有已处理完的包,
    /// 于是下游 `take_input` 看到的引用数 ≥ 2,CoW 必然复制 —— 「线性管线零拷贝」
    /// 这条不变量(docs/design.md §3.4)会对任何多节点管线静默失效。
    /// 顺带也避免大帧被无谓地多留一个周期。
    pub fn clear_inputs(&mut self) {
        for slot in &mut self.inputs {
            *slot = None;
        }
        for b in &mut self.input_batches {
            b.clear();
        }
    }

    /// 失败时丢弃暂存,不把半成品输出传播下去(docs/design.md §7.7)。
    pub fn discard_staging(&mut self) {
        for s in &mut self.staging {
            s.clear();
        }
        self.next_bounds.fill(None);
    }

    pub fn node_name_cstr(&self) -> *const c_char {
        self.names.intern(&self.node_name)
    }
    pub fn kernel_name_cstr(&self) -> *const c_char {
        self.names.intern(&self.kernel_name)
    }
    pub fn intern(&self, s: &str) -> *const c_char {
        self.names.intern(s)
    }

    pub fn input(&self, idx: usize) -> Option<&Packet> {
        self.input_at(idx, 0)
    }

    /// 本次调用某输入口的包数(单包策略恒 0/1;`batch` 策略为该批实际大小)。
    pub fn input_count(&self, idx: usize) -> usize {
        match self.input_batches.get(idx) {
            Some(b) if !b.is_empty() => b.len(),
            _ => self.inputs.get(idx).map_or(0, |s| s.is_some() as usize),
        }
    }

    /// 借用某输入口的第 `k` 个包(单包策略仅 `k==0` 有效)。统一单包 / 批两种交付。
    pub fn input_at(&self, idx: usize, k: usize) -> Option<&Packet> {
        match self.input_batches.get(idx) {
            Some(b) if !b.is_empty() => b.get(k),
            _ if k == 0 => self.inputs.get(idx)?.as_ref(),
            _ => None,
        }
    }

    /// 取走输入包:所有权移交算子,槽位变空。这是 CoW 零拷贝的前提 ——
    /// 不取走的话上下文仍持一份引用,`make_mutable` 必然复制。
    pub fn take_input(&mut self, idx: usize) -> Packet {
        match self.inputs.get_mut(idx) {
            Some(slot) => slot.take().unwrap_or_default(),
            None => Packet::empty(),
        }
    }

    pub fn emit(&mut self, out_idx: usize, mut pkt: Packet) -> Result<()> {
        if out_idx >= self.staging.len() {
            return Err(Error::InvalidArg(format!(
                "node `{}`: emit output port index {out_idx} out of range (of {} total)",
                self.node_name,
                self.staging.len()
            )));
        }
        // 未设时间戳则继承当前输入时间戳(与 forward 行为一致)
        if pkt.timestamp() == Timestamp::unset() {
            pkt.set_timestamp(self.input_ts);
        }
        self.staging[out_idx].push(pkt);
        Ok(())
    }

    /// 零拷贝直通:只克隆引用计数。
    pub fn forward(&mut self, in_idx: usize, out_idx: usize) -> Result<()> {
        let pkt = self
            .inputs
            .get(in_idx)
            .and_then(|s| s.clone())
            .ok_or_else(|| {
                Error::InvalidArg(format!(
                    "node `{}`: forward input port {in_idx} is empty or out of range",
                    self.node_name
                ))
            })?;
        self.emit(out_idx, pkt)
    }

    pub fn set_next_bound(&mut self, out_idx: usize, bound: Timestamp) {
        if let Some(slot) = self.next_bounds.get_mut(out_idx) {
            *slot = Some(bound);
        }
    }

    pub fn log(&self, level: i32, msg: &str) {
        // 快路:没装 sink 时连这层 `format!`(加节点名前缀)都不该做 —— 它会堆分配,
        // 而 `cc.Log` 在算子里可能是**每包**调用的。
        if !crate::runtime::log_enabled() {
            return;
        }
        crate::runtime::log(level, &format!("[{}] {}", self.node_name, msg));
    }

    pub fn set_error(&mut self, msg: &str) {
        self.error_msg = Some(msg.to_string());
    }

    /// 组合出带节点名前缀的错误。
    pub fn take_error(&mut self, code: i32) -> Error {
        let detail = self
            .error_msg
            .take()
            .unwrap_or_else(|| format!("returned status code {code}"));
        Error::Kernel(format!("[{}] {}", self.node_name, detail))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GraphConfig;

    fn ports(decls: &[&str]) -> Arc<PortTable> {
        let v: Vec<String> = decls.iter().map(|s| s.to_string()).collect();
        Arc::new(PortTable::build(&v, "test").unwrap())
    }

    fn ctx(opts: &str) -> Context {
        let value: serde_yaml::Value = serde_yaml::from_str(opts).unwrap();
        Context::new(
            "n".into(),
            "K".into(),
            ports(&["a"]),
            ports(&["b", "c"]),
            Arc::new(Options::new(value)),
            Arc::new(BTreeMap::new()),
            Arc::new(GraphShared::new(
                GraphConfig::from_yaml("nodes: []").unwrap(),
            )),
        )
    }

    #[test]
    fn options_scalars_and_dotted_path() {
        let c = ctx("threshold: 0.5\nn: 7\nflag: true\nname: hi\nroi:\n  x: 8\n  y: 9\n");
        assert_eq!(c.options.f64("threshold"), Some(0.5));
        assert_eq!(c.options.i64("n"), Some(7));
        assert_eq!(c.options.bool("flag"), Some(true));
        assert_eq!(c.options.str("name"), Some("hi"));
        assert_eq!(c.options.i64("roi.x"), Some(8), "dotted path reads nested");
        assert_eq!(c.options.i64("roi.y"), Some(9));
        assert_eq!(c.options.i64("nope"), None);
        assert_eq!(c.options.i64("roi.nope"), None);
    }

    #[test]
    fn integer_reads_as_float() {
        // YAML 里写 1 而不是 1.0 是常态,不该读不出来
        let c = ctx("scale: 2");
        assert_eq!(c.options.f64("scale"), Some(2.0));
    }

    #[test]
    fn options_arrays() {
        let c = ctx("mean: [0.1, 0.2, 0.3]\nsizes: [8, 16]\nnames: [a, b]");
        assert_eq!(c.options.count("mean"), 3);
        let mut f = [0.0f64; 4];
        assert_eq!(c.options.f64_array("mean", &mut f), 3);
        assert_eq!(&f[..3], &[0.1, 0.2, 0.3]);

        let mut i = [0i64; 2];
        assert_eq!(c.options.i64_array("sizes", &mut i), 2);
        assert_eq!(i, [8, 16]);

        // cap 小于实际长度:只填 cap 个,但返回真实长度(调用方据此判断是否被截断)
        let mut small = [0.0f64; 2];
        assert_eq!(c.options.f64_array("mean", &mut small), 3);
        assert_eq!(&small, &[0.1, 0.2]);

        assert_eq!(c.options.count("missing"), 0);
        assert_eq!(c.options.f64_array("missing", &mut f), 0);
    }

    #[test]
    fn options_json_fallback() {
        let c = ctx("a: 1\nnested:\n  deep:\n    x: [1, 2]\n");
        let j = c.options.json();
        assert!(j.contains("\"a\":1"), "{j}");
        assert!(j.contains("deep"), "{j}");
    }

    #[test]
    fn empty_options_json_is_object() {
        let c = ctx("~"); // YAML null
        assert_eq!(
            c.options.json(),
            "{}",
            "empty options should be {{}} not null"
        );
    }

    #[test]
    fn emit_inherits_input_timestamp() {
        let mut c = ctx("{}");
        c.input_ts = Timestamp(42);
        c.emit(0, Packet::new(1i32)).unwrap();
        assert_eq!(c.staging[0][0].timestamp(), Timestamp(42));
        // 显式设过的不被覆盖
        c.emit(1, Packet::new(2i32).at(Timestamp(7))).unwrap();
        assert_eq!(c.staging[1][0].timestamp(), Timestamp(7));
    }

    #[test]
    fn emit_out_of_range_is_error() {
        let mut c = ctx("{}");
        let err = c.emit(9, Packet::new(1i32)).unwrap_err();
        assert!(err.to_string().contains("out of range"), "{err}");
    }

    #[test]
    fn forward_clones_reference_only() {
        let mut c = ctx("{}");
        let p = Packet::new(5i32);
        c.inputs[0] = Some(p);
        c.forward(0, 0).unwrap();
        // 输入槽与 staging 指向同一 payload
        assert_eq!(c.inputs[0].as_ref().unwrap().ref_count(), 2);
        assert_eq!(c.staging[0][0].get::<i32>(), Some(&5));
    }

    #[test]
    fn forward_from_empty_slot_is_error() {
        let mut c = ctx("{}");
        assert!(
            c.forward(0, 0).is_err(),
            "forward from empty slot must error"
        );
    }

    #[test]
    fn take_input_empties_slot_enabling_cow() {
        let mut c = ctx("{}");
        c.inputs[0] = Some(Packet::new(5i32));
        let p = c.take_input(0);
        assert!(c.inputs[0].is_none(), "slot must be empty after take");
        assert_eq!(
            p.ref_count(),
            1,
            "must be exclusive after take so CoW is zero-copy"
        );
        // 重复取走返回空包而不是 panic
        assert!(c.take_input(0).is_empty());
    }

    #[test]
    fn discard_staging_on_failure() {
        let mut c = ctx("{}");
        c.emit(0, Packet::new(1i32)).unwrap();
        c.set_next_bound(1, Timestamp(3));
        c.discard_staging();
        assert!(
            c.staging[0].is_empty(),
            "must not propagate half-finished output on failure"
        );
        assert!(c.next_bounds[1].is_none());
    }

    #[test]
    fn error_carries_node_name() {
        let mut c = ctx("{}");
        c.set_error("model load failed");
        let e = c.take_error(3);
        assert!(
            e.to_string().contains("[n]"),
            "error should carry the node name: {e}"
        );
        assert!(e.to_string().contains("model load failed"), "{e}");
    }

    #[test]
    fn error_without_message_still_useful() {
        let mut c = ctx("{}");
        let e = c.take_error(3);
        assert!(e.to_string().contains("status code 3"), "{e}");
    }
}
