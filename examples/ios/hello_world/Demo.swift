// Demo.swift —— iOS 上用 Swift 调用引擎(通过 module.modulemap 暴露的 C 模块 LmFlowC)。
//
// flow.h 是纯 C ABI,Swift 直接调,无需 Obj-C 桥。跨界数据用内建类型(这里 I64),
// 与 C++/Kotlin/Python 侧一致。真实相机/推理场景把每帧按 LMFLOW_TYPE_BUFFER 送入。
//
// 日志:引擎(core + 算子)日志经 lmflow_set_log_callback 透出 —— 这里装一个 os_log sink,
// 把它接到 iOS 系统日志。引擎侧零平台依赖,平台适配在宿主(本文件)这一层。

import LmFlowC
import os

private let lmflowLogger = Logger(subsystem: "com.lmflow.demo", category: "engine")

// 顶层 @convention(c) 回调:不捕获上下文,才能作为 C 函数指针传给引擎。
private func lmflowLogSink(_ user: UnsafeMutableRawPointer?,
                           _ level: LMFlowLogLevel,
                           _ msg: UnsafePointer<CChar>?) {
    let text = msg.map { String(cString: $0) } ?? ""
    let type: OSLogType
    if level == LMFLOW_LOG_ERROR { type = .error }
    else if level == LMFLOW_LOG_INFO { type = .info }
    else if level == LMFLOW_LOG_DEBUG { type = .debug }
    else { type = .default }  // WARN 等
    lmflowLogger.log(level: type, "\(text, privacy: .public)")
}

enum LmFlow {
    /// 把引擎日志接到 iOS os_log(core + 算子日志都走这一个 sink)。幂等。
    static func installLogging() { lmflow_set_log_callback(lmflowLogSink, nil) }

    /// 最小管线 in --ScaleKernel(factor)--> out:返回每个输入的 factor 倍。
    static func runScale(_ inputs: [Int64], factor: Int64) -> [Int64] {
        installLogging()
        precondition(lmflow_abi_version() == UInt32(LMFLOW_ABI_VERSION), "ABI mismatch")
        lmflow_register_builtin_kernels()  // 幂等

        guard let g = lmflow_graph_new() else { fatalError("graph_new") }
        defer { lmflow_graph_free(g) }

        let yaml = """
        nodes:
          - name: "scale"
            kernel: "ScaleKernel"
            input_ports: ["in"]
            output_ports: ["out"]
            options: { factor: \(factor) }
        input_ports: ["in"]
        output_ports: ["out"]
        """
        guard lmflow_graph_init_from_yaml(g, yaml) == LMFLOW_OK else {
            fatalError(String(cString: lmflow_last_error()))
        }

        guard let poller = lmflow_graph_add_poller(g, "out"),
              lmflow_graph_start(g) == LMFLOW_OK else { fatalError("start") }
        defer { lmflow_poller_free(poller) }

        let input = lmflow_graph_input(g, "in")
        defer { lmflow_input_free(input) }

        var out: [Int64] = []
        for (i, v) in inputs.enumerated() {
            _ = lmflow_input_send(input, lmflow_packet_from_i64(v, Int64(i)))
            var pkt = LMFlowPacket()
            if lmflow_poller_next(poller, &pkt) {
                var r: Int64 = 0
                if lmflow_packet_as_i64(&pkt, &r) { out.append(r) }
                lmflow_packet_drop(&pkt)  // 语义 3:poller 移交所有权,必须释放
            }
        }
        lmflow_graph_close_all_inputs(g)
        lmflow_graph_wait_done(g)
        return out
    }
}

// 用法:
//   let out = LmFlow.runScale([1, 2, 3], factor: 2)   // → [2, 4, 6];引擎日志自动进 os_log
//   print(out)
