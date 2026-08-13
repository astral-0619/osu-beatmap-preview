//! 与 root 包 `src/pipeline` 同构的模块（只搬入了下载器部分）。
//!
//! 下载器代码自 root 原样迁入，逻辑未动；对外接口保持一致：
//! [`download_beatmap_file`] / [`resolve_beatmap_set_id`] /
//! [`download_beatmapset_archive`]。

pub mod downloader;

pub use downloader::{download_beatmap_file, download_beatmapset_archive, resolve_beatmap_set_id};
