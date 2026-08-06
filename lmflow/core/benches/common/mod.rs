use std::sync::Once;

use lmflow::{register_kernel, Kernel, KernelContract, KernelCtx};

#[derive(Default)]
struct BenchPassThrough;

impl Kernel for BenchPassThrough {
    fn get_contract(contract: &mut KernelContract) {
        contract.input_any(0);
        contract.output_any(0);
    }

    fn process(&mut self, context: &mut KernelCtx) -> lmflow::Result<()> {
        context.forward(0, 0)
    }
}

#[derive(Default)]
struct BenchSink;

impl Kernel for BenchSink {
    fn get_contract(contract: &mut KernelContract) {
        contract.input_any(0);
    }

    fn process(&mut self, _context: &mut KernelCtx) -> lmflow::Result<()> {
        Ok(())
    }
}

pub fn register_bench_kernels() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        register_kernel::<BenchPassThrough>("BenchPassThrough").unwrap();
        register_kernel::<BenchSink>("BenchSink").unwrap();
    });
}
