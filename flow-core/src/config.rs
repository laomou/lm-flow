//! 图配置的 YAML 表示。
//!
//! 原则:**用到本版本未实现的字段就报错,不静默忽略** —— 否则用户以为开了并行
//! (或丢帧策略),实际没有,而且无从察觉。见 docs/design.md §0.2。

use serde::Deserialize;

use crate::status::{Error, Result};

fn default_max_queue_size() -> usize {
    100
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutorConfig {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub r#type: String,
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

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    #[serde(default)]
    pub name: String,
    pub kernel: String,
    #[serde(default)]
    pub input_ports: Vec<String>,
    #[serde(default)]
    pub output_ports: Vec<String>,
    #[serde(default)]
    pub executor: String,
    /// 本版本仅支持 0/1;>1 会在校验阶段报 UNSUPPORTED。
    #[serde(default)]
    pub max_in_flight: usize,
    #[serde(default)]
    pub options: serde_yaml::Value,
    #[serde(default)]
    pub input_policy: InputPolicyConfig,
    /// 预留给子图名(ADR #27)。本版本填了即报 UNSUPPORTED。
    #[serde(default)]
    pub r#type: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphConfig {
    #[serde(default)]
    pub executors: Vec<ExecutorConfig>,
    #[serde(default)]
    pub nodes: Vec<NodeConfig>,
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
    /// 全局水位:全图在途字节上限(0 = 不限;仅内建 payload 可计)
    #[serde(default)]
    pub max_queued_bytes: u64,
    /// 单次算子回调超过该时长即打 WARN(0 = 关闭)
    #[serde(default)]
    pub watchdog_ms: u64,
}

impl GraphConfig {
    pub fn from_yaml(text: &str) -> Result<Self> {
        let cfg: GraphConfig = serde_yaml::from_str(text)
            .map_err(|e| Error::InvalidArg(format!("YAML parse failed: {e}")))?;
        cfg.check_supported()?;
        Ok(cfg)
    }

    /// 只检查「本版本是否支持」,拓扑合法性在 Graph::build 里查。
    fn check_supported(&self) -> Result<()> {
        for n in &self.nodes {
            let who = if n.name.is_empty() {
                n.kernel.clone()
            } else {
                n.name.clone()
            };
            if n.max_in_flight > 1 && n.executor.is_empty() {
                // max_in_flight > 1 只有配了线程池才有意义:默认执行器是宿主主线程,
                // 单线程下并行度恒为 1。宁可报错也不让用户误以为开了并行。
                return Err(Error::InvalidArg(format!(
                    "node `{who}`: max_in_flight={} requires an executor (thread pool) as well -- \
                     the default executor is the host main thread, so there is no parallelism",
                    n.max_in_flight
                )));
            }
            if !n.r#type.is_empty() {
                return Err(Error::Unsupported(format!(
                    "node `{who}`: type=`{}` -- subgraph not yet implemented",
                    n.r#type
                )));
            }
            match n.input_policy.r#type.as_str() {
                "sync" | "immediate" => {}
                "fixed_size" => {
                    if n.input_policy.capacity == 0 {
                        // 容量 0 意味着「每个包都丢」,几乎肯定是漏配
                        return Err(Error::InvalidArg(format!(
                            "node `{who}`: fixed_size policy capacity must be >= 1"
                        )));
                    }
                }
                // sync_set 的分组合法性(名字存在、完整划分)在 Graph::build 里查 ——
                // 那里才有输入口名表。这里只放行类型名。
                "sync_set" => {
                    if n.input_policy.sets.is_empty() {
                        return Err(Error::InvalidArg(format!(
                            "node `{who}`: sync_set policy must provide sets (input port groups)"
                        )));
                    }
                }
                other => {
                    return Err(Error::InvalidArg(format!(
                    "node `{who}`: unknown input_policy `{other}` (valid: sync / immediate / fixed_size / sync_set)"
                )))
                }
            }
        }
        for e in &self.executors {
            if e.r#type != "ThreadPoolExecutor" && !e.r#type.is_empty() {
                return Err(Error::InvalidArg(format!(
                    "unknown executor type `{}` (only ThreadPoolExecutor is currently supported)",
                    e.r#type
                )));
            }
        }
        Ok(())
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
        // 静默忽略是最坏的结果 —— 用户会以为开了某个特性,实际没有
        // max_in_flight>1 但没配 executor:单线程下并行度恒为 1,必须报错
        let err = GraphConfig::from_yaml(
            r#"
nodes:
  - name: "n"
    kernel: "K"
    max_in_flight: 4
"#,
        )
        .unwrap_err();
        assert_eq!(err.code(), crate::status::code::INVALID_ARG);
        assert!(err.to_string().contains("max_in_flight"), "{err}");

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
