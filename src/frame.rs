//! 枠の窓（主の枠と別窓）: 見出し・タブ・区切りの描画とマウス
//!
//! 主の枠は `register_window_client` に渡す（本体が `WS_CHILD` を付けてエリアに置く）。
//! 別窓はプラグインが作る最上位の窓で、所有者はメイン窓。
//! はめ込んだ本体の窓は区画の中身の矩形に置かれ、`WS_CLIPCHILDREN` でその上には描かない。

use std::cell::RefCell;
use std::collections::HashMap;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::app::{self, with_app, Mouse};
use crate::drag::{self, Source};
use crate::host::{h, raw};
use crate::menu;
use crate::model::{self, Layout, Node, Rect};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameKind {
    Main,
    Float,
}

const CLASS_NAME: PCWSTR = w!("SplitLayout_H_Frame");

#[derive(Clone, Copy, Debug)]
pub struct Metrics {
    pub dpi: u32,
    pub header: i32,
    pub gap: i32,
    pub grip: i32,
    pub guide: i32,
    pub tab_pad: i32,
}

#[derive(Clone, Debug)]
pub struct TabBox {
    pub pane: u64,
    pub index: usize,
    /// 配置での鍵
    pub key: String,
    /// 出す名前
    pub label: String,
    pub rect: Rect,
}

// 色（COLORREF は 0x00BBGGRR）
const C_GAP: u32 = 0x0014_1414;
const C_HEADER: u32 = 0x0026_2626;
const C_TAB_ACTIVE: u32 = 0x003A_3A3A;
const C_ACCENT: u32 = 0x00E6_8E3A;
const C_BODY: u32 = 0x001E_1E1E;
const C_TEXT: u32 = 0x00E6_E6E6;
const C_TEXT_DIM: u32 = 0x009A_9A9A;
const C_TEXT_GONE: u32 = 0x0070_7070;

thread_local! {
    static FONTS: RefCell<HashMap<u32, isize>> = RefCell::new(HashMap::new());
}

fn scale(v: i32, dpi: u32) -> i32 {
    (v * dpi as i32 + 48) / 96
}

pub fn metrics(hwnd: HWND) -> Metrics {
    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
    Metrics {
        dpi,
        header: scale(24, dpi),
        gap: scale(4, dpi).max(3),
        grip: scale(2, dpi),
        guide: scale(36, dpi),
        tab_pad: scale(10, dpi),
    }
}

pub fn client_rect(hwnd: HWND) -> Rect {
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rc);
    }
    Rect::new(rc.left, rc.top, rc.right, rc.bottom)
}

fn font(dpi: u32) -> HFONT {
    FONTS.with(|f| {
        let mut map = f.borrow_mut();
        let v = *map.entry(dpi).or_insert_with(|| unsafe {
            CreateFontW(
                -scale(12, dpi),
                0,
                0,
                0,
                FW_NORMAL.0 as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                CLEARTYPE_QUALITY,
                0,
                w!("Yu Gothic UI"),
            )
            .0 as isize
        });
        HFONT(v as *mut _)
    })
}

pub fn delete_fonts() {
    FONTS.with(|f| {
        for (_, v) in f.borrow_mut().drain() {
            unsafe {
                let _ = DeleteObject(HGDIOBJ(v as *mut _));
            }
        }
    });
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// タブの矩形（文字の幅で決める）。見出しからはみ出すタブは載せない
pub fn compute_tabs(hwnd: HWND, root: &Node, layout: &Layout, m: &Metrics, labels: &HashMap<String, String>) -> Vec<TabBox> {
    let mut out = Vec::new();
    unsafe {
        let dc = GetDC(Some(hwnd));
        let old = SelectObject(dc, font(m.dpi).into());
        for pb in layout.panes.iter().filter(|p| p.visible) {
            let Some(pane) = root.pane(pb.id) else { continue };
            let mut x = pb.header.l;
            for (i, t) in pane.tabs.iter().enumerate() {
                let label = labels.get(t).cloned().unwrap_or_else(|| t.clone());
                let text = wide(&label);
                let mut size = SIZE::default();
                let _ = GetTextExtentPoint32W(dc, &text, &mut size);
                let wdt = (size.cx + m.tab_pad * 2).max(scale(48, m.dpi));
                if x >= pb.header.r {
                    break;
                }
                let r = Rect::new(x, pb.header.t, (x + wdt).min(pb.header.r), pb.header.b);
                out.push(TabBox { pane: pb.id, index: i, key: t.clone(), label, rect: r });
                x += wdt + 1;
            }
        }
        SelectObject(dc, old);
        ReleaseDC(Some(hwnd), dc);
    }
    out
}

// ---- 窓を作る ----

fn register_class() -> Option<windows::Win32::Foundation::HINSTANCE> {
    unsafe {
        let instance = GetModuleHandleW(None).ok()?;
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_DBLCLKS,
            lpfnWndProc: Some(frame_proc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).ok()?,
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        Some(instance.into())
    }
}

pub fn unregister_class() {
    unsafe {
        if let Ok(instance) = GetModuleHandleW(None) {
            let _ = UnregisterClassW(CLASS_NAME, Some(instance.into()));
        }
    }
}

/// 主の枠（`register_window_client` に渡す）
pub fn create_main(name: &str) -> Option<HWND> {
    let instance = register_class()?;
    let title: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS_NAME,
            PCWSTR(title.as_ptr()),
            WS_POPUP | WS_CLIPCHILDREN,
            0,
            0,
            640,
            480,
            None,
            None,
            Some(instance),
            None,
        )
        .ok()
    }
}

/// 別窓（所有者はメイン窓）
pub fn create_float(owner: HWND, r: [i32; 4]) -> Option<HWND> {
    let instance = register_class()?;
    let n = app::with_app(|a| a.frames.iter().filter(|f| f.kind == FrameKind::Float).count()).unwrap_or(0) + 1;
    let title: Vec<u16> = format!("分割レイアウト（別窓 {n}）").encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        // タスクバーにボタンを持たせる。持たないと、所有者のある窓は最小化で画面の左下（時計の上）に小さな帯として残る
        let hwnd = CreateWindowExW(
            WS_EX_APPWINDOW,
            CLASS_NAME,
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            r[0],
            r[1],
            r[2],
            r[3],
            Some(owner),
            None,
            Some(instance),
            None,
        )
        .ok()?;
        // アイコンは本体のもの（タスクバーと Alt+Tab に出る）
        for (kind, idx) in [(ICON_BIG, GCLP_HICON), (ICON_SMALL, GCLP_HICONSM)] {
            let icon = SendMessageW(owner, WM_GETICON, Some(WPARAM(kind as usize)), Some(LPARAM(0))).0;
            let icon = if icon != 0 { icon } else { GetClassLongPtrW(owner, idx) as isize };
            if icon != 0 {
                SendMessageW(hwnd, WM_SETICON, Some(WPARAM(kind as usize)), Some(LPARAM(icon)));
            }
        }
        Some(hwnd)
    }
}

// ---- 窓の手続き ----

fn lparam_point(lp: LPARAM) -> (i32, i32) {
    ((lp.0 & 0xFFFF) as i16 as i32, ((lp.0 >> 16) & 0xFFFF) as i16 as i32)
}

fn to_screen(hwnd: HWND, x: i32, y: i32) -> POINT {
    let mut p = POINT { x, y };
    unsafe {
        let _ = ClientToScreen(hwnd, &mut p);
    }
    p
}

unsafe extern "system" fn frame_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    crate::guard::run("frame_proc", || frame_proc_inner(hwnd, msg, wp, lp)).unwrap_or_else(|| DefWindowProcW(hwnd, msg, wp, lp))
}

unsafe fn frame_proc_inner(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_SIZE => {
            app::relayout(hwnd);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        WM_SETCURSOR if (lp.0 & 0xFFFF) as u32 == HTCLIENT => {
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let _ = ScreenToClient(hwnd, &mut pt);
            let axis = with_app(|app| {
                let f = app.frame(raw(hwnd))?;
                let m = metrics(hwnd);
                f.layout.splitter_at(pt.x, pt.y, m.grip).map(|s| s.axis)
            })
            .flatten();
            match axis {
                Some(model::Axis::Horizontal) => {
                    SetCursor(LoadCursorW(None, IDC_SIZEWE).ok());
                    LRESULT(1)
                }
                Some(model::Axis::Vertical) => {
                    SetCursor(LoadCursorW(None, IDC_SIZENS).ok());
                    LRESULT(1)
                }
                None => DefWindowProcW(hwnd, msg, wp, lp),
            }
        }
        WM_LBUTTONDOWN => {
            let (x, y) = lparam_point(lp);
            lbutton_down(hwnd, x, y);
            LRESULT(0)
        }
        WM_LBUTTONDBLCLK => {
            let (x, y) = lparam_point(lp);
            toggle_maximize_at(hwnd, x, y);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let (x, y) = lparam_point(lp);
            mouse_move(hwnd, x, y);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let (x, y) = lparam_point(lp);
            lbutton_up(hwnd, x, y);
            LRESULT(0)
        }
        WM_CAPTURECHANGED => {
            cancel_mouse();
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            let (x, y) = lparam_point(lp);
            menu::show(hwnd, x, y);
            LRESULT(0)
        }
        WM_SHOWWINDOW if lp.0 == 0 => {
            // 本体の「表示」で枠を消したら中身を本体の分離の窓に戻し、出したらはめ込み直す
            if wp.0 == 0 {
                app::release_frame(raw(hwnd));
            } else {
                app::post_scan();
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_EXITSIZEMOVE => {
            app::save();
            LRESULT(0)
        }
        WM_DPICHANGED => {
            let r = &*(lp.0 as *const RECT);
            let _ = SetWindowPos(hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
            app::relayout(hwnd);
            LRESULT(0)
        }
        WM_CLOSE => {
            let is_float = with_app(|a| a.frame(raw(hwnd)).map(|f| f.kind == FrameKind::Float)).flatten().unwrap_or(false);
            if is_float {
                app::close_float(raw(hwnd));
                LRESULT(0)
            } else {
                DefWindowProcW(hwnd, msg, wp, lp)
            }
        }
        WM_DESTROY => {
            let kind = with_app(|a| a.frame(raw(hwnd)).map(|f| f.kind)).flatten();
            match kind {
                Some(FrameKind::Main) => app::release_all(false),
                Some(FrameKind::Float) => app::release_frame(raw(hwnd)),
                None => {}
            }
            LRESULT(0)
        }
        WM_TIMER if wp.0 == app::TIMER_RESUME => {
            app::resume();
            LRESULT(0)
        }
        WM_TIMER if wp.0 == app::TIMER_UNCLOAK => {
            let _ = KillTimer(Some(hwnd), app::TIMER_UNCLOAK);
            app::scan(true);
            let _ = SetTimer(Some(hwnd), app::TIMER_LIVE_TABS, app::LIVE_TABS_AFTER_MS, None);
            LRESULT(0)
        }
        WM_TIMER if wp.0 == app::TIMER_LIVE_TABS => {
            let _ = KillTimer(Some(hwnd), app::TIMER_LIVE_TABS);
            app::prefer_live_tabs();
            LRESULT(0)
        }
        app::WM_APP_SCAN => {
            app::scan(false);
            LRESULT(0)
        }
        app::WM_APP_PANEL_GONE => {
            app::panel_gone(wp.0 as isize);
            LRESULT(0)
        }
        app::WM_APP_REFRESH_PANEL => {
            app::refresh_for_panel(wp.0 as isize);
            LRESULT(0)
        }
        app::WM_APP_FLOAT_DROP => {
            app::float_drop(wp.0 as isize);
            LRESULT(0)
        }
        crate::check::WM_APP_CHECK => {
            crate::check::run_check();
            LRESULT(0)
        }
        app::WM_APP_FRONT => {
            app::handle_front_requests();
            LRESULT(0)
        }
        app::WM_APP_SWITCH => {
            app::switch_to(wp.0);
            LRESULT(0)
        }
        app::WM_APP_OPEN_FLOAT => {
            app::open_saved_or_new_float(wp.0 != 0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

// ---- マウス ----

enum Press {
    None,
    Splitter,
    Tab(Option<isize>),
}

fn lbutton_down(hwnd: HWND, x: i32, y: i32) {
    let key = raw(hwnd);
    let press = with_app(|app| {
        if !matches!(app.mouse, Mouse::None) {
            return Press::None;
        }
        let m = metrics(hwnd);
        let Some(f) = app.frame(key) else { return Press::None };
        if let Some(sp) = f.layout.splitter_at(x, y, m.grip) {
            let span = match sp.axis {
                model::Axis::Horizontal => sp.span.w(),
                model::Axis::Vertical => sp.span.h(),
            };
            app.mouse = Mouse::Splitter { frame: key, id: sp.id, span, original: f.root.clone() };
            return Press::Splitter;
        }
        let Some(tab) = f.tabs.iter().find(|t| t.rect.contains(x, y)).cloned() else { return Press::None };
        if let Some(f) = app.frame_mut(key) {
            f.root.set_active(tab.pane, tab.index);
        }
        let panel = app.embedded_by_key(&tab.key).map(|e| e.hwnd);
        app.mouse = Mouse::TabPress { pane: tab.pane, key: tab.key, start: to_screen(hwnd, x, y) };
        Press::Tab(panel)
    })
    .unwrap_or(Press::None);
    match press {
        Press::None => {}
        Press::Splitter => unsafe {
            SetCapture(hwnd);
        },
        Press::Tab(panel) => unsafe {
            SetCapture(hwnd);
            app::relayout(hwnd);
            if let Some(p) = panel {
                crate::host::activate_panel(h(p));
            }
        },
    }
}

fn mouse_move(hwnd: HWND, x: i32, y: i32) {
    if unsafe { GetAsyncKeyState(0x1B) } < 0 {
        let busy = with_app(|a| !matches!(a.mouse, Mouse::None)).unwrap_or(false);
        if busy {
            cancel_mouse();
            unsafe {
                let _ = ReleaseCapture();
            }
            return;
        }
    }
    let key = raw(hwnd);
    let screen = to_screen(hwnd, x, y);
    enum Next {
        Nothing,
        Relayout,
        Drag,
    }
    let next = with_app(|app| {
        let m = metrics(hwnd);
        match &app.mouse {
            Mouse::Splitter { frame, id, span, original } if *frame == key => {
                let (id, span) = (*id, *span);
                let mut root = original.clone();
                let f = app.frames.iter_mut().find(|f| f.hwnd == key)?;
                let sp = *f.layout.splitters.iter().find(|s| s.id == id)?;
                // 始めたときの木から計算し直す（区切りに接していない区画の幅を保つ）
                root.set_ratio_keeping(id, model::ratio_at(&sp, x, y, m.gap), span, m.gap);
                f.root = root;
                Some(Next::Relayout)
            }
            Mouse::TabPress { start, pane, key: tab_key, .. } => {
                let (dx, dy) = unsafe { (GetSystemMetrics(SM_CXDRAG), GetSystemMetrics(SM_CYDRAG)) };
                if (screen.x - start.x).abs() > dx || (screen.y - start.y).abs() > dy {
                    let s = drag::Session::new(Source::Tab { key: tab_key.clone(), pane: *pane });
                    app.mouse = Mouse::Dragging(s);
                    Some(Next::Drag)
                } else {
                    Some(Next::Nothing)
                }
            }
            Mouse::Dragging(_) => Some(Next::Drag),
            _ => Some(Next::Nothing),
        }
    })
    .flatten()
    .unwrap_or(Next::Nothing);
    match next {
        Next::Nothing => {}
        Next::Relayout => app::relayout(hwnd),
        Next::Drag => drag::update(screen),
    }
}

fn lbutton_up(hwnd: HWND, x: i32, y: i32) {
    let screen = to_screen(hwnd, x, y);
    let prev = with_app(|app| std::mem::replace(&mut app.mouse, Mouse::None)).unwrap_or(Mouse::None);
    unsafe {
        let _ = ReleaseCapture();
    }
    match prev {
        Mouse::Splitter { .. } => app::save(),
        Mouse::Dragging(s) if matches!(s.source, Source::Tab { .. }) => drag::commit_tab(s, screen),
        Mouse::Dragging(s) => {
            // 分離の窓のドラッグは本体の移動の処理が受け持つ。ここに来たら戻しておく
            with_app(|app| app.mouse = Mouse::Dragging(s));
        }
        _ => {}
    }
}

fn cancel_mouse() {
    let had_tab_drag = with_app(|app| match &app.mouse {
        Mouse::Splitter { .. } | Mouse::TabPress { .. } => {
            app.mouse = Mouse::None;
            false
        }
        Mouse::Dragging(s) if matches!(s.source, Source::Tab { .. }) => {
            app.mouse = Mouse::None;
            true
        }
        _ => false,
    })
    .unwrap_or(false);
    if had_tab_drag {
        drag::hide_overlay();
    }
}

fn toggle_maximize_at(hwnd: HWND, x: i32, y: i32) {
    let changed = with_app(|app| {
        let f = app.frame_mut(raw(hwnd))?;
        let pb = f.layout.pane_at(x, y)?;
        if !pb.header.contains(x, y) {
            return None;
        }
        let id = pb.id;
        f.maximized = if f.maximized == Some(id) { None } else { Some(id) };
        Some(())
    })
    .flatten()
    .is_some();
    if changed {
        app::relayout(hwnd);
    }
}

pub fn toggle_maximize(hwnd: HWND, pane: u64) {
    with_app(|app| {
        if let Some(f) = app.frame_mut(raw(hwnd)) {
            f.maximized = if f.maximized == Some(pane) { None } else { Some(pane) };
        }
    });
    app::relayout(hwnd);
}

// ---- 描く ----

enum Body {
    /// はめ込んだ窓が描く
    Live,
    /// 文字を出す
    Text(String),
}

struct PaneDraw {
    header: Rect,
    body: Rect,
    content: Body,
    maximized: bool,
}

struct TabDraw {
    rect: Rect,
    title: String,
    active: bool,
    live: bool,
}

fn paint(hwnd: HWND) {
    let key = raw(hwnd);
    let m = metrics(hwnd);
    let client = client_rect(hwnd);
    let (panes, tabs) = with_app(|app| {
        let Some(f) = app.frame(key) else { return (Vec::new(), Vec::new()) };
        let live = |k: &str| {
            app.embedded
                .values()
                .any(|e| e.key == k && e.frame == key && unsafe { IsWindowVisible(h(e.hwnd)).as_bool() })
        };
        let label_of = |k: &str| {
            f.tabs.iter().find(|t| t.key == k).map(|t| t.label.clone()).unwrap_or_else(|| crate::host::label(k, None))
        };
        let mut panes = Vec::new();
        for pb in f.layout.panes.iter().filter(|p| p.visible) {
            let Some(pane) = f.root.pane(pb.id) else { continue };
            let content = match pane.active_title() {
                None => Body::Text(
                    "パネルのタブや、本体で分離した窓のタイトルバーをここへドラッグ\n右クリック →「パネルを追加」でも足せます".into(),
                ),
                Some(t) if live(t) => Body::Live,
                Some(t) if app.embedded_by_key(t).is_some() => {
                    Body::Text(format!("{}\n非表示です。本体の「表示」メニューで出すと、ここに入ります", label_of(t)))
                }
                Some(t) => Body::Text(format!("{}\n本体で「ウィンドウを分離」すると、ここに入ります", label_of(t))),
            };
            panes.push(PaneDraw { header: pb.header, body: pb.body, content, maximized: f.maximized == Some(pb.id) });
        }
        let tabs = f
            .tabs
            .iter()
            .map(|t| {
                let active = f.root.pane(t.pane).is_some_and(|p| p.active == t.index);
                TabDraw { rect: t.rect, title: t.label.clone(), active, live: live(&t.key) || app.embedded_by_key(&t.key).is_some() }
            })
            .collect();
        (panes, tabs)
    })
    .unwrap_or_default();

    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let dc = BeginPaint(hwnd, &mut ps);
        let (w, hgt) = (client.w().max(1), client.h().max(1));
        let mem = CreateCompatibleDC(Some(dc));
        let bmp = CreateCompatibleBitmap(dc, w, hgt);
        let old_bmp = SelectObject(mem, bmp.into());
        let old_font = SelectObject(mem, font(m.dpi).into());
        SetBkMode(mem, TRANSPARENT);

        fill(mem, client, C_GAP);
        for p in &panes {
            fill(mem, p.header, C_HEADER);
            if let Body::Text(t) = &p.content {
                fill(mem, p.body, C_BODY);
                let mut r = RECT { left: p.body.l + 8, top: p.body.t + 8, right: p.body.r - 8, bottom: p.body.b - 8 };
                SetTextColor(mem, COLORREF(C_TEXT_DIM));
                let mut text = wide(t);
                // 縦の中央に寄せる（DT_VCENTER は 1 行のときしか効かない）
                let mut calc = r;
                DrawTextW(mem, &mut text, &mut calc, DT_CENTER | DT_WORDBREAK | DT_CALCRECT);
                let th = calc.bottom - calc.top;
                r.top += ((r.bottom - r.top) - th).max(0) / 2;
                DrawTextW(mem, &mut text, &mut r, DT_CENTER | DT_WORDBREAK);
            }
            if p.maximized {
                // 最大化中の印: 見出しの下に細い線
                fill(mem, Rect::new(p.header.l, p.header.b - 1, p.header.r, p.header.b), C_ACCENT);
            }
        }
        for t in &tabs {
            if t.active {
                fill(mem, t.rect, C_TAB_ACTIVE);
                fill(mem, Rect::new(t.rect.l, t.rect.t, t.rect.r, t.rect.t + scale(2, m.dpi)), C_ACCENT);
            }
            let color = if !t.live {
                C_TEXT_GONE
            } else if t.active {
                C_TEXT
            } else {
                C_TEXT_DIM
            };
            SetTextColor(mem, COLORREF(color));
            let mut r = RECT { left: t.rect.l + m.tab_pad, top: t.rect.t, right: t.rect.r - 2, bottom: t.rect.b };
            let mut text = wide(&t.title);
            DrawTextW(mem, &mut text, &mut r, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
        }

        let _ = BitBlt(dc, 0, 0, w, hgt, Some(mem), 0, 0, SRCCOPY);
        SelectObject(mem, old_font);
        SelectObject(mem, old_bmp);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
        let _ = EndPaint(hwnd, &ps);
    }
}

fn fill(dc: HDC, r: Rect, color: u32) {
    if r.is_empty() {
        return;
    }
    unsafe {
        let brush = CreateSolidBrush(COLORREF(color));
        let rc = RECT { left: r.l, top: r.t, right: r.r, bottom: r.b };
        FillRect(dc, &rc, brush);
        let _ = DeleteObject(brush.into());
    }
}
