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
use parking_lot::Mutex;
use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};

mod modes;
mod renderer;

use modes::{BeatmapState, Mode};

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

/// Kotlin passes the ANativeWindow pointer created from the Flutter
/// Texture's Surface. The render thread starts (or restarts) rendering.
#[no_mangle]
pub extern "system" fn Java_io_github_astral_osu_OsuRenderPlugin_nativeSurfaceCreated<'frame>(
    _env: EnvUnowned<'frame>,
    _class: JClass<'frame>,
    window_ptr: jlong,
) {
    let s = shared();
    s.window.store(window_ptr, Ordering::Release);
    renderer::surface_created(window_ptr as *mut c_void);
}

/// Surface destroyed (rotation / backgrounding). Stops the render thread.
#[no_mangle]
pub extern "system" fn Java_io_github_astral_osu_OsuRenderPlugin_nativeSurfaceDestroyed<'frame>(
    _env: EnvUnowned<'frame>,
    _class: JClass<'frame>,
) {
    shared().window.store(0, Ordering::Release);
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

/// Load a beatmap file (`.osu` or `.osz`). On success returns the audio
/// file path (for ExoPlayer) as a Java string.
#[no_mangle]
pub extern "system" fn Java_io_github_astral_osu_OsuRenderPlugin_nativeLoadBeatmap<'frame>(
    mut env: EnvUnowned<'frame>,
    _class: JClass<'frame>,
    path: jni::objects::JString<'frame>,
) -> jni::objects::JString<'frame> {
    let outcome = env.with_env(|env: &mut jni::Env<'frame>| -> jni::errors::Result<jni::objects::JString<'frame>> {
        let path: String = path.try_to_string(env)?;
        let result = renderer::load_beatmap(&path);
        match result {
            Ok(audio_path) => env.new_string(audio_path),
            Err(e) => {
                log::error!("load_beatmap failed: {e}");
                env.new_string("")
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
