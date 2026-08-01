cmake_minimum_required(VERSION 3.16)
project(lmflow_napi CXX)
set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

# OHOS NDK 的 CMake 工具链会传入 OHOS_ARCH。标准系统 OHOS 是 target_os=linux,
# 引擎按对应 rust target 交叉编好的静态库放在 target/<triple>/release 下(见 README)。
if(OHOS_ARCH STREQUAL "arm64-v8a")
  set(RUST_TARGET aarch64-unknown-linux-ohos)
elseif(OHOS_ARCH STREQUAL "armeabi-v7a")
  set(RUST_TARGET armv7-unknown-linux-ohos)
elseif(OHOS_ARCH STREQUAL "x86_64")
  set(RUST_TARGET x86_64-unknown-linux-ohos)
else()
  set(RUST_TARGET aarch64-unknown-linux-ohos)
endif()

get_filename_component(REPO_ROOT "${CMAKE_CURRENT_SOURCE_DIR}/../../../.." ABSOLUTE)

add_library(lmflow SHARED lmflow_napi.cpp)
target_include_directories(lmflow PRIVATE "${REPO_ROOT}/include")
# libace_napi.z.so:OHOS 的 NAPI 运行时;libhilog_ndk.z.so:HiLog(日志桥用);
# libflow_core.a:交叉编好的引擎。
target_link_libraries(lmflow
    "${REPO_ROOT}/target/${RUST_TARGET}/release/libflow_core.a"
    libace_napi.z.so
    libhilog_ndk.z.so)
