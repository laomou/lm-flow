find_package(Vulkan REQUIRED)

add_library(lmflow_vulkan_support INTERFACE)
target_include_directories(lmflow_vulkan_support INTERFACE
  "$<BUILD_INTERFACE:${CMAKE_CURRENT_SOURCE_DIR}/include>"
  "$<INSTALL_INTERFACE:include>")
target_link_libraries(lmflow_vulkan_support INTERFACE Vulkan::Vulkan)

add_library(lmflow_vulkan INTERFACE)
add_library(lmflow::vulkan ALIAS lmflow_vulkan)
set_target_properties(lmflow_vulkan PROPERTIES EXPORT_NAME vulkan)
target_link_libraries(lmflow_vulkan INTERFACE lmflow::lmflow lmflow_vulkan_support)

if(LMFLOW_BUILD_VULKAN_ADAPTER AND LMFLOW_BUILD_TESTS
   AND NOT LMFLOW_BUILD_PYTHON
   AND CMAKE_SOURCE_DIR STREQUAL LMFLOW_ROOT)
  add_executable(lmflow_vulkan_test tests/vulkan_test.cc)
  target_link_libraries(lmflow_vulkan_test PRIVATE lmflow::vulkan)
  add_dependencies(lmflow_vulkan_test flow_engine)
  add_test(NAME lmflow_vulkan_test COMMAND lmflow_vulkan_test)
endif()

if(LMFLOW_BUILD_VULKAN_ADAPTER AND LMFLOW_BUILD_BENCHMARKS
   AND NOT LMFLOW_BUILD_PYTHON
   AND CMAKE_SOURCE_DIR STREQUAL LMFLOW_ROOT)
  # 分阶段基准(upload / dispatch / download / roundtrip)。输出与
  # benchmarks/native_throughput.cc 同一 JSON 形状,可直接喂给 compare_reports.py。
  add_executable(lmflow_vulkan_bench benchmarks/vulkan_bench.cc)
  target_link_libraries(lmflow_vulkan_bench PRIVATE lmflow::vulkan)
  add_dependencies(lmflow_vulkan_bench flow_engine)
endif()

if(LMFLOW_BUILD_VULKAN_ADAPTER AND CMAKE_SOURCE_DIR STREQUAL LMFLOW_ROOT)
  install(FILES include/lmflow/vulkan.hpp DESTINATION include/lmflow)
endif()
