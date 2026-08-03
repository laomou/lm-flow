# cpp/tests —— C++ 测试。由根 CMakeLists add_subdirectory 进来;用父作用域的 lmflow::flow_core。

# flow.hpp 糖层单元测试:纯头,不链引擎。
add_executable(flow_hpp_test flow_hpp_test.cc)
target_include_directories(flow_hpp_test PRIVATE "${LMFLOW_SRC}/core/include")
add_test(NAME flow_hpp_test COMMAND flow_hpp_test)

# flow_cv.hpp 转换测试:仅当装了 OpenCV 才编(引擎本身不含 CV)。
find_package(OpenCV QUIET)
if(OpenCV_FOUND)
  add_executable(flow_cv_test flow_cv_test.cc)
  target_include_directories(flow_cv_test PRIVATE "${LMFLOW_SRC}/core/include" ${OpenCV_INCLUDE_DIRS})
  target_link_libraries(flow_cv_test PRIVATE lmflow::flow_core ${OpenCV_LIBS})
  add_dependencies(flow_cv_test flow_engine)
  add_test(NAME flow_cv_test COMMAND flow_cv_test)
  message(STATUS "lmflow: OpenCV ${OpenCV_VERSION} found — building flow_cv_test")
else()
  message(STATUS "lmflow: OpenCV not found — skipping flow_cv_test (engine needs no OpenCV)")
endif()
