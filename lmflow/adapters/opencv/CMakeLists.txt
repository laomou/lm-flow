find_package(OpenCV REQUIRED COMPONENTS core)

add_library(lmflow_opencv_support INTERFACE)
target_include_directories(lmflow_opencv_support INTERFACE
  "$<BUILD_INTERFACE:${CMAKE_CURRENT_SOURCE_DIR}/include>"
  "$<INSTALL_INTERFACE:include>"
  ${OpenCV_INCLUDE_DIRS})
target_link_libraries(lmflow_opencv_support INTERFACE ${OpenCV_LIBS})

add_library(lmflow_opencv INTERFACE)
add_library(lmflow::opencv ALIAS lmflow_opencv)
set_target_properties(lmflow_opencv PROPERTIES EXPORT_NAME opencv)
target_link_libraries(lmflow_opencv INTERFACE lmflow::lmflow lmflow_opencv_support)

if(LMFLOW_BUILD_OPENCV_ADAPTER AND LMFLOW_BUILD_TESTS
   AND NOT LMFLOW_BUILD_PYTHON
   AND CMAKE_SOURCE_DIR STREQUAL LMFLOW_ROOT)
  add_executable(lmflow_opencv_test tests/opencv_test.cc)
  target_link_libraries(lmflow_opencv_test PRIVATE lmflow::opencv)
  add_dependencies(lmflow_opencv_test flow_engine)
  add_test(NAME lmflow_opencv_test COMMAND lmflow_opencv_test)
endif()

if(LMFLOW_BUILD_OPENCV_ADAPTER AND CMAKE_SOURCE_DIR STREQUAL LMFLOW_ROOT)
  install(FILES include/lmflow/opencv.hpp DESTINATION include/lmflow)
endif()
