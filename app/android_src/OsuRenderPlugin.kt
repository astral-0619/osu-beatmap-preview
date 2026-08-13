package io.github.astral.osu

import android.content.Context
import android.view.Surface
import android.net.Uri
import android.os.Handler
import android.os.Looper
import androidx.media3.common.MediaItem
import androidx.media3.exoplayer.ExoPlayer
import io.flutter.embedding.engine.plugins.FlutterPlugin
import io.flutter.plugin.common.MethodCall
import io.flutter.plugin.common.MethodChannel
import io.flutter.view.TextureRegistry
import java.io.File

/**
 * Flutter 侧插件：Flutter Texture 上挂 wgpu 渲染，ExoPlayer 只播音频当主时钟。
 *
 * 数据流：
 *   loadByBid(bid) → 下载 .osz → nativeLoadBeatmap 解包/解析（Rust）→
 *   ExoPlayer 播音频 → 每 16ms 把播放位置推给 nativeSetAudioTimeMs →
 *   Rust 渲染线程按时间画当前帧到 Texture 的 Surface。
 */
class OsuRenderPlugin : FlutterPlugin, MethodChannel.MethodCallHandler {

    private lateinit var channel: MethodChannel
    private lateinit var context: Context
    private lateinit var textureRegistry: TextureRegistry
    private var textureEntry: TextureRegistry.SurfaceTextureEntry? = null
    private var player: ExoPlayer? = null
    private var downloading = false
    private var logPollDeadline = 0L
    private val handler = Handler(Looper.getMainLooper())

    private val clockPoller = object : Runnable {
        override fun run() {
            pushClock()
            handler.postDelayed(this, 16)
        }
    }

    companion object {
        init {
            System.loadLibrary("osu_android")
        }
    }

    // 声明在类本体（非 companion），保证 JNI 符号就是
    // Java_io_github_astral_osu_OsuRenderPlugin_*，与 Rust 侧一致。
    private external fun nativeSurfaceCreated(surface: Surface)
    private external fun nativeSurfaceDestroyed()
    private external fun nativeLoadBeatmap(path: String, workDir: String): String
    private external fun nativeSetAudioTimeMs(positionMs: Long)
    private external fun nativeSetPaused(paused: Boolean)
    private external fun nativeSetMode(mode: Int)
    // 智能下载（Rust 侧，与原 osu-beatmap-preview 下载器同一套逻辑）：
    // 返回本地 .osz 路径，失败返回 "ERR:<详情>"。
    private external fun nativeDownloadByBid(bid: Int, cacheDir: String): String
    // 取走 Rust 侧累积的下载日志（取走后清空），用于实时上屏。
    private external fun nativeTakeDownloadLog(): String
    // 当前帧缓冲尺寸（高 32 位=宽，低 32 位=高；0=尚未出帧）。
    private external fun nativeGetFrameSize(): Long

    override fun onAttachedToEngine(binding: FlutterPlugin.FlutterPluginBinding) {
        context = binding.applicationContext
        textureRegistry = binding.textureRegistry
        channel = MethodChannel(binding.binaryMessenger, "io.github.astral.osu/render")
        channel.setMethodCallHandler(this)
    }

    override fun onDetachedFromEngine(binding: FlutterPlugin.FlutterPluginBinding) {
        channel.setMethodCallHandler(null)
        teardown()
    }

    override fun onMethodCall(call: MethodCall, result: MethodChannel.Result) {
        try {
            when (call.method) {
                "loadByBid" -> {
                    val bid = (call.argument<Number>("bid") ?: throw IllegalArgumentException("no bid")).toInt()
                    val dir = File(context.cacheDir, "beatmaps").apply { mkdirs() }
                    // 下载走 Rust 侧智能下载（多镜像竞速 + CF 优选 IP），
                    // 后台线程执行，期间轮询把下载日志实时上屏。
                    report("智能下载中（多镜像竞速）…")
                    downloading = true
                    logPollDeadline = System.currentTimeMillis() + 15 * 60_000
                    val cacheDirPath = dir.absolutePath
                    val poller = object : Runnable {
                        override fun run() {
                            if (System.currentTimeMillis() > logPollDeadline) return
                            val logs = nativeTakeDownloadLog()
                            if (logs.isNotEmpty()) report(logs)
                            handler.postDelayed(this, 500)
                        }
                    }
                    handler.post(poller)
                    Thread {
                        val out = nativeDownloadByBid(bid, cacheDirPath)
                        handler.post {
                            downloading = false
                            val logs = nativeTakeDownloadLog()
                            try {
                                if (out.startsWith("ERR:")) {
                                    val detail = out.removePrefix("ERR:")
                                    val msg = if (logs.isNotEmpty()) "$detail\n$logs" else detail
                                    logPollDeadline = 0
                                    result.error("osu_download", msg, null)
                                } else {
                                    report("下载完成，解析谱面…")
                                    result.success(setup(out))
                                }
                            } catch (e: Exception) {
                                logPollDeadline = 0
                                result.error("osu_render", e.message ?: "unknown error", null)
                            }
                        }
                    }.start()
                }
                "loadFile" -> {
                    val path = call.argument<String>("path") ?: throw IllegalArgumentException("no path")
                    result.success(setup(path))
                }
                "play" -> { player?.play(); result.success(null) }
                "pause" -> { player?.pause(); pushClock(); result.success(null) }
                "seekTo" -> {
                    val ms = (call.argument<Number>("ms") ?: 0).toLong()
                    player?.seekTo(ms); pushClock(); result.success(null)
                }
                "setMode" -> {
                    val mode = (call.argument<Number>("mode") ?: 0).toInt().coerceIn(0, 3)
                    nativeSetMode(mode); result.success(null)
                }
                "positionMs" -> result.success(player?.currentPosition ?: 0L)
                "durationMs" -> result.success(player?.duration?.takeIf { it > 0 } ?: 0L)
                "frameSize" -> {
                    val v = nativeGetFrameSize()
                    result.success(listOf((v shr 32).toInt(), (v and 0xFFFFFFFFL).toInt()))
                }
                "dispose" -> { teardown(); result.success(null) }
                else -> result.notImplemented()
            }
        } catch (e: Exception) {
            result.error("osu_render", e.message ?: "unknown error", null)
        }
    }

    /** 建 Texture + Surface 喂给 Rust，再用 Rust 返回的音频路径起 ExoPlayer。 */
    private fun setup(oszPath: String): Map<String, Any> {
        teardown()
        val entry = textureRegistry.createSurfaceTexture()
        textureEntry = entry
        // 横屏 16:9 渲染缓冲，与 Rust 侧 SURFACE_WIDTH/HEIGHT 一致。
        // 不设的话 SurfaceTexture 默认 1x1，交换链尺寸全错（竖屏画面根因）。
        entry.surfaceTexture().setDefaultBufferSize(1280, 720)
        val surface = Surface(entry.surfaceTexture())
        nativeSurfaceCreated(surface)

        val audioPath = nativeLoadBeatmap(oszPath, context.cacheDir.absolutePath)
        if (audioPath.isEmpty() || audioPath.startsWith("ERR:")) {
            val logs = nativeTakeDownloadLog()
            teardown()
            throw IllegalStateException(
                "谱面解析失败：$audioPath" + if (logs.isNotEmpty()) "\n$logs" else ""
            )
        }
        val p = ExoPlayer.Builder(context).build()
        p.setMediaItem(MediaItem.fromUri(Uri.fromFile(File(audioPath))))
        p.prepare()
        p.playWhenReady = true
        player = p
        handler.post(clockPoller)
        // 渲染器初始化的日志晚于 load 返回才产生（渲染线程首帧等），
        // 延长轮询窗口把它们带上屏。
        logPollDeadline = System.currentTimeMillis() + 10_000
        return mapOf(
            "textureId" to entry.id(),
            "audioPath" to audioPath,
            "durationMs" to (p.duration.takeIf { it > 0 } ?: 0L),
        )
    }

    private fun pushClock() {
        val p = player ?: return
        nativeSetAudioTimeMs(p.currentPosition)
        nativeSetPaused(!p.isPlaying)
    }

    private fun teardown() {
        handler.removeCallbacks(clockPoller)
        player?.release()
        player = null
        if (textureEntry != null) {
            nativeSurfaceDestroyed()
            textureEntry!!.release()
            textureEntry = null
        }
    }

    /** 下载/解析过程实时上报到 Dart 侧（method channel 反向调用）。 */
    private fun report(text: String) {
        try {
            channel.invokeMethod("status", mapOf("text" to text))
        } catch (_: Exception) {
            // Dart 侧 handler 未注册时忽略
        }
    }
}
