//! lmflow 引擎核心。
//!
//! 一个数据流图计算框架:计算描述成有向图,节点是**算子(Kernel)**,
//! 边上流动**带时间戳的数据包(Packet)**。
//!
//! 对外有两套接口:
//!  * **Rust API**(本 crate 的公开条目)—— 仓库内的 Rust 宿主与测试直接用;
//!  * **C ABI**([`ffi`] 模块 / `include/flow.h`)—— 外部 C/C++/Python 宿主用。
//!
//! 算子用 C++ 编写(见 `cpp/kernels.cc`),由 `build.rs` 编译链入,经函数指针
//! vtable 被引擎回调。
//!
//! 设计文档:`docs/design.md`。

pub mod config;
pub mod context;
pub mod executor;
pub mod ffi;
pub mod graph;
pub mod kernel;
pub mod packet;
pub mod runtime;
pub mod status;
pub mod timestamp;

pub use graph::{Graph, Input, Poller, State};
pub use packet::{BufferData, Builtin, Packet};
pub use status::{Error, Result};
pub use timestamp::Timestamp;

extern "C" {
    /// 由 `cpp/kernels.cc` 提供:显式聚合注册内置 C++ 算子的实现。
    ///
    /// 用显式函数而非静态初始化,是因为静态初始化对象在静态库中可能被链接器裁剪
    /// (见 docs/design.md §14 风险登记)。C ABI 的 `lmflow_register_builtin_kernels`
    /// 由下方 Rust 包装导出(这样它也能出现在 cdylib 的动态导出表里)。
    fn lmflow_register_builtin_kernels_impl();
}

/// 注册内置 C++ 算子。**幂等**:重复调用只在首次生效。
///
/// 必须在 [`Graph::from_yaml`] 之前调用,否则会报「算子未注册」。
pub fn register_builtin_kernels() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe { lmflow_register_builtin_kernels_impl() });
}

/// C ABI:注册内置算子(见 `include/flow.h`)。是 [`register_builtin_kernels`] 的导出包装 ——
/// 由 Rust 定义并 `#[no_mangle]` 导出,保证它同时出现在静态库和 cdylib 的符号表里,
/// 与 `flow.h` 的声明一致。
///
/// # Safety
/// 无参数、无指针入参;内部幂等。可从任意线程安全调用。
#[no_mangle]
pub unsafe extern "C" fn lmflow_register_builtin_kernels() {
    register_builtin_kernels();
}
