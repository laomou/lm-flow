cmake_minimum_required(VERSION 3.18)
project(lmflow_vulkan_consumer LANGUAGES CXX)

find_package(lmflow REQUIRED COMPONENTS vulkan)

add_executable(vulkan_sdk_smoke main.cc)
target_link_libraries(vulkan_sdk_smoke PRIVATE lmflow::vulkan)
