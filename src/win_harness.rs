//! 窓の結合テスト（cargo test。本体なしで、本体の偽物の窓に対して動かす）
//!
//! 本体と同じクラス名 `aviutl2Manager` のメイン窓と、分離したパネルの窓（所有者がメイン窓の最上位の窓）を作り、
//! プラグインの登録と同じ手順で枠を組み立てる。そこへ実際の窓のメッセージ（クリック・ドラッグ・本体の移動や表示の切り替え・
//! 閉じる）を流し、画面の遷移が正しいかを、自己点検（`check::violations`）と場面ごとの期待値で確かめる。
//!
//! 窓は画面の外（-20000）に作り、前面に出さない。テストの間も、操作中の画面には何も出ない。
//! 窓はスレッドに属するので、場面は 1 つのテストの中で順に回す（`win_harness_all`）。

use std::cell::Cell;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{ClientToScreen, ScreenToClient};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::GetFocus;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::app::{self, with_app, App, Frame, Mouse, MAIN_FRAME};
use crate::check;
use crate::drag;
use crate::frame::{self, FrameKind};
use crate::host::{self, h, raw};
use crate::model::{self, Dock, Layout, Node, Rect, Target};
use crate::store::Store;

const ORIGIN: i32 = -20000;

thread_local! {
    /// 偽物のメイン窓が WM_CLOSE で閉じるか（偽なら閉じるのを取り消した扱い）
    static CLOSE_DESTROYS: Cell<bool> = const { Cell::new(false) };
    /// 窓ごとの、位置か大きさが変わった回数（本体のパネルはそのたびに描き直すので、少ないほどよい）
    static MOVED: std::cell::RefCell<std::collections::HashMap<isize, u32>> = std::cell::RefCell::new(Default::default());
}

fn moved(hwnd: HWND) -> u32 {
    MOVED.with(|m| m.borrow().get(&raw(hwnd)).copied().unwrap_or(0))
}

unsafe extern "system" fn fake_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_CLOSE && !CLOSE_DESTROYS.with(|c| c.get()) {
        return LRESULT(0);
    }
    if msg == WM_WINDOWPOSCHANGED {
        let pos = &*(lp.0 as *const WINDOWPOS);
        if !pos.flags.contains(SWP_NOMOVE) || !pos.flags.contains(SWP_NOSIZE) {
            MOVED.with(|m| *m.borrow_mut().entry(raw(hwnd)).or_insert(0) += 1);
        }
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn register_fake_class() {
    unsafe {
        let instance = GetModuleHandleW(None).unwrap();
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(fake_proc),
            hInstance: instance.into(),
            lpszClassName: w!("aviutl2Manager"),
            ..Default::default()
        };
        RegisterClassExW(&wc);
    }
}

fn pump() {
    unsafe {
        let mut msg = MSG::default();
        for _ in 0..100 {
            let mut any = false;
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                any = true;
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            if !any {
                break;
            }
        }
    }
}

fn pump_for(ms: u64) {
    let until = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < until {
        pump();
        std::thread::sleep(Duration::from_millis(10));
    }
    pump();
}

fn lp(x: i32, y: i32) -> LPARAM {
    LPARAM((((y as i16 as u16 as u32) << 16) | (x as i16 as u16 as u32)) as isize)
}

fn send(hwnd: HWND, msg: u32, wp: usize, l: LPARAM) {
    unsafe {
        SendMessageW(hwnd, msg, Some(WPARAM(wp)), Some(l));
    }
}

fn assert_ok(ctx: &str) {
    pump();
    let v = check::violations();
    assert!(v.is_empty(), "{ctx}: 自己点検で崩れ {v:#?}");
}

struct Harness {
    main: HWND,
    frame: HWND,
    panels: Vec<(String, HWND)>,
}

impl Harness {
    /// 偽物の本体を組み立てる。`root` は主の枠の配置、`panels` は分離したパネル（題, プラグインの窓か）
    fn new(root: Node, store: Option<Store>, panels: &[(&str, bool)]) -> Harness {
        register_fake_class();
        unsafe {
            let instance = GetModuleHandleW(None).unwrap();
            let main = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("aviutl2Manager"),
                w!("偽物の本体"),
                WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
                ORIGIN,
                ORIGIN,
                1600,
                900,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .unwrap();
            let _ = ShowWindow(main, SW_SHOWNA);
            // 枠（本体の register_window_client と同じく、WS_CHILD を付けてメイン窓の子にする）
            let frame = frame::create_main("分割レイアウト").unwrap();
            MAIN_FRAME.store(raw(frame), Ordering::Release);
            let st = GetWindowLongPtrW(frame, GWL_STYLE);
            SetWindowLongPtrW(frame, GWL_STYLE, (st & !(WS_POPUP.0 as isize)) | WS_CHILD.0 as isize);
            let _ = SetParent(frame, Some(main));
            let mut store = store.unwrap_or_default();
            store.current.main = root.clone();
            app::install(App {
                main_window: raw(main),
                store,
                writable: false,
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
            assert!(host::hook_main(main));
            host::install_win_event();
            let _ = SetWindowPos(frame, None, 0, 0, 1580, 860, SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW);
            let mut hz = Harness { main, frame, panels: Vec::new() };
            for (i, (t, plugin)) in panels.iter().enumerate() {
                hz.add_panel(t, *plugin, i as i32);
            }
            pump();
            hz
        }
    }

    fn add_panel(&mut self, title: &str, plugin: bool, i: i32) -> HWND {
        unsafe {
            let instance = GetModuleHandleW(None).unwrap();
            let t = wide(title);
            let p = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("aviutl2Manager"),
                PCWSTR(t.as_ptr()),
                WS_OVERLAPPEDWINDOW,
                ORIGIN + 100 + i * 30,
                ORIGIN + 100 + i * 20,
                400 + i,
                300 + i,
                Some(self.main),
                None,
                Some(instance.into()),
                None,
            )
            .unwrap();
            if plugin {
                let _ = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("STATIC"),
                    w!("プラグインの中身"),
                    WS_CHILD | WS_VISIBLE,
                    0,
                    0,
                    100,
                    100,
                    Some(p),
                    None,
                    Some(instance.into()),
                    None,
                );
            }
            let _ = ShowWindow(p, SW_SHOWNA);
            self.panels.push((title.to_string(), p));
            p
        }
    }

    /// プラグインの窓（中の窓の題を `inner` にする。手掛かりは「Static|<inner>」）
    fn add_plugin(&mut self, title: &str, inner: &str, i: i32) -> HWND {
        unsafe {
            let instance = GetModuleHandleW(None).unwrap();
            let t = wide(title);
            let p = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("aviutl2Manager"),
                PCWSTR(t.as_ptr()),
                WS_OVERLAPPEDWINDOW,
                ORIGIN + 100 + i * 30,
                ORIGIN + 100 + i * 20,
                400 + i,
                300 + i,
                Some(self.main),
                None,
                Some(instance.into()),
                None,
            )
            .unwrap();
            let inner = wide(inner);
            let _ = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                PCWSTR(inner.as_ptr()),
                WS_CHILD | WS_VISIBLE,
                0,
                0,
                100,
                100,
                Some(p),
                None,
                Some(instance.into()),
                None,
            );
            let _ = ShowWindow(p, SW_SHOWNA);
            self.panels.push((title.to_string(), p));
            p
        }
    }

    fn panel(&self, title: &str) -> HWND {
        self.panels.iter().find(|p| p.0 == title).map(|p| p.1).unwrap_or_else(|| panic!("パネル {title} が無い"))
    }

    fn tab_rect(&self, key: &str) -> Rect {
        with_app(|a| a.frames.iter().flat_map(|f| f.tabs.iter()).find(|t| t.key == key).map(|t| t.rect))
            .flatten()
            .unwrap_or_else(|| panic!("タブ {key} が無い"))
    }

    fn pane_body(&self, key: &str) -> Rect {
        with_app(|a| {
            let f = a.frame(raw(self.frame))?;
            let id = f.root.pane_of(key)?;
            f.layout.pane_box(id).map(|p| p.body)
        })
        .flatten()
        .unwrap_or_else(|| panic!("区画 {key} が無い"))
    }

    fn root(&self) -> Node {
        with_app(|a| a.frame(raw(self.frame)).map(|f| f.root.clone())).flatten().unwrap()
    }

    fn is_embedded(&self, hwnd: HWND) -> bool {
        with_app(|a| a.embedded.contains_key(&raw(hwnd))).unwrap_or(false)
    }

    fn active_of(&self, key: &str) -> Option<String> {
        let root = self.root();
        let id = root.pane_of(key)?;
        root.pane(id)?.active_title().map(|s| s.to_string())
    }

    /// 枠のクライアント座標でドラッグする（押す → 動かす → 離す）
    fn drag(&self, from: (i32, i32), to: (i32, i32)) {
        let f = self.frame;
        send(f, WM_LBUTTONDOWN, 1, lp(from.0, from.1));
        for k in 1..=6 {
            let x = from.0 + (to.0 - from.0) * k / 6;
            let y = from.1 + (to.1 - from.1) * k / 6;
            send(f, WM_MOUSEMOVE, 1, lp(x, y));
        }
        send(f, WM_LBUTTONUP, 0, lp(to.0, to.1));
        pump();
    }

    fn click(&self, at: (i32, i32)) {
        send(self.frame, WM_LBUTTONDOWN, 1, lp(at.0, at.1));
        send(self.frame, WM_LBUTTONUP, 0, lp(at.0, at.1));
        pump();
    }

    /// 区画の中央のガイド（dock）の中心（枠のクライアント座標）
    fn guide(&self, key: &str, dock: Dock) -> (i32, i32) {
        let m = frame::metrics(self.frame);
        let (_, r) = model::pane_guides(self.pane_body(key), m.guide).into_iter().find(|g| g.0 == dock).unwrap();
        r.center()
    }

    /// 窓の位置（枠のクライアント座標）
    fn rect_in_frame(&self, hwnd: HWND) -> (i32, i32, i32, i32) {
        unsafe {
            let mut r = RECT::default();
            let _ = GetWindowRect(hwnd, &mut r);
            let mut tl = POINT { x: r.left, y: r.top };
            let _ = ScreenToClient(self.frame, &mut tl);
            (tl.x, tl.y, r.right - r.left, r.bottom - r.top)
        }
    }

    fn visible(&self, hwnd: HWND) -> bool {
        unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) as u32 & WS_VISIBLE.0 != 0 }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        unsafe {
            app::release_all(false);
            let floats: Vec<isize> = with_app(|a| a.frames.iter().filter(|f| f.kind == FrameKind::Float).map(|f| f.hwnd).collect()).unwrap_or_default();
            host::uninstall(Some(self.main));
            drag::destroy_overlay();
            for f in floats {
                let _ = DestroyWindow(h(f));
            }
            let _ = DestroyWindow(self.frame);
            for (_, p) in &self.panels {
                if IsWindow(Some(*p)).as_bool() {
                    let _ = DestroyWindow(*p);
                }
            }
            CLOSE_DESTROYS.with(|c| c.set(true));
            let _ = DestroyWindow(self.main);
            CLOSE_DESTROYS.with(|c| c.set(false));
            let _ = app::take();
            MAIN_FRAME.store(0, Ordering::Release);
            pump();
        }
    }
}

/// [A | B,C] / [D,E]（上の左右と下）。F はどこにも無い
fn base_layout() -> Node {
    let mut n = Node::default();
    n.insert(Target::Root, "A", Dock::Center);
    let a = n.pane_of("A").unwrap();
    n.insert(Target::Pane(a), "B", Dock::Right);
    let b = n.pane_of("B").unwrap();
    n.insert(Target::Pane(b), "C", Dock::Center);
    n.set_active(b, 0);
    n.insert(Target::Root, "D", Dock::Bottom);
    let d = n.pane_of("D").unwrap();
    n.insert(Target::Pane(d), "E", Dock::Center);
    n.set_active(d, 0);
    n
}

const BASE: [(&str, bool); 6] = [("A", false), ("B", false), ("C", true), ("D", false), ("E", true), ("F", false)];

fn s_embed_and_tabs() {
    let hz = Harness::new(base_layout(), None, &BASE);
    for t in ["A", "B", "C", "D", "E"] {
        assert!(hz.is_embedded(hz.panel(t)), "{t} がはめ込まれていない");
    }
    assert!(!hz.is_embedded(hz.panel("F")), "配置に無い F がはめ込まれた");
    assert!(unsafe { GetParent(hz.panel("F")) }.is_err() || unsafe { GetParent(hz.panel("F")) }.unwrap() != hz.frame);
    assert!(hz.visible(hz.panel("B")) && !hz.visible(hz.panel("C")), "前のタブ B と裏のタブ C");
    assert_eq!(hz.rect_in_frame(hz.panel("A")), {
        let b = hz.pane_body("A");
        (b.l, b.t, b.w(), b.h())
    });
    assert_ok("はめ込んだ直後");

    // タブのクリックで切り替わり、焦点が移る。関係の無い区画の窓（A・D・E）は動かさない
    // 裏に回る B も動かさない（隠すだけ。動かすと本体のパネルは描き直す）
    let before: Vec<u32> = ["A", "B", "D", "E"].iter().map(|t| moved(hz.panel(t))).collect();
    let c = hz.tab_rect("C").center();
    hz.click(c);
    let after: Vec<u32> = ["A", "B", "D", "E"].iter().map(|t| moved(hz.panel(t))).collect();
    assert_eq!(before, after, "タブの切り替えで、関係の無い窓か裏に回る窓が動いた");
    assert_eq!(hz.active_of("C").as_deref(), Some("C"));
    assert!(hz.visible(hz.panel("C")) && !hz.visible(hz.panel("B")));
    assert_eq!(unsafe { GetFocus() }, hz.panel("C"), "焦点が C に移っていない");
    assert_ok("タブのクリック");

    // 見出しの中で並べ替える（B を C の右へ）
    let b = hz.tab_rect("B").center();
    let c = hz.tab_rect("C");
    hz.drag(b, (c.r - 2, c.center().1));
    let root = hz.root();
    let pane = root.pane(root.pane_of("B").unwrap()).unwrap();
    assert_eq!(pane.tabs, vec!["C", "B"], "並べ替え");
    assert_ok("並べ替え");
}

fn s_drag_dock_and_out() {
    let hz = Harness::new(base_layout(), None, &BASE);
    // D を A の右のガイドへ: A の区画が左右に分かれる
    let d = hz.tab_rect("D").center();
    hz.drag(d, hz.guide("A", Dock::Right));
    let root = hz.root();
    assert_ne!(root.pane_of("D"), root.pane_of("E"), "D が E の区画から出ていない");
    assert_ne!(root.pane_of("D"), root.pane_of("A"));
    assert!(hz.pane_body("D").l > hz.pane_body("A").l, "D が A の右に無い");
    assert_ok("区画の右へ落とす");

    // E を枠の外で離す: 本体の分離の窓に戻る
    let e = hz.tab_rect("E").center();
    hz.drag(e, (-3000, -3000));
    let p = hz.panel("E");
    assert!(!hz.is_embedded(p), "E が外れていない");
    assert!(!hz.root().contains("E"));
    unsafe {
        assert!(GetParent(p).is_err() || GetParent(p).unwrap().0.is_null() || GetParent(p).unwrap() == hz.main, "E の親");
        assert_eq!(GetWindow(p, GW_OWNER).ok(), Some(hz.main), "E の所有者がメイン窓に戻っていない");
        let st = GetWindowLongPtrW(p, GWL_STYLE) as u32;
        assert!(st & WS_CHILD.0 == 0 && st & WS_CAPTION.0 == WS_CAPTION.0, "E のスタイルが戻っていない");
        assert!(hz.visible(p) && !host::is_cloaked(p));
    }
    assert_ok("枠の外へ出す");

    // 外した E を、メニューの「パネルを追加」と同じく A の区画のタブに入れる
    let a = hz.root().pane_of("A").unwrap();
    app::add_window(hz.panel("E"), hz.frame, Target::Pane(a), Dock::Center);
    pump();
    assert_eq!(hz.active_of("A").as_deref(), Some("E"));
    assert_ok("パネルを追加");
}

fn s_splitter_and_maximize() {
    let hz = Harness::new(base_layout(), None, &BASE);
    // 上下の区切りを 120px 下げる: 上の区画の高さが増え、A と B の幅は変わらない
    let (sp, a_before, b_before) = with_app(|a| {
        let f = a.frame(raw(hz.frame)).unwrap();
        let sp = *f.layout.splitters.iter().find(|s| s.axis == model::Axis::Vertical).unwrap();
        (sp, f.layout.pane_box(f.root.pane_of("A").unwrap()).unwrap().rect, f.layout.pane_box(f.root.pane_of("B").unwrap()).unwrap().rect)
    })
    .unwrap();
    let (cx, cy) = sp.rect.center();
    let c_moves = moved(hz.panel("C"));
    hz.drag((cx, cy), (cx, cy + 120));
    assert_eq!(moved(hz.panel("C")), c_moves, "裏のタブ C を区切りのドラッグで動かした");
    let a_after = hz.pane_body("A");
    assert!((a_after.h() + 24 - a_before.h() - 120).abs() <= 2, "上の区画が 120px 伸びていない: {a_before:?} → {a_after:?}");
    assert_eq!(hz.pane_body("B").w(), b_before.w(), "B の幅が変わった");
    assert_ok("上下の区切り");

    // 見出しのダブルクリックで A の区画を広げる・戻す
    let ha = hz.tab_rect("A");
    let at = (ha.r + 30, ha.center().1);
    send(hz.frame, WM_LBUTTONDBLCLK, 1, lp(at.0, at.1));
    pump();
    assert!(hz.visible(hz.panel("A")) && !hz.visible(hz.panel("B")) && !hz.visible(hz.panel("D")), "最大化で他が隠れていない");
    assert_ok("最大化");
    send(hz.frame, WM_LBUTTONDBLCLK, 1, lp(at.0, at.1));
    pump();
    assert!(hz.visible(hz.panel("B")) && hz.visible(hz.panel("D")));
    assert_ok("最大化を戻す");
}

fn s_host_actions() {
    let hz = Harness::new(base_layout(), None, &BASE);
    let a = hz.panel("A");
    // 本体が動かした → 区画へ押し返す
    unsafe {
        let _ = SetWindowPos(a, None, 5, 5, 50, 50, SWP_NOZORDER | SWP_NOACTIVATE);
    }
    assert_ok("本体が動かした");
    // 本体が隠した（表示を切った）→ タブは残り、点検の対象外。出したら戻る
    unsafe {
        let _ = ShowWindow(a, SW_HIDE);
    }
    pump();
    assert!(host::host_hidden(a) && hz.root().contains("A"));
    assert_ok("本体が隠した");
    unsafe {
        let _ = ShowWindow(a, SW_SHOWNA);
    }
    assert_ok("本体が出した");
    assert!(hz.visible(a));

    // 裏のタブ C を本体が 1 枚だけ前面に出した → C が前に出る
    assert_eq!(hz.active_of("C").as_deref(), Some("B"));
    unsafe {
        let _ = SetWindowPos(hz.panel("C"), Some(HWND_TOP), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
    }
    pump();
    assert_eq!(hz.active_of("C").as_deref(), Some("C"), "1 枚だけ前面に出したのにタブが前に出ない");
    assert_ok("1 枚だけ前面");

    // 裏のタブ B と E を 1 回の操作でまとめて前面に出した → どちらも動かない
    unsafe {
        let _ = SetWindowPos(hz.panel("B"), Some(HWND_TOP), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        let _ = SetWindowPos(hz.panel("E"), Some(HWND_TOP), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
    }
    pump();
    assert_eq!(hz.active_of("B").as_deref(), Some("C"), "まとめて前面に出したのにタブが動いた");
    assert_eq!(hz.active_of("E").as_deref(), Some("D"));
    assert_ok("まとめて前面");

    // 題が変わる → タブの名前が今の題、鍵はそのまま
    unsafe {
        let _ = SetWindowTextW(hz.panel("D"), w!("D [項目]"));
    }
    pump();
    assert!(hz.root().contains("D"));
    let label = with_app(|x| x.frames[0].tabs.iter().find(|t| t.key == "D").map(|t| t.label.clone())).flatten();
    assert_eq!(label.as_deref(), Some("D [項目]"));
    assert_ok("題が変わる");

    // 本体が窓を壊した（エリアへ戻した）→ タブは灰色で残る。同じ題で出し直すと同じタブに入る
    unsafe {
        let _ = DestroyWindow(a);
    }
    pump();
    assert!(hz.root().contains("A") && with_app(|x| x.embedded_by_key("A").is_none()).unwrap());
    assert_ok("本体が壊した");
    let mut hz = hz;
    hz.panels.retain(|p| p.0 != "A");
    let a2 = hz.add_panel("A", false, 9);
    pump();
    assert!(hz.is_embedded(a2), "出し直した A が入らない");
    assert_ok("出し直した");
}

fn s_close_cancel_and_restart() {
    let hz = Harness::new(base_layout(), None, &BASE);
    // はめ込む前の位置を控える
    let before: Vec<(String, isize, [i32; 4])> = with_app(|a| a.embedded.values().map(|e| (e.key.clone(), e.hwnd, e.top_rect())).collect()).unwrap();
    // 閉じる（偽物の本体は取り消す）: その場で全部が元の最上位の窓に戻り、クロークで隠れている
    send(hz.main, WM_CLOSE, 0, LPARAM(0));
    for (k, p, r) in &before {
        let p = h(*p);
        unsafe {
            assert!(GetWindow(p, GW_OWNER).ok() == Some(hz.main), "{k} の所有者");
            let st = GetWindowLongPtrW(p, GWL_STYLE) as u32;
            assert!(st & WS_CHILD.0 == 0, "{k} が子のまま");
            let mut wr = RECT::default();
            let _ = GetWindowRect(p, &mut wr);
            assert_eq!([wr.left, wr.top, wr.right - wr.left, wr.bottom - wr.top], *r, "{k} の位置が元に戻っていない");
            assert!(host::is_cloaked(p), "{k} が隠れていない（終了時にばらけて見える）");
        }
    }
    // 閉じるのが取り消された → 1 秒後にはめ込み直す（クロークは解ける）
    pump_for(1400);
    for (k, p, _) in &before {
        assert!(hz.is_embedded(h(*p)), "{k} がはめ込み直されていない");
        assert!(!host::is_cloaked(h(*p)), "{k} のクロークが残っている");
    }
    assert_ok("閉じるのを取り消した");

    // 次の起動（題が変わった色設定の窓を、前回の位置で見分ける）
    let store = with_app(|a| {
        a.save();
        a.store.clone()
    })
    .unwrap();
    let rect_a = store.rects.get("A").copied().unwrap();
    drop(hz);
    let mut root = base_layout();
    root.renumber();
    let hz = Harness::new(root, Some(store), &[("B", false), ("C", true), ("D", false), ("E", true)]);
    let mut hz = hz;
    let renamed = hz.add_panel("対象 [項目]", false, 0);
    unsafe {
        let _ = SetWindowPos(renamed, None, rect_a[0], rect_a[1], rect_a[2], rect_a[3], SWP_NOZORDER | SWP_NOACTIVATE);
    }
    app::scan(false);
    pump();
    assert_eq!(with_app(|a| a.embedded.get(&raw(renamed)).map(|e| e.key.clone())).flatten().as_deref(), Some("A"), "前回の位置で見分けられない");
    assert_ok("次の起動");
}

/// 題が変わるプラグインの窓（本体が言語ファイルで分離の窓の題を訳す）。2026-10-13 に本番で、英語で起動した回に
/// 「色空間ピッカー」が「Color Space Picker」という別のタブになり、日本語に戻すと、その灰色のタブが前に出たまま
/// 本物の窓が裏に回って見えなくなった
fn s_renamed_plugin_window() {
    let pane_of = |tabs: &[&str]| {
        let mut n = Node::default();
        for t in tabs {
            n.insert(Target::Root, t, Dock::Center);
        }
        n
    };
    let key_of = |p: HWND| with_app(|a| a.embedded.get(&raw(p)).map(|e| e.key.clone())).flatten();

    // 1 回目: 「P」をはめ込み、中の窓の手掛かりを覚える
    let mut hz = Harness::new(pane_of(&["Q", "P"]), None, &[("Q", false)]);
    let p = hz.add_plugin("P", "Pの中身", 1);
    app::scan(false);
    pump();
    assert_eq!(key_of(p).as_deref(), Some("P"));
    // 手掛かりが同じプラグインの窓がもう 1 枚あると、どちらか決められないので覚えない
    let twin = hz.add_plugin("T", "同じ中身", 2);
    let twin2 = hz.add_plugin("T2", "同じ中身", 3);
    let store = with_app(|a| {
        a.save();
        a.store.clone()
    })
    .unwrap();
    assert_eq!(store.fps.get("Static|Pの中身").map(String::as_str), Some("P"), "中の窓の手掛かりを覚えていない");
    assert!(!hz.is_embedded(twin) && !hz.is_embedded(twin2));
    assert_ok("題が変わる窓: 1 回目");
    drop(hz);

    // 2 回目（言語を変えた）: 題が「P-en」。前の起動で入った跡の灰色のタブ「P-en」が前に出ている
    let mut root = pane_of(&["Q", "P", "P-en"]);
    root.renumber();
    let mut hz = Harness::new(root, Some(store.clone()), &[("Q", false)]);
    assert_eq!(hz.active_of("Q").as_deref(), Some("P-en"));
    let p = hz.add_plugin("P-en", "Pの中身", 1);
    app::scan(false);
    pump();
    assert_eq!(key_of(p).as_deref(), Some("P"), "訳された題の窓を、中の窓の手掛かりで同じタブとして見分けられない");
    assert!(!hz.root().contains("P-en"), "跡の灰色のタブが残っている");
    assert_eq!(hz.active_of("Q").as_deref(), Some("P"));
    assert!(hz.visible(p), "窓が隠れたまま");
    assert_ok("題が変わる窓: 2 回目");
    drop(hz);

    // 3 回目: 見分けられない跡のタブ（窓の無いタブ）が前に出ている → 起動の後に、いちばん近い窓のあるタブを前に出す
    let mut root = pane_of(&["Q", "P", "ghost"]);
    root.renumber();
    let mut hz = Harness::new(root, Some(store), &[("Q", false)]);
    let p = hz.add_plugin("P", "Pの中身", 1);
    app::scan(false);
    pump();
    assert_eq!(hz.active_of("Q").as_deref(), Some("ghost"));
    assert!(!hz.visible(p), "裏のタブは隠れている（前提）");
    app::prefer_live_tabs();
    pump();
    assert_eq!(hz.active_of("Q").as_deref(), Some("P"), "灰色の前のタブを、窓のあるタブに譲らせていない");
    assert!(hz.visible(p), "窓が隠れたまま");
    assert!(hz.root().contains("ghost"), "見分けられないタブは消さない（灰色のまま残す）");
    // 前のタブに窓があれば何もしない
    app::prefer_live_tabs();
    pump();
    assert_eq!(hz.active_of("Q").as_deref(), Some("P"));
    assert_ok("題が変わる窓: 3 回目");
}

fn s_float_and_switch() {
    let hz = Harness::new(base_layout(), None, &BASE);
    // 別窓を開き、B を移す
    app::open_saved_or_new_float(true);
    pump();
    let float = with_app(|a| a.frames.iter().find(|f| f.kind == FrameKind::Float).map(|f| f.hwnd)).flatten().unwrap();
    let from = hz.root().pane_of("B").unwrap();
    app::move_tab("B", from, drag::Drop { frame: float, target: Target::Root, dock: Dock::Center });
    pump();
    assert_eq!(unsafe { GetParent(hz.panel("B")) }.ok(), Some(h(float)), "B が別窓に移っていない");
    assert_ok("別窓へ移す");
    // 別窓を閉じる → B は本体の分離の窓に戻る
    send(h(float), WM_CLOSE, 0, LPARAM(0));
    pump();
    assert!(!hz.is_embedded(hz.panel("B")));
    assert_ok("別窓を閉じる");

    // 配置を 2 つ保存して 30 回切り替える
    app::save_as("L1");
    let a = hz.root().pane_of("A").unwrap();
    app::add_window(hz.panel("F"), hz.frame, Target::Pane(a), Dock::Bottom);
    pump();
    app::save_as("L2");
    for i in 0..30 {
        app::switch_to(i % 2);
        pump();
        assert_ok(&format!("切り替え {i}"));
        let want_f = i % 2 == 1;
        assert_eq!(hz.is_embedded(hz.panel("F")), want_f, "切り替え {i} で F");
        assert!(host::cloaked().is_empty(), "切り替え {i} の後にクロークが残っている");
    }
}

/// 負荷: パネル 60 枚・区画 30 前後で、ランダムな操作を続ける
fn s_load() {
    let titles: Vec<String> = (0..60).map(|i| format!("P{i:02}")).collect();
    let specs: Vec<(&str, bool)> = titles.iter().map(|t| (t.as_str(), t.ends_with('7'))).collect();
    let hz = Harness::new(Node::default(), None, &specs);
    let mut seed: u64 = 0x1234_5678;
    let mut rnd = |n: usize| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % n.max(1) as u64) as usize
    };
    // 1 枚ずつ、ランダムな区画の辺か中央へ足す
    for t in &titles {
        let panes: Vec<u64> = hz.root().panes().iter().map(|p| p.id).collect();
        let dock = if panes.len() < 30 { [Dock::Left, Dock::Right, Dock::Top, Dock::Bottom][rnd(4)] } else { Dock::Center };
        app::add_window(hz.panel(t), hz.frame, Target::Pane(panes[rnd(panes.len())]), dock);
    }
    pump();
    assert_ok("60 枚を足した");
    let started = Instant::now();
    let steps = 300;
    for step in 0..steps {
        let root = hz.root();
        let keys = root.titles();
        match rnd(5) {
            0 => {
                // タブのクリック
                let k = &keys[rnd(keys.len())];
                if let Some(r) = with_app(|a| a.frames[0].tabs.iter().find(|t| &t.key == k).map(|t| t.rect)).flatten() {
                    hz.click(r.center());
                }
            }
            1 => {
                // タブを別の区画の辺へ（メッセージでドラッグ）
                let k = keys[rnd(keys.len())].clone();
                let target = keys[rnd(keys.len())].clone();
                if let Some(r) = with_app(|a| a.frames[0].tabs.iter().find(|t| t.key == k).map(|t| t.rect)).flatten() {
                    let dock = [Dock::Left, Dock::Right, Dock::Top, Dock::Bottom, Dock::Center][rnd(5)];
                    hz.drag(r.center(), hz.guide(&target, dock));
                }
            }
            2 => {
                // 区切りを動かす
                let sp = with_app(|a| a.frames[0].layout.splitters.clone()).unwrap();
                if !sp.is_empty() {
                    let s = sp[rnd(sp.len())];
                    let (cx, cy) = s.rect.center();
                    let d = rnd(200) as i32 - 100;
                    match s.axis {
                        model::Axis::Horizontal => hz.drag((cx, cy), (cx + d, cy)),
                        model::Axis::Vertical => hz.drag((cx, cy), (cx, cy + d)),
                    }
                }
            }
            3 => {
                // 外して入れ直す
                let k = keys[rnd(keys.len())].clone();
                app::remove_key(&k, None);
                pump();
                let panes: Vec<u64> = hz.root().panes().iter().map(|p| p.id).collect();
                app::add_window(hz.panel(&k), hz.frame, Target::Pane(panes[rnd(panes.len())]), Dock::Center);
            }
            _ => {
                // 本体が裏のタブを 1 枚前面に出す
                let k = keys[rnd(keys.len())].clone();
                unsafe {
                    let _ = SetWindowPos(hz.panel(&k), Some(HWND_TOP), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
                }
            }
        }
        assert_ok(&format!("負荷 {step} 手目"));
    }
    let per = started.elapsed() / steps;
    let n_panes = hz.root().panes().len();
    eprintln!("負荷: 区画 {n_panes}・タブ 60 で 1 手 {per:?}、{}", check::summary_line());
    assert!(check::max_relayout() < Duration::from_millis(100), "並べ直しが遅い: 最大 {:?}", check::max_relayout());
    assert!(check::checks() > steps as u64, "後回しの自己点検が走っていない（{} 回）", check::checks());

    // 区切りのドラッグ 300 回分（マウスの移動 1 回ごとに並べ直す）
    let sp = with_app(|a| a.frames[0].layout.splitters[0]).unwrap();
    let (cx, cy) = sp.rect.center();
    let t = Instant::now();
    send(hz.frame, WM_LBUTTONDOWN, 1, lp(cx, cy));
    for i in 0..300 {
        let d = ((i % 100) as i32) - 50;
        match sp.axis {
            model::Axis::Horizontal => send(hz.frame, WM_MOUSEMOVE, 1, lp(cx + d, cy)),
            model::Axis::Vertical => send(hz.frame, WM_MOUSEMOVE, 1, lp(cx, cy + d)),
        }
    }
    send(hz.frame, WM_LBUTTONUP, 0, lp(cx, cy));
    let per_move = t.elapsed() / 300;
    eprintln!("負荷: 区切りのドラッグ 1 移動 {per_move:?}");
    assert!(per_move < Duration::from_millis(16), "区切りのドラッグが 1 フレームに収まらない: {per_move:?}");
    assert_ok("区切りのドラッグの後");
    // 画面の座標を使う所の確かめ（枠の位置）
    let mut o = POINT::default();
    unsafe {
        let _ = ClientToScreen(hz.frame, &mut o);
    }
    assert!(o.x < -10000, "テストの窓が画面の中に出ている");
}

#[test]
fn win_harness_all() {
    s_embed_and_tabs();
    s_drag_dock_and_out();
    s_splitter_and_maximize();
    s_host_actions();
    s_close_cancel_and_restart();
    s_renamed_plugin_window();
    s_float_and_switch();
    s_load();
}
