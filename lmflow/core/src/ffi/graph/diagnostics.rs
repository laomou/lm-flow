#![allow(clippy::missing_safety_doc)]
use super::*;

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_total_queued(g: *mut LMFlowGraph) -> usize {
    guard_val(0, || {
        graph_of(g).map_or(0, |gr| gr.inner().shared.total_queued())
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_total_queued_bytes(g: *mut LMFlowGraph) -> u64 {
    guard_val(0, || {
        graph_of(g).map_or(0, |gr| gr.inner().shared.total_queued_bytes())
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_queue_depth(
    g: *mut LMFlowGraph,
    port: *const c_char,
) -> usize {
    guard_val(INVALID_ID, || {
        graph_of(g)
            .and_then(|gr| cstr(port).and_then(|p| gr.inner().queue_depth_by_name(p)))
            .unwrap_or(INVALID_ID)
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_dropped_count(
    g: *mut LMFlowGraph,
    port: *const c_char,
) -> u64 {
    guard_val(0, || {
        graph_of(g)
            .and_then(|gr| cstr(port).and_then(|p| gr.inner().dropped_by_name(p)))
            .unwrap_or(0)
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_num_input_ports(g: *mut LMFlowGraph) -> usize {
    guard_val(0, || {
        graph_of(g).map_or(0, |gr| gr.inner().num_input_ports())
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_num_output_ports(g: *mut LMFlowGraph) -> usize {
    guard_val(0, || {
        graph_of(g).map_or(0, |gr| gr.inner().num_output_ports())
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_num_nodes(g: *mut LMFlowGraph) -> usize {
    guard_val(0, || graph_of(g).map_or(0, |gr| gr.inner().nodes_len()))
}

fn graph_arena() -> &'static runtime::CStrArena {
    static A: std::sync::LazyLock<runtime::CStrArena> =
        std::sync::LazyLock::new(runtime::CStrArena::default);
    &A
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_input_port_name(
    g: *mut LMFlowGraph,
    idx: usize,
) -> *const c_char {
    guard_val(c"".as_ptr(), || {
        graph_of(g)
            .and_then(|gr| {
                gr.inner()
                    .input_port_name_at(idx)
                    .map(|s| graph_arena().intern(s))
            })
            .unwrap_or(c"".as_ptr())
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_output_port_name(
    g: *mut LMFlowGraph,
    idx: usize,
) -> *const c_char {
    guard_val(c"".as_ptr(), || {
        graph_of(g)
            .and_then(|gr| {
                gr.inner()
                    .output_port_name_at(idx)
                    .map(|s| graph_arena().intern(s))
            })
            .unwrap_or(c"".as_ptr())
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_node_name(g: *mut LMFlowGraph, idx: usize) -> *const c_char {
    guard_val(c"".as_ptr(), || {
        graph_of(g)
            .and_then(|gr| {
                gr.inner()
                    .node_name_at(idx)
                    .map(|s| graph_arena().intern(s))
            })
            .unwrap_or(c"".as_ptr())
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_node_num_input_ports(
    g: *mut LMFlowGraph,
    node_idx: usize,
) -> usize {
    guard_val(0, || {
        graph_of(g).map_or(0, |graph| graph.inner().node_input_ports_len(node_idx))
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_node_input_port_name(
    g: *mut LMFlowGraph,
    node_idx: usize,
    port_idx: usize,
) -> *const c_char {
    guard_val(c"".as_ptr(), || {
        graph_of(g)
            .and_then(|graph| {
                graph
                    .inner()
                    .node_input_port_name_at(node_idx, port_idx)
                    .map(|name| graph_arena().intern(name))
            })
            .unwrap_or(c"".as_ptr())
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_dump(g: *mut LMFlowGraph) -> *const c_char {
    guard_val(c"".as_ptr(), || {
        thread_local! {
            static BUF: std::cell::RefCell<std::ffi::CString> =
                std::cell::RefCell::new(std::ffi::CString::default());
        }
        let text = graph_of(g).map_or_else(|| "(uninitialized)".to_string(), |gr| gr.dump());
        BUF.with(|b| {
            *b.borrow_mut() = std::ffi::CString::new(text).unwrap_or_default();
            b.borrow().as_ptr()
        })
    })
}

pub const LMFLOW_DOT_TOPOLOGY: i32 = 0;
pub const LMFLOW_DOT_COMPACT: i32 = 1;
pub const LMFLOW_DOT_DIAGNOSTICS: i32 = 2;

/// 显式选择 Graphviz DOT 的详细程度。返回值同 `dump`:线程局部缓冲,调用方不得 free。
#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_to_dot_view(g: *mut LMFlowGraph, view: i32) -> *const c_char {
    guard_val(c"".as_ptr(), || {
        thread_local! {
            static BUF: std::cell::RefCell<std::ffi::CString> =
                std::cell::RefCell::new(std::ffi::CString::default());
        }
        let view = match view {
            LMFLOW_DOT_TOPOLOGY => crate::graph::DotView::Topology,
            LMFLOW_DOT_COMPACT => crate::graph::DotView::Compact,
            LMFLOW_DOT_DIAGNOSTICS => crate::graph::DotView::Diagnostics,
            value => {
                last_error::set(&format!(
                    "invalid DOT view {value}; expected 0 (topology), 1 (compact), or 2 (diagnostics)"
                ));
                return c"".as_ptr();
            }
        };
        // 未初始化图 → 合法的空 digraph(而非文本占位符),便于直接喂 graphviz。
        let text = graph_of(g).map_or_else(
            || "digraph lmflow {\n}\n".to_string(),
            |gr| gr.to_dot_with_view(view),
        );
        BUF.with(|b| {
            *b.borrow_mut() = std::ffi::CString::new(text).unwrap_or_default();
            b.borrow().as_ptr()
        })
    })
}

#[repr(C)]
pub struct LMFlowNodeStats {
    pub struct_size: u32,
    pub reserved0: u32,
    pub node_name: *const c_char,
    pub kernel_name: *const c_char,
    pub running: bool,
    pub running_for_us: i64,
    pub processed: u64,
    pub errors: u64,
    pub total_process_us: i64,
    pub max_process_us: i64,
    pub packets_in: u64,
    pub packets_out: u64,
    pub peak_queue_depth: usize,
    pub queued: usize,
}

#[repr(C)]
pub struct LMFlowInputQueueStats {
    pub struct_size: u32,
    pub reserved0: u32,
    pub node_name: *const c_char,
    pub port_name: *const c_char,
    pub producer_name: *const c_char,
    pub packet_capacity: usize,
    pub queued_packets: usize,
    pub queued_bytes: u64,
    pub reserved_packets: usize,
    pub peak_queued_packets: usize,
    pub peak_queued_bytes: u64,
    pub blocked: bool,
    pub reserved1: [u8; 7],
    pub blocked_for_us: u64,
    pub block_events: u64,
    pub total_blocked_us: u64,
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_node_stats(
    g: *mut LMFlowGraph,
    idx: usize,
    out: *mut LMFlowNodeStats,
) -> bool {
    guard_val(false, || {
        if out.is_null() {
            return false;
        }
        let declared = (*out).struct_size as usize;
        if declared < std::mem::size_of::<LMFlowNodeStats>() {
            last_error::set(
                "LMFlowNodeStats.struct_size too small -- set it to sizeof(LMFlowNodeStats)",
            );
            return false;
        }
        let Some(gr) = graph_of(g) else { return false };
        let Some(s) = gr.node_stats(idx) else {
            return false;
        };
        std::ptr::write(
            out,
            LMFlowNodeStats {
                struct_size: std::mem::size_of::<LMFlowNodeStats>() as u32,
                reserved0: 0,
                node_name: graph_arena().intern(&s.node_name),
                kernel_name: graph_arena().intern(&s.kernel_name),
                running: s.running,
                running_for_us: s.running_for_us,
                processed: s.processed,
                errors: s.errors,
                total_process_us: s.total_process_us,
                max_process_us: s.max_process_us,
                packets_in: s.packets_in,
                packets_out: s.packets_out,
                peak_queue_depth: s.peak_queue_depth,
                queued: s.queued,
            },
        );
        true
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_input_queue_stats(
    g: *mut LMFlowGraph,
    node_idx: usize,
    port_idx: usize,
    out: *mut LMFlowInputQueueStats,
) -> bool {
    guard_val(false, || {
        if out.is_null() {
            return false;
        }
        let declared = (*out).struct_size as usize;
        if declared < std::mem::size_of::<LMFlowInputQueueStats>() {
            last_error::set(
                "LMFlowInputQueueStats.struct_size too small -- set it to sizeof(LMFlowInputQueueStats)",
            );
            return false;
        }
        let Some(graph) = graph_of(g) else {
            return false;
        };
        let Some(stats) = graph.input_queue_stats(node_idx, port_idx) else {
            return false;
        };
        std::ptr::write(
            out,
            LMFlowInputQueueStats {
                struct_size: std::mem::size_of::<LMFlowInputQueueStats>() as u32,
                reserved0: 0,
                node_name: graph_arena().intern(&stats.node_name),
                port_name: graph_arena().intern(&stats.port_name),
                producer_name: stats
                    .producer_name
                    .as_deref()
                    .map_or(c"".as_ptr(), |name| graph_arena().intern(name)),
                packet_capacity: stats.packet_capacity.unwrap_or(0),
                queued_packets: stats.queued_packets,
                queued_bytes: stats.queued_bytes,
                reserved_packets: stats.reserved_packets,
                peak_queued_packets: stats.peak_queued_packets,
                peak_queued_bytes: stats.peak_queued_bytes,
                blocked: stats.blocked,
                reserved1: [0; 7],
                blocked_for_us: stats.blocked_for_us,
                block_events: stats.block_events,
                total_blocked_us: stats.total_blocked_us,
            },
        );
        true
    })
}

#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_counter_value(
    g: *mut LMFlowGraph,
    name: *const c_char,
) -> i64 {
    guard_val(0, || {
        graph_of(g)
            .and_then(|gr| cstr(name).map(|n| gr.inner().shared.counter_value(n)))
            .unwrap_or(0)
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_counter_count(g: *mut LMFlowGraph) -> usize {
    guard_val(0, || {
        graph_of(g).map_or(0, |gr| gr.inner().shared.counter_names().len())
    })
}
#[no_mangle]
pub unsafe extern "C" fn lmflow_graph_counter_name(
    g: *mut LMFlowGraph,
    idx: usize,
) -> *const c_char {
    guard_val(c"".as_ptr(), || {
        graph_of(g)
            .and_then(|gr| {
                gr.inner()
                    .shared
                    .counter_names()
                    .get(idx)
                    .map(|n| graph_arena().intern(n))
            })
            .unwrap_or(c"".as_ptr())
    })
}
