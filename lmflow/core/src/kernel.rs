//! 算子:vtable 抽象、全局注册表、端口表与契约。

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

use crate::config::{parse_port_spec, RouteConfig, RouteMode};
use crate::context::Context;
use crate::status::{Error, Result};

/// 算子的函数指针表(布局与 `include/flow.h` 的 `LMFlowKernelVTable` 一致)。
/// `ctx`/`contract` 参数在 C 侧是 `LMFlowContext*`/`LMFlowContract*`,此处用 `*mut c_void`
/// 以免模块间循环依赖;ffi 层负责转换。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct KernelVTable {
    pub create: Option<unsafe extern "C" fn(*mut c_void) -> *mut c_void>,
    pub get_contract: Option<unsafe extern "C" fn(*mut c_void, *mut c_void)>,
    pub open: Option<unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32>,
    pub process: Option<unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32>,
    pub close: Option<unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32>,
    pub destroy: Option<unsafe extern "C" fn(*mut c_void)>,
}

#[derive(Clone, Copy)]
pub struct KernelReg {
    pub vtable: KernelVTable,
    pub factory: *mut c_void,
    pub language: KernelLanguage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum KernelLanguage {
    Unknown = 0,
    Rust = 1,
    Cpp = 2,
    Python = 3,
    C = 4,
}

impl KernelLanguage {
    pub fn label(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Rust => "Rust",
            Self::Cpp => "C++",
            Self::Python => "Python",
            Self::C => "C",
        }
    }
}

// 安全性:factory 由注册方提供、要求指向静态存储或长生命周期对象;
// 注册表只在 init 阶段读取,不做解引用以外的操作。
unsafe impl Send for KernelReg {}
unsafe impl Sync for KernelReg {}

/// 全局算子注册表。用 BTreeMap 以保证枚举顺序确定(便于诊断输出可复现)。
static REGISTRY: LazyLock<Mutex<BTreeMap<String, KernelReg>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

pub fn register(name: &str, vtable: KernelVTable, factory: *mut c_void) -> Result<()> {
    register_with_language(name, vtable, factory, KernelLanguage::Unknown)
}

pub fn register_with_language(
    name: &str,
    vtable: KernelVTable,
    factory: *mut c_void,
    language: KernelLanguage,
) -> Result<()> {
    if name.is_empty() {
        return Err(Error::InvalidArg("kernel name must not be empty".into()));
    }
    if vtable.process.is_none() {
        return Err(Error::InvalidArg(format!(
            "kernel `{name}` vtable is missing process (the only required callback)"
        )));
    }
    let mut reg = REGISTRY.lock().expect("registry lock poisoned");
    if reg.contains_key(name) {
        return Err(Error::InvalidArg(format!(
            "kernel `{name}` already registered"
        )));
    }
    reg.insert(
        name.to_string(),
        KernelReg {
            vtable,
            factory,
            language,
        },
    );
    Ok(())
}

pub fn lookup(name: &str) -> Option<KernelReg> {
    REGISTRY
        .lock()
        .expect("registry lock poisoned")
        .get(name)
        .copied()
}

pub fn registered_names() -> Vec<String> {
    REGISTRY
        .lock()
        .expect("registry lock poisoned")
        .keys()
        .cloned()
        .collect()
}

/// 「算子未注册」的统一报错 —— 必须列出可用名字,否则用户无从下手
/// (常见原因是对应 kernel 组件尚未注册，或算子名拼错)。
fn not_registered(name: &str) -> Error {
    let registered = registered_names();
    let suggestion = crate::diagnostic::did_you_mean(name, registered.iter().map(String::as_str));
    Error::NotFound(format!(
        "kernel `{name}` not registered{suggestion}. registered: [{}]",
        registered.join(", ")
    ))
}

/// 一个节点持有的算子实例。Drop 时回调 C 侧 `destroy`(RAII)。
pub struct KernelInstance {
    self_ptr: *mut c_void,
    vtable: KernelVTable,
    kernel_name: String,
    language: KernelLanguage,
    internal: Option<RouteRuntime>,
}

struct RouteRuleStats {
    evaluated: AtomicU64,
    matched: AtomicU64,
    emitted: AtomicU64,
}

struct RouteRuntime {
    config: RouteConfig,
    rules: Vec<RouteRuleStats>,
    default_emitted: AtomicU64,
    unmatched: AtomicU64,
    dropped: AtomicU64,
    errors: AtomicU64,
    missing_metadata: AtomicU64,
    evaluation_errors: AtomicU64,
}

#[derive(Debug, Clone)]
pub(crate) struct RouteStatsSnapshot {
    pub config: RouteConfig,
    pub rules: Vec<(u64, u64, u64)>,
    pub default_emitted: u64,
    pub unmatched: u64,
    pub dropped: u64,
    pub errors: u64,
    pub missing_metadata: u64,
    pub evaluation_errors: u64,
}

// 安全性:self_ptr 指向算子实例,由「节点独占令牌」保证任一时刻只被单线程访问
// (docs/design.md §7.0 R3)。
unsafe impl Send for KernelInstance {}
unsafe impl Sync for KernelInstance {}

impl KernelInstance {
    pub fn create(kernel_name: &str) -> Result<Self> {
        let reg = lookup(kernel_name).ok_or_else(|| not_registered(kernel_name))?;
        let self_ptr = match reg.vtable.create {
            // 安全性:create 由算子侧提供,契约是返回可传给其它回调的实例指针。
            Some(f) => unsafe { f(reg.factory) },
            None => std::ptr::null_mut(), // 无状态算子
        };
        Ok(Self {
            self_ptr,
            vtable: reg.vtable,
            kernel_name: kernel_name.to_string(),
            language: reg.language,
            internal: None,
        })
    }

    pub fn create_route(config: RouteConfig) -> Self {
        let rules = config
            .routes
            .iter()
            .map(|_| RouteRuleStats {
                evaluated: AtomicU64::new(0),
                matched: AtomicU64::new(0),
                emitted: AtomicU64::new(0),
            })
            .collect();
        Self {
            self_ptr: std::ptr::null_mut(),
            vtable: KernelVTable {
                create: None,
                get_contract: None,
                open: None,
                process: None,
                close: None,
                destroy: None,
            },
            kernel_name: "__lmflow.route".into(),
            language: KernelLanguage::Rust,
            internal: Some(RouteRuntime {
                config,
                rules,
                default_emitted: AtomicU64::new(0),
                unmatched: AtomicU64::new(0),
                dropped: AtomicU64::new(0),
                errors: AtomicU64::new(0),
                missing_metadata: AtomicU64::new(0),
                evaluation_errors: AtomicU64::new(0),
            }),
        }
    }

    pub(crate) fn route_stats(&self) -> Option<RouteStatsSnapshot> {
        let route = self.internal.as_ref()?;
        Some(RouteStatsSnapshot {
            config: route.config.clone(),
            rules: route
                .rules
                .iter()
                .map(|stats| {
                    (
                        stats.evaluated.load(Ordering::Relaxed),
                        stats.matched.load(Ordering::Relaxed),
                        stats.emitted.load(Ordering::Relaxed),
                    )
                })
                .collect(),
            default_emitted: route.default_emitted.load(Ordering::Relaxed),
            unmatched: route.unmatched.load(Ordering::Relaxed),
            dropped: route.dropped.load(Ordering::Relaxed),
            errors: route.errors.load(Ordering::Relaxed),
            missing_metadata: route.missing_metadata.load(Ordering::Relaxed),
            evaluation_errors: route.evaluation_errors.load(Ordering::Relaxed),
        })
    }

    pub(crate) fn reset_stats(&self) {
        let Some(route) = &self.internal else { return };
        for stats in &route.rules {
            stats.evaluated.store(0, Ordering::Relaxed);
            stats.matched.store(0, Ordering::Relaxed);
            stats.emitted.store(0, Ordering::Relaxed);
        }
        route.default_emitted.store(0, Ordering::Relaxed);
        route.unmatched.store(0, Ordering::Relaxed);
        route.dropped.store(0, Ordering::Relaxed);
        route.errors.store(0, Ordering::Relaxed);
        route.missing_metadata.store(0, Ordering::Relaxed);
        route.evaluation_errors.store(0, Ordering::Relaxed);
    }

    pub fn kernel_name(&self) -> &str {
        &self.kernel_name
    }

    pub fn language(&self) -> KernelLanguage {
        self.language
    }

    /// 调 C 侧 `get_contract` 填充契约。
    ///
    /// # Safety
    /// `contract` 必须是指向存活 [`Contract`] 的有效指针,且在本调用期间不被其它代码访问。
    pub unsafe fn fill_contract(kernel_name: &str, contract: *mut c_void) -> Result<()> {
        let reg = lookup(kernel_name).ok_or_else(|| not_registered(kernel_name))?;
        if let Some(f) = reg.vtable.get_contract {
            // 安全性:contract 指向存活的 Contract;回调期内不会被别处访问。
            unsafe { f(reg.factory, contract) };
        }
        Ok(())
    }

    /// 以下三者传入的 `ctx` 是 `*mut Context` 的擦除形式。
    /// 调用期间**不持有任何引擎锁**(docs/design.md §7.0 R1)。
    ///
    /// # Safety
    /// `ctx` 必须是指向存活 [`crate::context::Context`] 的有效指针,且调用方持有
    /// 本节点的**独占令牌**(docs/design.md §7.0 R3)—— 否则算子回调可能与引擎
    /// 对同一 Context 的访问竞争。
    pub unsafe fn open(&self, ctx: *mut c_void) -> i32 {
        match self.vtable.open {
            Some(f) => unsafe { f(self.self_ptr, ctx) },
            None => 0,
        }
    }
    /// # Safety
    /// 同 [`Self::open`]。
    pub unsafe fn process(&self, ctx: *mut c_void) -> i32 {
        if let Some(route) = &self.internal {
            let context = unsafe { &mut *(ctx as *mut Context) };
            return match route_process(route, context) {
                Ok(()) => 0,
                Err(error) => {
                    context.set_error(&error.to_string());
                    -1
                }
            };
        }
        match self.vtable.process {
            Some(f) => unsafe { f(self.self_ptr, ctx) },
            None => 0,
        }
    }
    /// # Safety
    /// 同 [`Self::open`]。
    pub unsafe fn close(&self, ctx: *mut c_void) -> i32 {
        match self.vtable.close {
            Some(f) => unsafe { f(self.self_ptr, ctx) },
            None => 0,
        }
    }
}

fn route_process(route: &RouteRuntime, context: &mut Context) -> Result<()> {
    let packet = context.input(0).cloned().unwrap_or_default();
    let mut matched = false;
    let mut default_output = None;
    for (index, rule) in route.config.routes.iter().enumerate() {
        if rule.default {
            default_output = Some(rule.to.as_str());
            continue;
        }
        route.rules[index].evaluated.fetch_add(1, Ordering::Relaxed);
        let predicate = rule
            .when
            .as_ref()
            .ok_or_else(|| Error::InvalidArg("route rule missing condition".into()))?;
        route.missing_metadata.fetch_add(
            predicate.missing_metadata_count(packet.metadata()),
            Ordering::Relaxed,
        );
        let hit = match predicate.evaluate(packet.metadata(), packet.timestamp()) {
            Ok(hit) => hit,
            Err(error) => {
                route.evaluation_errors.fetch_add(1, Ordering::Relaxed);
                return Err(error);
            }
        };
        if hit {
            route.rules[index].matched.fetch_add(1, Ordering::Relaxed);
            let out = context
                .out_ports
                .index_by_name(&rule.to)
                .ok_or_else(|| Error::InvalidArg(format!("unknown route output `{}`", rule.to)))?;
            context.emit(out, packet.clone())?;
            route.rules[index].emitted.fetch_add(1, Ordering::Relaxed);
            context.shared.counter_add(
                &format!("route.{}.matched.{}", context.node_name, rule.to),
                1,
            );
            context.shared.counter_add(
                &format!("route.{}.emitted.{}", context.node_name, rule.to),
                1,
            );
            matched = true;
            if route.config.mode == RouteMode::First {
                break;
            }
        }
    }
    if !matched {
        if let Some(port) = default_output {
            let out = context
                .out_ports
                .index_by_name(port)
                .ok_or_else(|| Error::InvalidArg(format!("unknown default output `{port}`")))?;
            context.emit(out, packet)?;
            route.default_emitted.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        route.unmatched.fetch_add(1, Ordering::Relaxed);
        match route.config.unmatched.as_str() {
            "drop" => {
                route.dropped.fetch_add(1, Ordering::Relaxed);
                context
                    .shared
                    .counter_add(&format!("route.{}.dropped", context.node_name), 1);
                Ok(())
            }
            "error" => {
                route.errors.fetch_add(1, Ordering::Relaxed);
                Err(Error::Kernel("route packet did not match any rule".into()))
            }
            port => {
                let out = context.out_ports.index_by_name(port).ok_or_else(|| {
                    Error::InvalidArg(format!("unknown unmatched output `{port}`"))
                })?;
                context
                    .shared
                    .counter_add(&format!("route.{}.emitted.{}", context.node_name, port), 1);
                context.emit(out, packet)?;
                Ok(())
            }
        }
    } else {
        Ok(())
    }
}

impl std::fmt::Debug for KernelInstance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "KernelInstance{{`{}`}}", self.kernel_name)
    }
}

impl Drop for KernelInstance {
    fn drop(&mut self) {
        if let Some(f) = self.vtable.destroy {
            if !self.self_ptr.is_null() {
                unsafe { f(self.self_ptr) };
            }
        }
        self.self_ptr = std::ptr::null_mut();
    }
}

/// 端口表:把 YAML 的端口声明整理成「序号 ↔ 名字 ↔ (tag,index)」三向查询。
///
/// **扁平序号 = 声明顺序**(ADR #17)。
#[derive(Debug, Default, Clone)]
pub struct PortTable {
    names: Vec<String>,
    tags: Vec<(String, usize)>,
    by_name: BTreeMap<String, usize>,
    by_tag: BTreeMap<(String, usize), usize>,
}

impl PortTable {
    pub fn build(decls: &[String], what: &str) -> Result<Self> {
        let mut t = PortTable::default();
        // 每个 tag 下自动编号的游标
        let mut auto: BTreeMap<String, usize> = BTreeMap::new();
        for decl in decls {
            let spec = parse_port_spec(decl)?;
            let idx = match spec.index {
                Some(i) => i,
                None => {
                    let c = auto.entry(spec.tag.clone()).or_insert(0);
                    let i = *c;
                    *c += 1;
                    i
                }
            };
            let flat = t.names.len();
            let key = (spec.tag.clone(), idx);
            if t.by_tag.contains_key(&key) {
                return Err(Error::InvalidArg(format!(
                    "{what}: port `{decl}` (tag=`{}`, index={idx}) is a duplicate",
                    spec.tag
                )));
            }
            if t.by_name.contains_key(&spec.name) {
                return Err(Error::InvalidArg(format!(
                    "{what}: port name `{}` is duplicated within the same kernel",
                    spec.name
                )));
            }
            t.by_tag.insert(key, flat);
            t.by_name.insert(spec.name.clone(), flat);
            t.names.push(spec.name);
            t.tags.push((spec.tag, idx));
        }
        t.check_tag_index_continuity(what)?;
        Ok(t)
    }

    /// 同一 tag 下的 index 必须从 0 连续(否则 `InputId(tag, i)` 会出现空洞)。
    fn check_tag_index_continuity(&self, what: &str) -> Result<()> {
        let mut per_tag: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
        for (tag, idx) in &self.tags {
            per_tag.entry(tag.as_str()).or_default().push(*idx);
        }
        for (tag, mut idxs) in per_tag {
            idxs.sort_unstable();
            for (expect, got) in idxs.iter().enumerate() {
                if expect != *got {
                    return Err(Error::InvalidArg(format!(
                        "{what}: tag `{tag}` index must be contiguous from 0, got {idxs:?}"
                    )));
                }
            }
        }
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
    pub fn name(&self, flat: usize) -> Option<&str> {
        self.names.get(flat).map(|s| s.as_str())
    }
    pub fn names(&self) -> &[String] {
        &self.names
    }
    pub fn id_by_tag(&self, tag: &str, index: usize) -> Option<usize> {
        self.by_tag.get(&(tag.to_string(), index)).copied()
    }
    pub fn index_by_name(&self, name: &str) -> Option<usize> {
        self.by_name.get(name).copied()
    }
}

/// A kernel's port contract: payload-type constraints plus required side packets.
///
/// Port counts and names come from YAML and are already known when the contract is built; the
/// kernel only fills in the types.
#[derive(Debug)]
pub struct Contract {
    pub inputs: std::sync::Arc<PortTable>,
    pub outputs: std::sync::Arc<PortTable>,
    /// 0 = 未声明(接受任意类型)
    pub input_types: Vec<u64>,
    pub output_types: Vec<u64>,
    pub required_side_packets: Vec<String>,
    error: Option<String>,
}

impl Contract {
    pub fn new(inputs: std::sync::Arc<PortTable>, outputs: std::sync::Arc<PortTable>) -> Self {
        let ni = inputs.len();
        let no = outputs.len();
        Self {
            inputs,
            outputs,
            input_types: vec![0; ni],
            output_types: vec![0; no],
            required_side_packets: Vec::new(),
            error: None,
        }
    }

    pub(crate) fn record_error(&mut self, message: impl Into<String>) {
        if self.error.is_none() {
            self.error = Some(message.into());
        }
    }

    pub(crate) fn take_error(&mut self) -> Option<String> {
        self.error.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decls(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn flat_index_follows_declaration_order() {
        // ADR #17:扁平序号就是 YAML 里的书写顺序,不按 tag 排序
        let t = PortTable::build(&decls(&["ZZZ:z", "plain", "AAA:a"]), "test").unwrap();
        assert_eq!(t.name(0), Some("z"));
        assert_eq!(t.name(1), Some("plain"));
        assert_eq!(t.name(2), Some("a"));
    }

    #[test]
    fn lookup_by_tag_and_by_name() {
        let t = PortTable::build(
            &decls(&["frames", "VIDEO:cam0", "MASK:0:m0", "MASK:1:m1"]),
            "test",
        )
        .unwrap();
        assert_eq!(t.id_by_tag("VIDEO", 0), Some(1));
        assert_eq!(t.id_by_tag("MASK", 1), Some(3));
        assert_eq!(t.id_by_tag("MASK", 5), None);
        assert_eq!(t.index_by_name("frames"), Some(0));
        assert_eq!(t.index_by_name("nope"), None);
        // 无 tag 的归入空 tag,自动编号
        assert_eq!(t.id_by_tag("", 0), Some(0));
    }

    #[test]
    fn auto_index_per_tag() {
        let t = PortTable::build(&decls(&["A:x", "A:y", "B:z"]), "test").unwrap();
        assert_eq!(t.id_by_tag("A", 0), Some(0));
        assert_eq!(t.id_by_tag("A", 1), Some(1));
        assert_eq!(t.id_by_tag("B", 0), Some(2));
    }

    #[test]
    fn rejects_duplicate_and_discontinuous() {
        assert!(
            PortTable::build(&decls(&["A:0:x", "A:0:y"]), "t").is_err(),
            "same tag and index duplicated"
        );
        assert!(
            PortTable::build(&decls(&["a", "a"]), "t").is_err(),
            "duplicate port name"
        );
        let err = PortTable::build(&decls(&["A:1:x"]), "t").unwrap_err();
        assert!(
            err.to_string().contains("contiguous"),
            "index has a gap: {err}"
        );
    }

    #[test]
    fn register_rejects_vtable_without_process() {
        let vt = KernelVTable {
            create: None,
            get_contract: None,
            open: None,
            process: None,
            close: None,
            destroy: None,
        };
        let err = register("__no_process__", vt, std::ptr::null_mut()).unwrap_err();
        assert!(err.to_string().contains("process"), "{err}");
    }

    #[test]
    fn lookup_missing_kernel_lists_available() {
        let err = KernelInstance::create("__definitely_missing__").unwrap_err();
        // 报错要能指导用户 —— 列出可用名字
        assert!(err.to_string().contains("registered"), "{err}");
    }
}
