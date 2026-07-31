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
        .file("../../cpp/kernels.cc")
        .file("../../cpp/abi_assert.cc")
        .warnings(true);
    build.compile("flow_cpp");

    println!("cargo:rerun-if-changed=../../cpp");
    println!("cargo:rerun-if-changed=../../include");
    // 供依赖方 / 外部 host 定位公共头
    println!("cargo:include={}/../../include", env!("CARGO_MANIFEST_DIR"));
}
