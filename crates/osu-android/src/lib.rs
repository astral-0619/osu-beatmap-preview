//! JNI entry points + global renderer state for the Android app.
//!
//! The Kotlin side owns the Surface (Flutter Texture) and ExoPlayer; this
//! library owns the wgpu device and a dedicated render thread. The audio
//! position (ExoPlayer) is the master clock: Kotlin pushes it via
//! `nativeSetAudioTimeMs` and the render thread draws the frame for that
//! time.

use jni::objects::JClass;
use jni::sys::{jint, jlong};
use jni::EnvUnowned;
use osu_beatmap_core::pipeline::{download_beatmapset_archive, resolve_beatmap_set_id};
use parking_lot::Mutex;
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};

mod modes;
mod renderer;

use modes::{BeatmapState, Mode};

/// 智能下载器（beatmap-core）的日志桥：事件行累积在进程内存里，
/// Kotlin 侧轮询 nativeTakeDownloadLog 拉走上屏（下载"痕迹"实时可见）。
static DOWNLOAD_LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

const DOWNLOAD_LOG_CAP: usize = 300;

pub(crate) fn push_download_log(line: String) {
    let mut v = DOWNLOAD_LOG.lock();
    if v.len() < DOWNLOAD_LOG_CAP {
        v.push(line);
    }
}

fn dl_event(step: &str, status: &str, bid: Option<&str>, msg: &str) {
    let line = format!("{step} {status} [{}] {msg}", bid.unwrap_or("-"));
    log::info!("dl: {line}");
    push_download_log(line);
}

fn dl_cache(kind: osu_beatmap_core::log::CacheKind, state: &str) {
    let line = format!("cache {:?} = {state}", kind);
    log::info!("dl: {line}");
    push_download_log(line);
}

fn dl_stage(name: &str, ms: f64) {
    let line = format!("stage {name} = {ms:.0} ms");
    log::info!("dl: {line}");
    push_download_log(line);
}

fn dl_stage_status(name: &str, status: &str) {
    let line = format!("stage {name} = {status}");
    log::info!("dl: {line}");
    push_download_log(line);
}

fn ensure_download_log_hook() {
    osu_beatmap_core::log::install(dl_event, dl_cache, dl_stage, dl_stage_status);
}

/// Global shared state between JNI thread and the render thread.
struct Shared {
    /// Raw ANativeWindow pointer handed over by Kotlin (0 = none).
    window: AtomicI64,
    /// ExoPlayer audio position in ms (master clock).
    audio_time_ms: AtomicI64,
    /// Paused flag (ExoPlayer state).
    paused: AtomicBool,
    /// Selected game mode (0=standard,1=taiko,2=catch,3=mania).
    mode: AtomicU32,
    /// Current playback speed multiplier x100 (e.g. 100 = 1.0x).
    speed_x100: AtomicU32,
}

static SHARED: Mutex<Option<Arc<Shared>>> = Mutex::new(None);

fn shared() -> Arc<Shared> {
    let mut g = SHARED.lock();
    if g.is_none() {
        *g = Some(Arc::new(Shared {
            window: AtomicI64::new(0),
            audio_time_ms: AtomicI64::new(0),
            paused: AtomicBool::new(true),
            mode: AtomicU32::new(0),
            speed_x100: AtomicU32::new(100),
        }));
    }
    Arc::clone(g.as_ref().unwrap())
}

#[no_mangle]
pub extern "system" fn JNI_OnLoad(_vm: jni::sys::JavaVM, _res: *mut c_void) -> jint {
    log::debug!("osu_android: JNI_OnLoad");
    0x00010006 // JNI_VERSION_1_6
}

// Android NDK: convert a Java Surface into an ANativeWindow (borrowed;
// the caller must ANativeWindow_release it when done).
unsafe extern "system" {
    #[link_name = "ANativeWindow_fromSurface"]
    fn anativewindow_from_surface(env: *mut jni::sys::JNIEnv, surface: jni::sys::jobject) -> *mut c_void;
    #[link_name = "ANativeWindow_release"]
    fn anativewindow_release(window: *mut c_void);
}

/// Kotlin passes the Java Surface wrapping the Flutter Texture's
/// SurfaceTexture. We convert it to an ANativeWindow here (the render
/// thread borrows it until `nativeSurfaceDestroyed`).
#[no_mangle]
pub extern "system" fn Java_io_github_astral_osu_OsuRenderPlugin_nativeSurfaceCreated<'frame>(
    env: EnvUnowned<'frame>,
    _class: JClass<'frame>,
    surface: jni::objects::JObject<'frame>,
) {
    let window_ptr = unsafe { anativewindow_from_surface(env.into_raw(), surface.into_raw()) };
    if window_ptr.is_null() {
        log::error!("ANativeWindow_fromSurface returned null");
        return;
    }
    let s = shared();
    // release the previous window if this is a re-create (rotation / resume)
    let old = s.window.swap(window_ptr as i64, Ordering::AcqRel);
    if old != 0 {
        unsafe { anativewindow_release(old as *mut c_void) };
    }
    renderer::surface_created(window_ptr);
}

/// Surface destroyed (rotation / backgrounding). Stops the render thread
/// and releases the ANativeWindow.
#[no_mangle]
pub extern "system" fn Java_io_github_astral_osu_OsuRenderPlugin_nativeSurfaceDestroyed<'frame>(
    _env: EnvUnowned<'frame>,
    _class: JClass<'frame>,
) {
    let s = shared();
    let old = s.window.swap(0, Ordering::AcqRel);
    if old != 0 {
        unsafe { anativewindow_release(old as *mut c_void) };
    }
    renderer::surface_destroyed();
}

/// ExoPlayer position (ms). Master clock for the renderer.
#[no_mangle]
pub extern "system" fn Java_io_github_astral_osu_OsuRenderPlugin_nativeSetAudioTimeMs<'frame>(
    _env: EnvUnowned<'frame>,
    _class: JClass<'frame>,
    position_ms: jlong,
) {
    shared().audio_time_ms.store(position_ms, Ordering::Relaxed);
}

#[no_mangle]
pub extern "system" fn Java_io_github_astral_osu_OsuRenderPlugin_nativeSetPaused<'frame>(
    _env: EnvUnowned<'frame>,
    _class: JClass<'frame>,
    paused: bool,
) {
    shared().paused.store(paused, Ordering::Relaxed);
}

#[no_mangle]
pub extern "system" fn Java_io_github_astral_osu_OsuRenderPlugin_nativeSetMode<'frame>(
    _env: EnvUnowned<'frame>,
    _class: JClass<'frame>,
    mode: jint,
) {
    shared().mode.store(mode.clamp(0, 3) as u32, Ordering::Relaxed);
}

#[no_mangle]
pub extern "system" fn Java_io_github_astral_osu_OsuRenderPlugin_nativeSetSpeed<'frame>(
    _env: EnvUnowned<'frame>,
    _class: JClass<'frame>,
    speed_x100: jint,
) {
    shared().speed_x100.store(speed_x100.max(25) as u32, Ordering::Relaxed);
}

/// 下载谱面集（智能下载：多镜像竞速 + Cloudflare 优选 IP，逻辑来自
/// 原 osu-beatmap-preview 的下载器，未做改动）。返回本地 .osz 路径；
/// 失败时返回 `ERR:<原因>`（原因含各镜像的熔断详情）。
#[no_mangle]
pub extern "system" fn Java_io_github_astral_osu_OsuRenderPlugin_nativeDownloadByBid<'frame>(
    mut env: EnvUnowned<'frame>,
    _class: JClass<'frame>,
    bid: jint,
    cache_dir: jni::objects::JString<'frame>,
) -> jni::objects::JString<'frame> {
    let outcome = env.with_env(
        |env: &mut jni::Env<'frame>| -> jni::errors::Result<jni::objects::JString<'frame>> {
            let bid = bid.to_string();
            let dir: String = cache_dir.try_to_string(env)?;
            ensure_download_log_hook();
            let temp_dir = PathBuf::from(&dir);
            let result = resolve_beatmap_set_id(&bid)
                .and_then(|set_id| download_beatmapset_archive(&bid, set_id, &temp_dir, false));
            match result {
                Ok(path) => env.new_string(path.to_string_lossy()),
                Err(e) => env.new_string(format!("ERR:{e}")),
            }
        },
    );
    match outcome.into_outcome() {
        jni::Outcome::Ok(s) => s,
        jni::Outcome::Err(e) => {
            log::error!("nativeDownloadByBid JNI error: {e}");
            jni::objects::JString::null()
        }
        jni::Outcome::Panic(p) => {
            log::error!("nativeDownloadByBid panicked: {p:?}");
            jni::objects::JString::null()
        }
    }
}

/// 取走累积的下载日志（返回后清空）。Kotlin 轮询此函数把
/// 下载过程实时上报给 Dart。
#[no_mangle]
pub extern "system" fn Java_io_github_astral_osu_OsuRenderPlugin_nativeTakeDownloadLog<'frame>(
    mut env: EnvUnowned<'frame>,
    _class: JClass<'frame>,
) -> jni::objects::JString<'frame> {
    let joined = {
        let mut v = DOWNLOAD_LOG.lock();
        let joined = v.join("\n");
        v.clear();
        joined
    };
    let outcome = env.with_env(|env: &mut jni::Env<'frame>| -> jni::errors::Result<jni::objects::JString<'frame>> {
        env.new_string(joined)
    });
    match outcome.into_outcome() {
        jni::Outcome::Ok(s) => s,
        jni::Outcome::Err(_) => jni::objects::JString::null(),
        jni::Outcome::Panic(p) => {
            log::error!("nativeTakeDownloadLog panicked: {p:?}");
            jni::objects::JString::null()
        }
    }
}

/// Load a beatmap file (`.osu` or `.osz`). On success returns the audio
/// file path (for ExoPlayer) as a Java string; on failure `ERR:<原因>`.
/// `work_dir` must be an app-writable dir (Android 的 temp_dir 不可写),
/// used for osz extraction.
#[no_mangle]
pub extern "system" fn Java_io_github_astral_osu_OsuRenderPlugin_nativeLoadBeatmap<'frame>(
    mut env: EnvUnowned<'frame>,
    _class: JClass<'frame>,
    path: jni::objects::JString<'frame>,
    work_dir: jni::objects::JString<'frame>,
) -> jni::objects::JString<'frame> {
    let outcome = env.with_env(|env: &mut jni::Env<'frame>| -> jni::errors::Result<jni::objects::JString<'frame>> {
        let path: String = path.try_to_string(env)?;
        let dir: String = work_dir.try_to_string(env)?;
        let result = renderer::load_beatmap(&path, &PathBuf::from(&dir));
        match result {
            Ok(audio_path) => env.new_string(audio_path),
            Err(e) => {
                log::error!("load_beatmap failed: {e}");
                push_download_log(format!("load: 失败: {e}"));
                env.new_string(format!("ERR:{e}"))
            }
        }
    });
    match outcome.into_outcome() {
        jni::Outcome::Ok(s) => s,
        jni::Outcome::Err(e) => {
            log::error!("nativeLoadBeatmap JNI error: {e}");
            jni::objects::JString::null()
        }
        jni::Outcome::Panic(p) => {
            log::error!("nativeLoadBeatmap panicked: {p:?}");
            jni::objects::JString::null()
        }
    }
}

/// Manually set the loaded beatmap state (used for tests / desktop demo).
pub fn set_state_for_test(state: BeatmapState) {
    renderer::set_state(Some(Arc::new(state)));
}

/// Current render time used by the render loop.
pub(crate) fn render_time() -> i64 {
    shared().audio_time_ms.load(Ordering::Relaxed)
}

pub(crate) fn current_mode() -> Mode {
    match shared().mode.load(Ordering::Relaxed) {
        1 => Mode::Taiko,
        2 => Mode::Catch,
        3 => Mode::Mania,
        _ => Mode::Standard,
    }
}

pub(crate) fn is_paused() -> bool {
    shared().paused.load(Ordering::Relaxed)
}

pub(crate) fn speed() -> f64 {
    (shared().speed_x100.load(Ordering::Relaxed) as f64) / 100.0
}
