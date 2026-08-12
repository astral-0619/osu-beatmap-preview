# app/ — Flutter 侧（Android 实时预览）

Rust 渲染在 `../crates/osu-android`（JNI cdylib），这里只是 Flutter 壳：
`lib/main.dart`（bid 输入 + Texture + 控制条）和 Kotlin 插件
`OsuRenderPlugin`（Flutter Texture ↔ wgpu Surface，ExoPlayer 播音频当主时钟）。

## 为什么没有 gradle 工程

仓库里不提交 gradle wrapper / android 模板文件——它们由 Flutter 版本决定，
本地用任何 Flutter stable 都能一键生成，避免 wrapper 二进制和版本漂移。

## 本地构建

```bash
cd app
flutter create --platforms=android --org io.github.astral --project-name osu_preview .
mkdir -p android/app/src/main/kotlin/io/github/astral/osu android/app/src/main/kotlin/io/github/astral/osu_preview
cp android_src/OsuRenderPlugin.kt android/app/src/main/kotlin/io/github/astral/osu/
cp android_src/MainActivity.kt   android/app/src/main/kotlin/io/github/astral/osu_preview/
python3 ../ci/patch_android.py

# Rust 侧（需先装 cargo-ndk 和 Android 交叉目标）
cargo ndk -t arm64-v8a -p osu-android -o android/app/src/main/jniLibs build --release

flutter run
```

## CI

`.github/workflows/android.yml` 在 x86_64 runner 上做完全一样的流程
（含 armeabi-v7a / x86_64），产出 `app-release.apk` 作为 artifact。
本地容器是 aarch64 且内核不支持 binfmt，所以 APK 构建以 Actions 为准。
