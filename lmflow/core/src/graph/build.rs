//! 建图与建图期校验:把 `GraphConfig` 变成可执行的 [`GraphInner`]。
//!
//! 与 [`super::dot`] / [`super::introspect`] 同理,单独成模块是为了让 `mod.rs` 只剩
//! **运行期并发核心**(调度 / 派发 / 终止 / 锁序)。这里跑的都是**一次性**逻辑:
//! 端口解析、契约询问、拓扑校验 —— 出错就建图失败,不涉及任何并发。
//! 本模块是 `graph` 的子模块,故可访问私有字段。
//!
//! 注意:**逐包**的运行期校验(`check_input_types` / `check_input_monotonic`)留在
//! `mod.rs`,它们在派发路径上,属于并发核心的一部分。

use std::cell::UnsafeCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use crate::config::{diagnostic_node_path, GraphConfig, GraphPlan, StatsLevel};
use crate::context::{Context, Options};
use crate::executor::{DelegatingExecutor, Executor, ThreadPool, DEFAULT_EXECUTOR_NAME};
use crate::kernel::{Contract, KernelInstance, PortTable};
use crate::runtime::{self, GraphShared};
use crate::status::{Error, Result};
use crate::timestamp::Timestamp;

use super::{
    Activity, Edge, EdgeId, GraphInner, InputPolicy, InputQueueStats, Node, NodeSched, NodeStats,
    OnError, State,
};

// ---------------------------------------------------------------- 构建与校验

impl GraphInner {
    pub(super) fn build(cfg: GraphConfig) -> Result<Self> {
        let plan = GraphPlan::build(cfg)?;
        let diagnostics = plan.diagnostics();
        let cfg = plan.config;
        let configured_stats = cfg.stats.unwrap_or_else(|| {
            cfg.stats_timing.map_or(StatsLevel::Basic, |enabled| {
                if enabled {
                    StatsLevel::Full
                } else {
                    StatsLevel::Basic
                }
            })
        });
        let stats_level = cfg.effective_stats_level();
        let edges: Vec<Edge> = plan
            .edges
            .iter()
            .map(|planned| {
                let mut edge = Edge::new(planned.name.clone());
                edge.is_graph_input = planned.graph_input;
                edge.is_graph_output = planned.graph_output;
                edge.producer = planned.producer;
                edge.consumers = planned.consumer_ports.clone();
                edge
            })
            .collect();
        let edge_by_name: BTreeMap<String, EdgeId> = plan.edge_by_name.clone();
        let graph_inputs = plan.graph_inputs.clone();
        let graph_outputs = plan.graph_outputs.clone();
        let input_by_name = plan.input_by_name.clone();
        let output_by_name = plan.output_by_name.clone();
        let node_port_tables: Vec<(Arc<PortTable>, Arc<PortTable>)> = plan
            .nodes
            .iter()
            .map(|node| (node.input_ports.clone(), node.output_ports.clone()))
            .collect();

        for diagnostic in diagnostics {
            runtime::log_warn(&diagnostic.message);
        }

        // 每节点每输入口是否为 back-edge(按口名匹配 config.back_edges)。反馈寄存器口不参与
        // 拓扑成环判定 / 就绪 / 终止 / 对齐。
        let back_edge_mask = plan.back_edge_mask.clone();

        // ---- 按计划创建执行器 ----
        //
        // `executors` 里写的**一律是宿主自己的执行器**,必须有名字 —— 节点靠名字引用它。
        // 默认执行器则完全由引擎持有:按 CPU 核数开的线程池,恒在下标 0,YAML 无从干涉。
        // 因此 `default` 是**保留名**,写了报错;否则图里会同时出现两个 `default`。
        // 节点侧 `executor` 留空即归默认执行器,归一化到同一个名字。于是默认执行器和
        // 其它执行器完全同构 —— 有名字、可索引、可提交任务,派任务时无需为它开特例。
        let mut executors: Vec<Executor> = vec![Executor::Pool(default_thread_pool(stats_level))];
        for (executor_index, e) in cfg.executors.iter().enumerate() {
            executors.push(
                build_executor(&e.name, e, stats_level)
                    .map_err(|error| error.context(format!("executors[{executor_index}]")))?,
            );
        }
        // ---- 收集契约 + 静态类型检查 ----
        //
        // 所有节点的端口表此时都已确定,先一次性询问契约,才能沿每条边同时看到
        // producer.output_type 与 consumer.input_type。能静态证明不兼容的连接在建图期
        // 直接拒绝;任一侧为 ANY/NONE(0)则保留运行期逐包检查。
        let mut contracts = Vec::with_capacity(cfg.nodes.len());
        let mut kernels = Vec::with_capacity(cfg.nodes.len());
        for (idx, n) in cfg.nodes.iter().enumerate() {
            let name = node_label(n, idx);
            let node_path = diagnostic_node_path(n, idx);
            let (ins, outs) = node_port_tables[idx].clone();
            let mut contract = Contract::new(ins, outs);
            if n.r#type != "route" {
                // 安全性:contract 是本栈帧上存活的对象,回调期间无人访问它。
                let contract_result = unsafe {
                    KernelInstance::fill_contract(
                        &n.kernel,
                        &mut contract as *mut Contract as *mut c_void,
                    )
                };
                contract_result.map_err(|error| error.context(format!("{node_path}.kernel")))?;
            }
            if let Some(error) = contract.take_error() {
                return Err(Error::InvalidArg(format!(
                    "{node_path}.kernel (node `{name}`): GetContract failed: {error}"
                )));
            }
            validate_contract(&node_path, &name, &contract)?;
            contracts.push(contract);
            // 保持历史顺序:每个节点都是 get_contract 后立刻 create,而不是先询问完
            // 所有契约再统一创建。静态检查失败时这些实例随局部 Vec 正常析构。
            kernels.push(if n.r#type == "route" {
                KernelInstance::create_route(
                    n.route
                        .clone()
                        .expect("validated route node has route configuration"),
                )
            } else {
                KernelInstance::create(&n.kernel)
                    .map_err(|error| error.context(format!("{node_path}.kernel")))?
            });
        }
        check_edge_type_compatibility(&cfg, &edges, &contracts)?;

        // ---- 建节点 ----
        let shared = Arc::new(GraphShared::new(cfg.clone()));
        let mut nodes = Vec::new();
        let mut required: Vec<(String, String)> = Vec::new();
        for ((idx, n), (contract, kernel)) in cfg
            .nodes
            .iter()
            .enumerate()
            .zip(contracts.into_iter().zip(kernels))
        {
            let name = node_label(n, idx);
            let (ins, outs) = node_port_tables[idx].clone();

            let planned = &plan.nodes[idx];
            let input_edges = planned.input_edges.clone();
            let output_edges = planned.output_edges.clone();

            // 0 视作 1。max_in_flight 个并行调用各需一个 context 槽。
            let mif = n.max_in_flight.max(1);
            let options = Arc::new(Options::new(n.options.clone()));
            let kernel_name = if n.r#type == "route" {
                "__lmflow.route".to_string()
            } else {
                n.kernel.clone()
            };
            let make_ctx = || {
                Context::new(
                    name.clone(),
                    kernel_name.clone(),
                    ins.clone(),
                    outs.clone(),
                    options.clone(),
                    Arc::new(BTreeMap::new()), // start 时替换为真实 side packets
                    shared.clone(),
                )
            };
            let ctxs: Vec<UnsafeCell<Context>> =
                (0..mif).map(|_| UnsafeCell::new(make_ctx())).collect();

            // 上面已校验过名字存在(节点侧留空归一化到默认执行器),故必有下标。
            let executor = planned.executor_index;

            nodes.push(Node {
                name,
                kernel_name,
                inputs: input_edges,
                outputs: output_edges,
                in_ports: ins.clone(),
                out_ports: outs,
                executor,
                policy: InputPolicy::from_config(&n.input_policy, &ins)?,
                input_types: contract.input_types.clone(),
                output_types: contract.output_types.clone(),
                kernel,
                ctxs,
                max_in_flight: mif,
                sched: Mutex::new(NodeSched::new(mif)),
                stats: NodeStats::default(),
                input_queues: (0..ins.len())
                    .map(|_| Mutex::new(VecDeque::new()))
                    .collect(),
                input_queue_capacity: ins
                    .names()
                    .iter()
                    .map(|port| {
                        n.input_queues
                            .ports
                            .get(port)
                            .and_then(|limits| limits.packets)
                            .unwrap_or(n.input_queues.packets)
                    })
                    .map(|capacity| (capacity != 0).then_some(capacity))
                    .collect(),
                input_queue_reserved: (0..ins.len()).map(|_| AtomicUsize::new(0)).collect(),
                input_queue_bytes: (0..ins.len()).map(|_| AtomicU64::new(0)).collect(),
                input_queue_stats: (0..ins.len()).map(|_| InputQueueStats::default()).collect(),
                input_closed: (0..ins.len()).map(|_| AtomicBool::new(false)).collect(),
                on_error: OnError::from_config(&n.on_error),
                min_period: if n.rate > 0.0 {
                    Some(std::time::Duration::from_secs_f64(1.0 / n.rate))
                } else {
                    None
                },
                last_fire: Mutex::new(None),
                input_is_back_edge: back_edge_mask[idx].clone(),
                source_done: AtomicBool::new(false),
                source_waiting: AtomicBool::new(false),
                source_wait_reason: AtomicUsize::new(0),
                source_wake_deadline_us: AtomicI64::new(0),
                source_yield_count: AtomicU64::new(0),
                source_wake_generation: AtomicU64::new(0),
                input_bounds: (0..ins.len())
                    .map(|_| Mutex::new(Timestamp::pre_stream()))
                    .collect(),
            });
            // 记录该算子声明的必需 side packet,start 时校验
            for name in &contract.required_side_packets {
                required.push((
                    name.clone(),
                    nodes.last().expect("just inserted").name.clone(),
                ));
            }
        }

        if configured_stats != StatsLevel::Full && shared.config.watchdog_ms > 0 {
            runtime::log_info(
                "stats is overridden to full because watchdog_ms > 0 (the watchdog needs per-call timing)",
            );
        } else if stats_level != StatsLevel::Full {
            runtime::log_info(
                "stats is not full: per-call timing, latency percentiles, CoW copies, and executor timing are disabled",
            );
        }

        Ok(Self {
            shared,
            nodes,
            edges,
            graph_inputs,
            graph_outputs,
            input_by_name,
            output_by_name,
            edge_by_name,
            state: Mutex::new(State::Initialized),
            executors,
            in_flight: AtomicUsize::new(0),
            delegated_cursor: AtomicUsize::new(0),
            delegated_running: AtomicBool::new(false),
            activity: (Mutex::new(Activity::default()), Condvar::new()),
            wakeup_callback: Mutex::new(None),
            wakeup_pending: AtomicBool::new(false),
            wakeup_generation: AtomicU64::new(0),
            paused: AtomicBool::new(false),
            blocked_flush_nodes: Mutex::new(BTreeSet::new()),
            side_packets: Mutex::new(BTreeMap::new()),
            required_side_packets: required,
            epoch: Instant::now(),
            run_started_us: AtomicI64::new(0),
            dot_intervals: Mutex::new(super::dot::DotIntervalBaselines::default()),
            stats_level,
            e2e_stats: super::node::E2eStats::default(),
        })
    }
}

/// 引擎隐式的默认执行器:按 CPU 核数开线程的线程池。
///
/// 不绑核、不设实时优先级,也不可配 —— 那些是场景相关的调优,宿主想控制就自己声明一个
/// 具名池(或具名 `DelegatingExecutor`),把节点用 `executor:` 指过去。
fn default_thread_pool(stats_level: StatsLevel) -> ThreadPool {
    let n = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    ThreadPool::new_with_stats(DEFAULT_EXECUTOR_NAME, n, Vec::new(), 0, stats_level)
}

/// 按 `type` 建一个执行器。空 `type` 视作 `ThreadPoolExecutor`(历史默认)。
///
/// `DelegatingExecutor` 不拥有线程,故 `num_threads`/`affinity`/`priority` 对它没有意义 ——
/// 写了就报错,不静默忽略(与「未知值明确拒掉」同规矩)。
fn build_executor(
    name: &str,
    e: &crate::config::ExecutorConfig,
    stats_level: StatsLevel,
) -> Result<Executor> {
    if e.r#type == "DelegatingExecutor" {
        for (field, set) in [
            ("num_threads", e.num_threads != 0),
            ("affinity", !e.affinity.is_empty()),
            ("priority", e.priority != 0),
        ] {
            if set {
                return Err(Error::InvalidArg(format!(
                    "executor `{name}`: DelegatingExecutor owns no threads, so `{field}` is \
                     meaningless -- it hands ready nodes back to the host thread. Drop the field, \
                     or use type: \"ThreadPoolExecutor\" if you wanted engine-owned threads"
                )));
            }
        }
        return Ok(Executor::Delegating(DelegatingExecutor::new_with_stats(
            name,
            stats_level,
        )));
    }
    Ok(Executor::Pool(ThreadPool::new_with_stats(
        name,
        e.num_threads,
        e.affinity.clone(),
        e.priority,
        stats_level,
    )))
}

fn validate_contract(path: &str, name: &str, contract: &Contract) -> Result<()> {
    for (which, types) in [
        ("input", &contract.input_types),
        ("output", &contract.output_types),
    ] {
        for (i, &type_id) in types.iter().enumerate() {
            crate::packet::validate_type_id(type_id).map_err(|error| {
                Error::InvalidArg(format!(
                    "{path}.kernel (node `{name}`): {which} port {i} declares an invalid type: \
                     {error}"
                ))
            })?;
        }
    }
    let mut required = std::collections::BTreeSet::new();
    for side_packet in &contract.required_side_packets {
        if side_packet.is_empty() {
            return Err(Error::InvalidArg(format!(
                "{path}.kernel (node `{name}`): GetContract declares an empty required side packet name"
            )));
        }
        if !required.insert(side_packet) {
            return Err(Error::InvalidArg(format!(
                "{path}.kernel (node `{name}`): GetContract declares required side packet `{side_packet}` more than once"
            )));
        }
    }
    Ok(())
}

fn check_edge_type_compatibility(
    cfg: &GraphConfig,
    edges: &[Edge],
    contracts: &[Contract],
) -> Result<()> {
    for edge in edges {
        let Some(producer) = edge.producer else {
            continue; // 图输入的实际类型由宿主逐包决定,只能运行期检查。
        };
        let producer_port = contracts[producer]
            .outputs
            .index_by_name(&edge.name)
            .expect("edge producer output must exist");
        let produced = contracts[producer].output_types[producer_port];
        if produced == crate::packet::type_id::NONE {
            continue; // producer 声明 ANY:无法静态证明,保留运行期检查。
        }
        for &(consumer, consumer_port) in &edge.consumers {
            let wanted = contracts[consumer].input_types[consumer_port];
            if wanted == crate::packet::type_id::NONE || wanted == produced {
                continue;
            }
            return Err(Error::InvalidArg(format!(
                "nodes[{consumer}].input_ports[{consumer_port}]: type mismatch on edge `{}`: \
                 nodes[{producer}] `{}` output port `{}` declares {}, but node `{}` input port `{}` declares {}",
                edge.name,
                node_label(&cfg.nodes[producer], producer),
                contracts[producer]
                    .outputs
                    .name(producer_port)
                    .unwrap_or("?"),
                crate::packet::type_name(produced),
                node_label(&cfg.nodes[consumer], consumer),
                contracts[consumer]
                    .inputs
                    .name(consumer_port)
                    .unwrap_or("?"),
                crate::packet::type_name(wanted),
            )));
        }
    }
    Ok(())
}

fn node_label(n: &crate::config::NodeConfig, idx: usize) -> String {
    if n.name.is_empty() {
        format!("{}#{}", n.kernel, idx)
    } else {
        n.name.clone()
    }
}
