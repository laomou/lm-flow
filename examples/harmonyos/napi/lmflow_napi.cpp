/*
 * lmflow_napi.cpp —— HarmonyOS NAPI 原生模块:把引擎 C ABI 暴露给 ArkTS。
 *
 * OHOS(标准系统)是 target_os=linux —— 引擎按 OHOS 目标交叉编后就是普通静态库
 * libflow_core.a,NAPI 层只依赖 include/flow.h。ArkTS 侧 `import lmflow` 后调用 runScale。
 * 跨界数据用内建类型(I64),与 C++/Kotlin/Swift/Python 侧一致。
 *
 * 构建见同目录 CMakeLists.txt 与 examples/harmonyos/README.md(用 DevEco Studio 的 CMake)。
 */
#include <napi/native_api.h>

#include <string>

#include "flow.h"

namespace {

// runScale(inputs: number[], factor: number): number[]
// 最小管线 in --ScaleKernel(factor)--> out:返回每个输入的 factor 倍。
napi_value RunScale(napi_env env, napi_callback_info info) {
  size_t argc = 2;
  napi_value args[2] = {nullptr, nullptr};
  napi_get_cb_info(env, info, &argc, args, nullptr, nullptr);

  uint32_t n = 0;
  napi_get_array_length(env, args[0], &n);
  int64_t factor = 1;
  napi_get_value_int64(env, args[1], &factor);

  lmflow_register_builtin_kernels();  // 幂等
  LMFlowGraph* g = lmflow_graph_new();
  std::string yaml =
      "nodes:\n  - name: \"scale\"\n    kernel: \"ScaleKernel\"\n"
      "    input_ports: [\"in\"]\n    output_ports: [\"out\"]\n"
      "    options: { factor: " + std::to_string(static_cast<long long>(factor)) + " }\n"
      "input_ports: [\"in\"]\noutput_ports: [\"out\"]\n";
  lmflow_graph_init_from_yaml(g, yaml.c_str());
  LMFlowPoller* poller = lmflow_graph_add_poller(g, "out");
  lmflow_graph_start(g);
  LMFlowInput* in = lmflow_graph_input(g, "in");

  napi_value result = nullptr;
  napi_create_array_with_length(env, n, &result);
  for (uint32_t i = 0; i < n; ++i) {
    napi_value el = nullptr;
    napi_get_element(env, args[0], i, &el);
    int64_t v = 0;
    napi_get_value_int64(env, el, &v);

    lmflow_input_send(in, lmflow_packet_from_i64(v, i));
    LMFlowPacket pkt;
    if (lmflow_poller_next(poller, &pkt)) {
      int64_t out = 0;
      lmflow_packet_as_i64(&pkt, &out);
      lmflow_packet_drop(&pkt);  // 语义 3:poller 移交所有权,必须释放
      napi_value nv = nullptr;
      napi_create_int64(env, out, &nv);
      napi_set_element(env, result, i, nv);
    }
  }
  lmflow_graph_close_all_inputs(g);
  lmflow_graph_wait_done(g);
  lmflow_input_free(in);
  lmflow_poller_free(poller);
  lmflow_graph_free(g);
  return result;
}

napi_value Init(napi_env env, napi_value exports) {
  napi_property_descriptor desc[] = {
      {"runScale", nullptr, RunScale, nullptr, nullptr, nullptr, napi_default, nullptr},
  };
  napi_define_properties(env, exports, sizeof(desc) / sizeof(desc[0]), desc);
  return exports;
}

}  // namespace

// OHOS 模块注册:.so 加载时把模块登记给 ArkTS 运行时。
static napi_module g_lmflowModule = {
    .nm_version = 1,
    .nm_flags = 0,
    .nm_filename = nullptr,
    .nm_register_func = Init,
    .nm_modname = "lmflow",
    .nm_priv = nullptr,
    .reserved = {0},
};

extern "C" __attribute__((constructor)) void RegisterLMFlowModule(void) {
  napi_module_register(&g_lmflowModule);
}
