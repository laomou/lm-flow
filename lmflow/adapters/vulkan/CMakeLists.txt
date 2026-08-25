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

# 随 adapter 提供的 GPU 算子,与 adapters/opencl 同构:一个算子一个 .cc、`.cc` 里静态自注册,
# 图里直接写 `kernel: VkResize`。whole-archive 是**必需**的(静态注册靠全局对象构造)。
# 与 adapter 头分成独立 target:头是纯 plumbing,只想用管道自己写算子的宿主链
# lmflow::vulkan 即可,不必背上这些算子和内嵌的 SPIR-V。
if(LMFLOW_BUILD_VULKAN_ADAPTER)
  file(GLOB _lmflow_vulkan_kernel_sources CONFIGURE_DEPENDS
       "${CMAKE_CURRENT_SOURCE_DIR}/kernels/*.cc")
  add_library(lmflow_vulkan_kernels_archive STATIC ${_lmflow_vulkan_kernel_sources})
  set_target_properties(lmflow_vulkan_kernels_archive PROPERTIES
    OUTPUT_NAME lmflow_vulkan_kernels
    POSITION_INDEPENDENT_CODE ON)
  # 内嵌的 SPIR-V 头在 shaders/ 下,算子源码直接 #include "resize_spv.h"
  target_include_directories(lmflow_vulkan_kernels_archive
    PRIVATE "${CMAKE_CURRENT_SOURCE_DIR}/shaders")
  target_link_libraries(lmflow_vulkan_kernels_archive PUBLIC lmflow_vulkan)
  add_dependencies(lmflow_vulkan_kernels_archive flow_engine)

  add_library(lmflow_vulkan_kernels INTERFACE)
  _lmflow_interface_whole_archive(lmflow_vulkan_kernels lmflow_vulkan_kernels_archive)
  target_link_libraries(lmflow_vulkan_kernels INTERFACE lmflow_vulkan)
  add_library(lmflow::vulkan_kernels ALIAS lmflow_vulkan_kernels)
  set_target_properties(lmflow_vulkan_kernels PROPERTIES EXPORT_NAME vulkan_kernels)
endif()

if(LMFLOW_BUILD_VULKAN_ADAPTER AND LMFLOW_BUILD_TESTS
   AND NOT LMFLOW_BUILD_PYTHON
   AND CMAKE_SOURCE_DIR STREQUAL LMFLOW_ROOT)
  add_executable(lmflow_vulkan_test tests/vulkan_test.cc)
  target_link_libraries(lmflow_vulkan_test PRIVATE lmflow::vulkan)
  add_dependencies(lmflow_vulkan_test flow_engine)
  add_test(NAME lmflow_vulkan_test COMMAND lmflow_vulkan_test)

  # buffer 池的行为:稳态零新分配(池化的收益指标本身)、池满时的淘汰策略、以及两个上限。
  # 数值测试覆盖不到这些 —— 把池改成永不命中或把淘汰写反,resize 照样全绿。
  add_executable(lmflow_vulkan_pool_test tests/pool_test.cc)
  target_link_libraries(lmflow_vulkan_pool_test PRIVATE lmflow::vulkan)
  add_dependencies(lmflow_vulkan_pool_test flow_engine)
  add_test(NAME lmflow_vulkan_pool_test COMMAND lmflow_vulkan_pool_test)

  # 随 adapter 的算子:链 lmflow::vulkan_kernels(VkResize 由该档案静态自注册),
  # 数值逐点对照测试内的 CPU 参考实现,并与 OpenCL 侧同一套约定。
  add_executable(lmflow_vulkan_resize_test tests/resize_test.cc)
  target_link_libraries(lmflow_vulkan_resize_test PRIVATE lmflow::vulkan_kernels)
  add_dependencies(lmflow_vulkan_resize_test flow_engine)
  add_test(NAME lmflow_vulkan_resize_test COMMAND lmflow_vulkan_resize_test)

  # 同一个 resize 用例在 LMFLOW_VK_FORCE_STAGING=1 下重跑:强制把每个 Image 当作 device-only
  # 显存,从而走 staging 上传(host→staging→device)与 staging 回读(device→staging→host)
  # 路径(见 vulkan.hpp detail::ForceStagingForTest)。两轮各自对照**同一份**测试内 CPU 参考
  # (容差 1e-3,见 resize_test.cc),而非互相逐字节比对;共用一个 oracle 即可让统一内存设备/
  # lavapipe 也覆盖 staging 回读逻辑(拷贝 + 时间线同步 + 延迟回收)。
  add_test(NAME lmflow_vulkan_resize_test_staging COMMAND lmflow_vulkan_resize_test)
  set_tests_properties(lmflow_vulkan_resize_test_staging PROPERTIES
    ENVIRONMENT "LMFLOW_VK_FORCE_STAGING=1")

  # VkAffine 数值 + 多算子 GPU 驻留链(VkUpload→VkResize→VkAffine→VkDownload,中间不落主机)。
  # 全部算子由 lmflow::vulkan_kernels 档案自注册,数值对照测试内的 CPU 参考。
  add_executable(lmflow_vulkan_chain_test tests/chain_test.cc)
  target_link_libraries(lmflow_vulkan_chain_test PRIVATE lmflow::vulkan_kernels)
  add_dependencies(lmflow_vulkan_chain_test flow_engine)
  add_test(NAME lmflow_vulkan_chain_test COMMAND lmflow_vulkan_chain_test)
endif()

if(LMFLOW_BUILD_VULKAN_ADAPTER AND LMFLOW_BUILD_BENCHMARKS
   AND NOT LMFLOW_BUILD_PYTHON
   AND CMAKE_SOURCE_DIR STREQUAL LMFLOW_ROOT)
  # 分阶段基准(upload / dispatch / download / roundtrip)。输出与
  # benchmarks/native_throughput.cc 同一 JSON 形状,可直接喂给 compare_reports.py。
  add_executable(lmflow_vulkan_bench benchmarks/vulkan_bench.cc)
  target_link_libraries(lmflow_vulkan_bench PRIVATE lmflow::vulkan)
  add_dependencies(lmflow_vulkan_bench flow_engine)

  # 图级基准:量「把第二个算子也放 GPU、中间结果不落主机」到底值不值 —— 这是 resize.cc 里
  # 「盈亏平衡在连续 2~3 个 GPU 算子」那句话的实测。两条对照图只差 affine 在哪算,故需要
  # 同时链 GPU 算子档案(VkResize/VkAffine/桥接)与 CPU 算子档案(Affine)。
  add_executable(lmflow_vulkan_chain_bench benchmarks/chain_bench.cc)
  target_link_libraries(lmflow_vulkan_chain_bench PRIVATE lmflow::vulkan_kernels lmflow::kernels)
  add_dependencies(lmflow_vulkan_chain_bench flow_engine)
endif()

if(LMFLOW_BUILD_VULKAN_ADAPTER AND CMAKE_SOURCE_DIR STREQUAL LMFLOW_ROOT)
  install(FILES include/lmflow/vulkan.hpp DESTINATION include/lmflow)
endif()
