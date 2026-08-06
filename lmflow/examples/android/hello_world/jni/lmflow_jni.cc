/*
 * lmflow_jni.cc —— Android JNI 桥:把引擎的 C ABI(flow.h)暴露给 Kotlin/Java。
 *
 * 关键点:引擎是可移植的 Rust + C++,交叉编到 aarch64-linux-android 后就是一个
 * 普通静态库 liblmflow.a。JNI 层只依赖 flow.h 这一层 C ABI —— 不碰引擎内部,
 * 也不需要引擎认识 JVM。数据在跨界处一律用**内建类型**(这里是 I64),故 Kotlin
 * 送进来的 long 能被 C++ 的 ScaleKernel 直接读。
 *
 * 演示的最小管线(与其他语言示例一致):in --ScaleKernel(factor)--> out
 *   Kotlin 传入 long[] 与 factor,native 侧建图、逐包送入、取回 factor 倍的结果。
 *
 * 构建见同目录 CMakeLists.txt 与 examples/android/README.md。
 */
#include <jni.h>

#include <string>
#include <vector>

#include "lmflow/flow.h"
#include "lmflow/flow_platform_log.hpp"  // 可选:把引擎日志接到 logcat

namespace {

// 把一次失败包装成 Java 异常抛回 JVM(而不是静默返回空)。
void ThrowRuntime(JNIEnv* env, const char* what) {
  std::string msg = what;
  const char* detail = lmflow_last_error();
  if (detail && *detail) {
    msg += ": ";
    msg += detail;
  }
  env->ThrowNew(env->FindClass("java/lang/RuntimeException"), msg.c_str());
}

}  // namespace

extern "C" {

// .so 加载时把引擎日志接到 Android logcat(core + 算子日志都走这一个 sink)。
JNIEXPORT jint JNICALL JNI_OnLoad(JavaVM*, void*) {
  lmflow::InstallPlatformLogSink();
  return JNI_VERSION_1_6;
}

/* LmFlow.abiVersion():握手用,校验 header 与 .so 版本一致。 */
JNIEXPORT jlong JNICALL
Java_com_lmflow_demo_LmFlow_abiVersion(JNIEnv*, jclass) {
  return static_cast<jlong>(lmflow_abi_version());
}

/* LmFlow.runScale(long[] inputs, long factor) -> long[]
 * 建一张 ScaleKernel 图,把 inputs 逐个送入,返回每个乘以 factor 的结果。 */
JNIEXPORT jlongArray JNICALL
Java_com_lmflow_demo_LmFlow_runScale(JNIEnv* env, jclass, jlongArray inputs, jlong factor) {
  LMFlowGraph* g = lmflow_graph_new();
  if (!g) {
    ThrowRuntime(env, "lmflow_graph_new failed");
    return nullptr;
  }

  // 没配 executor → 跑在调用线程,任务在 poller_next 期间被抽取(与 hello_world_host 一致)。
  std::string yaml =
      "nodes:\n"
      "  - name: \"scale\"\n"
      "    kernel: \"ScaleKernel\"\n"
      "    input_ports: [\"in\"]\n"
      "    output_ports: [\"out\"]\n"
      "    options: { factor: " + std::to_string(static_cast<long long>(factor)) + " }\n"
      "input_ports: [\"in\"]\n"
      "output_ports: [\"out\"]\n";

  if (lmflow_graph_init_from_yaml(g, yaml.c_str()) != LMFLOW_OK) {
    ThrowRuntime(env, "init_from_yaml failed");
    lmflow_graph_free(g);
    return nullptr;
  }

  LMFlowPoller* poller = lmflow_graph_add_poller(g, "out");
  if (!poller || lmflow_graph_start(g) != LMFLOW_OK) {
    ThrowRuntime(env, "add_poller/start failed");
    lmflow_poller_free(poller);
    lmflow_graph_free(g);
    return nullptr;
  }
  LMFlowInput* in = lmflow_graph_input(g, "in");

  const jsize n = env->GetArrayLength(inputs);
  jlong* src = env->GetLongArrayElements(inputs, nullptr);
  std::vector<int64_t> out;
  out.reserve(n);

  for (jsize i = 0; i < n; ++i) {
    if (lmflow_input_send(in, lmflow_packet_from_i64(src[i], i)) != LMFLOW_OK) {
      env->ReleaseLongArrayElements(inputs, src, JNI_ABORT);
      ThrowRuntime(env, "input_send failed");
      lmflow_input_free(in); lmflow_poller_free(poller); lmflow_graph_free(g);
      return nullptr;
    }
    LMFlowPacket pkt;
    if (lmflow_poller_next(poller, &pkt)) {
      int64_t v = 0;
      if (lmflow_packet_as_i64(&pkt, &v)) out.push_back(v);
      lmflow_packet_drop(&pkt);  // 语义 3:poller 移交所有权,必须释放
    }
  }
  env->ReleaseLongArrayElements(inputs, src, JNI_ABORT);

  lmflow_graph_close_all_inputs(g);
  lmflow_graph_wait_done(g);
  lmflow_input_free(in);
  lmflow_poller_free(poller);
  lmflow_graph_free(g);

  jlongArray result = env->NewLongArray(static_cast<jsize>(out.size()));
  env->SetLongArrayRegion(result, 0, static_cast<jsize>(out.size()),
                          reinterpret_cast<const jlong*>(out.data()));
  return result;
}

}  // extern "C"
