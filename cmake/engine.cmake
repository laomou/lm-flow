# Cargo builds the pure Rust static core. CMake builds the optional C++ kernels
# and exposes static/shared variants selected by LMFLOW_BUILD_SHARED_LIBS.

set(_lmflow_optimized "$<OR:$<CONFIG:Release>,$<CONFIG:RelWithDebInfo>,$<CONFIG:MinSizeRel>>")

if(MSVC)
  set(LMFLOW_CORE_BUILD_FILENAME "lmflow.lib")
  set(LMFLOW_CORE_INSTALL_FILENAME "lmflow_core_static.lib")
else()
  set(LMFLOW_CORE_BUILD_FILENAME "liblmflow.a")
  set(LMFLOW_CORE_INSTALL_FILENAME "liblmflow_core.a")
endif()

if(LMFLOW_RUST_TARGET)
  set(_lmflow_rust_target_dir "${LMFLOW_SRC}/core/target/${LMFLOW_RUST_TARGET}")
  set(_lmflow_rust_target_args --target "${LMFLOW_RUST_TARGET}")
else()
  set(_lmflow_rust_target_dir "${LMFLOW_SRC}/core/target")
  set(_lmflow_rust_target_args)
endif()
set(LMFLOW_CORE_LIB
    "${_lmflow_rust_target_dir}/$<IF:${_lmflow_optimized},release,debug>/${LMFLOW_CORE_BUILD_FILENAME}")
set(_lmflow_core_debug "${_lmflow_rust_target_dir}/debug/${LMFLOW_CORE_BUILD_FILENAME}")
set(_lmflow_core_release "${_lmflow_rust_target_dir}/release/${LMFLOW_CORE_BUILD_FILENAME}")

find_program(CARGO cargo REQUIRED)
find_package(Threads REQUIRED)

add_custom_target(flow_engine ALL
  BYPRODUCTS "${LMFLOW_CORE_LIB}"
  COMMAND ${CARGO} build "$<${_lmflow_optimized}:--release>" ${_lmflow_rust_target_args}
  WORKING_DIRECTORY "${LMFLOW_SRC}/core"
  COMMENT "cargo build ($<IF:${_lmflow_optimized},release,dev> profile) — pure Rust core"
  COMMAND_EXPAND_LISTS VERBATIM USES_TERMINAL)

add_library(lmflow_core_static STATIC IMPORTED GLOBAL)
set_target_properties(lmflow_core_static PROPERTIES
  IMPORTED_CONFIGURATIONS "DEBUG;RELEASE"
  IMPORTED_LOCATION_DEBUG "${_lmflow_core_debug}"
  IMPORTED_LOCATION_RELEASE "${_lmflow_core_release}"
  IMPORTED_LOCATION "${_lmflow_core_debug}"
  MAP_IMPORTED_CONFIG_RELWITHDEBINFO Release
  MAP_IMPORTED_CONFIG_MINSIZEREL Release)
target_include_directories(lmflow_core_static INTERFACE
  "$<BUILD_INTERFACE:${LMFLOW_SRC}/include>"
  "$<INSTALL_INTERFACE:include>")
target_link_libraries(lmflow_core_static INTERFACE Threads::Threads)
if(WIN32)
  target_link_libraries(lmflow_core_static INTERFACE kernel32 ntdll userenv ws2_32 dbghelp)
else()
  target_link_libraries(lmflow_core_static INTERFACE ${CMAKE_DL_LIBS} m)
endif()
if(MSVC)
  target_compile_options(lmflow_core_static INTERFACE /MD)
endif()
add_library(lmflow::core_static ALIAS lmflow_core_static)

function(_lmflow_whole_archive target)
  set(_libraries ${ARGN})
  if(MSVC)
    target_link_libraries(${target} PRIVATE ${_libraries})
    foreach(_library IN LISTS _libraries)
      target_link_options(${target} PRIVATE "/WHOLEARCHIVE:$<TARGET_FILE:${_library}>")
    endforeach()
  elseif(APPLE)
    target_link_libraries(${target} PRIVATE ${_libraries})
    foreach(_library IN LISTS _libraries)
      target_link_options(${target} PRIVATE
        "SHELL:-Wl,-force_load,$<TARGET_FILE:${_library}>")
    endforeach()
  else()
    target_link_libraries(${target} PRIVATE
      "-Wl,--whole-archive" ${_libraries} "-Wl,--no-whole-archive")
  endif()
endfunction()

function(_lmflow_interface_whole_archive target library)
  if(MSVC)
    target_link_libraries(${target} INTERFACE ${library})
    target_link_options(${target} INTERFACE "/WHOLEARCHIVE:$<TARGET_FILE:${library}>")
  elseif(APPLE)
    target_link_libraries(${target} INTERFACE ${library})
    target_link_options(${target} INTERFACE
      "SHELL:-Wl,-force_load,$<TARGET_FILE:${library}>")
  else()
    target_link_libraries(${target} INTERFACE
      "-Wl,--whole-archive" ${library} "-Wl,--no-whole-archive")
  endif()
endfunction()

function(_lmflow_add_windows_exports target)
  if(NOT MSVC)
    return()
  endif()

  file(STRINGS "${LMFLOW_SRC}/include/lmflow/flow.h" _lmflow_header_lines
       REGEX "^[A-Za-z_][A-Za-z0-9_ *]*lmflow_[a-z_0-9]+\\(")
  set(_lmflow_exports)
  foreach(_line IN LISTS _lmflow_header_lines)
    string(REGEX MATCH "lmflow_[a-z_0-9]+" _symbol "${_line}")
    if(_symbol)
      list(APPEND _lmflow_exports "${_symbol}")
    endif()
  endforeach()
  list(REMOVE_DUPLICATES _lmflow_exports)
  list(SORT _lmflow_exports)

  set(_def "${CMAKE_CURRENT_BINARY_DIR}/${target}.def")
  file(WRITE "${_def}" "EXPORTS\n")
  foreach(_symbol IN LISTS _lmflow_exports)
    file(APPEND "${_def}" "  ${_symbol}\n")
  endforeach()
  target_link_options(${target} PRIVATE "/DEF:${_def}")
endfunction()

if(LMFLOW_BUILD_KERNELS)
  file(GLOB _lmflow_kernel_sources CONFIGURE_DEPENDS
       "${LMFLOW_SRC}/cpp/kernels/*.cc")
  add_library(lmflow_kernels_archive STATIC
    ${_lmflow_kernel_sources}
    "${LMFLOW_SRC}/cpp/abi_assert.cc")
  set_target_properties(lmflow_kernels_archive PROPERTIES
    OUTPUT_NAME lmflow_kernels
    POSITION_INDEPENDENT_CODE ON)
  target_include_directories(lmflow_kernels_archive PUBLIC
    "$<BUILD_INTERFACE:${LMFLOW_SRC}/include>"
    "$<INSTALL_INTERFACE:include>")
  target_link_libraries(lmflow_kernels_archive PUBLIC lmflow_core_static)
  add_dependencies(lmflow_kernels_archive flow_engine)

  add_library(lmflow_kernels INTERFACE)
  _lmflow_interface_whole_archive(lmflow_kernels lmflow_kernels_archive)
  target_include_directories(lmflow_kernels INTERFACE
    "$<BUILD_INTERFACE:${LMFLOW_SRC}/include>"
    "$<INSTALL_INTERFACE:include>")
  add_library(lmflow::kernels ALIAS lmflow_kernels)
endif()

if(LMFLOW_BUILD_SHARED_LIBS)
  add_library(lmflow_core_shared SHARED "${LMFLOW_ROOT}/cmake/lmflow_link.cc")
  set_target_properties(lmflow_core_shared PROPERTIES
    OUTPUT_NAME lmflow_core
    WINDOWS_EXPORT_ALL_SYMBOLS ON)
  add_dependencies(lmflow_core_shared flow_engine)
  _lmflow_whole_archive(lmflow_core_shared lmflow_core_static)
  _lmflow_add_windows_exports(lmflow_core_shared)
  add_library(lmflow::core ALIAS lmflow_core_shared)

  add_library(lmflow_complete SHARED "${LMFLOW_ROOT}/cmake/lmflow_link.cc")
  set_target_properties(lmflow_complete PROPERTIES
    OUTPUT_NAME lmflow
    WINDOWS_EXPORT_ALL_SYMBOLS ON)
  add_dependencies(lmflow_complete flow_engine)
  if(LMFLOW_BUILD_KERNELS)
    _lmflow_whole_archive(lmflow_complete lmflow_core_static lmflow_kernels_archive)
  else()
    _lmflow_whole_archive(lmflow_complete lmflow_core_static)
  endif()
  _lmflow_add_windows_exports(lmflow_complete)
else()
  add_library(lmflow_core INTERFACE)
  target_link_libraries(lmflow_core INTERFACE lmflow_core_static)
  add_library(lmflow::core ALIAS lmflow_core)

  add_library(lmflow_complete INTERFACE)
  if(LMFLOW_BUILD_KERNELS)
    target_link_libraries(lmflow_complete INTERFACE lmflow_kernels)
  endif()
  target_link_libraries(lmflow_complete INTERFACE lmflow_core_static)
endif()

add_library(lmflow::lmflow ALIAS lmflow_complete)
