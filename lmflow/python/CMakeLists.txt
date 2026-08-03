# python —— pybind11 扩展 _lmflow(由 CMake 编;pybind11 用 third_party 的 git submodule)。
# 只在根 CMakeLists 以 -DLMFLOW_BUILD_PYTHON=ON 时被 add_subdirectory 进来
# (scikit-build-core 出 wheel 时会开这个开关)。

set(PYBIND11_FINDPYTHON ON)  # 让 pybind11 用现代 FindPython
find_package(Python REQUIRED COMPONENTS Interpreter Development.Module)

# pybind11:vendored git submodule(仓库根 third_party/pybind11),不从 pip 抓。
add_subdirectory("${LMFLOW_ROOT}/third_party/pybind11" pybind11)

# numpy 头:numpy 是运行时依赖(不 vendor),从当前 Python 的 numpy 取 include 路径。
execute_process(
  COMMAND "${Python_EXECUTABLE}" -c "import numpy; print(numpy.get_include())"
  OUTPUT_VARIABLE NUMPY_INCLUDE OUTPUT_STRIP_TRAILING_WHITESPACE
  RESULT_VARIABLE _np_rc)
if(NOT _np_rc EQUAL 0)
  message(FATAL_ERROR "lmflow: 找不到 numpy(编扩展需要 numpy 头)。请 pip install numpy")
endif()

set(_srcs src/bindings.cc)
if(LMFLOW_WITH_CV_TEST)
  # 测试专用:把 CV 算子(链 OpenCV)也编进扩展,让 Python 能调用 CvInvertTest。
  list(APPEND _srcs "${LMFLOW_SRC}/cpp/tests/cv_test_register.cc")
endif()

pybind11_add_module(_lmflow ${_srcs})
target_include_directories(_lmflow PRIVATE "${LMFLOW_SRC}/include" "${NUMPY_INCLUDE}")
target_link_libraries(_lmflow PRIVATE lmflow::core)
add_dependencies(_lmflow flow_engine)  # cargo 先出库再链

if(LMFLOW_WITH_CV_TEST)
  find_package(OpenCV REQUIRED)
  target_compile_definitions(_lmflow PRIVATE LMFLOW_WITH_CV_TEST)
  target_include_directories(_lmflow PRIVATE "${LMFLOW_SRC}/cpp/tests" ${OpenCV_INCLUDE_DIRS})
  target_link_libraries(_lmflow PRIVATE ${OpenCV_LIBS})
endif()

# 装进 wheel 的 lmflow 包:scikit-build-core 把 install 产物打进 wheel 的 lmflow/。
install(TARGETS _lmflow DESTINATION lmflow)
