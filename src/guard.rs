//! panic を窓の手続きの外へ出さない
//!
//! `extern "system"` の関数（窓の手続き・サブクラス・WinEvent）から panic が抜けると、Rust はプロセスごと止める
//! （`C0000409` / `FAST_FAIL_FATAL_APP_EXIT`）。本体が落ち、`aviutl2.ini` も保存されない。
//! そこで手続きの中身を `run` で包み、panic はその処理を飛ばすだけにする。
//! panic の文面と場所は、ログとは別に `Plugin/SplitLayout_H/panic.log` へその場で書く（落ちるとログの書き出しが失われるため）。

use std::io::Write as _;

fn panic_log_path() -> std::path::PathBuf {
    aviutl2::config::app_data_path().join("Plugin").join("SplitLayout_H").join("panic.log")
}

/// panic のときに文面・場所・スタックを書くフックを足す（この DLL の panic だけに効く）
pub fn install_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let bt = std::backtrace::Backtrace::force_capture();
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let text = format!("--- {secs} v{} {info}\n{bt}\n", env!("CARGO_PKG_VERSION"));
        let p = panic_log_path();
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
            let _ = f.write_all(text.as_bytes());
            let _ = f.flush();
        }
        prev(info);
    }));
}

/// f を呼び、panic したら None（その処理を飛ばす）
pub fn run<R>(name: &str, f: impl FnOnce() -> R) -> Option<R> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(r) => Some(r),
        Err(_) => {
            tracing::error!("{name} の中で panic した（処理を飛ばした。Plugin/SplitLayout_H/panic.log に場所がある）");
            None
        }
    }
}
