//! osu-beatmap-core: reusable beatmap logic shared by the CLI previewer
//! and the Android realtime (wgpu) renderer.
//!
//! Modules: `.osu` parsing, data models, mods & difficulty, game-mode
//! conversion, slider path geometry, time selection helpers, and the
//! smart beatmap downloader (mirror race + Cloudflare preferred IP).
//! No rendering or platform code lives here.

pub mod common;
pub mod core;
pub mod log;
pub mod parser;
pub mod pipeline;
