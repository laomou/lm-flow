#![allow(dead_code)]

use std::sync::Once;

use lmflow::{register_kernel, Graph, Kernel, KernelContract, KernelCtx};

#[derive(Default)]
struct TestPassThrough;

impl Kernel for TestPassThrough {
    fn get_contract(contract: &mut KernelContract) {
        contract.input_any(0);
        contract.output_any(0);
    }

    fn process(&mut self, context: &mut KernelCtx) -> lmflow::Result<()> {
        context.forward(0, 0)
    }
}

#[derive(Default)]
struct TestSink;

impl Kernel for TestSink {
    fn get_contract(contract: &mut KernelContract) {
        contract.input_any(0);
    }

    fn process(&mut self, context: &mut KernelCtx) -> lmflow::Result<()> {
        context.counter_add("sink.packets", 1);
        Ok(())
    }

    fn close(&mut self, context: &mut KernelCtx) -> lmflow::Result<()> {
        context.counter_add("sink.closed", 1);
        Ok(())
    }
}

pub fn register_test_kernels() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        register_kernel::<TestPassThrough>("PassThrough").unwrap();
        register_kernel::<TestSink>("Sink").unwrap();
    });
}

pub fn graph_from_yaml(text: &str) -> lmflow::Result<Graph> {
    register_test_kernels();
    Graph::from_yaml(text)
}
