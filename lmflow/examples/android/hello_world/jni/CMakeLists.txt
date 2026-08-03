cmake_minimum_required(VERSION 3.18)
project(lmflow_jni CXX)
set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

# Gradle/NDK 会传入 ANDROID_ABI;映射到 cargo 的 target 三元组,以定位**预先交叉编好**的
# 引擎静态库(见 README:先 `cargo build --release --target <triple>`)。引擎不依赖 JVM,
# 交叉编后就是普通 .a,JNI 层只 link 它 + Android 的 log 库。
if(ANDROID_ABI STREQUAL "arm64-v8a")
  set(RUST_TARGET aarch64-linux-android)
elseif(ANDROID_ABI STREQUAL "armeabi-v7a")
  set(RUST_TARGET armv7-linux-androideabi)
elseif(ANDROID_ABI STREQUAL "x86_64")
  set(RUST_TARGET x86_64-linux-android)
elseif(ANDROID_ABI STREQUAL "x86")
  set(RUST_TARGET i686-linux-android)
else()
  message(FATAL_ERROR "未支持的 ANDROID_ABI: ${ANDROID_ABI}")
endif()

get_filename_component(LMFLOW_SRC "${CMAKE_CURRENT_SOURCE_DIR}/../../../.." ABSOLUTE)
set(FLOW_CORE "${LMFLOW_SRC}/core/target/${RUST_TARGET}/release/liblmflow.a"
    CACHE FILEPATH "为该 ABI 预编的 liblmflow.a")

add_library(lmflow_jni SHARED lmflow_jni.cc)
target_include_directories(lmflow_jni PRIVATE "${LMFLOW_SRC}/include")
# log:Android 系统日志;引擎的 C++ 算子需要 C++ 运行时,NDK 默认已带。
target_link_libraries(lmflow_jni "${FLOW_CORE}" log)
