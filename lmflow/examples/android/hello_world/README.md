# Android 集成示例(JNI)

演示如何在 Android 上通过 **JNI** 调用引擎。引擎是可移植的 Rust + C++,交叉编到
Android 后就是一个普通静态库 `libflow_core.a`;JNI 层只依赖 `include/flow.h` 这一层
C ABI,不碰引擎内部,也不需要引擎认识 JVM。

```
examples/android/
  jni/lmflow_jni.cc     JNI 桥:把 C ABI 暴露成 LmFlow 的 native 方法
  jni/CMakeLists.txt    NDK 构建:link 预编的 libflow_core.a,产出 liblmflow_jni.so
  app/LmFlow.kt         Kotlin 门面 + 用法
```

## 构建步骤

1. **交叉编引擎**(每个要支持的 ABI 各一次):

   ```bash
   rustup target add aarch64-linux-android
   # NDK 工具链(路径按本机 NDK 调整)
   export TOOL="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin"
   export CC_aarch64_linux_android="$TOOL/aarch64-linux-android21-clang"
   export CXX_aarch64_linux_android="$TOOL/aarch64-linux-android21-clang++"
   export AR_aarch64_linux_android="$TOOL/llvm-ar"
   export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$TOOL/aarch64-linux-android21-clang"
   cargo build --release --target aarch64-linux-android
   ```

2. **在 Gradle 里挂上这个 CMake**(`app/build.gradle`):

   ```groovy
   android {
     defaultConfig { ndk { abiFilters "arm64-v8a" } }
     externalNativeBuild { cmake { path "../jni/CMakeLists.txt" } }
   }
   ```

   Gradle 会把 `ANDROID_ABI` 传给 CMake,后者据此定位第 1 步产出的 `.a`。

3. 把 `app/LmFlow.kt` 放进你的包(`com.lmflow.demo`,或改包名并同步 JNI 函数名),
   即可 `LmFlow.runScale(...)`。

## CI 覆盖

`ci.yml` 的 `cross-android` job 用预装 NDK **真的把 JNI 桥连同引擎交叉编成
`liblmflow_jni.so`** —— 证明 C ABI + JNI 桥在 Android 工具链下能编能链。
(完整 APK 构建需要 Gradle/签名,不在 CI 内;引擎到 Android 的可移植性由此 job 保证。)

## 真实数据

示例走 I64 只为最简。相机/推理场景把每帧按 `LMFLOW_TYPE_BUFFER` 送入,
C++ 算子零拷贝读成 `cv::Mat` / 张量 —— 桥接方式不变,只是 packet 构造换成 buffer。
