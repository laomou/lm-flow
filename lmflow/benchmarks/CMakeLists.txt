if(NOT LMFLOW_BUILD_KERNELS)
  message(FATAL_ERROR "LMFLOW_BUILD_BENCHMARKS requires LMFLOW_BUILD_KERNELS=ON")
endif()

add_executable(lmflow_native_throughput native_throughput.cc)
target_link_libraries(lmflow_native_throughput PRIVATE lmflow::lmflow)
add_dependencies(lmflow_native_throughput flow_engine)
