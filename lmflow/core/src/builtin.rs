//! The engine's **default Rust kernels** — no C++, available in every configuration.
//!
//! There are **deliberately only two**, and both are purely structural with no assumptions about
//! the payload:
//!
//! | Name | What it does | Ports |
//! |---|---|---|
//! | [`PassThrough`] | zero-copy forward (wiring / placeholder) | 1 → 1, any type |
//! | [`Sink`] | consume only, so a branch can terminate itself | 1 → 0, any type |
//!
//! **Why not more.** Kernels like `Scale` / `Sum` / `Zip` / `Filter` would all have to assume the
//! payload is some concrete type such as `i64`, and ADR #6 states plainly that the engine never
//! interprets payloads — putting integer arithmetic into the engine proper contradicts it.
//! Demonstrating engine semantics (reading options, emitting on close, timestamp alignment,
//! advancing bounds, `source_done`) is the job of the 18 built-in C++ kernels in `../cpp/kernels/`
//! and of `examples/`, not of the engine library that gets published. Fan-out needs no kernel
//! either: **one edge can feed several consumers** natively (see §7, the edge model).
//!
//! Both are written with [`crate::Kernel`] and registered through exactly the same vtable as
//! C++ and Python kernels. [`register_defaults`] runs once when the first graph is built, so YAML
//! can refer to them by name with no registration call from the host.
//!
//! The names deliberately carry **no `Kernel` suffix**, to avoid colliding with the built-in C++
//! kernels (`PassThroughKernel` and friends) — the registry is keyed by name, and registering a
//! duplicate is an error.

use std::sync::Once;

use crate::kernel_api::{register_kernel, Kernel, KernelContract, KernelCtx};
use crate::runtime::{LOG_DEBUG, LOG_INFO};
use crate::status::Result;

/// Zero-copy forward: hands input 0 straight to output 0, reusing the same payload.
///
/// Registered as **`PassThrough`**. Accepts any payload type; useful for wiring, placeholders and
/// tests.
#[derive(Default)]
pub struct PassThrough;

impl Kernel for PassThrough {
    fn get_contract(c: &mut KernelContract) {
        c.input_any(0);
        c.output_any(0);
    }
    fn process(&mut self, cc: &mut KernelCtx) -> Result<()> {
        cc.forward(0, 0)
    }
}

/// A terminal sink: consumes packets and produces nothing (no output ports), so a branch can
/// terminate itself instead of requiring the host to poll it.
///
/// Registered as **`Sink`**. It reports through the engine log rather than stdout — a library
/// should not commandeer the host's output — and bumps the **per-graph** counters
/// `sink.packets` / `sink.closed`, which tests can assert on directly.
#[derive(Default)]
pub struct Sink {
    count: i64,
}

impl Kernel for Sink {
    fn get_contract(c: &mut KernelContract) {
        c.input_any(0);
    }
    fn process(&mut self, cc: &mut KernelCtx) -> Result<()> {
        // 每包一条 debug 日志:**先问再格式化**。默认没装 sink 时 `format!` 的堆分配
        // 纯属浪费 —— 这条 process 是每包都走的。
        if cc.log_enabled() {
            let ts = cc.input_timestamp();
            cc.log(LOG_DEBUG, &format!("received packet @ ts={}", ts.0));
        }
        cc.counter_add("sink.packets", 1);
        self.count += 1;
        Ok(())
    }
    fn close(&mut self, cc: &mut KernelCtx) -> Result<()> {
        cc.log(
            LOG_INFO,
            &format!("processed {} packets in total", self.count),
        );
        cc.counter_add("sink.closed", 1);
        Ok(())
    }
}

/// Register every default Rust kernel. **Idempotent**, and called automatically by
/// [`crate::Graph::from_config`].
///
/// A registration failure — which can only mean the host already registered a kernel of its own
/// under the same name — is silently ignored. The host's kernel wins; the engine should not fail
/// graph construction just to install one of its own.
pub fn register_defaults() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = register_kernel::<PassThrough>("PassThrough");
        let _ = register_kernel::<Sink>("Sink");
    });
}
