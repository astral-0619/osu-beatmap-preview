//! 下载器日志桥（facade）。
//!
//! `downloader/` 里的代码原本调用 root 包的 `crate::log::*` 系列函数；
//! 移入 beatmap-core 后，`crate::log` 就绑定到这里。由宿主（CLI 的
//! `src/log` 或 Android 的 JNI 层）各自 `install` 一套 hook，下载器本体
//! 一行不改。未 install 时全部静默。

use std::sync::OnceLock;

/// 与 root 包 `src/log/context.rs` 的 CacheKind 保持一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheKind {
    Osu,
    Osz,
    Audio,
    Output,
}

type EventHook = fn(&str, &str, Option<&str>, &str);
type CacheHook = fn(CacheKind, &str);
type StageHook = fn(&str, f64);
type StageStatusHook = fn(&str, &str);

struct Hooks {
    event: EventHook,
    cache: CacheHook,
    stage: StageHook,
    stage_status: StageStatusHook,
}

static HOOKS: OnceLock<Hooks> = OnceLock::new();

/// 安装日志桥（每个进程一次，重复调用以第一次为准）。
pub fn install(event: EventHook, cache: CacheHook, stage: StageHook, stage_status: StageStatusHook) {
    let _ = HOOKS.set(Hooks {
        event,
        cache,
        stage,
        stage_status,
    });
}

pub fn event(step: &str, status: &str, bid: Option<&str>, msg: &str) {
    if let Some(h) = HOOKS.get() {
        (h.event)(step, status, bid, msg);
    }
}

pub fn record_cache(kind: CacheKind, state: &str) {
    if let Some(h) = HOOKS.get() {
        (h.cache)(kind, state);
    }
}

pub fn record_stage(name: &str, ms: f64) {
    if let Some(h) = HOOKS.get() {
        (h.stage)(name, ms);
    }
}

pub fn record_stage_status(name: &str, status: &str) {
    if let Some(h) = HOOKS.get() {
        (h.stage_status)(name, status);
    }
}
