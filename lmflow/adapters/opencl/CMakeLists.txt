find_package(OpenCL REQUIRED)

add_library(lmflow_opencl_support INTERFACE)
target_include_directories(lmflow_opencl_support INTERFACE
  "$<BUILD_INTERFACE:${CMAKE_CURRENT_SOURCE_DIR}/include>"
  "$<INSTALL_INTERFACE:include>")
target_link_libraries(lmflow_opencl_support INTERFACE OpenCL::OpenCL)

add_library(lmflow_opencl INTERFACE)
add_library(lmflow::opencl ALIAS lmflow_opencl)
set_target_properties(lmflow_opencl PROPERTIES EXPORT_NAME opencl)
target_link_libraries(lmflow_opencl INTERFACE lmflow::lmflow lmflow_opencl_support)

if(LMFLOW_BUILD_OPENCL_ADAPTER AND LMFLOW_BUILD_TESTS
   AND NOT LMFLOW_BUILD_PYTHON
   AND CMAKE_SOURCE_DIR STREQUAL LMFLOW_ROOT)
  add_executable(lmflow_opencl_test tests/opencl_test.cc)
  target_link_libraries(lmflow_opencl_test PRIVATE lmflow::opencl)
  add_dependencies(lmflow_opencl_test flow_engine)
  add_test(NAME lmflow_opencl_test COMMAND lmflow_opencl_test)
endif()

if(LMFLOW_BUILD_OPENCL_ADAPTER AND LMFLOW_BUILD_BENCHMARKS
   AND NOT LMFLOW_BUILD_PYTHON
   AND CMAKE_SOURCE_DIR STREQUAL LMFLOW_ROOT)
  # 分阶段基准(upload / dispatch / download / roundtrip)。输出与
  # benchmarks/native_throughput.cc 同一 JSON 形状,可直接喂给 compare_reports.py。
  add_executable(lmflow_opencl_bench benchmarks/opencl_bench.cc)
  target_link_libraries(lmflow_opencl_bench PRIVATE lmflow::opencl)
  add_dependencies(lmflow_opencl_bench flow_engine)
endif()

if(LMFLOW_BUILD_OPENCL_ADAPTER AND CMAKE_SOURCE_DIR STREQUAL LMFLOW_ROOT)
  install(FILES include/lmflow/opencl.hpp DESTINATION include/lmflow)
endif()
