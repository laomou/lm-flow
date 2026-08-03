# cmake/install-sdk.cmake —— 安装原生 SDK:公共头 + liblmflow.a + find_package(lmflow) 配置。
# 由根 CMakeLists include(在定义好 lmflow_core / LMFLOW_LIB 之后)。头在 lmflow/ 源码下,
# 配置模板在仓库根 cmake/。

install(FILES
    "${LMFLOW_SRC}/include/lmflow/flow.h"
    "${LMFLOW_SRC}/include/lmflow/flow.hpp"
    "${LMFLOW_SRC}/include/lmflow/flow_cv.hpp"
    "${LMFLOW_SRC}/include/lmflow/flow_platform_log.hpp"
    DESTINATION include/lmflow)
install(FILES "${LMFLOW_LIB}" DESTINATION lib)

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
