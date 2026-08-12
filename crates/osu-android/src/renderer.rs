//! wgpu device + render thread + simple 2D shape batcher.
//!
//! All game visuals are drawn as flat colored triangles in NDC space
//! (y-up), batched into one vertex buffer per frame and submitted with a
//! single alpha-blended pipeline.

use parking_lot::Mutex;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use raw_window_handle::{AndroidNdkWindowHandle, RawWindowHandle};

use crate::modes::{BeatmapState, load_beatmap_state};
use crate::{is_paused, render_time, speed};

// ---------------- global renderer state ----------------
struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    size: (u32, u32),
}

static RENDERER: Mutex<Option<Renderer>> = Mutex::new(None);
static STATE: Mutex<Option<Arc<BeatmapState>>> = Mutex::new(None);
static STOP: AtomicBool = AtomicBool::new(false);
static THREAD: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);

pub(crate) fn set_state(state: Option<Arc<BeatmapState>>) {
    *STATE.lock() = state;
}

/// Kotlin -> nativeSurfaceCreated(windowPtr). Creates the wgpu surface from
/// the ANativeWindow and starts the render thread.
pub(crate) fn surface_created(window: *mut c_void) {
    if window.is_null() {
        return;
    }
    if let Err(e) = init_renderer(window) {
        log::error!("renderer init failed: {e}");
        return;
    }
    // (re)start render loop
    let mut t = THREAD.lock();
    if t.is_none() {
        STOP.store(false, Ordering::Release);
        *t = Some(std::thread::spawn(render_loop));
    }
}

/// Kotlin -> nativeSurfaceDestroyed(). Stops rendering.
pub(crate) fn surface_destroyed() {
    STOP.store(true, Ordering::Release);
    *RENDERER.lock() = None;
}

fn init_renderer(window: *mut c_void) -> Result<(), String> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    let raw = RawWindowHandle::AndroidNdk(AndroidNdkWindowHandle::new(
        NonNull::new(window as *mut std::ffi::c_void).ok_or("null ANativeWindow")?,
    ));
    let surface = unsafe {
        instance
            .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: None,
                raw_window_handle: raw,
            })
            .map_err(|e| format!("create_surface: {e}"))?
    };
    let adapter = pollster_block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: Some(&surface),
        force_fallback_adapter: false,
        apply_limit_buckets: false,
    }))
    .map_err(|e| format!("request_adapter: {e}"))?;
    let (device, queue) = pollster_block_on(adapter.request_device(
        &wgpu::DeviceDescriptor {
            label: Some("osu-android"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            memory_hints: wgpu::MemoryHints::MemoryUsage,
            trace: wgpu::Trace::Off,
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
        },
    ))
    .map_err(|e| format!("request_device: {e}"))?;

    let caps = surface.get_capabilities(&adapter);
    let format = caps
        .formats
        .iter()
        .copied()
        .find(|f| f.is_srgb())
        .or_else(|| caps.formats.first().copied())
        .ok_or("no surface format")?;
    let config = wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format,
        width: 720,
        height: 1280,
        present_mode: wgpu::PresentMode::Fifo,
        alpha_mode: caps.alpha_modes[0],
        view_formats: vec![],
        desired_maximum_frame_latency: 2,
        color_space: wgpu::SurfaceColorSpace::Auto,
    };
    surface.configure(&device, &config);
    *RENDERER.lock() = Some(Renderer { surface, device, queue, config, size: (720, 1280) });
    Ok(())
}

fn pollster_block_on<F: std::future::Future>(fut: F) -> F::Output {
    use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
    unsafe fn rw_clone(_: *const ()) -> RawWaker {
        RawWaker::new(std::ptr::null(), &RW_VTABLE)
    }
    unsafe fn rw_wake(_: *const ()) {}
    unsafe fn rw_wake_by_ref(_: *const ()) {}
    unsafe fn rw_drop(_: *const ()) {}
    static RW_VTABLE: RawWakerVTable = RawWakerVTable::new(rw_clone, rw_wake, rw_wake_by_ref, rw_drop);
    let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &RW_VTABLE)) };
    let mut cx = Context::from_waker(&waker);
    let mut fut = Box::pin(fut);
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

fn render_loop() {
    while !STOP.load(Ordering::Acquire) {
        if let Some(r) = RENDERER.lock().as_mut() {
            let (w, h) = match r.surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(frame)
                | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => {
                    let (w, h) = (frame.texture.width(), frame.texture.height());
                    let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
                    render_one(&r.device, &r.queue, &view, w, h);
                    r.queue.present(frame);
                    (w, h)
                }
                wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                    r.surface.configure(&r.device, &r.config);
                    r.size
                }
                wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                    r.size
                }
                wgpu::CurrentSurfaceTexture::Validation => {
                    log::warn!("surface validation error; skipping frame");
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    r.size
                }
            };
            r.size = (w, h);
        }
        std::thread::sleep(std::time::Duration::from_millis(16));
    }
}

fn render_one(device: &wgpu::Device, queue: &wgpu::Queue, view: &wgpu::TextureView, w: u32, h: u32) {
    // build vertices for the current frame
    let mut batcher = ShapeBatcher::new(w as f32, h as f32);
    let t_ms = render_time();
    let state = STATE.lock();
    if let Some(state) = state.as_ref() {
        let paused = is_paused();
        let spd = speed();
        crate::modes::draw_frame(&mut batcher, state, t_ms, (w as f32, h as f32), paused, spd);
    }
    let vertices = batcher.vertices;

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: None,
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.035, g: 0.035, b: 0.05, a: 1.0 }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        if !vertices.is_empty() {
            let vbo = device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: (vertices.len() * std::mem::size_of::<Vertex>()) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            queue.write_buffer(&vbo, 0, bytemuck::cast_slice(&vertices));
            pass.set_vertex_buffer(0, vbo.slice(..));
            pass.draw(0..vertices.len() as u32, 0..1);
        }
    }
    queue.submit(Some(encoder.finish()));
}

// ---------------- vertex / batcher ----------------
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct Vertex {
    pub pos: [f32; 2],
    pub color: [f32; 4],
}

impl Vertex {
    fn new(x: f32, y: f32, color: [f32; 4]) -> Self {
        Self { pos: [x, y], color }
    }
}

pub(crate) struct ShapeBatcher {
    vertices: Vec<Vertex>,
    w: f32,
    h: f32,
}

impl ShapeBatcher {
    fn new(w: f32, h: f32) -> Self {
        Self { vertices: Vec::with_capacity(8192), w, h }
    }

    /// Playfield coords (0..512, 0..384, y-down) -> NDC
    pub(crate) fn push_point(&mut self, x: f32, y: f32, color: [f32; 4]) {
        let aspect = self.w / self.h;
        // fit 4:3 playfield into the screen, centered, letterboxed
        let pw = 512.0_f32;
        let ph = 384.0_f32;
        let scale = if aspect > pw / ph { self.h / ph } else { self.w / pw };
        let nx = (x - pw / 2.0) * scale / (self.w / 2.0);
        let ny = -(y - ph / 2.0) * scale / (self.h / 2.0);
        self.vertices.push(Vertex::new(nx, ny, color));
    }

    pub(crate) fn tri(&mut self, a: (f32, f32), b: (f32, f32), c: (f32, f32), color: [f32; 4]) {
        self.push_point(a.0, a.1, color);
        self.push_point(b.0, b.1, color);
        self.push_point(c.0, c.1, color);
    }

    pub(crate) fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: [f32; 4]) {
        self.tri((x, y), (x + w, y), (x + w, y + h), color);
        self.tri((x, y), (x + w, y + h), (x, y + h), color);
    }

    pub(crate) fn circle(&mut self, cx: f32, cy: f32, r: f32, color: [f32; 4], segments: usize) {
        let seg = segments.max(8);
        for i in 0..seg {
            let a0 = i as f32 / seg as f32 * std::f32::consts::TAU;
            let a1 = (i + 1) as f32 / seg as f32 * std::f32::consts::TAU;
            let p0 = (cx + r * a0.cos(), cy + r * a0.sin());
            let p1 = (cx + r * a1.cos(), cy + r * a1.sin());
            self.tri((cx, cy), p0, p1, color);
        }
    }

    /// ring with thickness, inner radius r, outer r+t
    pub(crate) fn ring(&mut self, cx: f32, cy: f32, r: f32, t: f32, color: [f32; 4], segments: usize) {
        let seg = segments.max(8);
        for i in 0..seg {
            let a0 = i as f32 / seg as f32 * std::f32::consts::TAU;
            let a1 = (i + 1) as f32 / seg as f32 * std::f32::consts::TAU;
            let i0 = (cx + r * a0.cos(), cy + r * a0.sin());
            let i1 = (cx + r * a1.cos(), cy + r * a1.sin());
            let o0 = (cx + (r + t) * a0.cos(), cy + (r + t) * a0.sin());
            let o1 = (cx + (r + t) * a1.cos(), cy + (r + t) * a1.sin());
            self.tri(i0, o0, o1, color);
            self.tri(i0, o1, i1, color);
        }
    }

    /// thick polyline with round-ish joins (quads per segment)
    pub(crate) fn polyline(&mut self, pts: &[(f32, f32)], width: f32, color: [f32; 4]) {
        let half = width / 2.0;
        for w in pts.windows(2) {
            let (a, b) = (w[0], w[1]);
            let dx = b.0 - a.0;
            let dy = b.1 - a.1;
            let len = (dx * dx + dy * dy).sqrt().max(0.001);
            let nx = -dy / len * half;
            let ny = dx / len * half;
            let p0 = (a.0 + nx, a.1 + ny);
            let p1 = (a.0 - nx, a.1 - ny);
            let p2 = (b.0 + nx, b.1 + ny);
            let p3 = (b.0 - nx, b.1 - ny);
            self.tri(p0, p1, p3, color);
            self.tri(p0, p3, p2, color);
        }
    }
}

// ---------------- beatmap loading ----------------
pub(crate) fn load_beatmap(path: &str) -> Result<String, String> {
    let p = Path::new(path);
    if p.extension().and_then(|e| e.to_str()) == Some("osz") {
        load_from_osz(p)
    } else {
        // .osu file: audio is a sibling file
        let state = load_beatmap_state(p)?;
        let audio = audio_path_next_to(p, state.audio_filename.as_deref().unwrap_or("audio.mp3"));
        set_state(Some(Arc::new(state)));
        Ok(audio.to_string_lossy().into_owned())
    }
}

fn load_from_osz(p: &Path) -> Result<String, String> {
    let file = std::fs::File::open(p).map_err(|e| format!("open osz: {e}"))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("zip: {e}"))?;
    let out_dir = std::env::temp_dir().join("osu-android-maps");
    std::fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;
    let mut osu_path: Option<PathBuf> = None;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| e.to_string())?;
        let Some(name) = entry.enclosed_name() else { continue };
        let name = name.to_path_buf();
        let ext = name.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext == "osu" && osu_path.is_none() {
            osu_path = Some(out_dir.join(&name));
        }
        if matches!(ext, "mp3" | "ogg" | "wav" | "flac") || ext == "osu" {
            let dest = out_dir.join(&name);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            let mut out = std::fs::File::create(&dest).map_err(|e| e.to_string())?;
            std::io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
        }
    }
    let osu_path = osu_path.ok_or("no .osu in archive")?;
    let state = load_beatmap_state(&osu_path)?;
    let audio = audio_path_next_to(&osu_path, state.audio_filename.as_deref().unwrap_or("audio.mp3"));
    set_state(Some(Arc::new(state)));
    Ok(audio.to_string_lossy().into_owned())
}

fn audio_path_next_to(osu: &Path, audio_filename: &str) -> PathBuf {
    let dir = osu.parent().unwrap_or(Path::new("."));
    dir.join(audio_filename)
}
