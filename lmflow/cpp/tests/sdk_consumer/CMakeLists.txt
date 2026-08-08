cmake_minimum_required(VERSION 3.18)
project(lmflow_sdk_consumer LANGUAGES CXX)

find_package(lmflow REQUIRED)

add_executable(sdk_raii_smoke main.cc)
target_link_libraries(sdk_raii_smoke PRIVATE lmflow::lmflow)
