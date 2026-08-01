cmake_minimum_required(VERSION 3.18)
project(lmflow VERSION 0.1.0 LANGUAGES CXX)

# ---------------------------------------------------------------------------
# 引擎是 Rust crate,由 cargo 权威构建(build.rs 经 cc 顺带编 cpp/ 下的 C++ 算子,
# 产出单一 libflow_core)。CMake **不取代** cargo —— 它驱动 cargo,并拥有 C++ 侧:
# 示例、测试,以及原生 SDK 的 install + find_package(lmflow)。
#   Rust 开发者仍可直接 `cargo build` / `cargo test`;Python 仍走 pip / build.py。
#   C/C++/移动端 + IDE + 打包走本 CMake。
#
# 用法:
#   cmake -B build -DCMAKE_BUILD_TYPE=Release && cmake --build build
#   ctest --test-dir build
#   cmake --install build --prefix /opt/lmflow      # 出 SDK(headers + lib + config)
# ---------------------------------------------------------------------------

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

option(LMFLOW_BUILD_TESTS "Build C++ tests (flow.hpp unit test; CV test when OpenCV is found)" ON)

# CMake 构建类型 → cargo profile。默认 Debug。
if(CMAKE_BUILD_TYPE MATCHES "^(Release|RelWithDebInfo|MinSizeRel)$")
  set(_cargo_flags --release)
  set(_cargo_dir release)
else()
  set(_cargo_flags "")
  set(_cargo_dir debug)
endif()

set(FLOW_LIB "${CMAKE_CURRENT_SOURCE_DIR}/target/${_cargo_dir}/libflow_core.a")

find_program(CARGO cargo REQUIRED)
find_package(Threads REQUIRED)

# cargo 驱动引擎构建(Rust + C++ 算子 → libflow_core.a)。
add_custom_command(
  OUTPUT "${FLOW_LIB}"
  COMMAND ${CARGO} build ${_cargo_flags}
  WORKING_DIRECTORY "${CMAKE_CURRENT_SOURCE_DIR}"
  COMMENT "cargo build ${_cargo_flags} — compiles the Rust engine + C++ kernels into libflow_core.a"
  VERBATIM USES_TERMINAL)
add_custom_target(flow_engine DEPENDS "${FLOW_LIB}")

# 引擎作为 IMPORTED 静态库对外(带公共头 + 需要的系统库)。
add_library(flow_core STATIC IMPORTED GLOBAL)
set_target_properties(flow_core PROPERTIES IMPORTED_LOCATION "${FLOW_LIB}")
target_include_directories(flow_core INTERFACE
  "$<BUILD_INTERFACE:${CMAKE_CURRENT_SOURCE_DIR}/include>"
  "$<INSTALL_INTERFACE:include>")
# 静态库内含 C++ 算子,链接方需 pthread / dl / m。
target_link_libraries(flow_core INTERFACE Threads::Threads ${CMAKE_DL_LIBS} m)
add_library(lmflow::flow_core ALIAS flow_core)

# C++ 宿主示例是**独立工程**(examples/cpp/,自带 CMakeLists,经 find_package 消费本 SDK),
# 不在这里当 target 建 —— 保持根工程聚焦「引擎 + 测试 + 装 SDK」。

# ---- 测试 ----
if(LMFLOW_BUILD_TESTS)
  enable_testing()

  # flow.hpp 糖层单元测试:纯头,不链引擎。
  add_executable(flow_hpp_test cpp/tests/flow_hpp_test.cc)
  target_include_directories(flow_hpp_test PRIVATE include)
  add_test(NAME flow_hpp_test COMMAND flow_hpp_test)

  # flow_cv.hpp 转换测试:仅当装了 OpenCV 才编(引擎本身不含 CV)。
  find_package(OpenCV QUIET)
  if(OpenCV_FOUND)
    add_executable(flow_cv_test cpp/tests/flow_cv_test.cc)
    target_include_directories(flow_cv_test PRIVATE include ${OpenCV_INCLUDE_DIRS})
    target_link_libraries(flow_cv_test PRIVATE flow_core ${OpenCV_LIBS})
    add_dependencies(flow_cv_test flow_engine)
    add_test(NAME flow_cv_test COMMAND flow_cv_test)
    message(STATUS "lmflow: OpenCV ${OpenCV_VERSION} found — building flow_cv_test")
  else()
    message(STATUS "lmflow: OpenCV not found — skipping flow_cv_test (engine needs no OpenCV)")
  endif()
endif()

# ---- 安装:原生 SDK(headers + lib + find_package 配置)----
install(FILES include/flow.h include/flow.hpp include/flow_cv.hpp include/flow_platform_log.hpp
        DESTINATION include)
install(FILES "${FLOW_LIB}" DESTINATION lib)

include(CMakePackageConfigHelpers)
configure_package_config_file(
  cmake/lmflowConfig.cmake.in
  "${CMAKE_CURRENT_BINARY_DIR}/lmflowConfig.cmake"
  INSTALL_DESTINATION lib/cmake/lmflow)
write_basic_package_version_file(
  "${CMAKE_CURRENT_BINARY_DIR}/lmflowConfigVersion.cmake"
  VERSION ${PROJECT_VERSION} COMPATIBILITY SameMajorVersion)
install(FILES
  "${CMAKE_CURRENT_BINARY_DIR}/lmflowConfig.cmake"
  "${CMAKE_CURRENT_BINARY_DIR}/lmflowConfigVersion.cmake"
  DESTINATION lib/cmake/lmflow)
