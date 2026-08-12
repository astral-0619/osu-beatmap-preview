//! Beatmap state and per-mode realtime drawing (wgpu backend).
//!
//! Reuses osu-beatmap-core for parsing, conversion, slider geometry and
//! timing. The original project's CPU `Img` pixel compositor is replaced by
//! flat-shaded wgpu triangles; positioning/timing formulas are ported from
//! the original render modules (see ARCHITECTURE.md).

use osu_beatmap_core::common::conv_catch::catch_convert;
use osu_beatmap_core::common::conv_mania::mania_convert;
use osu_beatmap_core::common::conv_taiko::taiko_convert;
use osu_beatmap_core::common::slider_path::{build_standard_slider_path, path_position_at};
use osu_beatmap_core::core::models::{
    CatchHitObject, HitObjects, ManiaHitObject, StandardHitObject, TaikoHitObject,
};
use osu_beatmap_core::parser::parse_beatmap;
use std::path::Path;

use crate::renderer::ShapeBatcher;

pub const PLAYFIELD_W: f32 = 512.0;
pub const PLAYFIELD_H: f32 = 384.0;
const OBJECT_RADIUS: f32 = 64.0;
const BROKEN_GAMEFIELD_ROUNDING_ALLOWANCE: f32 = 1.00041;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Standard,
    Taiko,
    Catch,
    Mania,
}

pub struct BeatmapState {
    pub title: String,
    pub artist: String,
    pub audio_filename: Option<String>,
    pub cs: f64,
    pub ar: f64,
    pub od: f64,
    pub combo_colors: Vec<[f32; 4]>,
    /// (time_ms, beat_length_ms) uninherited timing points
    pub timing: Vec<(i64, f64)>,
    pub standard: Vec<StandardHitObject>,
    pub taiko: Vec<TaikoHitObject>,
    pub catch: Vec<CatchHitObject>,
    pub mania: Vec<ManiaHitObject>,
}

const DEFAULT_COMBO_COLORS: [[f32; 4]; 4] = [
    [1.0, 0.752, 0.0, 1.0],   // orange
    [1.0, 0.0, 1.0, 1.0],     // magenta
    [0.0, 0.792, 1.0, 1.0],   // light blue
    [0.0, 1.0, 0.25, 1.0],    // green
];

pub fn load_beatmap_state(path: &Path) -> Result<BeatmapState, String> {
    let beatmap = parse_beatmap(path).map_err(|e| format!("parse: {e}"))?;
    let standard: Vec<StandardHitObject> = match &beatmap.hit_objects {
        HitObjects::Standard(v) => v.clone(),
        _ => Vec::new(),
    };

    let taiko = taiko_convert(&beatmap, 1, None)
        .ok()
        .and_then(|b| match b.hit_objects {
            HitObjects::Taiko(v) => Some(v),
            _ => None,
        })
        .unwrap_or_default();
    let catch = catch_convert(&beatmap, 2, None)
        .ok()
        .and_then(|b| match b.hit_objects {
            HitObjects::Catch(v) => Some(v),
            _ => None,
        })
        .unwrap_or_default();
    let mania = mania_convert(&beatmap, 3, None)
        .ok()
        .and_then(|b| match b.hit_objects {
            HitObjects::Mania(v) => Some(v),
            _ => None,
        })
        .unwrap_or_default();

    // timing: uninherited points only
    let mut timing: Vec<(i64, f64)> = beatmap
        .timing_points
        .iter()
        .filter(|tp| tp.uninherited && tp.beat_length > 0.0)
        .map(|tp| (tp.time as i64, tp.beat_length))
        .collect();
    timing.sort_by_key(|t| t.0);
    if timing.is_empty() {
        timing.push((0, 500.0));
    }

    let colors: Vec<[f32; 4]> = if beatmap.combo_colors.is_empty() {
        DEFAULT_COMBO_COLORS.to_vec()
    } else {
        beatmap
            .combo_colors
            .iter()
            .map(|&[r, g, b]| [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0])
            .collect()
    };

    Ok(BeatmapState {
        title: beatmap.metadata.get("Title").unwrap_or("").to_string(),
        artist: beatmap.metadata.get("Artist").unwrap_or("").to_string(),
        audio_filename: beatmap.audio_filename().map(str::to_string),
        cs: beatmap.difficulty.get_f64_or("CircleSize", 5.0),
        ar: beatmap
            .difficulty
            .get_f64("ApproachRate")
            .unwrap_or(beatmap.difficulty.get_f64_or("OverallDifficulty", 5.0)),
        od: beatmap.difficulty.get_f64_or("OverallDifficulty", 5.0),
        combo_colors: colors,
        timing,
        standard,
        taiko,
        catch,
        mania,
    })
}

/// Beat length (ms) in effect at `time_ms` (first uninherited point ≤ time).
fn beat_len_at(state: &BeatmapState, time_ms: i64) -> f64 {
    let mut bl = 500.0;
    for &(t, l) in &state.timing {
        if t <= time_ms {
            bl = l;
        } else {
            break;
        }
    }
    bl
}

// ---------------- per-frame dispatch ----------------
pub fn draw_frame(
    batcher: &mut ShapeBatcher,
    state: &BeatmapState,
    t_ms: i64,
    _size: (f32, f32),
    _paused: bool,
    speed: f64,
) {
    let mode = crate::current_mode();
    let t = t_ms;
    match mode {
        Mode::Standard => draw_standard(batcher, state, t, speed),
        Mode::Taiko => draw_taiko(batcher, state, t, speed),
        Mode::Catch => draw_catch(batcher, state, t, speed),
        Mode::Mania => draw_mania(batcher, state, t, speed),
    }
    let _ = t;
}

// ---------------- standard ----------------
fn ar_preempt(ar: f64) -> f64 {
    if ar > 5.0 {
        1200.0 + (450.0 - 1200.0) * ((ar - 5.0) / 5.0)
    } else {
        1200.0 + (1800.0 - 1200.0) * ((5.0 - ar) / 5.0)
    }
}

fn circle_radius(cs: f64) -> f32 {
    let scale = (1.0 - 0.7 * ((cs - 5.0) / 5.0)) / 2.0 * BROKEN_GAMEFIELD_ROUNDING_ALLOWANCE as f64;
    (OBJECT_RADIUS * scale as f32).max(4.0)
}

fn combo_color(state: &BeatmapState, combo_idx: usize) -> [f32; 4] {
    let n = state.combo_colors.len().max(1);
    state.combo_colors[combo_idx % n]
}

/// Build per-object combo color index following new_combo / combo_offset.
fn combo_indices(state: &BeatmapState) -> Vec<usize> {
    let mut idx = 0usize;
    let mut out = Vec::with_capacity(state.standard.len());
    for ho in &state.standard {
        if ho.new_combo {
            idx = (idx + 1 + ho.combo_offset.unsigned_abs() as usize) % 8;
        }
        out.push(idx);
    }
    out
}

fn draw_standard(batcher: &mut ShapeBatcher, state: &BeatmapState, t_ms: i64, speed: f64) {
    let preempt = ar_preempt(state.ar) / speed;
    let fade_in = (400.0 * (preempt / 450.0).min(1.0)).max(1.0);
    let r = circle_radius(state.cs);
    let combos = combo_indices(state);

    for (i, ho) in state.standard.iter().enumerate() {
        let visible_from = ho.start_time as f64 - preempt - fade_in;
        let visible_to = ho.end_time.max(ho.start_time) as f64 + 300.0 / speed;
        if (t_ms as f64) < visible_from || (t_ms as f64) > visible_to {
            continue;
        }
        let alpha = if (t_ms as f64) < ho.start_time as f64 {
            ((t_ms as f64 - (ho.start_time as f64 - preempt)) / fade_in).clamp(0.0, 1.0)
        } else {
            (1.0 - (t_ms as f64 - ho.end_time.max(ho.start_time) as f64) / (300.0 / speed))
                .clamp(0.0, 1.0)
        };
        if alpha <= 0.01 {
            continue;
        }
        let color = combo_color(state, combos[i]);

        if ho.hit_type & 8 != 0 {
            draw_spinner(batcher, ho, t_ms, alpha as f32);
            continue;
        }
        if ho.hit_type & 2 != 0 {
            draw_slider(batcher, ho, color, t_ms, alpha as f32, r);
        } else {
            // hit circle: white fill + combo-color border ring
            batcher.circle(ho.x as f32, ho.y as f32, r, [1.0, 1.0, 1.0, alpha as f32], 28);
            batcher.ring(ho.x as f32, ho.y as f32, r * 0.82, r * 0.18, mulc(color, alpha as f32), 28);
        }
        // approach circle
        if (t_ms as f64) < ho.start_time as f64 {
            let progress = (ho.start_time as f64 - t_ms as f64) / preempt;
            let ar_r = r * (1.0 + 2.0 * progress.clamp(0.0, 1.0)) as f32;
            let aa = (0.7 * alpha).min(0.7);
            batcher.ring(ho.x as f32, ho.y as f32, ar_r, 2.5, [1.0, 1.0, 1.0, aa as f32], 36);
        }
    }
}

fn draw_slider(
    batcher: &mut ShapeBatcher,
    ho: &StandardHitObject,
    color: [f32; 4],
    t_ms: i64,
    alpha: f32,
    r: f32,
) {
    let points: Vec<(i32, i32)> = ho.slider_points.clone();
    if points.is_empty() {
        return;
    }
    let slider_type = ho.slider_type.as_deref().unwrap_or("B");
    let path = build_standard_slider_path(ho.x, ho.y, &points, slider_type, ho.slider_pixel_length);
    if path.points.len() < 2 {
        return;
    }
    let pts: Vec<(f32, f32)> = path.points.iter().map(|&(x, y)| (x as f32, y as f32)).collect();
    // body: white underlay + color core
    batcher.polyline(&pts, r * 1.1, [1.0, 1.0, 1.0, alpha * 0.9]);
    batcher.polyline(&pts, r * 0.6, mulc(color, alpha));

    // slider ball
    if t_ms >= ho.start_time && t_ms <= ho.end_time {
        let progress = (t_ms - ho.start_time) as f64 / (ho.end_time - ho.start_time).max(1) as f64;
        let (bx, by) = path_position_at(&path, progress.clamp(0.0, 1.0));
        batcher.circle(bx as f32, by as f32, r * 0.6, [1.0, 1.0, 1.0, alpha], 20);
        batcher.ring(bx as f32, by as f32, r * 0.6, r * 0.18, mulc(color, alpha), 20);
    }
}

fn draw_spinner(batcher: &mut ShapeBatcher, ho: &StandardHitObject, t_ms: i64, alpha: f32) {
    let dur = (ho.end_time - ho.start_time).max(1) as f32;
    let progress = ((t_ms - ho.start_time) as f32 / dur).clamp(0.0, 1.0);
    let cx = PLAYFIELD_W / 2.0;
    let cy = PLAYFIELD_H / 2.0;
    let base = 96.0;
    batcher.ring(cx, cy, base * (1.0 - progress) + 8.0, 14.0, [1.0, 1.0, 1.0, alpha * 0.9], 48);
    // rotating tick marks
    let angle = progress * std::f32::consts::TAU * 6.0;
    for k in 0..6 {
        let a = angle + k as f32 * std::f32::consts::TAU / 6.0;
        let (s, c) = a.sin_cos();
        let x0 = cx + c * base * 0.4;
        let y0 = cy + s * base * 0.4;
        let x1 = cx + c * base * 0.8;
        let y1 = cy + s * base * 0.8;
        batcher.polyline(&[(x0, y0), (x1, y1)], 3.0, [1.0, 1.0, 1.0, alpha * 0.7]);
    }
}

// ---------------- taiko ----------------
const TAIKO_HIT_X: f32 = 150.0;
const TAIKO_LANE_Y: f32 = 192.0;

fn draw_taiko(batcher: &mut ShapeBatcher, state: &BeatmapState, t_ms: i64, speed: f64) {
    let px_per_ms = 150.0_f64 / beat_len_at(state, t_ms).max(200.0) * speed;
    // judgement line
    batcher.circle(TAIKO_HIT_X, TAIKO_LANE_Y, 44.0, [0.25, 0.25, 0.3, 1.0], 32);
    batcher.ring(TAIKO_HIT_X, TAIKO_LANE_Y, 44.0, 5.0, [0.5, 0.5, 0.55, 1.0], 32);
    for ho in &state.taiko {
        let dt = ho.start_time - t_ms;
        let x = TAIKO_HIT_X + (dt as f64 * px_per_ms) as f32;
        if x < -80.0 || x > PLAYFIELD_W + 80.0 {
            continue;
        }
        let alpha = if x > PLAYFIELD_W { 1.0 - (x - PLAYFIELD_W) / 80.0 } else { 1.0 };
        let is_kat = ho.hitsound & (2 | 8) != 0; // whistle/clap -> rim
        let big = false; // v1: big notes not encoded in converted output
        let r = if big { 36.0 } else { 26.0 };
        if ho.hit_type == 8 {
            // swell/drumroll: small yellow ticks along remaining time
            let end_x = TAIKO_HIT_X + ((ho.end_time - t_ms) as f64 * px_per_ms) as f32;
            let steps = ((end_x - x).abs() / 24.0).clamp(2.0, 60.0) as usize;
            for i in 0..steps {
                let f = i as f32 / steps as f32;
                let tx = x + (end_x - x) * f;
                batcher.circle(tx, TAIKO_LANE_Y, 8.0, [0.91, 0.78, 0.24, alpha], 14);
            }
        } else if is_kat {
            batcher.circle(x, TAIKO_LANE_Y, r, [0.26, 0.56, 0.67, alpha], 24);
            batcher.ring(x, TAIKO_LANE_Y, r, 5.0, [1.0, 1.0, 1.0, alpha * 0.8], 24);
        } else {
            batcher.circle(x, TAIKO_LANE_Y, r, [0.92, 0.27, 0.17, alpha], 24);
            batcher.ring(x, TAIKO_LANE_Y, r * 0.72, r * 0.28, [1.0, 1.0, 1.0, alpha * 0.55], 24);
        }
    }
}

// ---------------- mania ----------------
const MANIA_HIT_Y: f32 = 344.0;
const MANIA_PX_PER_BEAT: f64 = 300.0;

fn draw_mania(batcher: &mut ShapeBatcher, state: &BeatmapState, t_ms: i64, speed: f64) {
    let lanes = state.cs.round().clamp(1.0, 10.0) as usize;
    let col_w = PLAYFIELD_W / lanes as f32;
    let px_per_ms = MANIA_PX_PER_BEAT / beat_len_at(state, t_ms).max(200.0) * speed;
    // keys
    for l in 0..lanes {
        let x = l as f32 * col_w;
        batcher.rect(x, 0.0, col_w, PLAYFIELD_H, [0.09, 0.09, 0.12, 1.0]);
        batcher.rect(x, MANIA_HIT_Y, col_w, PLAYFIELD_H - MANIA_HIT_Y, [0.13, 0.13, 0.17, 1.0]);
        batcher.rect(x + 1.0, MANIA_HIT_Y - 6.0, col_w - 2.0, 6.0, [1.0, 1.0, 1.0, 0.85]);
    }
    for ho in &state.mania {
        let dt = ho.start_time - t_ms;
        let y = MANIA_HIT_Y - (dt as f64 * px_per_ms) as f32;
        let x = ho.lane as f32 * col_w;
        let h = 18.0;
        if y < -h || y - h > PLAYFIELD_H {
            continue;
        }
        let color = [0.95, 0.95, 1.0, 1.0];
        batcher.rect(x + 1.0, y, col_w - 2.0, h, color);
        if ho.is_long_note {
            let tail = MANIA_HIT_Y - ((ho.end_time - t_ms) as f64 * px_per_ms) as f32;
            let top = tail.max(y);
            let bottom = tail.min(y);
            batcher.rect(x + col_w * 0.3, top - h, col_w * 0.4, (bottom - top).max(0.0) + h, [0.8, 0.85, 1.0, 0.5]);
            batcher.rect(x + 1.0, tail, col_w - 2.0, h, [0.7, 0.75, 1.0, 1.0]);
        }
    }
}

// ---------------- catch ----------------
const CATCH_FALL_MS: f64 = 900.0;
const CATCHER_Y: f32 = 330.0;

fn draw_catch(batcher: &mut ShapeBatcher, state: &BeatmapState, t_ms: i64, speed: f64) {
    let fall = (CATCHER_Y as f64 + 60.0) / (CATCH_FALL_MS / speed);
    let mut catcher_x: Option<f32> = None;
    for ho in &state.catch {
        let dt = ho.start_time - t_ms;
        if dt > (CATCH_FALL_MS / speed) as i64 || dt < -300 {
            continue;
        }
        let y = (CATCHER_Y as f64 - dt as f64 * fall) as f32;
        let x = ho.x as f32;
        if dt <= 0 {
            catcher_x = Some(x);
        }
        batcher.circle(x, y.clamp(-40.0, CATCHER_Y), 22.0, [0.98, 0.9, 0.4, 1.0], 20);
        batcher.ring(x, y.clamp(-40.0, CATCHER_Y), 22.0, 3.0, [1.0, 1.0, 1.0, 0.9], 20);
    }
    if let Some(cx) = catcher_x {
        let w = 76.0;
        batcher.rect(cx - w / 2.0, CATCHER_Y - 8.0, w, 16.0, [0.75, 0.75, 1.0, 1.0]);
        batcher.rect(cx - w / 2.0, CATCHER_Y - 16.0, 10.0, 24.0, [0.9, 0.9, 1.0, 1.0]);
        batcher.rect(cx + w / 2.0 - 10.0, CATCHER_Y - 16.0, 10.0, 24.0, [0.9, 0.9, 1.0, 1.0]);
    }
}

fn mulc(c: [f32; 4], a: f32) -> [f32; 4] {
    [c[0] * a, c[1] * a, c[2] * a, c[3] * a]
}
