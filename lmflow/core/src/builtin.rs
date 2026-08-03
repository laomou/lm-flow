//! 引擎自带的**默认 Rust 算子** —— 零 C++,任何配置下都可用。
//!
//! 这里**刻意只有两个**,且都是**纯结构性、零领域假设**的:
//!
//! | 名字 | 作用 | 端口 |
//! |---|---|---|
//! | [`PassThrough`] | 零拷贝直通(接线 / 占位) | 1 → 1,任意类型 |
//! | [`Sink`] | 只消费不产出,让分支能自行终结 | 1 → 0,任意类型 |
//!
//! **为什么不多塞几个**:`Scale`/`Sum`/`Zip`/`Filter` 这类算子都得假设 payload 是 i64
//! 之类的具体类型,而 ADR #6 明确「引擎不解释 payload」—— 把整数算术塞进引擎本体与之相悖。
//! 演示引擎语义(读 options、close 时产出、时间戳对齐、推进 bound、`source_done`)是
//! `../cpp/kernels/` 那 18 个内置 C++ 算子与 `examples/` 的职责,不该由发布出去的引擎库承担。
//! 扇出也不需要算子:**一条边可以直接挂多个消费者**,是引擎原生能力(见 §7 边模型)。
//!
//! 这两个算子用 [`crate::Kernel`] 写成、经与 C++/Python 完全相同的 vtable 注册,
//! 首次建图时自动注册一次(见 [`register_defaults`]),YAML 里直接按名字引用即可,
//! 宿主不需要调用任何注册函数。
//!
//! 名字刻意**不带 `Kernel` 后缀**,以免与内置 C++ 算子(`PassThroughKernel` 等)重名 ——
//! 注册表按名字唯一,重名注册直接报错。

use std::sync::Once;

use crate::kernel_api::{register_kernel, Kernel, KernelContract, KernelCtx};
use crate::runtime::{LOG_DEBUG, LOG_INFO};
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

/// 汇点:只消费、不产出(零输出口),让一条分支能自行终结而不必宿主去 poll。
///
/// 注册名 **`Sink`**。走引擎日志而非 stdout(库不该抢宿主的输出);同时累加**按图**
/// 计数器 `sink.packets` / `sink.closed`,便于测试直接断言。
#[derive(Default)]
pub struct Sink {
    count: i64,
}

impl Kernel for Sink {
    fn get_contract(c: &mut KernelContract) {
        c.input_any(0);
    }
    fn process(&mut self, cc: &mut KernelCtx) -> Result<()> {
        let ts = cc.input_timestamp();
        cc.log(LOG_DEBUG, &format!("received packet @ ts={}", ts.0));
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

/// 注册全部默认 Rust 算子。**幂等**,由 [`crate::Graph::from_config`] 自动调用。
///
/// 注册失败(唯一可能:宿主已用同名注册过自己的算子)被静默忽略 —— 宿主的算子优先,
/// 引擎不该因为要塞自带算子而让建图失败。
pub fn register_defaults() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = register_kernel::<PassThrough>("PassThrough");
        let _ = register_kernel::<Sink>("Sink");
    });
}
