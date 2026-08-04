# cmake/engine.cmake —— 公共基座:cargo 构建 Rust 引擎 → IMPORTED lmflow::core。
# 由根 CMakeLists include;各子目录(cpp/tests、python)都用这个 target。
# 需要调用方先 set 好:LMFLOW_ROOT(仓库根)、LMFLOW_SRC(lmflow/ 源码根)。

# cargo 的 profile 目录随构建配置变化。**不能在配置期读 `CMAKE_BUILD_TYPE`** ——
# 那个变量只对**单配置**生成器(Unix Makefiles / 单配置 Ninja)有意义;多配置生成器
# (Ninja Multi-Config、Visual Studio)的配置是**构建期**由 `--config` 决定的,配置期
# 通常为空。原先的 `if(CMAKE_BUILD_TYPE MATCHES ...)` 在那类生成器下会静默退到 debug:
#
#   cmake -B b -G "Ninja Multi-Config" && cmake --build b --config Release --target flow_engine
#   → cargo 实际建的是 `dev` profile(实测输出 "Finished `dev` profile [unoptimized]"),
#     于是 C++ 侧按 Release 编、却链进一个**未优化的 debug 引擎**,且不给任何提示。
#
# 故改用生成器表达式,让 profile 跟着**实际构建的那个配置**走。单配置生成器行为不变。
set(_lmflow_optimized "$<OR:$<CONFIG:Release>,$<CONFIG:RelWithDebInfo>,$<CONFIG:MinSizeRel>>")

# 引擎 crate(lmflow)的 target 落在它自己目录下。
# cargo 只有 dev / release 两档,故 RelWithDebInfo / MinSizeRel 都映到 release。
set(LMFLOW_LIB "${LMFLOW_SRC}/core/target/$<IF:${_lmflow_optimized},release,debug>/liblmflow.a")
set(_lmflow_lib_debug "${LMFLOW_SRC}/core/target/debug/liblmflow.a")
set(_lmflow_lib_release "${LMFLOW_SRC}/core/target/release/liblmflow.a")

find_program(CARGO cargo REQUIRED)
find_package(Threads REQUIRED)

# cargo 是权威编译器(CMake 不能直接编 Rust);这里只驱动它。build.rs 经 cc 顺带编
# cpp/ 下的 C++ 算子,一并进 liblmflow.a。ALL:让 `cmake --build` 默认就出这个库。
# 让 flow_engine **每次都调 cargo**(cargo 自己做增量,clean 时秒回)—— 否则 CMake 不追踪
# .rs 变更、会复用陈旧的 .a(本地增量构建曾因此链到旧符号)。cargo 只在 .a 真变时更新它,
# 故下游链接仍按需重链。
# COMMAND_EXPAND_LISTS:让 `--release` 的生成器表达式在 Debug 下展开为**无参数**,
# 而不是一个空字符串参数(cargo 会拒掉空参数)。
add_custom_target(flow_engine ALL
  BYPRODUCTS "${LMFLOW_LIB}"
  COMMAND ${CARGO} build "$<${_lmflow_optimized}:--release>" --features builtin-kernels
  WORKING_DIRECTORY "${LMFLOW_SRC}/core"
  COMMENT "cargo build ($<IF:${_lmflow_optimized},release,dev> profile) — Rust engine + C++ kernels → liblmflow.a"
  COMMAND_EXPAND_LISTS VERBATIM USES_TERMINAL)

# 按配置分别给出产物位置(多配置生成器要靠 IMPORTED_LOCATION_<CONFIG> 选)。
# 裸 IMPORTED_LOCATION 是兜底:单配置生成器未传 CMAKE_BUILD_TYPE 时用 debug。
add_library(lmflow_core STATIC IMPORTED GLOBAL)
set_target_properties(lmflow_core PROPERTIES
  IMPORTED_CONFIGURATIONS "DEBUG;RELEASE"
  IMPORTED_LOCATION_DEBUG "${_lmflow_lib_debug}"
  IMPORTED_LOCATION_RELEASE "${_lmflow_lib_release}"
  IMPORTED_LOCATION "${_lmflow_lib_debug}"
  MAP_IMPORTED_CONFIG_RELWITHDEBINFO Release
  MAP_IMPORTED_CONFIG_MINSIZEREL Release)
target_include_directories(lmflow_core INTERFACE
  "$<BUILD_INTERFACE:${LMFLOW_SRC}/include>"
  "$<INSTALL_INTERFACE:include>")
target_link_libraries(lmflow_core INTERFACE Threads::Threads ${CMAKE_DL_LIBS} m)
add_library(lmflow::core ALIAS lmflow_core)
