# examples/cpp/hello_world —— 一个**完整、可独立编译**的 C++ 工程,演示如何消费 lmflow 引擎。
# 只依赖 flow.h 这层 C ABI(+ 可选 flow.hpp 糖层),不碰引擎内部。
#
# 两种方式都开箱即用:
#
#   A) 仓库内直接构建(无需先装 SDK)—— 找不到已装的 lmflow 就从源码构建引擎(驱动 cargo):
#        cmake -S lmflow/examples/cpp/hello_world -B lmflow/examples/cpp/hello_world/build
#        cmake --build lmflow/examples/cpp/hello_world/build
#        ./lmflow/examples/cpp/hello_world/build/hello_world_host      # 输出 out: 0..9
#
#   B) 消费已装好的 SDK(外部工程的标准姿势):
#        # 先在仓库根出 SDK:cmake -S . -B build && cmake --build build
#        #                    cmake --install build --prefix /opt/lmflow
#        cmake -S lmflow/examples/cpp/hello_world -B build -Dlmflow_DIR=/opt/lmflow/lib/cmake/lmflow
#        cmake --build build

cmake_minimum_required(VERSION 3.18)
project(lmflow_hello_world LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

# 已由上层(如某个聚合 CMakeLists)提供 lmflow::flow_core 就直接用;否则自己获取:
# 优先已装好的 SDK;仓库内直接构建、没装 SDK 时,回退到从源码构建引擎(仓库根,驱动 cargo)。
# 两种情况最终都拿到 lmflow::flow_core。
if(NOT TARGET lmflow::flow_core)
  find_package(lmflow QUIET)
  if(NOT lmflow_FOUND)
    message(STATUS "lmflow: 未找到已装 SDK —— 从源码构建引擎(仓库根,驱动 cargo)")
    set(LMFLOW_BUILD_TESTS OFF)  # 只要引擎,不建引擎自带的测试
    add_subdirectory("${CMAKE_CURRENT_SOURCE_DIR}/../../../.." "${CMAKE_BINARY_DIR}/lmflow-engine")
  endif()
endif()

add_executable(hello_world_host hello_world_host.cc)
target_link_libraries(hello_world_host PRIVATE lmflow::flow_core)  # 头 + liblmflow.a + 系统库

# 源码构建(add_subdirectory)时:确保 cargo 先出库再链接。装了 SDK 时 flow_engine 不存在,跳过。
if(TARGET flow_engine)
  add_dependencies(hello_world_host flow_engine)
endif()
