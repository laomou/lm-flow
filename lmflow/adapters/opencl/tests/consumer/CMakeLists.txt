cmake_minimum_required(VERSION 3.18)
project(lmflow_opencl_consumer LANGUAGES CXX)

find_package(lmflow REQUIRED COMPONENTS opencl)

add_executable(opencl_sdk_smoke main.cc)
target_link_libraries(opencl_sdk_smoke PRIVATE lmflow::opencl)
