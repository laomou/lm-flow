//! 建图期变换:跨文件 `include` 合并 + 子图(subgraph)展开(ADR #27)。
//!
//! 两者都在 parse 与 check_supported/build 之间,把带 `subgraphs` / `type:` 节点的
//! [`GraphConfig`] 变成**展平**的等价配置(所有节点都是算子节点、无 `type:`、无 `subgraphs`)。
//! 运行时引擎 / 调度器完全不感知子图 —— 连边纯按端口名字符串,展开只是多产出些节点和名字。
//!
//! 命名空间约定:子图实例 `d`(type=Denoise)的内部节点 `a` → 名字 `d/a`、内部边 `d/<边名>`;
//! 子图边界口按**位置**重映射到实例节点连接的外部边名。用 `/` 分隔(`:` 会被
//! [`parse_port_spec`] 当 tag 分隔符,不能用)。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::config::{parse_port_spec, GraphConfig, NodeConfig, PortSpec, SubgraphConfig};
use crate::status::{Error, Result};

/// 把 `include` 引用的文件递归读进来,合并它们的 `subgraphs` 到 `cfg`。
///
/// `main_path` 是主文件路径(用于 (a) 解析相对 include 的基准目录 =其父目录,
/// (b) 去重 / 断环:先塞进 visited,避免被 include 回环重复读)。只取被引文件的
/// `subgraphs`;其 nodes / executors / ports 忽略(include 是子图库,不是子图本身)。
/// 子图重名(跨不同文件)→ 报错;同一文件被引多次(菱形 include)→ 去重,不报错。
pub(crate) fn resolve_includes(mut cfg: GraphConfig, main_path: &Path) -> Result<GraphConfig> {
    let mut visited: BTreeSet<PathBuf> = BTreeSet::new();
    if let Ok(canon) = main_path.canonicalize() {
        visited.insert(canon);
    }
    let base = main_path.parent().unwrap_or_else(|| Path::new("."));
    let includes = std::mem::take(&mut cfg.include);
    for inc in &includes {
        merge_include(&mut cfg.subgraphs, inc, base, &mut visited)?;
    }
    Ok(cfg)
}

/// 读一个被 include 的文件,递归处理它自己的 include,把它的 `subgraphs` 并进 `dst`。
fn merge_include(
    dst: &mut BTreeMap<String, SubgraphConfig>,
    rel: &str,
    base: &Path,
    visited: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    let path = base.join(rel);
    let canon = path
        .canonicalize()
        .map_err(|e| Error::InvalidArg(format!("include `{rel}`: {e}")))?;
    if !visited.insert(canon.clone()) {
        // 已经读过(菱形 include / 回环)—— 去重,直接跳过。
        return Ok(());
    }
    let text = std::fs::read_to_string(&canon)
        .map_err(|e| Error::InvalidArg(format!("include `{rel}`: {e}")))?;
    let inc_cfg = GraphConfig::parse(&text)?;

    // 被引文件自己的 include:相对它所在目录再解析。
    let inc_base = canon.parent().unwrap_or_else(|| Path::new("."));
    for sub in &inc_cfg.include {
        merge_include(dst, sub, inc_base, visited)?;
    }

    for (name, sg) in inc_cfg.subgraphs {
        if dst.contains_key(&name) {
            return Err(Error::InvalidArg(format!(
                "subgraph `{name}` defined more than once (via include `{rel}`)"
            )));
        }
        dst.insert(name, sg);
    }
    Ok(())
}

/// 展开所有子图实例节点,返回展平的配置(nodes 全为算子节点,subgraphs 清空)。
pub(crate) fn expand(mut cfg: GraphConfig) -> Result<GraphConfig> {
    let subgraphs = std::mem::take(&mut cfg.subgraphs);
    let nodes = std::mem::take(&mut cfg.nodes);
    let mut out: Vec<NodeConfig> = Vec::with_capacity(nodes.len());
    let mut stack: Vec<String> = Vec::new();
    inline(
        &nodes,
        "",
        &BTreeMap::new(),
        &subgraphs,
        &mut out,
        &mut stack,
    )?;
    cfg.nodes = out;
    cfg.include = Vec::new();
    Ok(cfg)
}

/// 把一组节点(顶层图或某子图内部)内联进 `out`。
///
/// - `prefix`:累积命名空间前缀(空串 = 顶层;`"d/"` / `"d/e/"` = 子图内部)。
/// - `rename`:边界口名 → 外部边名(把子图边界接到实例节点连的那条边)。
fn inline(
    nodes: &[NodeConfig],
    prefix: &str,
    rename: &BTreeMap<String, String>,
    subgraphs: &BTreeMap<String, SubgraphConfig>,
    out: &mut Vec<NodeConfig>,
    stack: &mut Vec<String>,
) -> Result<()> {
    for n in nodes {
        let inputs = remap_ports(&n.input_ports, prefix, rename)?;
        let outputs = remap_ports(&n.output_ports, prefix, rename)?;

        if n.r#type.is_empty() {
            // 算子节点:必须有 kernel。拷贝一份、换名字 + 端口名。
            if n.kernel.is_empty() {
                return Err(Error::InvalidArg(format!(
                    "node `{}` has neither `kernel` nor `type` -- one is required",
                    node_who(n, prefix)
                )));
            }
            let mut nn = n.clone();
            // 顶层名字保持原样(空名交给 build 的 node_label 处理);子图内部才加前缀。
            if !prefix.is_empty() {
                nn.name = format!("{prefix}{}", node_label(n));
            }
            nn.input_ports = inputs;
            nn.output_ports = outputs;
            nn.r#type = String::new();
            // back_edges 是输入口名,随端口名一并重映射(边界 → 外部边名 / 内部 → 命名空间前缀)。
            nn.back_edges = n
                .back_edges
                .iter()
                .map(|b| remap_name(b, prefix, rename))
                .collect();
            out.push(nn);
        } else {
            // 子图实例节点:递归内联。
            if !n.kernel.is_empty() {
                return Err(Error::InvalidArg(format!(
                    "node `{}` has both `kernel` and `type` -- only one is allowed",
                    node_who(n, prefix)
                )));
            }
            let sg = subgraphs.get(&n.r#type).ok_or_else(|| {
                Error::InvalidArg(format!(
                    "node `{}`: unknown subgraph `{}`",
                    node_who(n, prefix),
                    n.r#type
                ))
            })?;
            if stack.iter().any(|s| s == &n.r#type) {
                return Err(Error::InvalidArg(format!(
                    "subgraph `{}` is recursive (expansion cycle: {} -> {})",
                    n.r#type,
                    stack.join(" -> "),
                    n.r#type
                )));
            }
            check_arity(&n.r#type, "input", sg.input_ports.len(), inputs.len())?;
            check_arity(&n.r#type, "output", sg.output_ports.len(), outputs.len())?;

            // 子图边界口(名字分量)→ 实例节点连的外部边名(名字分量)。
            let mut child = BTreeMap::new();
            bind_boundary(&mut child, &sg.input_ports, &inputs)?;
            bind_boundary(&mut child, &sg.output_ports, &outputs)?;

            let child_prefix = format!("{prefix}{}/", node_label(n));
            stack.push(n.r#type.clone());
            inline(&sg.nodes, &child_prefix, &child, subgraphs, out, stack)?;
            stack.pop();
        }
    }
    Ok(())
}

/// 重映射一组端口声明:只改 name 分量,保留 tag/index。
/// name 在 `rename` 中(边界口)→ 换外部边名;否则(内部边)→ 加 `prefix` 前缀。
fn remap_ports(
    decls: &[String],
    prefix: &str,
    rename: &BTreeMap<String, String>,
) -> Result<Vec<String>> {
    decls
        .iter()
        .map(|d| {
            let spec = parse_port_spec(d)?;
            let name = match rename.get(&spec.name) {
                Some(edge) => edge.clone(),
                None => format!("{prefix}{}", spec.name),
            };
            Ok(reassemble(&spec, &name))
        })
        .collect()
}

/// 把一个裸端口名按边界重映射规则映射(back_edges 用;逻辑同 [`remap_ports`] 的 name 分量)。
fn remap_name(name: &str, prefix: &str, rename: &BTreeMap<String, String>) -> String {
    match rename.get(name) {
        Some(edge) => edge.clone(),
        None => format!("{prefix}{name}"),
    }
}

/// 把边界口声明与实例节点已重映射的外部口声明按位置绑定:边界 name → 外部 name。
fn bind_boundary(
    map: &mut BTreeMap<String, String>,
    boundary: &[String],
    external: &[String],
) -> Result<()> {
    for (b, e) in boundary.iter().zip(external.iter()) {
        let bn = parse_port_spec(b)?.name;
        let en = parse_port_spec(e)?.name;
        map.insert(bn, en);
    }
    Ok(())
}

fn check_arity(sg: &str, which: &str, want: usize, got: usize) -> Result<()> {
    if want != got {
        return Err(Error::InvalidArg(format!(
            "subgraph `{sg}`: instance provides {got} {which} port(s) but the subgraph declares {want}"
        )));
    }
    Ok(())
}

/// 按原声明的 tag/index 重新拼出端口声明,只替换 name 分量。
fn reassemble(spec: &PortSpec, name: &str) -> String {
    match (spec.tag.is_empty(), spec.index) {
        (true, _) => name.to_string(),
        (false, None) => format!("{}:{}", spec.tag, name),
        (false, Some(i)) => format!("{}:{}:{}", spec.tag, i, name),
    }
}

/// 节点的展示名(名字优先,其次 kernel,再次 type)。
fn node_label(n: &NodeConfig) -> String {
    if !n.name.is_empty() {
        n.name.clone()
    } else if !n.kernel.is_empty() {
        n.kernel.clone()
    } else {
        n.r#type.clone()
    }
}

/// 错误信息里用的定位串:带上命名空间前缀。
fn node_who(n: &NodeConfig, prefix: &str) -> String {
    format!("{prefix}{}", node_label(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sg(input: &[&str], output: &[&str], nodes: Vec<NodeConfig>) -> SubgraphConfig {
        SubgraphConfig {
            nodes,
            input_ports: input.iter().map(|s| s.to_string()).collect(),
            output_ports: output.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn kernel_node(name: &str, kernel: &str, input: &[&str], output: &[&str]) -> NodeConfig {
        NodeConfig {
            name: name.to_string(),
            kernel: kernel.to_string(),
            input_ports: input.iter().map(|s| s.to_string()).collect(),
            output_ports: output.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn instance(name: &str, ty: &str, input: &[&str], output: &[&str]) -> NodeConfig {
        let mut n = kernel_node(name, "", input, output);
        n.r#type = ty.to_string();
        n
    }

    #[test]
    fn no_subgraph_is_identity() {
        let cfg = GraphConfig::from_yaml(
            r#"
nodes:
  - { name: n1, kernel: PassThroughKernel, input_ports: ["a"], output_ports: ["b"] }
input_ports: ["a"]
output_ports: ["b"]
"#,
        )
        .unwrap();
        assert_eq!(cfg.nodes.len(), 1);
        assert_eq!(cfg.nodes[0].name, "n1");
        assert_eq!(cfg.nodes[0].input_ports, vec!["a".to_string()]);
        assert!(cfg.subgraphs.is_empty());
    }

    #[test]
    fn expands_chain_subgraph_with_namespaced_ports() {
        let mut cfg = GraphConfig {
            executors: vec![],
            nodes: vec![
                instance("d", "Denoise", &["raw"], &["clean"]),
                kernel_node("s", "ScaleKernel", &["clean"], &["final"]),
            ],
            include: vec![],
            subgraphs: BTreeMap::new(),
            input_ports: vec!["raw".into()],
            output_ports: vec!["final".into()],
            max_queue_size: 100,
            max_queued_packets: 0,
            watchdog_ms: 0,
            ..Default::default()
        };
        cfg.subgraphs.insert(
            "Denoise".into(),
            sg(
                &["sin"],
                &["sout"],
                vec![
                    kernel_node("a", "BlurKernel", &["sin"], &["mid"]),
                    kernel_node("b", "SharpenKernel", &["mid"], &["sout"]),
                ],
            ),
        );

        let flat = expand(cfg).unwrap();
        // d 展开成 d/a, d/b;加上 s → 3 个节点。
        assert_eq!(flat.nodes.len(), 3);
        let names: Vec<&str> = flat.nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["d/a", "d/b", "s"]);
        // 边界重映射:d/a 吃外部边 raw、产内部边 d/mid;d/b 吃 d/mid、产外部边 clean。
        assert_eq!(flat.nodes[0].input_ports, vec!["raw".to_string()]);
        assert_eq!(flat.nodes[0].output_ports, vec!["d/mid".to_string()]);
        assert_eq!(flat.nodes[1].input_ports, vec!["d/mid".to_string()]);
        assert_eq!(flat.nodes[1].output_ports, vec!["clean".to_string()]);
        assert!(flat.nodes.iter().all(|n| n.r#type.is_empty()));
        assert!(flat.subgraphs.is_empty());
    }

    #[test]
    fn unknown_subgraph_is_rejected() {
        let mut cfg = GraphConfig {
            executors: vec![],
            nodes: vec![instance("d", "Nope", &["raw"], &["clean"])],
            include: vec![],
            subgraphs: BTreeMap::new(),
            input_ports: vec!["raw".into()],
            output_ports: vec!["clean".into()],
            max_queue_size: 100,
            max_queued_packets: 0,
            watchdog_ms: 0,
            ..Default::default()
        };
        cfg.subgraphs
            .insert("Denoise".into(), sg(&["sin"], &["sout"], vec![]));
        let err = expand(cfg).unwrap_err();
        assert!(format!("{err:?}").contains("unknown subgraph"));
    }

    #[test]
    fn arity_mismatch_is_rejected() {
        let mut cfg = GraphConfig {
            executors: vec![],
            nodes: vec![instance("d", "Two", &["raw"], &["clean"])],
            include: vec![],
            subgraphs: BTreeMap::new(),
            input_ports: vec!["raw".into()],
            output_ports: vec!["clean".into()],
            max_queue_size: 100,
            max_queued_packets: 0,
            watchdog_ms: 0,
            ..Default::default()
        };
        // 子图要 2 个输入,实例只给 1 个。
        cfg.subgraphs
            .insert("Two".into(), sg(&["x", "y"], &["z"], vec![]));
        let err = expand(cfg).unwrap_err();
        assert!(format!("{err:?}").contains("input port"));
    }

    #[test]
    fn cycle_is_rejected() {
        let mut cfg = GraphConfig {
            executors: vec![],
            nodes: vec![instance("a", "A", &["in"], &["out"])],
            include: vec![],
            subgraphs: BTreeMap::new(),
            input_ports: vec!["in".into()],
            output_ports: vec!["out".into()],
            max_queue_size: 100,
            max_queued_packets: 0,
            watchdog_ms: 0,
            ..Default::default()
        };
        // A 内部又实例化 A → 无限递归,须报错。
        cfg.subgraphs.insert(
            "A".into(),
            sg(
                &["in"],
                &["out"],
                vec![instance("self", "A", &["in"], &["out"])],
            ),
        );
        let err = expand(cfg).unwrap_err();
        assert!(format!("{err:?}").contains("recursive"));
    }

    #[test]
    fn both_kernel_and_type_is_rejected() {
        let mut node = instance("d", "Denoise", &["raw"], &["clean"]);
        node.kernel = "Oops".into();
        let mut cfg = GraphConfig {
            executors: vec![],
            nodes: vec![node],
            include: vec![],
            subgraphs: BTreeMap::new(),
            input_ports: vec!["raw".into()],
            output_ports: vec!["clean".into()],
            max_queue_size: 100,
            max_queued_packets: 0,
            watchdog_ms: 0,
            ..Default::default()
        };
        cfg.subgraphs
            .insert("Denoise".into(), sg(&["sin"], &["sout"], vec![]));
        let err = expand(cfg).unwrap_err();
        assert!(format!("{err:?}").contains("both"));
    }
}
