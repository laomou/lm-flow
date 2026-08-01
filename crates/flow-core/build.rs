//! 构建脚本:编译 `cpp/` 下的 C++ 算子并链入本 crate。
//!
//! `cpp/abi_assert.cc` 只含 static_assert —— 若 `include/flow.h` 的跨界结构体布局与
//! `src/ffi.rs` 约定的不一致,这里就会**编译失败**,而不是留到运行期内存错乱。
//! (Rust 侧的同名断言在 `tests/abi_layout.rs`,两边钉在同一组常量上。)

fn main() {
    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++17")
        .include("../../include")
        .warnings(true);
    // 内置算子:cpp/kernels/ 下一文件一算子 + register.cc 聚合。逐个收集编入 ——
    // 新增算子文件会被自动编进来(但注册仍需在 register.cc 里显式登记,见 ADR #14)。
    let mut kernels: Vec<std::path::PathBuf> = std::fs::read_dir("../../cpp/kernels")
        .expect("读取 cpp/kernels 目录失败")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "cc"))
        .collect();
    kernels.sort(); // 固定编译顺序,便于复现
    for k in &kernels {
        build.file(k);
    }
    // abi_assert.cc:只含 static_assert,布局不一致就编译失败(不留到运行期)。
    build.file("../../cpp/abi_assert.cc");
    build.compile("flow_cpp");

    println!("cargo:rerun-if-changed=../../cpp");
    println!("cargo:rerun-if-changed=../../include");
    // 供依赖方 / 外部 host 定位公共头
    println!("cargo:include={}/../../include", env!("CARGO_MANIFEST_DIR"));
}
