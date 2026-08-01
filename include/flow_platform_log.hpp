/*
 * flow_platform_log.hpp —— 可选便利头:一行把引擎日志接到当前平台的日志系统。
 *
 * **不属于 ABI,也不进 core。** 引擎只认 lmflow_set_log_callback(那是抽象层);
 * core 与算子的日志都经它透出。本头只提供一个**现成的平台 sink** 并帮你装上:
 *
 *     #include "flow_platform_log.hpp"
 *     lmflow::InstallPlatformLogSink();   // 之后 core + 算子日志都进平台日志
 *
 * 平台选择发生在**宿主编译期**(下面的 #ifdef),所以 libflow_core 仍然零平台依赖 ——
 * 是宿主在链接 -llog / libhilog / 系统框架,不是引擎。想要别的去向(文件、崩溃上报、
 * 测试缓冲、什么都不做)就别用本头,自己给 lmflow_set_log_callback 传一个 sink 即可。
 *
 * 链接:Android 需 -llog;OpenHarmony 需 libhilog_ndk.z.so;Apple 的 os_log 在 libSystem 内。
 * 回调可能在任意工作线程被调用;这几个平台日志 API 都线程安全,无需额外加锁。
 */
#ifndef LMFLOW_PLATFORM_LOG_HPP_
#define LMFLOW_PLATFORM_LOG_HPP_

#include <cstdio>

#include "flow.h"

#if defined(__ANDROID__)
#  include <android/log.h>
#elif defined(__APPLE__)
#  include <os/log.h>
#elif defined(__OHOS__)
#  include <hilog/log.h>
#endif

namespace lmflow {

/// 把一条引擎日志转发到平台日志系统(级别按平台映射)。
inline void PlatformLogSink(void* /*user*/, LMFlowLogLevel level, const char* msg) {
  if (msg == nullptr) msg = "";
#if defined(__ANDROID__)
  int prio = ANDROID_LOG_DEBUG;
  switch (level) {
    case LMFLOW_LOG_ERROR: prio = ANDROID_LOG_ERROR; break;
    case LMFLOW_LOG_WARN:  prio = ANDROID_LOG_WARN;  break;
    case LMFLOW_LOG_INFO:  prio = ANDROID_LOG_INFO;  break;
    default: break;
  }
  __android_log_print(prio, "lmflow", "%s", msg);
#elif defined(__APPLE__)
  os_log_type_t t = OS_LOG_TYPE_DEBUG;
  switch (level) {
    case LMFLOW_LOG_ERROR: t = OS_LOG_TYPE_ERROR;   break;
    case LMFLOW_LOG_WARN:  t = OS_LOG_TYPE_DEFAULT; break;  // os_log 无 WARN,用 default
    case LMFLOW_LOG_INFO:  t = OS_LOG_TYPE_INFO;    break;
    default: break;
  }
  os_log_with_type(OS_LOG_DEFAULT, t, "%{public}s", msg);
#elif defined(__OHOS__)
  LogLevel lvl = LOG_DEBUG;
  switch (level) {
    case LMFLOW_LOG_ERROR: lvl = LOG_ERROR; break;
    case LMFLOW_LOG_WARN:  lvl = LOG_WARN;  break;
    case LMFLOW_LOG_INFO:  lvl = LOG_INFO;  break;
    default: break;
  }
  OH_LOG_Print(LOG_APP, lvl, 0x0000, "lmflow", "%{public}s", msg);
#else
  // 桌面/其它:无平台日志系统,退到 stderr(带级别前缀)。
  const char* tag = "DEBUG";
  switch (level) {
    case LMFLOW_LOG_ERROR: tag = "ERROR"; break;
    case LMFLOW_LOG_WARN:  tag = "WARN";  break;
    case LMFLOW_LOG_INFO:  tag = "INFO";  break;
    default: break;
  }
  std::fprintf(stderr, "[lmflow %s] %s\n", tag, msg);
#endif
}

/// 一行装上平台 sink:此后 core 与算子的所有日志都进平台日志系统。
inline void InstallPlatformLogSink() { lmflow_set_log_callback(&PlatformLogSink, nullptr); }

}  // namespace lmflow

#endif  // LMFLOW_PLATFORM_LOG_HPP_
