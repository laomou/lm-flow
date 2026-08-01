// Index.ets —— ArkTS 侧用法。native 模块名 "lmflow" 由 napi/lmflow_napi.cpp 注册,
// 打包为 liblmflow.so。真实工程里应配套一份 .d.ts 声明类型(见 README)。

import lmflow from 'liblmflow.so';

// runScale(inputs: number[], factor: number): number[]
// 最小管线:每个输入乘以 factor。
export function demo(): void {
  const out: number[] = lmflow.runScale([1, 2, 3], 2);  // → [2, 4, 6]
  console.info('lmflow runScale => ' + out.join(','));
}

/*
 * 真实相机/推理场景:把每帧图像按 LMFLOW_TYPE_BUFFER 送入(而非 number),
 * C++ 算子零拷贝读成 cv::Mat / 张量。桥接方式相同,只是 packet 构造换成 buffer。
 */
