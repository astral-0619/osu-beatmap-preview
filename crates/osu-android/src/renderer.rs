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

use raw_window_handle::{AndroidNdkWindowHandle, DisplayHandle, RawWindowHandle};

/// Android 的所有权式 display 句柄。raw_window_handle 自带的
/// `DisplayHandle` 是 borrowed 且内部含 NonNull（非 Send/Sync），
/// 而 wgpu 的 `InstanceDescriptor::display` 要求 Send + Sync + 'static。
#[derive(Debug, Clone, Copy)]
struct OwnedAndroidDisplay;

impl raw_window_handle::HasDisplayHandle for OwnedAndroidDisplay {
    fn display_handle(
        &self,
    ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        Ok(DisplayHandle::android())
    }
}

use crate::modes::{BeatmapState, load_beatmap_state};
use crate::{is_paused, render_time, speed};

// ---------------- global renderer state ----------------
struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    size: (u32, u32),
}

/// 纯色 2D 着色器：NDC 顶点 + RGBA 颜色直通。
const FLAT_SHADER: &str = r#"
struct VsIn {
    @location(0) pos: vec2<f32>,
    @location(1) color: vec4<f32>,
};
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) color: vec4<f32>,
};
@vertex
fn vs_main(in: VsIn) -> VsOut {
    var out: VsOut;
    out.pos = vec4<f32>(in.pos, 0.0, 1.0);
    out.color = in.color;
    return out;
}
@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return in.color;
}
"#;

const VERTEX_ATTRIBUTES: &[wgpu::VertexAttribute] = &[
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x2,
        offset: 0,
        shader_location: 0,
    },
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x4,
        offset: 8,
        shader_location: 1,
    },
];

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
        crate::push_download_log("render: ANativeWindow 为空".to_string());
        return;
    }
    if let Err(e) = init_renderer(window) {
        log::error!("renderer init failed: {e}");
        crate::push_download_log(format!("render: 初始化失败: {e}"));
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
    crate::push_download_log("render: 创建 wgpu 实例…".to_string());
    // Android 上 wgpu 要求实例带 DisplayHandle 才能建 surface
    // （GLES 的 EGL display 用默认值即可，Android 句柄本身是空的）。
    // 默认 backends=PRIMARY 不含 GL，这里显式把 Vulkan+GLES 都打开。
    let mut desc = wgpu::InstanceDescriptor::new_with_display_handle_from_env(Box::new(
        OwnedAndroidDisplay,
    ));
    desc.backends = wgpu::Backends::VULKAN | wgpu::Backends::GL;
    let instance = wgpu::Instance::new(desc);
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
    crate::push_download_log("render: surface 创建成功".to_string());
    let adapter = pollster_block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: Some(&surface),
        force_fallback_adapter: false,
        apply_limit_buckets: false,
    }))
    .map_err(|e| format!("request_adapter: {e}"))?;
    let info = adapter.get_info();
    crate::push_download_log(format!(
        "render: adapter {} ({:?}, {:?})",
        info.name, info.backend, info.device_type
    ));
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
    // 颜色按 sRGB 空间直接给（2D 绘画惯例），优先选非 sRGB 格式，
    // 避免 wgpu 把线性值写进 sRGB 交换链导致整体发灰发亮。
    let format = caps
        .formats
        .iter()
        .copied()
        .find(|f| !f.is_srgb())
        .or_else(|| caps.formats.iter().copied().find(|f| f.is_srgb()))
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
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("flat-color"),
        source: wgpu::ShaderSource::Wgsl(FLAT_SHADER.into()),
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: None,
        bind_group_layouts: &[],
        immediate_size: 0,
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("flat"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            compilation_options: Default::default(),
            buffers: &[Some(wgpu::VertexBufferLayout {
                array_stride: std::mem::size_of::<Vertex>() as u64,
                step_mode: wgpu::VertexStepMode::Vertex,
                attributes: VERTEX_ATTRIBUTES,
            })],
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_main"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    });
    *RENDERER.lock() = Some(Renderer { surface, device, queue, config, pipeline, size: (720, 1280) });
    crate::push_download_log(format!("render: 设备就绪 ({:?} 格式)", format));
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

/// 当前帧缓冲尺寸（Flutter 侧据此设置 Texture 控件的宽高比）。
pub(crate) fn frame_size() -> (u32, u32) {
    RENDERER.lock().as_ref().map(|r| r.size).unwrap_or((0, 0))
}

fn render_loop() {
    let mut first_frame_logged = false;
    while !STOP.load(Ordering::Acquire) {
        if let Some(r) = RENDERER.lock().as_mut() {
            let (w, h) = match r.surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(frame)
                | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => {
                    let (w, h) = (frame.texture.width(), frame.texture.height());
                    if !first_frame_logged {
                        first_frame_logged = true;
                        crate::push_download_log(format!("render: 首帧输出 {w}x{h}"));
                    }
                    let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
                    render_one(&r.device, &r.queue, &view, w, h, &r.pipeline);
                    r.queue.present(frame);
                    (w, h)
                }
                wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                    crate::push_download_log("render: surface 过期，重配置".to_string());
                    r.surface.configure(&r.device, &r.config);
                    r.size
                }
                wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                    r.size
                }
                wgpu::CurrentSurfaceTexture::Validation => {
                    log::warn!("surface validation error; skipping frame");
                    crate::push_download_log("render: surface 校验错误，跳过帧".to_string());
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    r.size
                }
            };
            r.size = (w, h);
        }
        std::thread::sleep(std::time::Duration::from_millis(16));
    }
}

fn render_one(device: &wgpu::Device, queue: &wgpu::Queue, view: &wgpu::TextureView, w: u32, h: u32, pipeline: &wgpu::RenderPipeline) {
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
                    // 深灰蓝底，明显区别于「Texture 全黑」——用于肉眼区分
                    // 渲染器存活（有清屏）与渲染器死了（纯黑）。
                    load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.10, g: 0.10, b: 0.14, a: 1.0 }),
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
            pass.set_pipeline(pipeline);
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
pub(crate) fn load_beatmap(path: &str, work_dir: &Path) -> Result<String, String> {
    let p = Path::new(path);
    crate::push_download_log(format!("load: 开始解析 {}", p.file_name().and_then(|n| n.to_str()).unwrap_or("?")));
    if p.extension().and_then(|e| e.to_str()) == Some("osz") {
        load_from_osz(p, work_dir)
    } else {
        // .osu file: audio is a sibling file
        let state = load_beatmap_state(p).map_err(|e| format!("parse osu: {e}"))?;
        let audio = audio_path_next_to(p, state.audio_filename.as_deref().unwrap_or("audio.mp3"));
        if !audio.exists() {
            crate::push_download_log(format!("load: 音频文件不存在: {}", audio.display()));
        }
        set_state(Some(Arc::new(state)));
        crate::push_download_log("load: 解析完成（单文件）".to_string());
        Ok(audio.to_string_lossy().into_owned())
    }
}

fn load_from_osz(p: &Path, work_dir: &Path) -> Result<String, String> {
    let file = std::fs::File::open(p).map_err(|e| format!("open osz: {e}"))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("zip: {e}"))?;
    // Android 的 temp_dir() = /data/local/tmp 对应用不可写，必须用
    // Kotlin 传入的 app cache 目录。
    let out_dir = work_dir.join("osu-android-maps");
    if out_dir.exists() {
        let _ = std::fs::remove_dir_all(&out_dir);
    }
    std::fs::create_dir_all(&out_dir).map_err(|e| format!("create extract dir: {e}"))?;
    crate::push_download_log(format!("load: 解压到 {}", out_dir.display()));
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
    let state = load_beatmap_state(&osu_path).map_err(|e| format!("parse osu: {e}"))?;
    let audio_name = state.audio_filename.as_deref().unwrap_or("audio.mp3");
    let audio = resolve_audio(&osu_path, &out_dir, audio_name);
    if !audio.exists() {
        crate::push_download_log(format!("load: 音频文件不存在: {}", audio.display()));
    }
    crate::push_download_log(format!(
        "load: 解析完成，{} 个物件",
        state.standard.len()
    ));
    set_state(Some(Arc::new(state)));
    Ok(audio.to_string_lossy().into_owned())
}

/// 先看 .osu 同级目录，找不到再按文件名在解压目录里递归找
/// （部分谱面把音频放在子文件夹里）。
fn resolve_audio(osu: &Path, out_dir: &Path, audio_filename: &str) -> PathBuf {
    let sibling = audio_path_next_to(osu, audio_filename);
    if sibling.exists() {
        return sibling;
    }
    let base = Path::new(audio_filename)
        .file_name()
        .map(|b| b.to_string_lossy().into_owned())
        .unwrap_or_else(|| audio_filename.to_string());
    fn walk(dir: &Path, base: &str) -> Option<PathBuf> {
        let entries = std::fs::read_dir(dir).ok()?;
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                if let Some(found) = walk(&p, base) {
                    return Some(found);
                }
            } else if p.file_name().is_some_and(|n| n == std::ffi::OsStr::new(base)) {
                return Some(p);
            }
        }
        None
    }
    walk(out_dir, &base).unwrap_or(sibling)
}

fn audio_path_next_to(osu: &Path, audio_filename: &str) -> PathBuf {
    let dir = osu.parent().unwrap_or(Path::new("."));
    dir.join(audio_filename)
}
