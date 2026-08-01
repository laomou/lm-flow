// Demo.swift —— iOS 上用 Swift 调用引擎(通过 module.modulemap 暴露的 C 模块 LmFlowC)。
//
// flow.h 是纯 C ABI,Swift 直接调,无需 Obj-C 桥。跨界数据用内建类型(这里 I64),
// 与 C++/Kotlin/Python 侧一致。真实相机/推理场景把每帧按 LMFLOW_TYPE_BUFFER 送入。

import LmFlowC

enum LmFlow {
    /// 最小管线 in --ScaleKernel(factor)--> out:返回每个输入的 factor 倍。
    static func runScale(_ inputs: [Int64], factor: Int64) -> [Int64] {
        precondition(lmflow_abi_version() == UInt32(LMFLOW_ABI_VERSION), "ABI 不匹配")
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
            var pkt = LmflowPacket()
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
//   let out = LmFlow.runScale([1, 2, 3], factor: 2)   // → [2, 4, 6]
//   print(out)
