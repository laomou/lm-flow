# cpp/tests —— C++ 测试。由根 CMakeLists add_subdirectory 进来。

# flow.hpp 糖层单元测试:纯头,不链引擎。
add_executable(flow_hpp_test flow_hpp_test.cc)
target_include_directories(flow_hpp_test PRIVATE "${LMFLOW_SRC}/include")
add_test(NAME flow_hpp_test COMMAND flow_hpp_test)

# buffer_util.hpp 的 F16 软件转换测试:同样纯头、不链引擎(只用 inline 函数)。
# 期望值是硬编码的 IEEE 754 位模式,不依赖 `_Float16` —— 故在 MSVC / arm64 上同样有效。
add_executable(buffer_util_test buffer_util_test.cc)
target_include_directories(buffer_util_test
  PRIVATE "${LMFLOW_SRC}/include" "${LMFLOW_SRC}/cpp/kernels")
add_test(NAME buffer_util_test COMMAND buffer_util_test)

# 链接完整产物后，bundled C++ kernels 应靠静态注册自动可用，宿主不调用任何注册函数。
add_executable(auto_register_test auto_register_test.cc)
target_include_directories(auto_register_test PRIVATE "${LMFLOW_SRC}/include")
target_link_libraries(auto_register_test PRIVATE lmflow::lmflow)
add_dependencies(auto_register_test flow_engine)
add_test(NAME auto_register_test COMMAND auto_register_test)
