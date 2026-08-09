cmake_minimum_required(VERSION 3.18)
project(lmflow_sdk_smoke LANGUAGES CXX)

find_package(lmflow REQUIRED)

add_executable(sdk_smoke ../shared_c_abi_smoke.cc)
target_link_libraries(sdk_smoke PRIVATE lmflow::lmflow)
