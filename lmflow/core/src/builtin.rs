//! 引擎自带的**默认 Rust 算子** —— 零 C++,任何配置下都可用。
//!
//! 这些算子用 [`crate::Kernel`] 写成、经与 C++/Python 完全相同的 vtable 注册,
//! 首次建图时自动注册一次(见 [`register_defaults`]),YAML 里直接按名字引用即可,
//! 宿主不需要调用任何注册函数。
//!
//! 与仓库内那 18 个**内置 C++ 算子**(`../cpp/kernels/`,`builtin-kernels` feature)是两回事:
//! 后者名字都带 `Kernel` 后缀(`PassThroughKernel` 等)、需显式
//! `register_builtin_kernels()`、且不随发布的 crate 分发。名字刻意不重合 ——
//! 注册表按名字唯一,重名会注册失败。

use std::sync::Once;

use crate::kernel_api::{register_kernel, Kernel, KernelContract, KernelCtx};
use crate::status::Result;

/// 零拷贝直通:把输入 0 原样转发到输出 0(复用同一 payload,不拷贝)。
///
/// 注册名 **`PassThrough`**。接受任意类型,常用于接线、占位与测试。
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

/// 注册全部默认 Rust 算子。**幂等**,由 [`crate::Graph::from_config`] 自动调用。
///
/// 注册失败(唯一可能:宿主已用同名注册过自己的算子)被静默忽略 —— 宿主的算子优先,
/// 引擎不该因为要塞自带算子而让建图失败。
pub fn register_defaults() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = register_kernel::<PassThrough>("PassThrough");
    });
}
