use lmflow::{config::GraphConfig, register_kernel, Error, Graph, Kernel, KernelCtx};
use std::sync::Once;
#[derive(Default)]
struct Source;
impl Kernel for Source {
    fn process(&mut self, c: &mut KernelCtx) -> lmflow::Result<()> {
        c.source_done();
        Ok(())
    }
}
fn register() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| register_kernel::<Source>("RateOverflowSource").unwrap());
}
fn yaml(rate: &str) -> String {
    format!("nodes: [{{name: source, kernel: RateOverflowSource, rate: {rate}}}]")
}
fn assert_invalid(error: Error) {
    assert!(matches!(error, Error::InvalidArg(_)), "{error}");
    let message = error.to_string();
    assert!(
        message.contains("nodes[0].rate") && message.contains("period"),
        "{message}"
    );
}
#[test]
fn configuration_and_preflight_reject_unrepresentable_periods() {
    for rate in ["1e-20", "1e-300", "1e-320"] {
        assert_invalid(GraphConfig::from_yaml(&yaml(rate)).unwrap_err());
        assert_invalid(GraphConfig::preflight_from_yaml(&yaml(rate)).unwrap_err());
    }
}
#[test]
fn graph_build_returns_invalid_argument_without_panicking() {
    register();
    for rate in [1e-20, 1e-300, 1e-320] {
        let mut config = GraphConfig::from_yaml(&yaml("10")).unwrap();
        config.nodes[0].rate = rate;
        assert_invalid(Graph::from_config(config).unwrap_err());
    }
}
#[test]
fn ffi_reports_invalid_argument_instead_of_internal_panic() {
    use lmflow::ffi::*;
    register();
    unsafe {
        let graph = lmflow_graph_new();
        let text = std::ffi::CString::new(yaml("1e-300")).unwrap();
        let status = lmflow_graph_init_from_yaml(graph, text.as_ptr());
        let message = std::ffi::CStr::from_ptr(lmflow_last_error())
            .to_string_lossy()
            .into_owned();
        lmflow_graph_free(graph);
        assert_eq!(status, lmflow::status::code::INVALID_ARG, "{message}");
        assert!(
            message.contains("nodes[0].rate") && message.contains("period"),
            "{message}"
        );
    }
}
#[test]
fn zero_normal_and_large_representable_periods_still_build() {
    register();
    for rate in ["0", "10", "0.001", "1e-19"] {
        let graph = Graph::from_yaml(&yaml(rate)).unwrap();
        graph.start().unwrap();
        graph
            .wait_done_timeout(std::time::Duration::from_secs(2))
            .unwrap();
    }
}
