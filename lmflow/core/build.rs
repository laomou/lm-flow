//! 构建脚本:可选地编译 `../cpp/` 下的内置 C++ 算子并链入本 crate。
//!
//! **默认不编任何 C++** —— 本 crate 发布出去就是纯 Rust 引擎(`cargo add lmflow`
//! 无需 C++ 工具链)。算子与引擎解耦:自己用 `trait Kernel` + `register_kernel` 写
//! Rust 算子,或经 C ABI 挂 C++/Python 算子。
//!
//! 开 `builtin-kernels` feature 时编 `../cpp/kernels/`(18 个内置 C++ 算子)+
//! `../cpp/abi_assert.cc`。**这条路只在本仓库内可用**:那两个目录在 crate 目录之外,
//! `cargo package` 不会把它们打进 tarball(cargo 只收包目录内的文件),所以 crates.io
//! 的消费者开这个 feature 会拿到下面那条明确的错误,而不是一堆找不到文件的噪音。
//! 仓库内的 CMake / Python / 移动端 / CI 都显式带 `--features builtin-kernels`。
//!
//! `../cpp/abi_assert.cc` 只含 static_assert(外加一个宏探针算子)—— 若
//! `../include/lmflow/flow.h` 的跨界结构体布局与 `src/ffi.rs` 约定的不一致,它就会
//! **编译失败**,而不是留到运行期内存错乱。(Rust 侧的同名断言在 `tests/abi_layout.rs`,
//! 两边钉在同一组常量上;CI 另有一条不经 cargo 的独立 `g++ -c cpp/abi_assert.cc`
//! 编译,所以默认构建不编 C++ 也不丢这层校验。)
//!
//! 注意 build.rs 看不到 `#[cfg(feature = ...)]`,只能读 cargo 注入的 `CARGO_FEATURE_*`。

fn main() {
    if std::env::var_os("CARGO_FEATURE_BUILTIN_KERNELS").is_none() {
        return; // 默认:纯 Rust,不调用任何 C++ 编译器
    }

    let kernels_dir = std::path::Path::new("../cpp/kernels");
    if !kernels_dir.is_dir() {
        panic!(
            "feature `builtin-kernels` 需要 ../cpp/kernels/(C++ 内置算子),但它不存在。\n\
             这些算子在 crate 目录之外,不随发布的 crate 一起分发 —— 因此该 feature 只在 \
             lm-flow 仓库内可用。\n\
             从 crates.io 使用请去掉该 feature(默认即纯 Rust 引擎),用 \
             `lmflow::register_kernel::<T>()` 注册自己的 Rust 算子。"
        );
    }

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++17")
        .include("../include")
        .warnings(true);
    // 内置算子:../cpp/kernels/ 下一文件一算子 + register.cc 聚合。逐个收集编入 ——
    // 新增算子文件会被自动编进来,但注册仍需在 register.cc 里显式登记
    // (见 docs/design.md §5.1 与 §14 风险登记:静态初始化对象在静态库中会被裁剪)。
    let mut kernels: Vec<std::path::PathBuf> = std::fs::read_dir(kernels_dir)
        .expect("读取 ../cpp/kernels 目录失败")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "cc"))
        .collect();
    kernels.sort(); // 固定编译顺序,便于复现
    for k in &kernels {
        build.file(k);
    }
    // abi_assert.cc:只含 static_assert,布局不一致就编译失败(不留到运行期)。
    build.file("../cpp/abi_assert.cc");
    build.compile("flow_cpp");

    println!("cargo:rerun-if-changed=../cpp");
    println!("cargo:rerun-if-changed=../include");
    // 供依赖方 / 外部 host 定位公共头(仅仓库内构建时有意义)
    println!("cargo:include={}/../include", env!("CARGO_MANIFEST_DIR"));
}
