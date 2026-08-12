# osu-android — 架构说明

Android 端实时渲染器（Flutter + Rust/wgpu），输入 osu! 谱面（.osu / .osz）与 bid，
在手机上按 ExoPlayer 主时钟实时渲染 Standard / Taiko / Catch / Mania 四模式。

## 分层

```
Kotlin (Flutter Plugin, app/android_src/OsuRenderPlugin.kt)
  │  JNI: OsuRenderPlugin_* (crates/osu-android/src/lib.rs)
  ▼
Rust cdylib `osu_android`
  ├─ lib.rs      JNI 入口（Surface jobject → ANativeWindow）+ 全局共享状态
  ├─ renderer.rs wgpu 实例/surface/渲染线程 + 2D 图元批量绘制（ShapeBatcher）
  └─ modes.rs    谱面状态 + 逐帧绘制（standard/taiko/catch/mania）
```

## 关键决策

1. **ExoPlayer 是主时钟**。音频位置由 Kotlin 经 `nativeSetAudioTimeMs(ms)` 推送，
   渲染线程每帧读取原子变量画对应时刻的画面，播放暂停/倍速直接复用 ExoPlayer。
2. **不做逐帧 RGBA 回传 Dart**。所有绘制直接发生在 Rust 侧 wgpu 渲染通道，
   结果经 Surface 呈现到 Flutter Texture，Dart 层零拷贝零转换。
3. **每帧单次 draw call**：全部游戏元素摊平成带色三角形（NDC 坐标），
   打进一个顶点缓冲，一次 alpha-blend pass 提交。
4. **复用 osu-beatmap-core**：解析、模型、mods、四模式转换、滑条几何与
   时间线全部来自 `osu_beatmap_core`，本 crate 只做平台接入与实时绘制。
   原先 CPU `Img` 像素合成器由平色三角形替代；定位/计时公式移植自原项目
   render 模块。
5. **旋转/前后台处理**：Surface 销毁时停渲染线程（`nativeSurfaceDestroyed`），
   重建 Surface 时在 `surface_created` 重启；丢失/过期的交换链纹理按
   wgpu 30 的 `CurrentSurfaceTexture` 状态机处理（Outdated/Lost 重 configure，
   Timeout/Occluded 跳帧）。

## 状态流

```
nativeLoadBeatmap(path)  → 解包 .osz/读 .osu → BeatmapState（含音频路径）
Kotlin 拿音频路径喂 ExoPlayer
nativeSurfaceCreated(surface) → ANativeWindow_fromSurface → wgpu surface → 渲染线程启动
ExoPlayer tick → nativeSetAudioTimeMs(t) → 渲染线程画 t 时刻帧
```

## 平台构建

- 目标：Android (aarch64/armv7/x86_64)，wgpu GLES/Vulkan 后端，JNI `cdylib`。
- 本机容器是 aarch64 且无 binfmt，x86_64 交叉编译工具链无法本地跑；
  APK 构建放 GitHub Actions x86_64 runner（`cargo ndk` + Gradle/Flutter）。
- Kotlin 插件经 Flutter Platform Channel 暴露 `OsuRenderPlugin`（Texture 注册、
  ExoPlayer 控制、模式/倍速设置），包名约定 `io.github.astral.osu`。

## 后续阶段

- [ ] Flutter 工程 + Kotlin 插件 + wgpu Surface ↔ Flutter Texture 打通
- [ ] ExoPlayer 主时钟接入与 UI（bid 输入、模式切换、倍速）
- [ ] 四模式绘制补全（皮肤资源、连击数、hp 条、mania 键区）
- [ ] mods/转换/导入流与 .osz 解压完善
- [ ] CI：GitHub Actions 出 APK
