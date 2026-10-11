//! ドラッグ: 落とす先の判定と、ガイド・予告を描く重ね窓
//!
//! 重ね窓は `WS_EX_LAYERED | WS_EX_TRANSPARENT` で、マウスを素通しする。最前面に置き、運んでいる分離の窓より上に出す。
//! 描き直すのは、落とす先か、カーソルの下の区画が変わったときだけ（毎回の移動で画素を塗り直さない）。

use std::cell::{Cell, RefCell};

use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, POINT, RECT, SIZE};
use windows::Win32::Graphics::Gdi::{
    ClientToScreen, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC, SelectObject,
    AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::app::{self, with_app, Mouse};
use crate::frame::{self, FrameKind};
use crate::host::{self, h};
use crate::model::{self, Dock, Rect, Target};

#[derive(Clone, Debug)]
pub enum Source {
    /// 枠のタブ
    Tab { key: String, pane: u64 },
    /// 本体の分離の窓（タイトルバーで運んでいる）
    Window { hwnd: isize },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Drop {
    pub frame: isize,
    pub target: Target,
    pub dock: Dock,
}

pub struct Session {
    pub source: Source,
    pub drop: Option<Drop>,
    /// どの枠の上にも無い
    pub outside: bool,
    /// 分離の窓の運び始めの位置（Esc で取り消すと OS がここへ戻す。戻っていたら落とさない）
    pub start_rect: Option<RECT>,
    /// 描いている内容（変わったときだけ描き直す）
    drawn: Option<(isize, Option<u64>, Option<Drop>)>,
}

impl Session {
    pub fn new(source: Source) -> Self {
        let start_rect = match &source {
            Source::Window { hwnd, .. } => {
                let mut r = RECT::default();
                unsafe {
                    let _ = GetWindowRect(h(*hwnd), &mut r);
                }
                Some(r)
            }
            Source::Tab { .. } => None,
        };
        Self { source, drop: None, outside: true, start_rect, drawn: None }
    }
}

/// 描く内容（画面座標）
struct Picture {
    area: Rect,
    guides: Vec<(Dock, Rect, bool)>,
    preview: Option<Rect>,
}

thread_local! {
    static OVERLAY: Cell<isize> = const { Cell::new(0) };
    static OVERLAY_SHOWN: Cell<bool> = const { Cell::new(false) };
    /// 画素の作業場（大きさが変わったときだけ作り直す）
    static PIXELS: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
}

const OVERLAY_CLASS: windows::core::PCWSTR = w!("SplitLayout_H_Overlay");

enum Outcome {
    Nothing,
    Hide,
    Draw(Picture),
    /// 同じ見出しの中でタブの順を入れ替えた（その枠を並べ直す）
    Reordered(isize),
}

/// カーソルが動いた（画面座標）
pub fn update(pt: POINT) {
    let outcome = with_app(|app| {
        let crate::app::App { mouse, frames, .. } = app;
        let Mouse::Dragging(session) = mouse else { return Outcome::Nothing };

        // 同じ見出しの中で左右に動かしている間は、ドッキングではなくタブの順を入れ替える
        if let Source::Tab { key, pane } = &session.source {
            let pane = *pane;
            if let Some(f) = frames.iter_mut().find(|f| f.root.pane(pane).is_some()) {
                let area = screen_client(h(f.hwnd));
                let m = frame::metrics(h(f.hwnd));
                if let Some(pb) = f.layout.pane_box(pane).copied() {
                    let hd = pb.header;
                    let (lx, ly) = (pt.x - area.l, pt.y - area.t);
                    let slack = m.header / 2;
                    if pb.visible && lx >= hd.l && lx < hd.r && ly >= hd.t - slack && ly < hd.b + slack {
                        session.drop = None;
                        session.outside = false;
                        session.drawn = None;
                        let tabs: Vec<(usize, Rect, bool)> =
                            f.tabs.iter().filter(|t| t.pane == pane).map(|t| (t.index, t.rect, t.key == *key)).collect();
                        let Some(&(cur, cur_rect, _)) = tabs.iter().find(|t| t.2) else { return Outcome::Hide };
                        let target = tabs
                            .iter()
                            .find(|t| lx >= t.1.l && lx < t.1.r)
                            .or_else(|| if lx < tabs[0].1.l { tabs.first() } else { tabs.last() })
                            .copied();
                        if let Some((to, to_rect, _)) = target {
                            let cw = cur_rect.w();
                            // 入れ替えた後もカーソルが動かしたタブの上に来るときだけ入れ替える（幅の違うタブで行ったり来たりしない）
                            let go = (to > cur && lx >= to_rect.r - cw) || (to < cur && lx < to_rect.l + cw);
                            if go {
                                f.root.move_tab(pane, cur, to);
                                return Outcome::Reordered(f.hwnd);
                            }
                        }
                        return Outcome::Hide;
                    }
                }
            }
        }

        // 別窓（最上位）を主の枠より先に見る
        let mut order: Vec<(isize, FrameKind)> = frames.iter().map(|f| (f.hwnd, f.kind)).collect();
        order.sort_by_key(|(_, k)| (*k == FrameKind::Main) as u8);
        let mut hit = None;
        for (fh, _) in order {
            let hwnd = h(fh);
            if !unsafe { IsWindowVisible(hwnd).as_bool() } {
                continue;
            }
            let area = screen_client(hwnd);
            if area.contains(pt.x, pt.y) {
                hit = Some((fh, area));
                break;
            }
        }
        let Some((fh, area)) = hit else {
            session.outside = true;
            session.drop = None;
            let key = (0, None, None);
            if session.drawn == Some(key) {
                return Outcome::Nothing;
            }
            session.drawn = Some(key);
            return Outcome::Hide;
        };
        session.outside = false;
        let Some(f) = frames.iter().find(|f| f.hwnd == fh) else { return Outcome::Nothing };
        let m = frame::metrics(h(fh));
        let (lx, ly) = (pt.x - area.l, pt.y - area.t);
        let to_screen = |r: Rect| Rect::new(r.l + area.l, r.t + area.t, r.r + area.l, r.b + area.t);
        let pane = f.layout.pane_at(lx, ly).copied();
        let mut guides: Vec<(Dock, Rect, Target)> = Vec::new();
        if let Some(p) = pane {
            for (d, r) in model::pane_guides(p.body, m.guide) {
                guides.push((d, to_screen(r), Target::Pane(p.id)));
            }
        }
        if !f.root.is_empty_pane() {
            let client = Rect::new(0, 0, area.w(), area.h());
            for (d, r) in model::root_guides(client, m.guide) {
                guides.push((d, to_screen(r), Target::Root));
            }
        }
        let drop = guides
            .iter()
            .find(|(_, r, _)| r.contains(pt.x, pt.y))
            .map(|(d, _, t)| Drop { frame: fh, target: *t, dock: *d });
        session.drop = drop;
        let key = (fh, pane.map(|p| p.id), drop);
        if session.drawn == Some(key) {
            return Outcome::Nothing;
        }
        session.drawn = Some(key);
        let preview = drop.map(|d| match d.target {
            Target::Root => to_screen(model::preview_rect(Rect::new(0, 0, area.w(), area.h()), d.dock, true)),
            Target::Pane(id) => {
                let pb = f.layout.pane_box(id).map(|p| p.rect).unwrap_or_default();
                to_screen(model::preview_rect(pb, d.dock, false))
            }
        });
        let guides = guides.into_iter().map(|(d, r, t)| (d, r, drop.is_some_and(|x| x.dock == d && x.target == t))).collect();
        Outcome::Draw(Picture { area, guides, preview })
    })
    .unwrap_or(Outcome::Nothing);
    match outcome {
        Outcome::Nothing => {}
        Outcome::Hide => hide_overlay(),
        Outcome::Draw(p) => draw(&p),
        Outcome::Reordered(f) => {
            hide_overlay();
            app::relayout(h(f));
        }
    }
}

/// タブのドラッグを離した（画面座標）
pub fn commit_tab(session: Session, pt: POINT) {
    hide_overlay();
    let Source::Tab { key, pane } = session.source else { return };
    if let Some(d) = session.drop {
        app::move_tab(&key, pane, d);
    } else if session.outside {
        // どの枠の外: 本体の分離の窓に戻して、カーソルの所に出す
        app::remove_key(&key, Some(POINT { x: pt.x - 40, y: pt.y - 12 }));
    } else {
        // 見出しの中での並べ替え（済んでいる）を保存する
        app::save();
    }
}

/// 分離の窓を落とすか（Esc で取り消されて元の位置に戻っていたら落とさない）
pub fn window_was_moved(session: &Session) -> bool {
    let (Source::Window { hwnd, .. }, Some(start)) = (&session.source, session.start_rect) else { return true };
    let mut now = RECT::default();
    unsafe {
        let _ = GetWindowRect(h(*hwnd), &mut now);
    }
    now != start
}

fn screen_client(hwnd: HWND) -> Rect {
    let mut rc = RECT::default();
    let mut origin = POINT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rc);
        let _ = ClientToScreen(hwnd, &mut origin);
    }
    Rect::new(origin.x, origin.y, origin.x + rc.right, origin.y + rc.bottom)
}

// ---- 重ね窓 ----

unsafe extern "system" fn overlay_proc(hwnd: HWND, msg: u32, wp: windows::Win32::Foundation::WPARAM, lp: windows::Win32::Foundation::LPARAM) -> windows::Win32::Foundation::LRESULT {
    crate::guard::run("overlay_proc", || overlay_proc_inner(hwnd, msg, wp, lp)).unwrap_or_else(|| DefWindowProcW(hwnd, msg, wp, lp))
}

unsafe fn overlay_proc_inner(hwnd: HWND, msg: u32, wp: windows::Win32::Foundation::WPARAM, lp: windows::Win32::Foundation::LPARAM) -> windows::Win32::Foundation::LRESULT {
    if msg == WM_NCHITTEST {
        return windows::Win32::Foundation::LRESULT(HTTRANSPARENT as isize);
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}

fn overlay() -> Option<HWND> {
    let v = OVERLAY.with(|c| c.get());
    if v != 0 {
        return Some(h(v));
    }
    unsafe {
        let instance = GetModuleHandleW(None).ok()?;
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(overlay_proc),
            hInstance: instance.into(),
            lpszClassName: OVERLAY_CLASS,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            OVERLAY_CLASS,
            w!(""),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .ok()?;
        OVERLAY.with(|c| c.set(host::raw(hwnd)));
        Some(hwnd)
    }
}

pub fn hide_overlay() {
    if OVERLAY_SHOWN.with(|c| c.replace(false)) {
        let v = OVERLAY.with(|c| c.get());
        if v != 0 {
            unsafe {
                let _ = ShowWindow(h(v), SW_HIDE);
            }
        }
    }
}

pub fn destroy_overlay() {
    let v = OVERLAY.with(|c| c.replace(0));
    unsafe {
        if v != 0 {
            let _ = DestroyWindow(h(v));
        }
        if let Ok(instance) = GetModuleHandleW(None) {
            let _ = UnregisterClassW(OVERLAY_CLASS, Some(instance.into()));
        }
    }
    PIXELS.with(|p| *p.borrow_mut() = Vec::new());
}

/// 乗算済みの ARGB
const fn argb(a: u32, r: u32, g: u32, b: u32) -> u32 {
    (a << 24) | ((r * a / 255) << 16) | ((g * a / 255) << 8) | (b * a / 255)
}

const PREVIEW: u32 = argb(0x60, 0x3a, 0x8e, 0xe6);
const PREVIEW_EDGE: u32 = argb(0xE0, 0x3a, 0x8e, 0xe6);
const GUIDE: u32 = argb(0xE8, 0x2c, 0x2c, 0x2c);
const GUIDE_EDGE: u32 = argb(0xFF, 0x80, 0x80, 0x80);
const GUIDE_MARK: u32 = argb(0xFF, 0x3a, 0x8e, 0xe6);
const GUIDE_HOT: u32 = argb(0xF0, 0x3a, 0x8e, 0xe6);
const GUIDE_HOT_MARK: u32 = argb(0xFF, 0xF0, 0xF0, 0xF0);

fn fill(buf: &mut [u32], w: i32, hgt: i32, r: Rect, c: u32) {
    let l = r.l.clamp(0, w);
    let rr = r.r.clamp(0, w);
    let t = r.t.clamp(0, hgt);
    let b = r.b.clamp(0, hgt);
    for y in t..b {
        let row = (y * w) as usize;
        buf[row + l as usize..row + rr as usize].fill(c);
    }
}

fn frame_rect(buf: &mut [u32], w: i32, hgt: i32, r: Rect, t: i32, c: u32) {
    fill(buf, w, hgt, Rect::new(r.l, r.t, r.r, r.t + t), c);
    fill(buf, w, hgt, Rect::new(r.l, r.b - t, r.r, r.b), c);
    fill(buf, w, hgt, Rect::new(r.l, r.t, r.l + t, r.b), c);
    fill(buf, w, hgt, Rect::new(r.r - t, r.t, r.r, r.b), c);
}

fn draw(p: &Picture) {
    let Some(hwnd) = overlay() else { return };
    let (w, hgt) = (p.area.w().max(1), p.area.h().max(1));
    let local = |r: Rect| Rect::new(r.l - p.area.l, r.t - p.area.t, r.r - p.area.l, r.b - p.area.t);
    PIXELS.with(|cell| {
        let mut buf = cell.borrow_mut();
        let n = (w * hgt) as usize;
        if buf.len() != n {
            *buf = vec![0; n];
        } else {
            buf.fill(0);
        }
        if let Some(pr) = p.preview {
            let r = local(pr);
            fill(&mut buf, w, hgt, r, PREVIEW);
            frame_rect(&mut buf, w, hgt, r, 2, PREVIEW_EDGE);
        }
        for (dock, gr, hot) in &p.guides {
            let r = local(*gr);
            fill(&mut buf, w, hgt, r, if *hot { GUIDE_HOT } else { GUIDE });
            frame_rect(&mut buf, w, hgt, r, 1, GUIDE_EDGE);
            // 中の印: 中央は四角、辺はその向きの帯
            let inner = r.inflate(-(r.w() / 5));
            let (iw, ih) = (inner.w(), inner.h());
            let mark = match dock {
                Dock::Center => inner.inflate(-(iw / 6)),
                Dock::Left => Rect::new(inner.l, inner.t, inner.l + iw / 3, inner.b),
                Dock::Right => Rect::new(inner.r - iw / 3, inner.t, inner.r, inner.b),
                Dock::Top => Rect::new(inner.l, inner.t, inner.r, inner.t + ih / 3),
                Dock::Bottom => Rect::new(inner.l, inner.b - ih / 3, inner.r, inner.b),
            };
            frame_rect(&mut buf, w, hgt, inner, 1, if *hot { GUIDE_HOT_MARK } else { GUIDE_EDGE });
            fill(&mut buf, w, hgt, mark, if *hot { GUIDE_HOT_MARK } else { GUIDE_MARK });
        }
        unsafe { present(hwnd, &buf, p.area) };
    });
    OVERLAY_SHOWN.with(|c| c.set(true));
}

unsafe fn present(hwnd: HWND, buf: &[u32], area: Rect) {
    let (w, hgt) = (area.w().max(1), area.h().max(1));
    let screen = GetDC(None);
    let mem = CreateCompatibleDC(Some(screen));
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -hgt,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
    if let Ok(bmp) = CreateDIBSection(Some(mem), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
        if !bits.is_null() {
            std::ptr::copy_nonoverlapping(buf.as_ptr(), bits as *mut u32, buf.len());
            let old = SelectObject(mem, bmp.into());
            let dst = POINT { x: area.l, y: area.t };
            let size = SIZE { cx: w, cy: hgt };
            let src = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let _ = UpdateLayeredWindow(
                hwnd,
                Some(screen),
                Some(&dst),
                Some(&size),
                Some(mem),
                Some(&src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
            SelectObject(mem, old);
        }
        let _ = DeleteObject(bmp.into());
    }
    let _ = DeleteDC(mem);
    ReleaseDC(None, screen);
    let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW);
}
