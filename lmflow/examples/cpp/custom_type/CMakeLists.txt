# examples/cpp/custom_type —— 完整可独立编译的 C++ 工程,演示**自定义类型**在算子间流动。
# 与 hello_world 同姿势:优先消费已装 SDK,仓库内没装 SDK 时回退到从源码构建引擎。
#
#   A) 仓库内直接构建:
#        cmake -S lmflow/examples/cpp/custom_type -B lmflow/examples/cpp/custom_type/build
#        cmake --build lmflow/examples/cpp/custom_type/build
#        ./lmflow/examples/cpp/custom_type/build/custom_type_host       # 输出 out: 0..5
#
#   B) 消费已装 SDK:
#        cmake -S lmflow/examples/cpp/custom_type -B build -Dlmflow_DIR=/opt/lmflow/lib/cmake/lmflow

cmake_minimum_required(VERSION 3.18)
project(lmflow_custom_type LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

if(NOT TARGET lmflow::lmflow)
  find_package(lmflow QUIET)
  if(NOT lmflow_FOUND)
    message(STATUS "lmflow: 未找到已装 SDK —— 从源码构建引擎(仓库根,驱动 cargo)")
    set(LMFLOW_BUILD_TESTS OFF)
    add_subdirectory("${CMAKE_CURRENT_SOURCE_DIR}/../../../.." "${CMAKE_BINARY_DIR}/lmflow-engine")
  endif()
endif()

add_executable(custom_type_host custom_type_host.cc)
target_link_libraries(custom_type_host PRIVATE lmflow::lmflow)

if(TARGET flow_engine)
  add_dependencies(custom_type_host flow_engine)
endif()
