//! 智能下载器已迁入 `osu-beatmap-core::pipeline::downloader`（逻辑原样保留，
//! 与 Android 侧共用）。本模块仅做再导出，保证 root 包调用点不变。
pub use osu_beatmap_core::pipeline::downloader::{download_beatmap_file, resolve_beatmap_set_id};
pub use osu_beatmap_core::pipeline::{download_beatmapset_archive};
