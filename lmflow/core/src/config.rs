//! 图配置的 YAML 表示。
//!
//! 原则:**用到本版本未实现的字段就报错,不静默忽略** —— 否则用户以为开了并行
//! (或丢帧策略),实际没有,而且无从察觉。见 docs/design.md §0.2。

use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

use crate::status::{Error, Result};

fn default_max_queue_size() -> usize {
    100
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
            stats: None,
            stats_timing: None,
        }
    }
}

/// 与 serde 默认逐字段对齐(照 [`InputPolicyConfig`] 的先例手写、不 derive):
/// `input_policy` 的默认 `type` 是 `"sync"` 而非空串,derive 会给错。
/// 有了它,以后给 `NodeConfig` 加字段不会再打断仓库内的结构体字面量。
impl Default for NodeConfig {
    fn default() -> Self {
        Self {
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
        config.validate_preflight()?;
        Ok(config)
    }

    /// File equivalent of [`Self::preflight_from_yaml`].
    pub fn preflight_from_yaml_file(path: &str) -> Result<Self> {
        let config = Self::from_yaml_file(path)?;
        config.validate_preflight()?;
        Ok(config)
    }

    fn validate_preflight(&self) -> Result<()> {
        let mut known_edges = BTreeSet::new();
        let mut producers = BTreeMap::new();
        for (index, declaration) in self.input_ports.iter().enumerate() {
            let spec = parse_port_spec(declaration)
                .map_err(|error| error.context(format!("input_ports[{index}]")))?;
            if !known_edges.insert(spec.name.clone()) {
                return Err(Error::InvalidArg(format!(
                    "input_ports[{index}]: graph input port `{}` is declared more than once",
                    spec.name
                )));
            }
        }
        let mut node_inputs = Vec::with_capacity(self.nodes.len());
        for (index, node) in self.nodes.iter().enumerate() {
            let inputs = crate::kernel::PortTable::build(
                &node.input_ports,
                &format!("nodes[{index}].input_ports"),
            )
            .map_err(|error| error.context(format!("nodes[{index}].input_ports")))?;
            let outputs = crate::kernel::PortTable::build(
                &node.output_ports,
                &format!("nodes[{index}].output_ports"),
            )
            .map_err(|error| error.context(format!("nodes[{index}].output_ports")))?;
            for name in outputs.names() {
                if self.input_ports.iter().any(|decl| {
                    parse_port_spec(decl)
                        .map(|spec| spec.name == *name)
                        .unwrap_or(false)
                }) {
                    return Err(Error::InvalidArg(format!(
                        "nodes[{index}].output_ports: port `{name}` is also a graph input"
                    )));
                }
                if let Some(previous) = producers.insert(name.clone(), index) {
                    return Err(Error::InvalidArg(format!(
                        "nodes[{index}].output_ports: port `{name}` has multiple producers \
                         (nodes[{previous}] and nodes[{index}])"
                    )));
                }
                known_edges.insert(name.clone());
            }
            node_inputs.push(inputs);
        }
        let graph_inputs: BTreeSet<String> = self
            .input_ports
            .iter()
            .map(|decl| parse_port_spec(decl).map(|spec| spec.name))
            .collect::<Result<_>>()?;
        let mut adjacency = vec![Vec::new(); self.nodes.len()];
        for (index, inputs) in node_inputs.iter().enumerate() {
            for (port, name) in inputs.names().iter().enumerate() {
                if !graph_inputs.contains(name) && !producers.contains_key(name) {
                    return Err(Error::InvalidArg(format!(
                        "nodes[{index}].input_ports[{port}]: port `{name}` has no producer"
                    )));
                }
                if let Some(&producer) = producers.get(name) {
                    if !self.nodes[index].back_edges.iter().any(|edge| edge == name) {
                        adjacency[producer].push(index);
                    }
                }
            }
        }
        for (index, declaration) in self.output_ports.iter().enumerate() {
            let spec = parse_port_spec(declaration)
                .map_err(|error| error.context(format!("output_ports[{index}]")))?;
            if !known_edges.contains(&spec.name) {
                return Err(Error::InvalidArg(format!(
                    "output_ports[{index}]: graph output `{}` has no producer",
                    spec.name
                )));
            }
        }
        check_preflight_acyclic(&adjacency, self)?;
        validate_preflight_executors(self)?;
        Ok(())
    }

    pub fn effective_stats_level(&self) -> StatsLevel {
        if self.watchdog_ms > 0 {
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
                "executors[{index}].name: executor name must not be empty"
            )));
        }
        if executor.name == "default" {
            return Err(Error::InvalidArg(format!(
                "executors[{index}].name: `default` is reserved for the implicit executor"
            )));
        }
        if !names.insert(&executor.name) {
            return Err(Error::InvalidArg(format!(
                "executors[{index}].name: executor `{}` is declared more than once",
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
                "executors[{index}]: DelegatingExecutor cannot configure worker threads, affinity, or priority"
            )));
        }
    }
    for (index, node) in config.nodes.iter().enumerate() {
        if !node.executor.is_empty() && !names.contains(&node.executor) {
            return Err(Error::InvalidArg(format!(
                "nodes[{index}].executor: undefined executor `{}`",
                node.executor
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
                    "nodes[{index}].max_in_flight: value {} requires an executor with at least two threads",
                    node.max_in_flight
                )));
            }
        }
    }
    for (index, node) in config.nodes.iter().enumerate() {
        if node.input_ports.is_empty()
            && !node.executor.is_empty()
            && config.executors.iter().any(|executor| {
                executor.name == node.executor && executor.r#type == "DelegatingExecutor"
            })
        {
            return Err(Error::InvalidArg(format!(
                "nodes[{index}].executor: source nodes cannot run on DelegatingExecutor"
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
                        "nodes[{index}].input_policy.sets[{group_index}]: group must not be empty"
                    )));
                }
                for name in group {
                    if !ports.contains(name) {
                        return Err(Error::InvalidArg(format!(
                                "nodes[{index}].input_policy.sets[{group_index}]: unknown input port `{name}`"
                            )));
                    }
                    if !seen.insert(name) {
                        return Err(Error::InvalidArg(format!(
                                "nodes[{index}].input_policy.sets[{group_index}]: input port `{name}` appears in multiple groups"
                            )));
                    }
                }
            }
            if let Some(name) = ports.iter().find(|name| !seen.contains(*name)) {
                return Err(Error::InvalidArg(format!(
                        "nodes[{index}].input_policy.sets: input port `{name}` is not assigned to a group"
                    )));
            }
        }
    }
    Ok(())
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
"#,
        )
        .unwrap();
        assert_eq!(cfg.executors[0].num_threads, 4);
        assert_eq!(cfg.nodes[0].executor, "cpu");
        assert_eq!(cfg.watchdog_ms, 5000);
        assert_eq!(cfg.max_queued_packets, 500);
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
