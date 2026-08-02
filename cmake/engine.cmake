# cmake/engine.cmake —— 公共基座:cargo 构建 Rust 引擎 → IMPORTED lmflow::flow_core。
# 由根 CMakeLists include;各子目录(cpp/tests、python)都用这个 target。
# 需要调用方先 set 好:LMFLOW_ROOT(仓库根)、LMFLOW_SRC(lmflow/ 源码根)。

if(CMAKE_BUILD_TYPE MATCHES "^(Release|RelWithDebInfo|MinSizeRel)$")
  set(_cargo_flags --release)
  set(_cargo_dir release)
else()
  set(_cargo_flags "")
  set(_cargo_dir debug)
endif()

# flow-core 是独立 crate,target 落在它自己目录下。
set(FLOW_LIB "${LMFLOW_SRC}/flow-core/target/${_cargo_dir}/libflow_core.a")

find_program(CARGO cargo REQUIRED)
find_package(Threads REQUIRED)

# cargo 是权威编译器(CMake 不能直接编 Rust);这里只驱动它。build.rs 经 cc 顺带编
# cpp/ 下的 C++ 算子,一并进 libflow_core.a。ALL:让 `cmake --build` 默认就出这个库。
add_custom_command(
  OUTPUT "${FLOW_LIB}"
  COMMAND ${CARGO} build ${_cargo_flags}
  WORKING_DIRECTORY "${LMFLOW_SRC}/flow-core"
  COMMENT "cargo build ${_cargo_flags} — Rust engine + C++ kernels → libflow_core.a"
  VERBATIM USES_TERMINAL)
add_custom_target(flow_engine ALL DEPENDS "${FLOW_LIB}")

add_library(flow_core STATIC IMPORTED GLOBAL)
set_target_properties(flow_core PROPERTIES IMPORTED_LOCATION "${FLOW_LIB}")
target_include_directories(flow_core INTERFACE
  "$<BUILD_INTERFACE:${LMFLOW_SRC}/include>"
  "$<INSTALL_INTERFACE:include>")
target_link_libraries(flow_core INTERFACE Threads::Threads ${CMAKE_DL_LIBS} m)
add_library(lmflow::flow_core ALIAS flow_core)
