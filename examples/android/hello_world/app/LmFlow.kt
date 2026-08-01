package com.lmflow.demo

/**
 * 引擎的 Kotlin 门面。native 库 "lmflow_jni" 由 jni/CMakeLists.txt 产出为
 * liblmflow_jni.so,内部静态链入交叉编好的 libflow_core.a(见 examples/android/README.md)。
 *
 * 设计上 Kotlin 侧只看见「送 long、取 long」这类内建类型 —— 与 C++/Python 侧完全一致,
 * 因为跨界数据是语言中立的内建类型(这里是 I64),不是某语言的原生对象。
 */
object LmFlow {
    init { System.loadLibrary("lmflow_jni") }

    /** 引擎 ABI 版本。应等于 include/flow.h 的 LMFLOW_ABI_VERSION,否则结构体布局可能错乱。 */
    external fun abiVersion(): Long

    /** 最小管线 in --ScaleKernel(factor)--> out:返回每个输入的 factor 倍。 */
    external fun runScale(inputs: LongArray, factor: Long): LongArray
}

/*
 * 用法(例如在 Activity / ViewModel 里):
 *
 *   check(LmFlow.abiVersion() == 1L) { "ABI 不匹配,请重编 native 库" }
 *   val out = LmFlow.runScale(longArrayOf(1, 2, 3), factor = 2)  // → [2, 4, 6]
 *   Log.i("lmflow", out.joinToString())
 *
 * 真实相机/推理场景:把每帧图像按 LMFLOW_TYPE_BUFFER 送入(而非 I64),
 * C++ 算子零拷贝读成 cv::Mat / 张量处理 —— 桥接方式相同,只是 packet 构造换成 buffer。
 */
