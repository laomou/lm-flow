# cpp/tests —— C++ 测试。由根 CMakeLists add_subdirectory 进来;用父作用域的 lmflow::core。

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

# flow_cv.hpp 转换测试:仅当装了 OpenCV 才编(引擎本身不含 CV)。
find_package(OpenCV QUIET)
if(OpenCV_FOUND)
  add_executable(flow_cv_test flow_cv_test.cc)
  target_include_directories(flow_cv_test PRIVATE "${LMFLOW_SRC}/include" ${OpenCV_INCLUDE_DIRS})
  target_link_libraries(flow_cv_test PRIVATE lmflow::core ${OpenCV_LIBS})
  add_dependencies(flow_cv_test flow_engine)
  add_test(NAME flow_cv_test COMMAND flow_cv_test)
  message(STATUS "lmflow: OpenCV ${OpenCV_VERSION} found — building flow_cv_test")
else()
  message(STATUS "lmflow: OpenCV not found — skipping flow_cv_test (engine needs no OpenCV)")
endif()
