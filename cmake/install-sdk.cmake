# Install the native SDK: public headers, pure Rust core, optional kernels, and
# the complete shared library selected by BUILD_SHARED_LIBS.

install(FILES
    "${LMFLOW_SRC}/include/lmflow/flow.h"
    "${LMFLOW_SRC}/include/lmflow/flow.hpp"
    "${LMFLOW_SRC}/include/lmflow/flow_platform_log.hpp"
    DESTINATION include/lmflow)

# Cargo owns the pure Rust archive, so install it as a file rather than a CMake target.
install(FILES "${LMFLOW_CORE_LIB}"
    DESTINATION lib
    RENAME "${LMFLOW_CORE_INSTALL_FILENAME}")

if(LMFLOW_BUILD_KERNELS)
  install(TARGETS lmflow_kernels_archive
      ARCHIVE DESTINATION lib)
endif()

if(BUILD_SHARED_LIBS)
  install(TARGETS lmflow_core_shared lmflow_complete
      RUNTIME DESTINATION bin
      LIBRARY DESTINATION lib
      ARCHIVE DESTINATION lib)
endif()

include(CMakePackageConfigHelpers)
configure_package_config_file(
    "${LMFLOW_ROOT}/cmake/lmflowConfig.cmake.in"
    "${CMAKE_CURRENT_BINARY_DIR}/lmflowConfig.cmake"
    INSTALL_DESTINATION lib/cmake/lmflow)
write_basic_package_version_file(
    "${CMAKE_CURRENT_BINARY_DIR}/lmflowConfigVersion.cmake"
    VERSION ${PROJECT_VERSION} COMPATIBILITY SameMajorVersion)
install(FILES
    "${CMAKE_CURRENT_BINARY_DIR}/lmflowConfig.cmake"
    "${CMAKE_CURRENT_BINARY_DIR}/lmflowConfigVersion.cmake"
    DESTINATION lib/cmake/lmflow)
