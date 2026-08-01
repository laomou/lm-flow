# iOS 集成示例(Swift 直调 C ABI)

`flow.h` 是**纯 C ABI**,Swift 通过一个 module map 就能 `import` 并直接调用,无需
Obj-C 桥。引擎交叉编到 iOS 后是普通静态库 `libflow_core.a`。

```
examples/ios/
  bridge_smoke.c     CI 用:iOS SDK 下编译+链接冒烟(证明头+库在 iOS 工具链可用)
  module.modulemap   把 flow.h 暴露成 Swift 可 import 的 C 模块 LmFlowC
  Demo.swift         Swift 用法(最小 Scale 管线)
```

## 构建步骤

1. **交叉编引擎**:

   ```bash
   rustup target add aarch64-apple-ios
   cargo build --release --target aarch64-apple-ios
   ```

2. **在 Xcode / SwiftPM 里**:
   - 头文件搜索路径加上 `examples/ios`(取 module map)与 `include`;
   - 链接第 1 步的 `target/aarch64-apple-ios/release/libflow_core.a`,并加 `-lc++`
     (引擎的 C++ 算子需要 C++ 运行时);
   - `import LmFlowC` 后即可调用 `lmflow_*`,用法见 `Demo.swift`。

## CI 覆盖

`ci.yml` 的 `cross-darwin` job(macOS runner,自带 Xcode iOS SDK)会:
- `cargo build --target aarch64-apple-ios` 交叉编引擎;
- 用 `xcrun --sdk iphoneos clang -arch arm64` **编译并链接** `bridge_smoke.c` +
  `libflow_core.a` 成一个 iOS arm64 可执行 —— 证明 C ABI 头在 iOS 下能解析、
  符号能解析、C++ 运行时能链。(不跑:runner 无真机/模拟器;可移植性由链接成功保证。)

## 真实数据

示例走 I64 只为最简;相机/推理场景把每帧按 `LMFLOW_TYPE_BUFFER` 送入,C++ 算子
零拷贝读成 `cv::Mat` / 张量。桥接方式不变,只是 packet 构造换成 buffer。
