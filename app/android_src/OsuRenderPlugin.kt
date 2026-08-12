package io.github.astral.osu

import android.content.Context
import android.graphics.Surface
import android.net.Uri
import android.os.Handler
import android.os.Looper
import androidx.media3.common.MediaItem
import androidx.media3.common.PlaybackParameters
import androidx.media3.exoplayer.ExoPlayer
import io.flutter.embedding.engine.plugins.FlutterPlugin
import io.flutter.plugin.common.MethodCall
import io.flutter.plugin.common.MethodChannel
import io.flutter.view.TextureRegistry
import okhttp3.OkHttpClient
import okhttp3.Request
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

        // JNI 符号名对应 Java_io_github_astral_osu_OsuRenderPlugin_*
        private external fun nativeSurfaceCreated(surface: Surface)
        private external fun nativeSurfaceDestroyed()
        private external fun nativeLoadBeatmap(path: String): String
        private external fun nativeSetAudioTimeMs(positionMs: Long)
        private external fun nativeSetPaused(paused: Boolean)
        private external fun nativeSetMode(mode: Int)
        private external fun nativeSetSpeed(speedX100: Int)
    }

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
                    val osz = File(dir, "$bid.osz")
                    if (!osz.exists() || !isZip(osz)) downloadOsz(bid, osz)
                    result.success(setup(osz.absolutePath))
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
                "setSpeed" -> {
                    val x100 = (call.argument<Number>("speedX100") ?: 100).toInt().coerceIn(25, 400)
                    val speed = x100 / 100f
                    player?.setPlaybackParameters(PlaybackParameters(speed))
                    nativeSetSpeed(x100); result.success(null)
                }
                "positionMs" -> result.success(player?.currentPosition ?: 0L)
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
        val surface = Surface(entry.surfaceTexture())
        nativeSurfaceCreated(surface)

        val audioPath = nativeLoadBeatmap(oszPath)
        val p = ExoPlayer.Builder(context).build()
        p.setMediaItem(MediaItem.fromUri(Uri.fromFile(File(audioPath))))
        p.prepare()
        p.playWhenReady = true
        player = p
        handler.post(clockPoller)
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

    private fun isZip(f: File): Boolean {
        if (f.length() < 1024) return false
        return f.inputStream().use { it.read() == 0x50 && it.read() == 0x4B } // "PK"
    }

    private fun downloadOsz(bid: Int, dest: File) {
        val mirrors = listOf(
            "https://osu.direct/api/d/$bid",
            "https://beatconnect.io/b/$bid/",
            "https://catboy.best/d/$bid",
        )
        var lastError: Exception? = null
        for (m in mirrors) {
            try {
                val req = Request.Builder()
                    .url(m)
                    .header("User-Agent", "osu-preview-android/0.1")
                    .build()
                OkHttpClient.Builder().followRedirects(true).build()
                    .newCall(req).execute().use { resp ->
                        val body = resp.body
                        if (!resp.isSuccessful || body == null) {
                            lastError = IllegalStateException("HTTP ${resp.code} from $m")
                            return@use
                        }
                        dest.outputStream().use { out ->
                            body.byteStream().use { it.copyTo(out) }
                        }
                        if (isZip(dest)) return
                        lastError = IllegalStateException("$m did not return a zip")
                    }
            } catch (e: Exception) {
                lastError = e
            }
        }
        throw RuntimeException("所有镜像下载失败: ${lastError?.message}", lastError)
    }
}
