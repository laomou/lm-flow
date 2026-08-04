cmake_minimum_required(VERSION 3.18)

# Rust 的 windows-msvc 标准库与 staticlib 固定使用动态 Release CRT(`/MD`,
# `/defaultlib:msvcrt`)。若 CMake 在 Debug 下默认用 `/MDd`,同一进程会混入两套 CRT,
# MSVC 会以 RuntimeLibrary mismatch 拒绝链接。CMP0091 必须在 project() 前启用,
# 让所有本工程目标无论配置都与 Rust 保持 `/MD`。
if(POLICY CMP0091)
  cmake_policy(SET CMP0091 NEW)
endif()
# 这里不能用 `if(MSVC)`:编译器尚未由 project() 启用,MSVC 变量此时还不存在。
# 非 MSVC 生成器会忽略这个变量。
set(CMAKE_MSVC_RUNTIME_LIBRARY "MultiThreadedDLL")

project(lmflow VERSION 0.2.0 LANGUAGES CXX)

# 引擎是 Rust crate(lmflow/core),cargo 权威构建(build.rs 经 cc 顺带编 lmflow/cpp
# 算子 → liblmflow)。本 CMake(仓库根)驱动 cargo,并拥有 C++/Python 侧。
# **各组件的构建规则在各自文件夹的 CMakeLists**,根文件只做:公共基座 + 编排 + SDK 安装。
#
# 用法:
#   cmake -B build -DCMAKE_BUILD_TYPE=Release && cmake --build build && ctest --test-dir build
#   cmake --install build --prefix /opt/lmflow            # 出 C/C++ SDK
#   -DLMFLOW_BUILD_PYTHON=ON   额外编 pybind11 扩展(scikit-build 出 wheel 时会开)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(LMFLOW_ROOT "${CMAKE_CURRENT_SOURCE_DIR}")   # 仓库根:cmake/ · third_party/ · pyproject
set(LMFLOW_SRC  "${LMFLOW_ROOT}/lmflow")          # 源码根:core/(含 cpp/ include/) · python/

option(LMFLOW_BUILD_TESTS  "Build C++ tests"                          ON)
option(LMFLOW_BUILD_PYTHON "Build the pybind11 Python extension"      OFF)
option(LMFLOW_WITH_CV_TEST "Compile the CV test kernel into _lmflow"  OFF)

include("${LMFLOW_ROOT}/cmake/engine.cmake")     # → lmflow::core(cargo 驱动)

if(LMFLOW_BUILD_PYTHON)
  add_subdirectory(lmflow/python)                 # 只编 + 装 pybind11 扩展(供 wheel)
elseif(CMAKE_SOURCE_DIR STREQUAL LMFLOW_ROOT)     # 作为顶层工程时:C++ 测试 + 装 C/C++ SDK
  if(LMFLOW_BUILD_TESTS)
    enable_testing()
    add_subdirectory(lmflow/cpp/tests)
  endif()
  include("${LMFLOW_ROOT}/cmake/install-sdk.cmake")
endif()
