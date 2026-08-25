//! 图配置的 YAML 表示。
//!
//! 原则:**用到本版本未实现的字段就报错,不静默忽略** —— 否则用户以为开了并行
//! (或丢帧策略),实际没有,而且无从察觉。见 docs/design.md §0.2。

use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::kernel::PortTable;
use crate::metadata::MetadataPredicate;
use crate::status::{Error, Result};

fn default_max_queue_size() -> usize {
    100
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RouteMode {
    #[default]
    First,
    All,
}

impl RouteMode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::First => "first",
            Self::All => "all",
        }
    }
}

impl RouteConfig {
    fn is_implicit_default(&self) -> bool {
        self.mode == RouteMode::First && self.unmatched == "drop" && self.routes.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteRule {
    pub to: String,
    #[serde(default)]
    pub when: Option<MetadataPredicate>,
    #[serde(default)]
    pub default: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteConfig {
    #[serde(default)]
    pub mode: RouteMode,
    #[serde(default = "default_route_unmatched")]
    pub unmatched: String,
    #[serde(default)]
    pub routes: Vec<RouteRule>,
}

fn default_route_unmatched() -> String {
    "drop".to_string()
}

/// 运行时统计级别。它只控制诊断记账，不改变调度与终止语义。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StatsLevel {
    /// 仅保留调度正确性和错误处理必需的状态。
    Off,
    /// 保留低成本吞吐、队列与背压统计。
    #[default]
    Basic,
    /// 额外记录耗时、百分位、CoW 和执行器诊断。
    Full,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutorConfig {
    /// 执行器名,节点用 `executor:` 按名引用。**必填**,且 `"default"` 是引擎保留名
    /// (那是隐式默认执行器,不写 `executor` 的节点归它)—— 两者都会在建图期报错。
    #[serde(default)]
    pub name: String,
    /// `"ThreadPoolExecutor"`(默认,空也算)= 自有工作线程的线程池;
    /// `"DelegatingExecutor"` = 不拥有线程,把就绪节点交还**宿主线程**跑
    /// (零并发、顺序确定、Python 算子不抢 GIL,但要宿主进入引擎才推进)。
    #[serde(default)]
    pub r#type: String,
    /// 工作线程数。`0` 视作 1。仅 `ThreadPoolExecutor` 有意义 ——
    /// 配在 `DelegatingExecutor` 上会报错而不是静默忽略。
    #[serde(default)]
    pub num_threads: usize,
    /// CPU 亲和力:worker `i` 绑到 `affinity[i % len]` 号核。空 = 不绑(默认)。
    /// 仅 Linux/Android 生效;其它平台忽略。用于实时/NUMA 场景固定核、减少迁移抖动。
    #[serde(default)]
    pub affinity: Vec<usize>,
    /// 实时优先级(SCHED_FIFO,1..=99)。0 = 不动(默认,普通分时)。
    /// 尽力而为:设实时调度需 CAP_SYS_NICE/root,拿不到就静默降级。仅 Linux/Android。
    #[serde(default)]
    pub priority: i32,
}

/// 输入策略(节点级可插拔)。见 docs/design.md §7.10。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputPolicyConfig {
    #[serde(default = "default_policy_type")]
    pub r#type: String,
    #[serde(default)]
    pub capacity: usize,
    /// `sync_set` 用:输入口分组(端口名),须完整划分全部输入口。
    #[serde(default)]
    pub sets: Vec<Vec<String>>,
}

fn default_policy_type() -> String {
    "sync".to_string()
}

impl Default for InputPolicyConfig {
    fn default() -> Self {
        Self {
            r#type: default_policy_type(),
            capacity: 0,
            sets: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputQueueLimitConfig {
    /// `None` = 继承节点默认值；`Some(0)` = 该端口不限包数。
    #[serde(default)]
    pub packets: Option<usize>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputQueuesConfig {
    /// 所有正向输入口的默认包数容量。0 = 不限。
    #[serde(default)]
    pub packets: usize,
    /// 按输入口名覆盖；省略时继承上面的默认值，显式 0 表示不限。
    #[serde(default)]
    pub ports: std::collections::BTreeMap<String, InputQueueLimitConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    /// 展开前的配置来源路径，仅用于诊断；不参与 YAML 序列化。
    #[serde(skip)]
    pub(crate) source_path: String,
    #[serde(default)]
    pub name: String,
    /// 算子名。与 `type` 二选一:算子节点填 `kernel`,子图实例节点填 `type`(见 expand)。
    #[serde(default)]
    pub kernel: String,
    #[serde(default)]
    pub input_ports: Vec<String>,
    #[serde(default)]
    pub output_ports: Vec<String>,
    /// 本节点跑在哪个执行器上(按 `executors[].name` 引用)。**空 = 默认执行器**。
    #[serde(default)]
    pub executor: String,
    /// 该节点允许的并发 `process` 数。`0`/`1` = 串行(默认);`>1` 要求所属执行器
    /// 有**多于一个线程**(委托执行器 0 线程、单线程池并行度恒为 1,均会在建图期报错)。
    #[serde(default)]
    pub max_in_flight: usize,
    #[serde(default)]
    pub options: serde_yaml::Value,
    #[serde(default)]
    pub input_policy: InputPolicyConfig,
    /// 正向输入口的无损包数容量。
    ///
    /// 内部生产者遇满不会阻塞 worker，而是保留本次 staging、释放执行线程，
    /// 等下游弹包后协作式恢复刷新。`packets` 为节点默认值，`ports` 按端口覆盖；
    /// 与有损 `fixed_size` 互斥。
    #[serde(default)]
    pub input_queues: InputQueuesConfig,
    /// 子图名(ADR #27):非空 = 本节点是该子图的实例,建图期展开内联;与 `kernel` 二选一。
    #[serde(default)]
    pub r#type: String,
    /// 反馈环(back-edge):本节点哪些**输入口名**是「最新值反馈寄存器」—— 容量 1、留最新
    /// 反馈值、消费一次,且**不参与就绪 / 终止 / 时间戳对齐**(见 docs/design.md)。
    /// 用它才能让边成环:未被 back_edge 打断的拓扑环仍在建图期报错。
    #[serde(default)]
    pub back_edges: Vec<String>,
    /// 本节点算子失败时怎么办:`"abort"`(默认)或 `"skip"`。
    ///
    /// * `abort` —— 记录首个错误、终止全图(历史行为)。
    /// * `skip` —— **丢掉出错的那一个包**、推进下游时间戳边界、计数并打 WARN,然后继续跑。
    ///   用于长跑实时管线:一帧坏数据不该杀掉整条流水线。
    ///
    /// 只有这两个值(没有单独的 `log`:`skip` 本身一定会计数并打日志 ——
    /// 本项目不接受静默的有损行为,见 §7.6)。
    #[serde(default)]
    pub on_error: String,
    /// **源节点(0 输入)**的产出速率上限,单位 Hz(每秒次数)。`0`(默认)= 不限速。
    ///
    /// 声明式定速:设了它,引擎保证每两次 `process` 之间至少间隔 `1/rate` 秒 ——
    /// 算子不必自己写 sleep。用于「30fps 合成源」这类场景,也顺带避免非自定速的源
    /// 灌爆下游(内部边不背压,见 §7.5)。
    ///
    /// 只对源节点有意义;非源节点由上游数据驱动,设了会在建图期报错。
    /// 实现:本次调用完成后进入执行器延迟队列，不占用等待中的 worker。
    #[serde(default)]
    pub rate: f64,
    /// Conditional output routing configuration. Route fields are flattened so
    /// YAML can declare `type: route`, `mode`, `routes`, and `unmatched` directly.
    #[serde(flatten)]
    pub route: Option<RouteConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphConfig {
    #[serde(default)]
    pub executors: Vec<ExecutorConfig>,
    #[serde(default)]
    pub nodes: Vec<NodeConfig>,
    /// 其它 YAML 文件路径(相对本文件目录),引入其 `subgraphs` 定义。仅 `from_yaml_file` 生效。
    #[serde(default)]
    pub include: Vec<String>,
    /// 可复用子图库:名字 → 一张小图;节点用 `type: <名字>` 实例化,建图期展开内联。
    #[serde(default)]
    pub subgraphs: std::collections::BTreeMap<String, SubgraphConfig>,
    /// 图输入口(外部送包的入口)
    #[serde(default)]
    pub input_ports: Vec<String>,
    /// 图输出口(poller / observer 挂载点)
    #[serde(default)]
    pub output_ports: Vec<String>,
    /// 图输入口队列上限,也用作内部边的软水位
    #[serde(default = "default_max_queue_size")]
    pub max_queue_size: usize,
    /// 全局水位:全图在途包数上限(0 = 不限)
    #[serde(default)]
    pub max_queued_packets: usize,
    /// 单次算子回调超过该时长即打 WARN(0 = 关闭)
    #[serde(default)]
    pub watchdog_ms: u64,
    /// 算子内 buffer 分配池上限(字节, 0 = 关闭)
    #[serde(default)]
    pub buffer_pool_max_bytes: usize,
    /// 运行时统计级别。省略时为 `basic`。
    ///
    /// `full` 才记录每次回调耗时、百分位、CoW 与执行器耗时；`off` 进一步关闭低成本
    /// 吞吐和高水位计数。`watchdog_ms > 0` 时会强制提升为 `full`。
    #[serde(default)]
    pub stats: Option<StatsLevel>,
    /// 旧版兼容字段：`true` 等价于 `stats: full`，`false` 等价于 `stats: basic`。
    ///
    /// 不得与 `stats` 同时出现。
    #[serde(default)]
    pub stats_timing: Option<bool>,
    /// 逐次调用 trace 的有界环容量(条数,0 = 关闭)。
    ///
    /// 大于 0 时,每次 Open/Process/Close 回调记一条 span 进有界环(满了丢最旧),可经
    /// `to_chrome_trace()` 导出成 chrome://tracing / perfetto 可读的 JSON。开启会**强制**
    /// 统计提升为 `full`(需要每次回调计时),故不要在稳态生产中长开。
    #[serde(default)]
    pub trace_capacity: usize,
}

/// **必须与上面的 serde 默认值保持一致** —— 否则「YAML 省略字段」与 Rust
/// `..Default::default()` 两条路会得到不同行为，故手写而不 derive。
impl Default for GraphConfig {
    fn default() -> Self {
        Self {
            executors: Vec::new(),
            nodes: Vec::new(),
            include: Vec::new(),
            subgraphs: std::collections::BTreeMap::new(),
            input_ports: Vec::new(),
            output_ports: Vec::new(),
            max_queue_size: default_max_queue_size(),
            max_queued_packets: 0,
            watchdog_ms: 0,
            buffer_pool_max_bytes: 0,
            stats: None,
            stats_timing: None,
            trace_capacity: 0,
        }
    }
}

/// 与 serde 默认逐字段对齐(照 [`InputPolicyConfig`] 的先例手写、不 derive):
/// `input_policy` 的默认 `type` 是 `"sync"` 而非空串,derive 会给错。
/// 有了它,以后给 `NodeConfig` 加字段不会再打断仓库内的结构体字面量。
impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            source_path: String::new(),
            name: String::new(),
            kernel: String::new(),
            input_ports: Vec::new(),
            output_ports: Vec::new(),
            executor: String::new(),
            max_in_flight: 0,
            options: serde_yaml::Value::default(),
            input_policy: InputPolicyConfig::default(),
            input_queues: InputQueuesConfig::default(),
            r#type: String::new(),
            back_edges: Vec::new(),
            on_error: String::new(),
            rate: 0.0,
            route: None,
        }
    }
}

/// 子图定义:一张可复用的小图(ADR #27)。**不声明 executor** —— 内部节点按名引用主图的执行器。
/// 边界口按**位置**对应实例节点的 `input_ports` / `output_ports`(见 expand)。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubgraphConfig {
    #[serde(default)]
    pub nodes: Vec<NodeConfig>,
    /// 子图边界输入口:按位置对应实例节点的 `input_ports`。
    #[serde(default)]
    pub input_ports: Vec<String>,
    /// 子图边界输出口:按位置对应实例节点的 `output_ports`。
    #[serde(default)]
    pub output_ports: Vec<String>,
}

impl GraphConfig {
    /// Parse, resolve includes, expand subgraphs, and validate configuration-only rules.
    ///
    /// This entry point deliberately does not access the kernel registry or create executors.
    /// Use it for deployment-time checks where the host may not have linked its kernels yet.
    pub fn preflight_from_yaml(text: &str) -> Result<Self> {
        let config = Self::from_yaml(text)?;
        Ok(GraphPlan::build(config)?.config)
    }

    /// File equivalent of [`Self::preflight_from_yaml`].
    pub fn preflight_from_yaml_file(path: &str) -> Result<Self> {
        let config = Self::from_yaml_file(path)?;
        Ok(GraphPlan::build(config)?.config)
    }

    fn validate_preflight(&self) -> Result<()> {
        validate_preflight_executors(self)?;
        Ok(())
    }

    pub fn effective_stats_level(&self) -> StatsLevel {
        if self.watchdog_ms > 0 || self.trace_capacity > 0 {
            // watchdog 与 trace 都依赖每次回调的计时,故都强制提升为 full。
            return StatsLevel::Full;
        }
        self.stats.unwrap_or_else(|| {
            self.stats_timing.map_or(StatsLevel::Basic, |enabled| {
                if enabled {
                    StatsLevel::Full
                } else {
                    StatsLevel::Basic
                }
            })
        })
    }

    /// 只做 serde 解析:不校验、不展开、不解析 include。管线内部用。
    pub fn parse(text: &str) -> Result<Self> {
        serde_yaml::from_str(text).map_err(|e| Error::InvalidArg(format!("YAML parse failed: {e}")))
    }

    /// 从 YAML 文本建图配置:解析 → 展开子图 → 校验,返回**展平**(无 `type:` 节点)的配置。
    /// 文本入口不支持 `include`(相对路径无从解析);需要 include 请用 [`GraphConfig::from_yaml_file`]。
    pub fn from_yaml(text: &str) -> Result<Self> {
        let cfg = Self::parse(text)?;
        if !cfg.include.is_empty() {
            return Err(Error::InvalidArg(
                "`include` is only supported when loading from a file (from_yaml_file); \
                 relative paths cannot be resolved from a text string"
                    .into(),
            ));
        }
        let flat = crate::expand::expand(cfg)?;
        flat.check_supported()?;
        Ok(flat)
    }

    /// 从 YAML 文件建图配置:读文件 → 按本文件目录递归解析 + 合并 `include` 的子图
    /// → 展开子图 → 校验。返回展平的配置。
    pub fn from_yaml_file(path: &str) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::InvalidArg(format!("failed to read `{path}`: {e}")))?;
        let cfg = Self::parse(&text).map_err(|error| error.context(format!("file `{path}`")))?;
        let merged = crate::expand::resolve_includes(cfg, std::path::Path::new(path))?;
        let flat = crate::expand::expand(merged)?;
        flat.check_supported()?;
        Ok(flat)
    }

    /// 只检查「本版本是否支持」,拓扑合法性在 Graph::build 里查。
    fn check_supported(&self) -> Result<()> {
        if self.stats.is_some() && self.stats_timing.is_some() {
            return Err(Error::InvalidArg(
                "`stats` and legacy `stats_timing` cannot be used together".into(),
            ));
        }
        for (node_index, n) in self.nodes.iter().enumerate() {
            let node_path = format!("nodes[{node_index}]");
            let who = if n.name.is_empty() {
                n.kernel.clone()
            } else {
                n.name.clone()
            };
            if n.r#type == "route" {
                let route = n.route.as_ref().ok_or_else(|| {
                    Error::InvalidArg(format!(
                        "{node_path}: route node `{who}` requires route fields (`routes` or `unmatched`)"
                    ))
                })?;
                if !n.kernel.is_empty() {
                    return Err(Error::InvalidArg(format!(
                        "{node_path}: route node `{who}` must not declare `kernel`"
                    )));
                }
                if n.input_ports.len() != 1 {
                    return Err(Error::InvalidArg(format!(
                        "{node_path}.input_ports (node `{who}`): route nodes require exactly one input port"
                    )));
                }
                if n.output_ports.is_empty() {
                    return Err(Error::InvalidArg(format!(
                        "{node_path}.output_ports (node `{who}`): route nodes require at least one output port"
                    )));
                }
                let output_names: BTreeSet<String> = n
                    .output_ports
                    .iter()
                    .map(|decl| parse_port_spec(decl).map(|spec| spec.name))
                    .collect::<Result<_>>()?;
                let mut defaults = 0usize;
                for (rule_index, rule) in route.routes.iter().enumerate() {
                    if !output_names.contains(&rule.to) {
                        return Err(Error::InvalidArg(format!(
                            "{node_path}.routes[{rule_index}].to (node `{who}`): unknown output port `{}`",
                            rule.to
                        )));
                    }
                    if rule.default {
                        defaults += 1;
                        if rule.when.is_some() {
                            return Err(Error::InvalidArg(format!(
                                "{node_path}.routes[{rule_index}] (node `{who}`): default route must not declare `when`"
                            )));
                        }
                    } else {
                        let predicate = rule.when.as_ref().ok_or_else(|| {
                            Error::InvalidArg(format!(
                                "{node_path}.routes[{rule_index}] (node `{who}`): non-default route requires `when`"
                            ))
                        })?;
                        predicate.validate().map_err(|error| {
                            error.context(format!("{node_path}.routes[{rule_index}].when"))
                        })?;
                    }
                }
                if defaults > 1 {
                    return Err(Error::InvalidArg(format!(
                        "{node_path}.routes (node `{who}`): at most one default route is allowed"
                    )));
                }
                match route.unmatched.as_str() {
                    "drop" | "error" => {}
                    target if output_names.contains(target) => {}
                    other => {
                        return Err(Error::InvalidArg(format!(
                            "{node_path}.unmatched (node `{who}`): expected `drop`, `error`, or an output port, got `{other}`"
                        )));
                    }
                }
                if route.routes.is_empty() && route.unmatched == "drop" {
                    return Err(Error::InvalidArg(format!(
                        "{node_path}.routes (node `{who}`): at least one route rule is required"
                    )));
                }
            } else if n
                .route
                .as_ref()
                .is_some_and(|route| !route.is_implicit_default())
            {
                return Err(Error::InvalidArg(format!(
                    "{node_path} (node `{who}`): route fields are only valid when `type: route`"
                )));
            }
            // `max_in_flight` 与「源节点该挂什么执行器」都要看解析出的执行器长什么样
            // (是池还是委托、几个线程),光看 YAML 里的名字答不上来 ——
            // 那两条校验在 Graph::build 的 check_node_executor_fit 里。
            // rate 定速:只对源节点有意义(非源由上游数据驱动),且必须为正。
            if n.rate != 0.0 {
                if !n.input_ports.is_empty() {
                    return Err(Error::InvalidArg(format!(
                        "{node_path}.rate (node `{who}`): rate only applies to source nodes (no input ports); \
                         a non-source is driven by upstream data"
                    )));
                }
                // 要求正的有限值。`> 0.0` 一并挡住 0、负数和 NaN(NaN 的比较恒假),
                // `is_finite` 挡住 inf。
                if !(n.rate.is_finite() && n.rate > 0.0) {
                    return Err(Error::InvalidArg(format!(
                        "{node_path}.rate (node `{who}`): rate must be a positive, finite number (Hz), got {}",
                        n.rate
                    )));
                }
            }
            match n.input_policy.r#type.as_str() {
                "sync" | "immediate" => {}
                "fixed_size" => {
                    if n.input_policy.capacity == 0 {
                        // 容量 0 意味着「每个包都丢」,几乎肯定是漏配
                        return Err(Error::InvalidArg(format!(
                            "{node_path}.input_policy.capacity (node `{who}`): fixed_size policy capacity must be >= 1"
                        )));
                    }
                }
                "batch" => {
                    // batch:攒够 capacity 个**对齐元组**一次交给算子。
                    if n.input_policy.capacity == 0 {
                        return Err(Error::InvalidArg(format!(
                            "{node_path}.input_policy.capacity (node `{who}`): batch policy capacity (the batch size) must be >= 1"
                        )));
                    }
                }
                // sync_set 的分组合法性(名字存在、完整划分)在 Graph::build 里查 ——
                // 那里才有输入口名表。这里只放行类型名。
                "sync_set" => {
                    if n.input_policy.sets.is_empty() {
                        return Err(Error::InvalidArg(format!(
                            "{node_path}.input_policy.sets (node `{who}`): sync_set policy must provide sets (input port groups)"
                        )));
                    }
                }
                other => {
                    return Err(Error::InvalidArg(format!(
                    "{node_path}.input_policy.type (node `{who}`): unknown input_policy `{other}`{} (valid: sync / immediate / fixed_size / sync_set / batch)",
                    crate::diagnostic::did_you_mean(
                        other,
                        ["sync", "immediate", "fixed_size", "sync_set", "batch"]
                    )
                )))
                }
            }
            let port_names: Vec<String> = n
                .input_ports
                .iter()
                .enumerate()
                .map(|(port_index, declaration)| {
                    parse_port_spec(declaration)
                        .map(|spec| spec.name)
                        .map_err(|error| {
                            error.context(format!("{node_path}.input_ports[{port_index}]"))
                        })
                })
                .collect::<Result<_>>()?;
            for port in n.input_queues.ports.keys() {
                if !port_names.contains(port) {
                    return Err(Error::InvalidArg(format!(
                        "{node_path}.input_queues.ports.{port} (node `{who}`): input queue capacity override references unknown input port `{port}`{}",
                        crate::diagnostic::did_you_mean(
                            port,
                            port_names.iter().map(String::as_str)
                        )
                    )));
                }
            }
            for (port, limits) in &n.input_queues.ports {
                if limits.packets.is_some_and(|capacity| capacity != 0)
                    && n.back_edges.contains(port)
                {
                    return Err(Error::InvalidArg(format!(
                        "{node_path}.input_queues.ports.{port} (node `{who}`): input queue capacity override for back-edge input `{port}` is not supported; \
                         back-edges always use their capacity-1 latest-value register"
                    )));
                }
            }
            let has_lossless_capacity = n.input_queues.packets != 0
                || n.input_queues
                    .ports
                    .values()
                    .any(|limits| limits.packets.is_some_and(|capacity| capacity != 0));
            if has_lossless_capacity && n.input_policy.r#type == "fixed_size" {
                return Err(Error::InvalidArg(format!(
                    "{node_path}.input_queues (node `{who}`): lossless input queue capacities cannot be combined with \
                     input_policy=fixed_size (lossy drop-oldest)"
                )));
            }

            // 错误策略:未知值明确拒掉,不静默当默认(与 input_policy / executor type 同规矩)。
            if !n.on_error.is_empty() && n.on_error != "abort" && n.on_error != "skip" {
                return Err(Error::InvalidArg(format!(
                    "{node_path}.on_error (node `{who}`): unknown on_error `{}`{} (expected \"abort\" or \"skip\")",
                    n.on_error,
                    crate::diagnostic::did_you_mean(&n.on_error, ["abort", "skip"])
                )));
            }

            // 反馈环:back_edges 名字须是本节点输入口;须留至少一个正向输入口驱动;不得与 sync_set 冲突。
            if !n.back_edges.is_empty() {
                for be in &n.back_edges {
                    if !port_names.contains(be) {
                        return Err(Error::InvalidArg(format!(
                            "{node_path}.back_edges (node `{who}`): back_edge `{be}` is not one of this node's input ports{}",
                            crate::diagnostic::did_you_mean(
                                be,
                                port_names.iter().map(String::as_str)
                            )
                        )));
                    }
                }
                let forward = port_names
                    .iter()
                    .filter(|p| !n.back_edges.contains(p))
                    .count();
                if forward == 0 {
                    return Err(Error::InvalidArg(format!(
                        "{node_path}.back_edges (node `{who}`): every input port is a back_edge -- a node needs at least one forward input to ever fire"
                    )));
                }
                if n.input_policy.r#type == "sync_set" {
                    for set in &n.input_policy.sets {
                        if let Some(name) = set.iter().find(|p| n.back_edges.contains(p)) {
                            return Err(Error::InvalidArg(format!(
                                "{node_path}.input_policy.sets (node `{who}`): back_edge `{name}` must not appear in a sync_set group"
                            )));
                        }
                    }
                }
            }
        }
        for (executor_index, e) in self.executors.iter().enumerate() {
            // 空 type 视作 ThreadPoolExecutor(历史默认)。字段是否对得上类型
            // (如 DelegatingExecutor 不该配 num_threads)在 Graph::build 里查。
            if !matches!(
                e.r#type.as_str(),
                "" | "ThreadPoolExecutor" | "DelegatingExecutor"
            ) {
                return Err(Error::InvalidArg(format!(
                    "executors[{executor_index}].type: unknown executor type `{}`{} (supported: ThreadPoolExecutor, DelegatingExecutor)",
                    e.r#type,
                    crate::diagnostic::did_you_mean(
                        &e.r#type,
                        ["ThreadPoolExecutor", "DelegatingExecutor"]
                    )
                )));
            }
        }
        Ok(())
    }
}

/// 展开并完成配置语义校验后的不可变建图计划。
///
/// 计划阶段不查询 kernel registry、不创建 executor、不启动线程；运行时建图和
/// `lmflow check-config` 都消费同一个计划，避免“预检通过但实际建图失败”的两套规则。
#[derive(Debug, Clone)]
pub struct GraphPlan {
    pub config: GraphConfig,
    pub nodes: Vec<GraphPlanNode>,
    pub edges: Vec<GraphPlanEdge>,
    pub(crate) graph_inputs: Vec<usize>,
    pub(crate) graph_outputs: Vec<usize>,
    pub(crate) input_by_name: BTreeMap<String, usize>,
    pub(crate) output_by_name: BTreeMap<String, usize>,
    pub(crate) edge_by_name: BTreeMap<String, usize>,
    pub(crate) back_edge_mask: Vec<Vec<bool>>,
}

#[derive(Debug, Clone)]
pub struct GraphPlanNode {
    pub index: usize,
    pub name: String,
    pub kernel: String,
    pub executor: String,
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    pub route: Option<RouteConfig>,
    pub(crate) input_ports: Arc<PortTable>,
    pub(crate) output_ports: Arc<PortTable>,
    pub(crate) input_edges: Vec<usize>,
    pub(crate) output_edges: Vec<usize>,
    pub(crate) executor_index: usize,
}

#[derive(Debug, Clone)]
pub struct GraphPlanEdge {
    pub name: String,
    pub producer: Option<usize>,
    pub consumers: Vec<usize>,
    pub graph_input: bool,
    pub graph_output: bool,
    pub(crate) consumer_ports: Vec<(usize, usize)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphPlanDiagnostic {
    pub code: String,
    pub message: String,
}

impl GraphPlan {
    pub fn build(config: GraphConfig) -> Result<Self> {
        config.validate_preflight()?;
        let mut known_edges = BTreeSet::new();
        let mut producers = BTreeMap::new();
        let mut graph_input_names = BTreeSet::new();
        for (index, declaration) in config.input_ports.iter().enumerate() {
            let spec = parse_port_spec(declaration)
                .map_err(|error| error.context(format!("input_ports[{index}]")))?;
            if !known_edges.insert(spec.name.clone()) {
                return Err(Error::InvalidArg(format!(
                    "input_ports[{index}]: graph input port `{}` is declared more than once",
                    spec.name
                )));
            }
            graph_input_names.insert(spec.name);
        }
        let mut port_tables = Vec::with_capacity(config.nodes.len());
        for (index, node) in config.nodes.iter().enumerate() {
            let node_path = diagnostic_node_path(node, index);
            let inputs = Arc::new(
                PortTable::build(&node.input_ports, &format!("{node_path}.input_ports"))
                    .map_err(|error| error.context(format!("{node_path}.input_ports")))?,
            );
            let outputs = Arc::new(
                PortTable::build(&node.output_ports, &format!("{node_path}.output_ports"))
                    .map_err(|error| error.context(format!("{node_path}.output_ports")))?,
            );
            for name in outputs.names() {
                if graph_input_names.contains(name) {
                    return Err(Error::InvalidArg(format!(
                        "{node_path}.output_ports: port `{name}` is also a graph input"
                    )));
                }
                if let Some(previous) = producers.insert(name.clone(), index) {
                    let previous_path = diagnostic_node_path(&config.nodes[previous], previous);
                    return Err(Error::InvalidArg(format!(
                        "{node_path}.output_ports: port `{name}` has multiple producers \
                         ({previous_path} and {node_path})"
                    )));
                }
                known_edges.insert(name.clone());
            }
            port_tables.push((inputs, outputs));
        }
        let mut adjacency = vec![Vec::new(); config.nodes.len()];
        for (index, (inputs, _)) in port_tables.iter().enumerate() {
            let node_path = diagnostic_node_path(&config.nodes[index], index);
            for (port, name) in inputs.names().iter().enumerate() {
                if !graph_input_names.contains(name) && !producers.contains_key(name) {
                    let suggestion = crate::diagnostic::did_you_mean(
                        name,
                        known_edges.iter().map(String::as_str),
                    );
                    return Err(Error::InvalidArg(format!(
                        "{node_path}.input_ports[{port}]: port `{name}` has no producer{suggestion}"
                    )));
                }
                if let Some(&producer) = producers.get(name) {
                    if !config.nodes[index]
                        .back_edges
                        .iter()
                        .any(|edge| edge == name)
                    {
                        adjacency[producer].push(index);
                    }
                }
            }
        }
        for (index, declaration) in config.output_ports.iter().enumerate() {
            let spec = parse_port_spec(declaration)
                .map_err(|error| error.context(format!("output_ports[{index}]")))?;
            if !known_edges.contains(&spec.name) {
                let suggestion = crate::diagnostic::did_you_mean(
                    &spec.name,
                    known_edges.iter().map(String::as_str),
                );
                return Err(Error::InvalidArg(format!(
                    "output_ports[{index}]: graph output `{}` has no producer{suggestion}",
                    spec.name,
                )));
            }
        }
        check_preflight_acyclic(&adjacency, &config)?;
        let mut edges = Vec::<GraphPlanEdge>::new();
        let mut edge_by_name = BTreeMap::<String, usize>::new();

        let mut graph_inputs = Vec::new();
        let mut input_by_name = BTreeMap::new();
        for declaration in &config.input_ports {
            let name = parse_port_spec(declaration)?.name;
            let edge = plan_edge(&name, &mut edges, &mut edge_by_name);
            edges[edge].graph_input = true;
            graph_inputs.push(edge);
            input_by_name.insert(name, edge);
        }

        let executor_index_by_name: BTreeMap<&str, usize> = std::iter::once(("default", 0))
            .chain(
                config
                    .executors
                    .iter()
                    .enumerate()
                    .map(|(index, executor)| (executor.name.as_str(), index + 1)),
            )
            .collect();

        let mut nodes = Vec::with_capacity(config.nodes.len());
        for (index, node) in config.nodes.iter().enumerate() {
            let (input_ports, output_ports) = port_tables[index].clone();
            let inputs = input_ports.names().to_vec();
            let outputs = output_ports.names().to_vec();
            let mut output_edges = Vec::with_capacity(outputs.len());
            for name in &outputs {
                let edge = plan_edge(name, &mut edges, &mut edge_by_name);
                edges[edge].producer = Some(index);
                output_edges.push(edge);
            }
            let mut input_edges = Vec::with_capacity(inputs.len());
            for (port, name) in inputs.iter().enumerate() {
                let edge = plan_edge(name, &mut edges, &mut edge_by_name);
                edges[edge].consumers.push(index);
                edges[edge].consumer_ports.push((index, port));
                input_edges.push(edge);
            }
            let executor = if node.executor.is_empty() {
                "default"
            } else {
                node.executor.as_str()
            };
            nodes.push(GraphPlanNode {
                index,
                name: if node.name.is_empty() {
                    let kind = if node.r#type == "route" {
                        "route"
                    } else {
                        node.kernel.as_str()
                    };
                    format!("{kind}#{index}")
                } else {
                    node.name.clone()
                },
                kernel: node.kernel.clone(),
                executor: executor.into(),
                inputs,
                outputs,
                route: (node.r#type == "route")
                    .then(|| node.route.clone())
                    .flatten(),
                input_ports,
                output_ports,
                input_edges,
                output_edges,
                executor_index: executor_index_by_name[executor],
            });
        }

        let mut graph_outputs = Vec::new();
        let mut output_by_name = BTreeMap::new();
        for declaration in &config.output_ports {
            let name = parse_port_spec(declaration)?.name;
            let edge = edge_by_name[&name];
            edges[edge].graph_output = true;
            graph_outputs.push(edge);
            output_by_name.insert(name, edge);
        }
        let back_edge_mask = config
            .nodes
            .iter()
            .zip(&nodes)
            .map(|(node, planned)| {
                planned
                    .inputs
                    .iter()
                    .map(|name| node.back_edges.contains(name))
                    .collect()
            })
            .collect();
        Ok(Self {
            config,
            nodes,
            edges,
            graph_inputs,
            graph_outputs,
            input_by_name,
            output_by_name,
            edge_by_name,
            back_edge_mask,
        })
    }

    /// Export the validated static plan as Graphviz DOT without loading kernels or executors.
    pub fn to_dot(&self) -> String {
        crate::dot::render_plan(self)
    }

    pub fn diagnostics(&self) -> Vec<GraphPlanDiagnostic> {
        let mut diagnostics = Vec::new();
        for edge in &self.edges {
            if !edge.consumers.is_empty() || edge.graph_output {
                continue;
            }
            if edge.graph_input {
                diagnostics.push(GraphPlanDiagnostic {
                    code: "unconsumed_graph_input".into(),
                    message: format!(
                        "graph input port `{}` is consumed by no node; packets sent in will be dropped",
                        edge.name
                    ),
                });
            } else if let Some(producer) = edge.producer {
                diagnostics.push(GraphPlanDiagnostic {
                    code: "unconsumed_node_output".into(),
                    message: format!(
                        "node `{}` output port `{}` has no downstream consumer and is not a graph output; output will be dropped",
                        self.nodes[producer].name, edge.name
                    ),
                });
            }
        }
        for executor in &self.config.executors {
            if executor.r#type == "DelegatingExecutor"
                || self.nodes.iter().any(|node| node.executor == executor.name)
            {
                continue;
            }
            diagnostics.push(GraphPlanDiagnostic {
                code: "unused_executor".into(),
                message: format!(
                    "executor `{}` is defined but not used by any node ({} threads will idle)",
                    executor.name,
                    executor.num_threads.max(1)
                ),
            });
        }
        for node in &self.nodes {
            let Some(route) = &node.route else { continue };
            if route.routes.iter().any(|rule| rule.default) && route.unmatched != "drop" {
                diagnostics.push(GraphPlanDiagnostic {
                    code: "route_unreachable_unmatched".into(),
                    message: format!(
                        "route node `{}` has a default rule, so unmatched policy `{}` is unreachable",
                        node.name, route.unmatched
                    ),
                });
            }
            if route.mode == RouteMode::First {
                for (index, rule) in route.routes.iter().enumerate() {
                    if rule.default {
                        continue;
                    }
                    if let Some(previous) = route.routes[..index]
                        .iter()
                        .position(|candidate| !candidate.default && candidate.when == rule.when)
                    {
                        diagnostics.push(GraphPlanDiagnostic {
                            code: "route_shadowed_rule".into(),
                            message: format!(
                                "route node `{}` rule {} duplicates earlier rule {}; first mode makes it unreachable",
                                node.name,
                                index + 1,
                                previous + 1
                            ),
                        });
                    }
                }
            }
            if route.mode == RouteMode::All {
                for (index, rule) in route.routes.iter().enumerate() {
                    if route.routes[..index].iter().any(|previous| {
                        !rule.default
                            && !previous.default
                            && previous.to == rule.to
                            && previous.when == rule.when
                    }) {
                        diagnostics.push(GraphPlanDiagnostic {
                            code: "route_duplicate_emit".into(),
                            message: format!(
                                "route node `{}` rule {} duplicates an earlier rule and may emit the same packet twice to `{}`",
                                node.name,
                                index + 1,
                                rule.to
                            ),
                        });
                    }
                }
            }
        }
        diagnostics
    }
}

fn plan_edge(
    name: &str,
    edges: &mut Vec<GraphPlanEdge>,
    edge_by_name: &mut BTreeMap<String, usize>,
) -> usize {
    if let Some(&index) = edge_by_name.get(name) {
        return index;
    }
    let index = edges.len();
    edges.push(GraphPlanEdge {
        name: name.to_string(),
        producer: None,
        consumers: Vec::new(),
        graph_input: false,
        graph_output: false,
        consumer_ports: Vec::new(),
    });
    edge_by_name.insert(name.to_string(), index);
    index
}

impl StatsLevel {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Basic => "basic",
            Self::Full => "full",
        }
    }
}

fn validate_preflight_executors(config: &GraphConfig) -> Result<()> {
    let mut names = BTreeSet::new();
    for (index, executor) in config.executors.iter().enumerate() {
        if executor.name.is_empty() {
            return Err(Error::InvalidArg(format!(
                "executors[{index}].name: executor entry must have a name; nodes select an executor by it"
            )));
        }
        if executor.name == "default" {
            return Err(Error::InvalidArg(format!(
                "executors[{index}].name: `default` is reserved for the implicit executor"
            )));
        }
        if !names.insert(&executor.name) {
            return Err(Error::InvalidArg(format!(
                "executors[{index}].name: executor `{}` defined more than once",
                executor.name
            )));
        }
        if !matches!(
            executor.r#type.as_str(),
            "" | "ThreadPoolExecutor" | "DelegatingExecutor"
        ) {
            return Err(Error::InvalidArg(format!(
                "executors[{index}].type: unknown executor type `{}`",
                executor.r#type
            )));
        }
        if executor.r#type == "DelegatingExecutor"
            && (executor.num_threads != 0
                || !executor.affinity.is_empty()
                || executor.priority != 0)
        {
            return Err(Error::InvalidArg(format!(
                "executors[{index}]: DelegatingExecutor owns no threads, so `num_threads`, \
                 `affinity`, and `priority` are meaningless; drop those fields or use \
                 type: \"ThreadPoolExecutor\""
            )));
        }
    }
    for (index, node) in config.nodes.iter().enumerate() {
        let node_path = diagnostic_node_path(node, index);
        if !node.executor.is_empty() && !names.contains(&node.executor) {
            let suggestion = crate::diagnostic::did_you_mean(
                &node.executor,
                names.iter().map(|name| name.as_str()),
            );
            return Err(Error::InvalidArg(format!(
                "{node_path}.executor: undefined executor `{}`{suggestion}",
                node.executor,
            )));
        }
        if node.max_in_flight > 1 {
            let threads = if node.executor.is_empty() {
                std::thread::available_parallelism()
                    .map(|value| value.get())
                    .unwrap_or(1)
            } else {
                config
                    .executors
                    .iter()
                    .find(|executor| executor.name == node.executor)
                    .map(|executor| executor.num_threads.max(1))
                    .unwrap_or(1)
            };
            if threads < 2 {
                return Err(Error::InvalidArg(format!(
                    "{node_path}.max_in_flight={} needs an executor with more than one thread, \
                     but the selected executor provides {} -- there would be no parallelism",
                    node.max_in_flight, threads
                )));
            }
        }
    }
    for (index, node) in config.nodes.iter().enumerate() {
        let node_path = diagnostic_node_path(node, index);
        if node.input_ports.is_empty()
            && !node.executor.is_empty()
            && config.executors.iter().any(|executor| {
                executor.name == node.executor && executor.r#type == "DelegatingExecutor"
            })
        {
            return Err(Error::InvalidArg(format!(
                "{node_path}.executor: source nodes cannot run on DelegatingExecutor"
            )));
        }
        if node.input_policy.r#type == "sync_set" {
            let ports: BTreeSet<String> = node
                .input_ports
                .iter()
                .map(|declaration| parse_port_spec(declaration).map(|spec| spec.name))
                .collect::<Result<_>>()?;
            let mut seen = BTreeSet::new();
            for (group_index, group) in node.input_policy.sets.iter().enumerate() {
                if group.is_empty() {
                    return Err(Error::InvalidArg(format!(
                        "{node_path}.input_policy.sets[{group_index}]: group must not be empty"
                    )));
                }
                for name in group {
                    if !ports.contains(name) {
                        return Err(Error::InvalidArg(format!(
                                "{node_path}.input_policy.sets[{group_index}]: unknown input port `{name}`"
                            )));
                    }
                    if !seen.insert(name) {
                        return Err(Error::InvalidArg(format!(
                                "{node_path}.input_policy.sets[{group_index}]: input port `{name}` appears in multiple groups"
                            )));
                    }
                }
            }
            if let Some(name) = ports.iter().find(|name| !seen.contains(*name)) {
                return Err(Error::InvalidArg(format!(
                    "{node_path}.input_policy.sets: input port `{name}` is not assigned to a group"
                )));
            }
        }
    }
    Ok(())
}

pub(crate) fn diagnostic_node_path(node: &NodeConfig, index: usize) -> String {
    if node.source_path.is_empty() {
        format!("nodes[{index}]")
    } else if node.name.is_empty() {
        node.source_path.clone()
    } else {
        format!("{} (expanded node `{}`)", node.source_path, node.name)
    }
}

fn check_preflight_acyclic(adjacency: &[Vec<usize>], config: &GraphConfig) -> Result<()> {
    let mut marks = vec![0u8; adjacency.len()];
    fn visit(
        node: usize,
        adjacency: &[Vec<usize>],
        marks: &mut [u8],
        config: &GraphConfig,
    ) -> Result<()> {
        marks[node] = 1;
        for &next in &adjacency[node] {
            if marks[next] == 1 {
                let current = preflight_node_label(&config.nodes[node], node);
                let target = preflight_node_label(&config.nodes[next], next);
                return Err(Error::InvalidArg(format!(
                    "topology cycle: node `{target}` -> ... -> `{current}`; mark a feedback input with `back_edges`"
                )));
            }
            if marks[next] == 0 {
                visit(next, adjacency, marks, config)?;
            }
        }
        marks[node] = 2;
        Ok(())
    }
    for node in 0..adjacency.len() {
        if marks[node] == 0 {
            visit(node, adjacency, &mut marks, config)?;
        }
    }
    Ok(())
}

fn preflight_node_label(node: &NodeConfig, index: usize) -> String {
    if node.name.is_empty() {
        format!("{}#{index}", node.kernel)
    } else {
        node.name.clone()
    }
}

/// 端口声明的解析结果:`"TAG:index:name"` / `"TAG:name"` / `"name"`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortSpec {
    pub tag: String,
    /// None 表示「按同 tag 内出现次序自动编号」
    pub index: Option<usize>,
    pub name: String,
}

/// 解析端口声明。参见 docs/design.md §6.4。
pub fn parse_port_spec(decl: &str) -> Result<PortSpec> {
    let parts: Vec<&str> = decl.split(':').collect();
    let spec = match parts.len() {
        1 => PortSpec {
            tag: String::new(),
            index: None,
            name: parts[0].to_string(),
        },
        2 => PortSpec {
            tag: parts[0].to_string(),
            index: None,
            name: parts[1].to_string(),
        },
        3 => {
            let idx: usize = parts[1].parse().map_err(|_| {
                Error::InvalidArg(format!(
                    "port declaration `{decl}`: index `{}` is not a non-negative integer",
                    parts[1]
                ))
            })?;
            PortSpec {
                tag: parts[0].to_string(),
                index: Some(idx),
                name: parts[2].to_string(),
            }
        }
        _ => {
            return Err(Error::InvalidArg(format!(
                "port declaration `{decl}` has an invalid number of segments (expected name / TAG:name / TAG:index:name)"
            )))
        }
    };
    if spec.name.is_empty() {
        return Err(Error::InvalidArg(format!(
            "port declaration `{decl}` has an empty name"
        )));
    }
    Ok(spec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_graph() {
        let cfg = GraphConfig::from_yaml(
            r#"
nodes:
  - name: "n1"
    kernel: "PassThroughKernel"
    input_ports: ["a"]
    output_ports: ["b"]
input_ports: ["a"]
output_ports: ["b"]
"#,
        )
        .unwrap();
        assert_eq!(cfg.nodes.len(), 1);
        assert_eq!(cfg.nodes[0].kernel, "PassThroughKernel");
        assert_eq!(cfg.input_ports, vec!["a"]);
        assert_eq!(cfg.max_queue_size, 100, "default queue limit");
        assert_eq!(cfg.effective_stats_level(), StatsLevel::Basic);
    }

    #[test]
    fn parses_and_plans_conditional_route() {
        let cfg = GraphConfig::from_yaml(
            r#"
input_ports: [detections]
output_ports: [high, medium, low]
nodes:
  - name: confidence_route
    type: route
    input_ports: [detections]
    output_ports: [high, medium, low]
    mode: first
    unmatched: error
    routes:
      - to: high
        when: { metadata: confidence, op: gte, value: 0.8 }
      - to: medium
        when:
          all:
            - { metadata: confidence, op: gte, value: 0.5 }
            - not: { metadata: suppressed, op: eq, value: true }
      - to: low
        default: true
"#,
        )
        .unwrap();
        let plan = GraphPlan::build(cfg).unwrap();
        let route = plan.nodes[0].route.as_ref().unwrap();
        assert_eq!(route.mode, RouteMode::First);
        assert_eq!(route.unmatched, "error");
        assert_eq!(route.routes.len(), 3);
        assert!(route.routes[2].default);
    }

    #[test]
    fn parses_route_all_mode_and_timestamp_condition() {
        let cfg = GraphConfig::from_yaml(
            r#"
input_ports: [input]
output_ports: [recent]
nodes:
  - type: route
    input_ports: [input]
    output_ports: [recent]
    mode: all
    unmatched: drop
    routes:
      - to: recent
        when: { timestamp: { op: gte, value: 1000 } }
"#,
        )
        .unwrap();
        let route = GraphPlan::build(cfg).unwrap().nodes[0]
            .route
            .clone()
            .unwrap();
        assert_eq!(route.mode, RouteMode::All);
    }

    #[test]
    fn rejects_invalid_route_configuration() {
        for (yaml, needle) in [
            (
                r#"
nodes:
  - type: route
    input_ports: [a, b]
    output_ports: [out]
    routes: [{ to: out, default: true }]
"#,
                "exactly one input",
            ),
            (
                r#"
nodes:
  - type: route
    input_ports: [in]
    output_ports: [out]
    routes: [{ to: typo, default: true }]
"#,
                "unknown output port",
            ),
            (
                r#"
nodes:
  - type: route
    input_ports: [in]
    output_ports: [out]
    routes:
      - { to: out, default: true }
      - { to: out, default: true }
"#,
                "at most one default",
            ),
            (
                r#"
nodes:
  - type: route
    input_ports: [in]
    output_ports: [out]
    routes: [{ to: out }]
"#,
                "requires `when`",
            ),
            (
                r#"
nodes:
  - kernel: K
    input_ports: [in]
    output_ports: [out]
    routes: [{ to: out, default: true }]
"#,
                "only valid when `type: route`",
            ),
        ] {
            let error = GraphConfig::from_yaml(yaml)
                .and_then(GraphPlan::build)
                .unwrap_err();
            assert!(error.to_string().contains(needle), "{error}");
        }
    }

    #[test]
    fn diagnoses_shadowed_and_duplicate_route_rules() {
        let first = GraphConfig::from_yaml(
            r#"
nodes:
  - name: router
    type: route
    input_ports: [in]
    output_ports: [a, b]
    mode: first
    unmatched: error
    routes:
      - { to: a, when: { metadata: score, op: gte, value: 0.5 } }
      - { to: b, when: { metadata: score, op: gte, value: 0.5 } }
      - { to: b, default: true }
input_ports: [in]
output_ports: [a, b]
"#,
        )
        .and_then(GraphPlan::build)
        .unwrap();
        let codes: Vec<_> = first
            .diagnostics()
            .into_iter()
            .map(|diagnostic| diagnostic.code)
            .collect();
        assert!(codes.contains(&"route_shadowed_rule".to_string()));
        assert!(codes.contains(&"route_unreachable_unmatched".to_string()));

        let all = GraphConfig::from_yaml(
            r#"
nodes:
  - name: router
    type: route
    input_ports: [in]
    output_ports: [out]
    mode: all
    routes:
      - { to: out, when: { metadata: score, op: gte, value: 0.5 } }
      - { to: out, when: { metadata: score, op: gte, value: 0.5 } }
input_ports: [in]
output_ports: [out]
"#,
        )
        .and_then(GraphPlan::build)
        .unwrap();
        assert!(all
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.code == "route_duplicate_emit"));
    }

    #[test]
    fn parses_stats_levels_and_legacy_timing() {
        for (yaml, expected) in [
            ("nodes: []\nstats: off", StatsLevel::Off),
            ("nodes: []\nstats: basic", StatsLevel::Basic),
            ("nodes: []\nstats: full", StatsLevel::Full),
            ("nodes: []\nstats_timing: false", StatsLevel::Basic),
            ("nodes: []\nstats_timing: true", StatsLevel::Full),
        ] {
            let cfg = GraphConfig::from_yaml(yaml).unwrap();
            assert_eq!(cfg.effective_stats_level(), expected, "{yaml}");
        }
    }

    #[test]
    fn rejects_new_and_legacy_stats_together() {
        let error =
            GraphConfig::from_yaml("nodes: []\nstats: basic\nstats_timing: false").unwrap_err();
        assert!(error.to_string().contains("cannot be used together"));
    }

    #[test]
    fn watchdog_forces_full_stats() {
        let cfg = GraphConfig::from_yaml("nodes: []\nstats: off\nwatchdog_ms: 1").unwrap();
        assert_eq!(cfg.effective_stats_level(), StatsLevel::Full);
    }

    #[test]
    fn parses_options_and_executors() {
        let cfg = GraphConfig::from_yaml(
            r#"
executors:
  - name: "cpu"
    type: "ThreadPoolExecutor"
    num_threads: 4
nodes:
  - name: "s"
    kernel: "ScaleKernel"
    executor: "cpu"
    input_ports: ["a"]
    output_ports: ["b"]
    options:
      factor: 3
      mean: [0.1, 0.2]
      roi: { x: 5 }
watchdog_ms: 5000
max_queued_packets: 500
buffer_pool_max_bytes: 268435456
"#,
        )
        .unwrap();
        assert_eq!(cfg.executors[0].num_threads, 4);
        assert_eq!(cfg.nodes[0].executor, "cpu");
        assert_eq!(cfg.watchdog_ms, 5000);
        assert_eq!(cfg.max_queued_packets, 500);
        assert_eq!(cfg.buffer_pool_max_bytes, 268435456);
        assert!(cfg.nodes[0].options.get("factor").is_some());
    }

    #[test]
    fn parses_executor_affinity() {
        let cfg = GraphConfig::from_yaml(
            r#"
executors:
  - { name: "rt", type: "ThreadPoolExecutor", num_threads: 2, affinity: [2, 3], priority: 10 }
nodes:
  - { name: "n", kernel: "K", input_ports: ["a"], output_ports: ["b"] }
"#,
        )
        .unwrap();
        assert_eq!(
            cfg.executors[0].affinity,
            vec![2, 3],
            "affinity list should be parsed"
        );
        assert_eq!(
            cfg.executors[0].priority, 10,
            "realtime priority should be parsed"
        );
        // 不配时默认:不绑核、优先级 0(普通分时)
        let cfg2 =
            GraphConfig::from_yaml("executors:\n  - { name: c, type: ThreadPoolExecutor }\n")
                .unwrap();
        assert!(
            cfg2.executors[0].affinity.is_empty(),
            "no CPU pinning by default"
        );
        assert_eq!(
            cfg2.executors[0].priority, 0,
            "ordinary time-sharing by default"
        );
    }

    #[test]
    fn rejects_unsupported_features_loudly() {
        // 静默忽略是最坏的结果 —— 用户会以为开了某个特性,实际没有。
        let err = GraphConfig::from_yaml(
            r#"
executors:
  - name: "nope"
    type: "FiberExecutor"
nodes:
  - name: "n"
    kernel: "K"
"#,
        )
        .unwrap_err();
        assert_eq!(err.code(), crate::status::code::INVALID_ARG);
        assert!(err.to_string().contains("unknown executor type"), "{err}");

        // max_in_flight>1 与「源节点该挂什么执行器」现在要看解析出的执行器长什么样
        // (是池还是委托、几个线程),校验在 Graph::build 里 —— 见 tests/max_in_flight.rs
        // 与 tests/concurrency.rs。
        // fixed_size 现已实现;仍保留「未实现的特性必须报错」这条原则的其它用例
    }

    #[test]
    fn rejects_unknown_fields_and_bad_policy() {
        assert!(
            GraphConfig::from_yaml("nodes:\n  - kernel: K\n    typo_field: 1\n").is_err(),
            "a misspelled field name must error rather than be silently ignored"
        );
        let err =
            GraphConfig::from_yaml("nodes:\n  - kernel: K\n    input_policy: { type: nonsense }\n")
                .unwrap_err();
        assert_eq!(err.code(), crate::status::code::INVALID_ARG);
    }

    #[test]
    fn port_spec_three_forms() {
        assert_eq!(
            parse_port_spec("frames").unwrap(),
            PortSpec {
                tag: String::new(),
                index: None,
                name: "frames".into()
            }
        );
        assert_eq!(
            parse_port_spec("VIDEO:cam0").unwrap(),
            PortSpec {
                tag: "VIDEO".into(),
                index: None,
                name: "cam0".into()
            }
        );
        assert_eq!(
            parse_port_spec("MASK:1:m1").unwrap(),
            PortSpec {
                tag: "MASK".into(),
                index: Some(1),
                name: "m1".into()
            }
        );
    }

    #[test]
    fn port_spec_rejects_malformed() {
        assert!(parse_port_spec("a:b:c:d").is_err(), "too many segments");
        assert!(
            parse_port_spec("TAG:x:name").is_err(),
            "index is not a number"
        );
        assert!(parse_port_spec("").is_err(), "empty name");
        assert!(parse_port_spec("TAG:").is_err(), "empty name");
    }
}
