# cmake/engine.cmake —— 公共基座:cargo 构建 Rust 引擎 → IMPORTED lmflow::core。
# 由根 CMakeLists include;各子目录(cpp/tests、python)都用这个 target。
# 需要调用方先 set 好:LMFLOW_ROOT(仓库根)、LMFLOW_SRC(lmflow/ 源码根)。

if(CMAKE_BUILD_TYPE MATCHES "^(Release|RelWithDebInfo|MinSizeRel)$")
  set(_cargo_flags --release)
  set(_cargo_dir release)
else()
  set(_cargo_flags "")
  set(_cargo_dir debug)
endif()

# 引擎 crate(lmflow)的 target 落在它自己目录下。
set(LMFLOW_LIB "${LMFLOW_SRC}/core/target/${_cargo_dir}/liblmflow.a")

find_program(CARGO cargo REQUIRED)
find_package(Threads REQUIRED)

# cargo 是权威编译器(CMake 不能直接编 Rust);这里只驱动它。build.rs 经 cc 顺带编
# cpp/ 下的 C++ 算子,一并进 liblmflow.a。ALL:让 `cmake --build` 默认就出这个库。
# 让 flow_engine **每次都调 cargo**(cargo 自己做增量,clean 时秒回)—— 否则 CMake 不追踪
# .rs 变更、会复用陈旧的 .a(本地增量构建曾因此链到旧符号)。cargo 只在 .a 真变时更新它,
# 故下游链接仍按需重链。
add_custom_target(flow_engine ALL
  BYPRODUCTS "${LMFLOW_LIB}"
  COMMAND ${CARGO} build ${_cargo_flags} --features builtin-kernels
  WORKING_DIRECTORY "${LMFLOW_SRC}/core"
  COMMENT "cargo build ${_cargo_flags} — Rust engine + C++ kernels → liblmflow.a"
  VERBATIM USES_TERMINAL)

add_library(lmflow_core STATIC IMPORTED GLOBAL)
set_target_properties(lmflow_core PROPERTIES IMPORTED_LOCATION "${LMFLOW_LIB}")
target_include_directories(lmflow_core INTERFACE
  "$<BUILD_INTERFACE:${LMFLOW_SRC}/include>"
  "$<INSTALL_INTERFACE:include>")
target_link_libraries(lmflow_core INTERFACE Threads::Threads ${CMAKE_DL_LIBS} m)
add_library(lmflow::core ALIAS lmflow_core)
