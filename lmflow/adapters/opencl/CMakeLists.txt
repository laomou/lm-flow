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

# 随 adapter 提供的 GPU 算子。与 cpp/kernels/ 同构:一个算子一个 .cc,`.cc` 里静态自注册,
# 所以图里直接写 `kernel: OclResize` 即可,宿主不必手写注册。
#
# ⚠ whole-archive 是**必需**的,不是优化:静态注册靠全局对象构造,普通静态链接会把
# 「没人引用」的目标文件整个丢掉,注册就静默消失(与 lmflow_kernels 同一处理)。
#
# 与 adapter 头分开成独立 target:头是纯 plumbing(Context/Image/Upload/Download),
# 算子是内容;只想用管道自己写算子的宿主链 lmflow::opencl 就够,不必背上这些算子。
if(LMFLOW_BUILD_OPENCL_ADAPTER)
  file(GLOB _lmflow_opencl_kernel_sources CONFIGURE_DEPENDS
       "${CMAKE_CURRENT_SOURCE_DIR}/kernels/*.cc")
  add_library(lmflow_opencl_kernels_archive STATIC ${_lmflow_opencl_kernel_sources})
  set_target_properties(lmflow_opencl_kernels_archive PROPERTIES
    OUTPUT_NAME lmflow_opencl_kernels
    POSITION_INDEPENDENT_CODE ON)
  target_link_libraries(lmflow_opencl_kernels_archive PUBLIC lmflow_opencl)
  add_dependencies(lmflow_opencl_kernels_archive flow_engine)

  add_library(lmflow_opencl_kernels INTERFACE)
  _lmflow_interface_whole_archive(lmflow_opencl_kernels lmflow_opencl_kernels_archive)
  target_link_libraries(lmflow_opencl_kernels INTERFACE lmflow_opencl)
  add_library(lmflow::opencl_kernels ALIAS lmflow_opencl_kernels)
  set_target_properties(lmflow_opencl_kernels PROPERTIES EXPORT_NAME opencl_kernels)
endif()

if(LMFLOW_BUILD_OPENCL_ADAPTER AND LMFLOW_BUILD_TESTS
   AND NOT LMFLOW_BUILD_PYTHON
   AND CMAKE_SOURCE_DIR STREQUAL LMFLOW_ROOT)
  add_executable(lmflow_opencl_test tests/opencl_test.cc)
  target_link_libraries(lmflow_opencl_test PRIVATE lmflow::opencl)
  add_dependencies(lmflow_opencl_test flow_engine)
  add_test(NAME lmflow_opencl_test COMMAND lmflow_opencl_test)

  # buffer 池的行为:稳态零新分配(收益指标本身)、池满时的淘汰策略、flags 不同不得互相复用
  # (这条是正确性,不是性能),以及两个上限。与 adapters/vulkan/tests/pool_test.cc 同构。
  add_executable(lmflow_opencl_pool_test tests/pool_test.cc)
  target_link_libraries(lmflow_opencl_pool_test PRIVATE lmflow::opencl)
  add_dependencies(lmflow_opencl_pool_test flow_engine)
  add_test(NAME lmflow_opencl_pool_test COMMAND lmflow_opencl_pool_test)

  # 随 adapter 的算子:链 lmflow::opencl_kernels(OclResize 由该档案静态自注册),
  # 数值逐点对照测试内的 CPU 参考实现。
  add_executable(lmflow_opencl_resize_test tests/resize_test.cc)
  target_link_libraries(lmflow_opencl_resize_test PRIVATE lmflow::opencl_kernels)
  add_dependencies(lmflow_opencl_resize_test flow_engine)
  add_test(NAME lmflow_opencl_resize_test COMMAND lmflow_opencl_resize_test)
endif()

if(LMFLOW_BUILD_OPENCL_ADAPTER AND LMFLOW_BUILD_BENCHMARKS
   AND NOT LMFLOW_BUILD_PYTHON
   AND CMAKE_SOURCE_DIR STREQUAL LMFLOW_ROOT)
  # 分阶段基准(upload / dispatch / download / roundtrip)。输出与
  # benchmarks/native_throughput.cc 同一 JSON 形状,可直接喂给 compare_reports.py。
  add_executable(lmflow_opencl_bench benchmarks/opencl_bench.cc)
  target_link_libraries(lmflow_opencl_bench PRIVATE lmflow::opencl)
  # 基准需要 GPU 执行耗时,故为它单独打开 profiling。注意这会让本二进制里
  # dispatch_enqueue 一类的主机侧数字比默认构建偏高约 5µs/次 —— 见 bench 文件头说明。
  target_compile_definitions(lmflow_opencl_bench PRIVATE LMFLOW_OCL_PROFILING=1)
  add_dependencies(lmflow_opencl_bench flow_engine)
endif()

if(LMFLOW_BUILD_OPENCL_ADAPTER AND CMAKE_SOURCE_DIR STREQUAL LMFLOW_ROOT)
  install(FILES include/lmflow/opencl.hpp DESTINATION include/lmflow)
endif()
