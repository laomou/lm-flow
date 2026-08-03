# HarmonyOS 集成示例(NAPI + ArkTS)

HarmonyOS 上原生能力通过 **NAPI** 暴露给 ArkTS。**OHOS 标准系统是 `target_os=linux`**
(rustc 已确认),引擎按 OHOS target 交叉编后就是普通静态库 `liblmflow.a`;NAPI 层只
依赖 `include/flow.h`。

```
examples/harmonyos/
  napi/lmflow_napi.cpp   NAPI 模块:把 C ABI 暴露成 ArkTS 可调的 runScale
  napi/CMakeLists.txt    OHOS NDK 构建:link 预编 liblmflow.a + libace_napi,产出 liblmflow.so
  ets/Index.ets          ArkTS 用法
```

## 构建步骤(DevEco Studio)

1. **交叉编引擎**到 OHOS 目标(tier-3,需 nightly + `-Z build-std`):

   ```bash
   rustup toolchain install nightly
   cargo +nightly build -Z build-std --release --target aarch64-unknown-linux-ohos
   ```

   工具链用 OHOS NDK 的 clang(在 `~/.cargo/config.toml` 里为该 target 配 linker/CC/CXX,
   指向 `$OHOS_NDK/native/llvm/bin`)。

2. 在 DevEco Studio 工程的 `CMakeLists.txt` 里 `add_subdirectory` 引入 `napi/`,
   把第 1 步产出的 `.a` 路径按 `RUST_TARGET` 对上。

3. `ets/Index.ets` 演示 `import lmflow from 'liblmflow.so'` 后调用。真实工程建议补一份
   `liblmflow.so.d.ts` 声明 `runScale(inputs: number[], factor: number): number[]`。

## CI 覆盖(诚实说明)

**GitHub runner 上没有 OHOS SDK/NDK,故本示例不进 CI 编译**,是参考实现。
但引擎到 OHOS 的可移植性有保障:OHOS 与 Linux 走**同一套代码路径**(`target_os=linux`),
而 Linux 路径由 `rust` / `external-host` / `tsan` 等门禁充分覆盖;`ci.yml` 的
`cross-android` job 还实证了「Rust + C++ 混合引擎经 NDK 交叉编」这条与 OHOS 高度同构的路。

## 真实数据

示例走 number(I64)只为最简;相机/推理场景把每帧按 `LMFLOW_TYPE_BUFFER` 送入,
C++ 算子零拷贝读成 `cv::Mat` / 张量。桥接方式不变,只是 packet 构造换成 buffer。
