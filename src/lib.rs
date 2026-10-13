//! SplitLayout_H（分割レイアウト）
//!
//! 本体で「ウィンドウを分離」したパネル（クラス `aviutl2Manager` の窓）を、この枠の子にはめ込み、
//! 縦横に自由に分割した区画へ並べる。タブ・区画の最大化・名前を付けた配置の切り替え・本体の外の別窓を持つ。
//! 仕様は `AI/specifications/20261010_SplitLayout_H_spec.md`。前提は `.claude/rules/au2-plugin-checklist.md`「本体の窓の構成」。
//!
//! 動くのは本体の UI スレッドだけ。自分ではスレッドを起こさない。
//! 本体の窓・メイン窓をサブクラス化し、WinEvent のフックを掛けるので、`Drop` で全部外す。
//! 編集の API（`call_edit_section` 等）は使わない。

mod app;
mod drag;
mod frame;
mod check;
mod guard;
mod host;
pub mod ident;
mod menu;
pub mod model;
#[cfg(test)]
mod model_fuzz;
#[cfg(test)]
mod win_harness;
pub mod store;

use std::sync::atomic::Ordering;

use aviutl2::generic::GlobalEditHandle;
use aviutl2::AnyResult;
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{DestroyWindow, IsWindow, PostMessageW};

use crate::app::{App, Frame, Mouse, MAIN_FRAME};
use crate::frame::FrameKind;
use crate::host::{h, raw};
use crate::model::Layout;

const NAME: &str = "分割レイアウト";
/// 編集メニューの「配置 N に切り替え」の数
const SLOTS: usize = 5;

pub(crate) static EDIT_HANDLE: GlobalEditHandle = GlobalEditHandle::new();

struct BorrowedHwnd(isize);

impl aviutl2::raw_window_handle::HasWindowHandle for BorrowedHwnd {
    fn window_handle(&self) -> Result<aviutl2::raw_window_handle::WindowHandle<'_>, aviutl2::raw_window_handle::HandleError> {
        let nz = std::num::NonZeroIsize::new(self.0).ok_or(aviutl2::raw_window_handle::HandleError::Unavailable)?;
        let raw = aviutl2::raw_window_handle::RawWindowHandle::Win32(aviutl2::raw_window_handle::Win32WindowHandle::new(nz));
        Ok(unsafe { aviutl2::raw_window_handle::WindowHandle::borrow_raw(raw) })
    }
}

fn post_to_main_frame(msg: u32, wp: usize) {
    let f = MAIN_FRAME.load(Ordering::Acquire);
    if f != 0 {
        unsafe {
            let _ = PostMessageW(Some(h(f)), msg, WPARAM(wp), LPARAM(0));
        }
    }
}

/// 初期化にかかった時間をログに 1 行出す（強化デバッグ。仕様 `AI/specifications/20261012_CurveEditor_H_キーフレーム速度_spec.md` の 7.0）。
/// `register` の始めに作り、`register` を抜けるとき（途中の `return` も含む）に new と register の時間を出す
struct InitTimeLog {
    new_time: std::time::Duration,
    register_start: std::time::Instant,
}

impl InitTimeLog {
    fn start(new_time: std::time::Duration) -> Self {
        Self { new_time, register_start: std::time::Instant::now() }
    }
}

impl Drop for InitTimeLog {
    fn drop(&mut self) {
        let register = self.register_start.elapsed();
        let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
        aviutl2::tracing::info!(
            "SplitLayout_H: 初期化 {:.1} ms（new {:.1} ms・register {:.1} ms）",
            ms(self.new_time + register),
            ms(self.new_time),
            ms(register)
        );
    }
}

#[aviutl2::plugin(GenericPlugin)]
struct SplitLayout {
    /// `new` にかかった時間（初期化の時間をログに出すため。`InitTimeLog`）
    init_new_time: std::time::Duration,
}

impl aviutl2::generic::GenericPlugin for SplitLayout {
    fn new(_info: aviutl2::AviUtl2Info) -> AnyResult<Self> {
        let init_t0 = std::time::Instant::now();
        guard::install_hook();
        let level = if cfg!(debug_assertions) { aviutl2::tracing::Level::DEBUG } else { aviutl2::tracing::Level::INFO };
        let _ = aviutl2::tracing_subscriber::fmt()
            .with_max_level(level)
            .event_format(aviutl2::logger::AviUtl2Formatter)
            .with_writer(aviutl2::logger::AviUtl2LogWriter)
            .try_init();
        Ok(Self { init_new_time: init_t0.elapsed() })
    }

    fn plugin_info(&self) -> aviutl2::generic::GenericPluginTable {
        aviutl2::generic::GenericPluginTable {
            name: NAME.to_string(),
            information: format!("{NAME} v{} by HexBrowns", env!("CARGO_PKG_VERSION")),
        }
    }

    fn on_project_load(&mut self, _project: &mut aviutl2::generic::ProjectFile) {
        // シーン名が変わる（シーンのプレビューの見分けに使う）ので見回り直す
        app::post_scan();
    }

    fn register(&mut self, registry: &mut aviutl2::generic::HostAppHandle) {
        // 抜けるときに初期化の時間をログに出す（`InitTimeLog`）
        let _init_time = InitTimeLog::start(self.init_new_time);
        EDIT_HANDLE.init(registry.create_edit_handle());
        let Some(main) = EDIT_HANDLE.get_host_app_window_raw().map(|w| w.hwnd.get()) else {
            tracing::error!("本体のメイン窓が取れないので、分割レイアウトを使えない");
            return;
        };
        let Some(frame) = frame::create_main(NAME) else {
            tracing::error!("枠の窓を作れなかった");
            return;
        };
        MAIN_FRAME.store(raw(frame), Ordering::Release);
        if let Err(e) = registry.register_window_client(NAME, &BorrowedHwnd(raw(frame))) {
            tracing::error!("枠を登録できなかった: {e}");
        }

        let loaded = store::load(&store::path());
        if let Some(m) = &loaded.message {
            tracing::warn!("{m}");
        }
        let root = loaded.store.current.main.clone();
        app::install(App {
            main_window: main,
            store: loaded.store,
            writable: loaded.writable,
            frames: vec![Frame {
                hwnd: raw(frame),
                kind: FrameKind::Main,
                root,
                maximized: None,
                layout: Layout::default(),
                tabs: Vec::new(),
            }],
            embedded: Default::default(),
            seen: Default::default(),
            mouse: Mouse::None,
            suspended: false,
        });
        if !host::hook_main(h(main)) {
            tracing::warn!("メイン窓を見張れない。終了の前に分離の窓へ戻せないので、配置を自分で戻してから終了してください");
        }
        host::install_win_event();
        // 別窓の復元と見回りは、本体の窓ができてから（メッセージのループが回り始めてから）
        post_to_main_frame(app::WM_APP_OPEN_FLOAT, 0);
        app::post_scan();
        // 起動時に隠した窓を、はめ込めないまま隠し続けないように
        unsafe {
            windows::Win32::UI::WindowsAndMessaging::SetTimer(Some(frame), app::TIMER_UNCLOAK, app::UNCLOAK_AFTER_MS, None);
        }

        for i in 0..SLOTS {
            registry.register_edit_menu(&format!("{NAME}\\配置 {} に切り替え", i + 1), move || {
                post_to_main_frame(app::WM_APP_SWITCH, i)
            });
        }
        registry.register_edit_menu(&format!("{NAME}\\別窓を開く"), || post_to_main_frame(app::WM_APP_OPEN_FLOAT, 1));

        if unsafe { GetModuleHandleW(windows::core::w!("DockLayout.aux2")) }.is_ok() {
            tracing::warn!("ドッキング配置（DockLayout）が入っている。どちらも本体の窓のドラッグを受け持つので、同時には使わないでください");
        }
        tracing::info!("{NAME} v{} を読み込んだ", env!("CARGO_PKG_VERSION"));
    }
}

impl Drop for SplitLayout {
    fn drop(&mut self) {
        // 本体の窓を元に戻してから、こちらの窓とフックを片付ける（DLL が外れた後に手続きが呼ばれないように）
        app::release_all(false);
        let (main, floats) = app::with_app(|a| {
            let floats: Vec<isize> = a.frames.iter().filter(|f| f.kind == FrameKind::Float).map(|f| f.hwnd).collect();
            a.frames.retain(|f| f.kind == FrameKind::Main);
            (a.main_window, floats)
        })
        .unwrap_or((0, Vec::new()));
        unsafe {
            for f in floats {
                if IsWindow(Some(h(f))).as_bool() {
                    let _ = DestroyWindow(h(f));
                }
            }
        }
        host::uninstall((main != 0).then(|| h(main)));
        drag::destroy_overlay();
        let frame = MAIN_FRAME.swap(0, Ordering::AcqRel);
        unsafe {
            if frame != 0 && IsWindow(Some(h(frame))).as_bool() {
                let _ = DestroyWindow(h(frame));
            }
        }
        frame::unregister_class();
        frame::delete_fonts();
        let _ = app::take();
    }
}

aviutl2::register_generic_plugin!(SplitLayout);
