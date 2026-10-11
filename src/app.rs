//! 全体の状態と操作
//!
//! 状態は UI スレッドの `thread_local` に 1 つ置く（本体の UI スレッドからしか触らない）。
//! **借りている間に、窓へメッセージを送る Win32 を呼ばない。** はめ込み・移動・キャプチャは、
//! 状態から「何をするか」を取り出してから、借りずに行う（返ってきたメッセージで二重に借りないため）。
//!
//! パネルは**鍵**で覚える。ふつうは窓の題で、シーンのプレビューだけは `host::SCENE_PREVIEW`
//! （題が表示中のシーン名になるので、題で覚えると別のパネルとして扱ってしまう）。

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::atomic::{AtomicIsize, Ordering};

use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{InvalidateRect, MonitorFromRect, MONITOR_DEFAULTTONULL};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::drag::{self, Drop, Source};
use crate::frame::{self, FrameKind};
use crate::host::{self, h, raw, Embedded};
use crate::model::{self, Dock, Layout, Node, Target};
use crate::store::{self, FloatWindow, LayoutSet, Named, Store};

pub const WM_APP_SCAN: u32 = WM_APP + 1;
pub const WM_APP_PANEL_GONE: u32 = WM_APP + 2;
pub const WM_APP_REFRESH_PANEL: u32 = WM_APP + 3;
pub const WM_APP_FLOAT_DROP: u32 = WM_APP + 4;
pub const WM_APP_SWITCH: u32 = WM_APP + 5;
pub const WM_APP_OPEN_FLOAT: u32 = WM_APP + 6;
pub const WM_APP_FRONT: u32 = WM_APP + 7;
pub const TIMER_RESUME: usize = 1;
/// 起動から少し経っても、はめ込めずに隠したままの窓があれば出す
pub const TIMER_UNCLOAK: usize = 2;
pub const UNCLOAK_AFTER_MS: u32 = 5000;

/// 隠れたタブ・最大化で隠れた区画の窓を置く場所（加えて `SW_HIDE` で隠す。`host::set_shown`）
const OFFSCREEN: i32 = -32000;

/// 主の枠（編集メニューのコールバックなど、別の所からメッセージを送る先）
pub static MAIN_FRAME: AtomicIsize = AtomicIsize::new(0);

pub struct Frame {
    pub hwnd: isize,
    pub kind: FrameKind,
    pub root: Node,
    pub maximized: Option<u64>,
    pub layout: Layout,
    pub tabs: Vec<frame::TabBox>,
}

/// マウスの状態（枠をまたぐ操作もあるので、枠ではなく全体に持つ）
pub enum Mouse {
    None,
    /// 区切りをドラッグ中。`original` は始めたときの木（毎回ここから計算し直して誤差を溜めない）
    Splitter { frame: isize, id: u64, span: i32, original: Node },
    /// タブを押した（まだ動かしていない）
    TabPress { pane: u64, key: String, start: POINT },
    /// タブか分離の窓をドラッグ中
    Dragging(drag::Session),
}

pub struct App {
    pub main_window: isize,
    pub store: Store,
    pub writable: bool,
    pub frames: Vec<Frame>,
    pub embedded: HashMap<isize, Embedded>,
    /// この起動の間に一度はめ込んだ窓の鍵（題が変わっても同じタブとして扱う。`ident`）
    pub seen: HashMap<isize, String>,
    pub mouse: Mouse,
    /// 本体が閉じかけている間は、はめ込まない
    pub suspended: bool,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    static SCAN_PENDING: Cell<bool> = const { Cell::new(false) };
    static FRONT_QUEUE: RefCell<Vec<(isize, usize)>> = const { RefCell::new(Vec::new()) };
    static FRONT_POSTED: Cell<bool> = const { Cell::new(false) };
}

/// 状態を借りて f を呼ぶ。借りられなければ（入れ子になった）None
pub fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| match a.try_borrow_mut() {
        Ok(mut g) => g.as_mut().map(f),
        Err(_) => {
            tracing::warn!("状態を二重に借りようとした（操作を 1 つ飛ばした）");
            None
        }
    })
}

/// 借りられなければ黙って None（本体の処理の途中から同期で呼ばれる所で使う）
fn try_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

pub fn install(app: App) {
    APP.with(|a| *a.borrow_mut() = Some(app));
}

pub fn take() -> Option<App> {
    APP.with(|a| a.borrow_mut().take())
}

impl App {
    pub fn frame(&self, hwnd: isize) -> Option<&Frame> {
        self.frames.iter().find(|f| f.hwnd == hwnd)
    }

    pub fn frame_mut(&mut self, hwnd: isize) -> Option<&mut Frame> {
        self.frames.iter_mut().find(|f| f.hwnd == hwnd)
    }

    /// 鍵を持つ枠
    pub fn frame_of_key(&self, key: &str) -> Option<isize> {
        self.frames.iter().find(|f| f.root.contains(key)).map(|f| f.hwnd)
    }

    pub fn embedded_by_key(&self, key: &str) -> Option<&Embedded> {
        self.embedded.values().find(|e| e.key == key)
    }

    pub fn all_keys(&self) -> Vec<String> {
        self.frames.iter().flat_map(|f| f.root.titles()).collect()
    }

    /// 鍵をどの枠の木からも取り除く
    pub fn remove_key_everywhere(&mut self, key: &str) {
        for f in &mut self.frames {
            if f.root.remove_title(key) && f.maximized.is_some_and(|m| f.root.pane(m).is_none()) {
                f.maximized = None;
            }
        }
    }

    pub fn current_set(&self) -> LayoutSet {
        let mut set = LayoutSet::default();
        for f in &self.frames {
            match f.kind {
                FrameKind::Main => set.main = f.root.clone(),
                FrameKind::Float => {
                    let mut r = RECT::default();
                    unsafe {
                        let _ = GetWindowRect(h(f.hwnd), &mut r);
                    }
                    set.windows.push(FloatWindow {
                        rect: [r.left, r.top, r.right - r.left, r.bottom - r.top],
                        root: f.root.clone(),
                    });
                }
            }
        }
        set
    }

    pub fn save(&mut self) {
        self.store.current = self.current_set();
        if !self.writable {
            return;
        }
        if let Err(e) = store::save(&store::path(), &self.store) {
            tracing::warn!("配置を保存できなかった: {e}");
        }
    }
}

fn main_window() -> Option<HWND> {
    with_app(|a| a.main_window).filter(|v| *v != 0).map(h)
}

fn main_frame() -> HWND {
    h(MAIN_FRAME.load(Ordering::Acquire))
}

/// 今のプロジェクトのシーン名。本体の最初のプロジェクトの読み込みの前は読めない（`get_scene_names` が panic する）ので空
pub fn scene_names() -> Vec<String> {
    if !crate::EDIT_HANDLE.is_ready() {
        return Vec::new();
    }
    crate::EDIT_HANDLE.get_scene_names().into_iter().map(|(n, _)| n).collect()
}

/// 窓の鍵を決める（`ident::resolve`）。借りている間に呼ぶ（窓にメッセージは送らない）
fn resolve_keys(app: &App, wins: &[crate::ident::Win], scenes: &[String]) -> Vec<String> {
    let layout_keys = app.all_keys();
    let embedded_keys: std::collections::HashSet<String> = app.embedded.values().map(|e| e.key.clone()).collect();
    let known = crate::ident::Known {
        seen: &app.seen,
        scenes,
        layout_keys: &layout_keys,
        embedded_keys: &embedded_keys,
        rects: &app.store.rects,
    };
    crate::ident::resolve(wins, &known)
}

/// はめ込んだことを覚える（鍵と、次の起動で見分けるための位置）
fn remember(app: &mut App, e: &Embedded) {
    app.seen.insert(e.hwnd, e.key.clone());
    app.store.rects.insert(e.key.clone(), e.top_rect());
}

// ---- 他の所から呼ばれる入口（メッセージで UI スレッドの後回しにする） ----

pub fn post_scan() {
    if MAIN_FRAME.load(Ordering::Acquire) == 0 || SCAN_PENDING.with(|c| c.replace(true)) {
        return;
    }
    unsafe {
        let _ = PostMessageW(Some(main_frame()), WM_APP_SCAN, WPARAM(0), LPARAM(0));
    }
}

pub fn post_panel_gone(panel: isize) {
    unsafe {
        let _ = PostMessageW(Some(main_frame()), WM_APP_PANEL_GONE, WPARAM(panel as usize), LPARAM(0));
    }
}

/// 本体が裏のタブの窓を出そうとした。`how` はログ用（1: 表示 2: 前面 3: 焦点）。
/// 本体の 1 回の操作の中で来た要求をためて、操作が終わってから（投げたメッセージで）まとめて見る
pub fn post_front(panel: isize, how: usize) {
    FRONT_QUEUE.with(|q| q.borrow_mut().push((panel, how)));
    if MAIN_FRAME.load(Ordering::Acquire) != 0 && !FRONT_POSTED.with(|c| c.replace(true)) {
        unsafe {
            let _ = PostMessageW(Some(main_frame()), WM_APP_FRONT, WPARAM(0), LPARAM(0));
        }
    }
}

/// ためた要求を見る。**1 枚だけなら**そのタブを前に出す（「色を編集」で色設定を開くなど）。
/// 2 枚以上をまとめて前面に出したのは本体の並べ直し（外した窓を閉じたときなど）なので、タブは動かさない
pub fn handle_front_requests() {
    FRONT_POSTED.with(|c| c.set(false));
    let reqs: Vec<(isize, usize)> = FRONT_QUEUE.with(|q| q.borrow_mut().drain(..).collect());
    let mut panels: Vec<isize> = reqs.iter().map(|r| r.0).collect();
    panels.sort_unstable();
    panels.dedup();
    match panels.as_slice() {
        [one] => {
            let how = reqs.iter().find(|r| r.0 == *one).map(|r| r.1).unwrap_or(0);
            bring_to_front(*one, how as isize);
        }
        [] => {}
        many => tracing::debug!("本体が {} 枚をまとめて前面に出した（並べ直し）ので、タブは動かさない", many.len()),
    }
}

/// 裏のタブを前に出す（本体が「色を編集」などでその窓を開こうとしたとき）
pub fn bring_to_front(panel: isize, how: isize) {
    let done = with_app(|app| {
        let e = app.embedded.get(&panel)?;
        let (key, frame) = (e.key.clone(), e.frame);
        let f = app.frame_mut(frame)?;
        let pane = f.root.pane_of(&key)?;
        if f.root.pane(pane)?.active_title() == Some(key.as_str()) && f.maximized.is_none_or(|m| m == pane) {
            return None;
        }
        f.root.activate_title(&key);
        if f.maximized.is_some_and(|m| m != pane) {
            f.maximized = None;
        }
        Some((key, frame))
    })
    .flatten();
    if let Some((key, frame)) = done {
        let how = match how {
            1 => "表示",
            2 => "前面",
            _ => "焦点",
        };
        tracing::info!("本体が「{key}」を出そうとした（{how}）ので、タブを前に出した");
        relayout(h(frame));
        save();
    }
}

pub fn post_refresh_for_panel(panel: isize) {
    unsafe {
        let _ = PostMessageW(Some(main_frame()), WM_APP_REFRESH_PANEL, WPARAM(panel as usize), LPARAM(0));
    }
}

/// 本体が分離の窓を出したその場で呼ばれる（WinEvent）。配置に入れる窓なら真（呼ぶ側がクロークする）。
/// シーン名はここでは読まない（本体の処理の途中なので）。子ウィンドウを持たない窓は、配置にシーンのプレビューがあれば入れる候補とみなす
pub fn wants_window(hwnd: HWND, has_child: bool, title: &str) -> bool {
    if title.is_empty() {
        return false;
    }
    try_app(|app| {
        if app.suspended || app.embedded.contains_key(&raw(hwnd)) {
            return false;
        }
        let main = h(app.main_window);
        if !host::is_panel_window(hwnd, main) {
            return false;
        }
        app.frame_of_key(title).is_some() || (!has_child && app.frame_of_key(host::SCENE_PREVIEW).is_some())
    })
    .unwrap_or(false)
}

// ---- はめ込みと並べ方 ----

/// 分離の窓を見回して、配置にあるものをはめ込み、ほかはドラッグを見られるようにする。
/// はめ込まなかった窓のクロークは、本体のメイン窓が見えていれば（起動が済んでいれば）解く。`force` なら必ず解く
pub fn scan(force_uncloak: bool) {
    SCAN_PENDING.with(|c| c.set(false));
    let Some(main) = main_window() else { return };
    let windows = host::enum_panel_windows(main);
    let scenes = scene_names();
    let wins: Vec<crate::ident::Win> = windows.iter().map(|w| host::describe(*w)).collect();
    let keys = with_app(|app| resolve_keys(app, &wins, &scenes)).unwrap_or_else(|| wins.iter().map(|w| w.title.clone()).collect());
    let keyed: Vec<(HWND, String)> = windows.iter().copied().zip(keys).collect();
    // v0.1 はシーンのプレビューを題（シーン名）で覚えていた。そのタブを鍵に書き換える
    if let Some((w, _)) = keyed.iter().find(|(_, k)| k == host::SCENE_PREVIEW) {
        let title = host::title_of(*w);
        with_app(|app| migrate_scene_preview(app, &title));
    }
    // どの窓をどの枠へ入れるかを決める（借りている間は窓に触らない）
    let (plan, suspended, ours): (Vec<(HWND, HWND, String)>, bool, Vec<HWND>) = with_app(|app| {
        let ours: Vec<HWND> = windows.iter().copied().filter(|w| contains_frame(app, *w)).collect();
        if app.suspended {
            return (Vec::new(), true, ours);
        }
        let mut plan = Vec::new();
        for (w, key) in &keyed {
            if app.embedded.contains_key(&raw(*w)) || ours.contains(w) {
                continue;
            }
            if let Some(frame) = app.frame_of_key(key) {
                // 見えていない枠へは入れない（入れるとパネルごと見えなくなる）
                if unsafe { IsWindowVisible(h(frame)).as_bool() } {
                    plan.push((*w, h(frame), key.clone()));
                }
            }
        }
        (plan, false, ours)
    })
    .unwrap_or_default();
    for &w in &windows {
        if !ours.contains(&w) {
            host::hook_float(w);
        }
    }
    let mut touched: Vec<HWND> = Vec::new();
    for (w, frame, key) in plan {
        if let Some(e) = host::embed(w, frame, key) {
            tracing::info!("はめ込んだ: {}", e.key);
            with_app(|app| {
                remember(app, &e);
                app.embedded.insert(e.hwnd, e);
            });
            if !touched.contains(&frame) {
                touched.push(frame);
            }
        }
    }
    for f in touched {
        relayout(f);
    }
    // はめ込まなかった窓のクロークを解く（閉じかけている間は隠したまま）
    if !suspended {
        let started = unsafe { IsWindowVisible(main).as_bool() };
        if started || force_uncloak {
            for c in host::cloaked() {
                let embedded = with_app(|app| app.embedded.contains_key(&c)).unwrap_or(true);
                if !embedded {
                    host::uncloak(h(c));
                }
            }
        }
    }
}

fn migrate_scene_preview(app: &mut App, title: &str) {
    if title.is_empty() || app.frame_of_key(host::SCENE_PREVIEW).is_some() || app.frame_of_key(title).is_none() {
        return;
    }
    for f in &mut app.frames {
        f.root.rename_title(title, host::SCENE_PREVIEW);
    }
    for n in &mut app.store.saved {
        n.layout.main.rename_title(title, host::SCENE_PREVIEW);
        for w in &mut n.layout.windows {
            w.root.rename_title(title, host::SCENE_PREVIEW);
        }
    }
    tracing::info!("タブ「{title}」をシーンのプレビューとして覚え直した");
    app.save();
}

/// 分離の窓が、こちらの枠を中に持っているか（主の枠ごと本体で分離したとき）
fn contains_frame(app: &App, w: HWND) -> bool {
    app.frames.iter().any(|f| unsafe { IsChild(w, h(f.hwnd)).as_bool() })
}

/// 枠の中を並べ直す（区画の矩形・タブ・はめ込んだ窓の位置）
pub fn relayout(frame_hwnd: HWND) {
    let started = std::time::Instant::now();
    let moves: Vec<(isize, RECT, bool)> = with_app(|app| {
        let key = raw(frame_hwnd);
        let embedded: Vec<(isize, String)> =
            app.embedded.values().filter(|e| e.frame == key).map(|e| (e.hwnd, e.key.clone())).collect();
        // タブの名前（シーンのプレビューは今のシーン名を出す）
        let all_embedded: HashMap<String, isize> = app.embedded.values().map(|e| (e.key.clone(), e.hwnd)).collect();
        let Some(f) = app.frame_mut(key) else { return Vec::new() };
        let labels: HashMap<String, String> =
            f.root.titles().into_iter().map(|k| (k.clone(), host::label(&k, all_embedded.get(&k).copied()))).collect();
        let metrics = frame::metrics(frame_hwnd);
        let client = frame::client_rect(frame_hwnd);
        f.layout = model::layout(&f.root, client, metrics.header, metrics.gap, f.maximized);
        f.tabs = frame::compute_tabs(frame_hwnd, &f.root, &f.layout, &metrics, &labels);
        let mut moves = Vec::new();
        for (hwnd, k) in embedded {
            let shown = f
                .root
                .pane_of(&k)
                .and_then(|id| Some(f.layout.pane_box(id)?.visible && f.root.pane(id)?.active_title() == Some(k.as_str())))
                .unwrap_or(false);
            let rect = f
                .root
                .pane_of(&k)
                .and_then(|id| {
                    let pane = f.root.pane(id)?;
                    let pb = f.layout.pane_box(id)?;
                    (pb.visible && pane.active_title() == Some(k.as_str())).then_some(pb.body)
                })
                .map(|b| RECT { left: b.l, top: b.t, right: b.r, bottom: b.b })
                .unwrap_or_else(|| {
                    let w = (client.w()).max(100);
                    let hh = (client.h()).max(100);
                    RECT { left: OFFSCREEN, top: OFFSCREEN, right: OFFSCREEN + w, bottom: OFFSCREEN + hh }
                });
            moves.push((hwnd, rect, shown));
        }
        moves
    })
    .unwrap_or_default();
    // 動かすのは、前に出ていて位置が前回と違う窓だけ。隠れている窓（裏のタブ・最大化で隠れた区画）は動かさない。
    // 本体のパネルは大きさが変わるたびに描き直すので、毎回すべてを動かすと実機で 1 回 100 ms を超えた（v0.3.3 の記録）
    let to_move: Vec<(isize, RECT)> = moves
        .iter()
        .filter(|(hwnd, r, shown)| *shown && host::target_of(*hwnd) != Some(*r))
        .map(|(hwnd, r, _)| (*hwnd, *r))
        .collect();
    for (hwnd, r) in &to_move {
        host::set_target(*hwnd, *r);
    }
    let mut bd = crate::check::Breakdown { moved: to_move.len(), ..Default::default() };
    let t_place = std::time::Instant::now();
    host::with_our_move(|| unsafe {
        if to_move.is_empty() {
            return;
        }
        if let Ok(mut dwp) = BeginDeferWindowPos(to_move.len() as i32) {
            for (hwnd, r) in &to_move {
                match DeferWindowPos(
                    dwp,
                    h(*hwnd),
                    None,
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                ) {
                    Ok(d) => dwp = d,
                    Err(_) => return,
                }
            }
            let _ = EndDeferWindowPos(dwp);
        }
    });
    bd.place = t_place.elapsed();
    // 前に出ているタブだけを表示し、裏のタブは隠す
    let t_show = std::time::Instant::now();
    for (hwnd, _, shown) in &moves {
        match host::set_shown(h(*hwnd), *shown) {
            Some(true) => bd.shown += 1,
            Some(false) => bd.hidden += 1,
            None => {}
        }
    }
    bd.show = t_show.elapsed();
    unsafe {
        let _ = InvalidateRect(Some(frame_hwnd), None, false);
    }
    crate::check::record_relayout(started.elapsed(), bd);
    crate::check::post_check();
}

pub fn relayout_all() {
    let frames: Vec<isize> = with_app(|app| app.frames.iter().map(|f| f.hwnd).collect()).unwrap_or_default();
    for f in frames {
        relayout(h(f));
    }
}

pub fn save() {
    with_app(|app| app.save());
}

/// 本体の窓が壊された（エリアへ戻された・閉じられた）。タブは灰色で残す
pub fn panel_gone(panel: isize) {
    let frame = with_app(|app| app.embedded.remove(&panel).map(|e| e.frame)).flatten();
    if let Some(f) = frame {
        relayout(h(f));
    }
}

/// はめ込んだ窓の表示か題が変わった（タブの名前と灰色を描き直す）
pub fn refresh_for_panel(panel: isize) {
    if let Some(f) = with_app(|app| app.embedded.get(&panel).map(|e| e.frame)).flatten() {
        relayout(h(f));
    }
}

/// 鍵を配置から外し、はめ込んでいれば本体の分離の窓に戻す
pub fn remove_key(key: &str, at: Option<POINT>) {
    let (e, frames) = with_app(|app| {
        app.remove_key_everywhere(key);
        let k = app.embedded.iter().find(|(_, e)| e.key == key).map(|(k, _)| *k);
        let e = k.and_then(|k| app.embedded.remove(&k));
        app.save();
        (e, app.frames.iter().map(|f| f.hwnd).collect::<Vec<_>>())
    })
    .unwrap_or((None, Vec::new()));
    if let Some(e) = e {
        host::release(&e, at, false);
    }
    for f in frames {
        relayout(h(f));
    }
}

/// 分離の窓（はめ込んでいないもの）を、枠の落とす先へ入れる
pub fn add_window(window: HWND, frame: HWND, target: Target, dock: Dock) {
    let scenes = scene_names();
    let win = host::describe(window);
    // 灰色のタブ（窓の無いタブ）を埋めるかを決めるために、今ある分離の窓の題を集める
    let live_titles: Vec<String> = main_window()
        .map(|m| host::enum_panel_windows(m).into_iter().filter(|w| *w != window).map(host::title_of).collect())
        .unwrap_or_default();
    let key = with_app(|app| {
        let key = resolve_keys(app, std::slice::from_ref(&win), &scenes).remove(0);
        // 区画の中央へ落とし、その区画の前のタブが灰色なら、そのタブとして入れる
        let fill = match (target, dock) {
            (Target::Pane(p), Dock::Center) => {
                let active = app.frame(raw(frame)).and_then(|f| f.root.pane(p)).and_then(|p| p.active_title().map(|s| s.to_string()));
                let embedded_keys: std::collections::HashSet<String> = app.embedded.values().map(|e| e.key.clone()).collect();
                crate::ident::placeholder_to_fill(&win, &key, active.as_deref(), &app.all_keys(), &live_titles, &embedded_keys)
            }
            _ => None,
        };
        if let Some(k) = fill {
            tracing::info!("「{}」を灰色のタブ「{k}」として入れた", win.title);
            app.seen.insert(win.id, k.clone());
            return Some(k);
        }
        app.remove_key_everywhere(&key);
        let f = app.frame_mut(raw(frame))?;
        if !f.root.insert(target, &key, dock) {
            f.root.insert(Target::Root, &key, Dock::Center);
        }
        f.maximized = None;
        Some(key)
    })
    .flatten();
    let Some(key) = key else { return };
    host::hook_float(window);
    if let Some(e) = host::embed(window, frame, key) {
        with_app(|app| {
            remember(app, &e);
            app.embedded.insert(e.hwnd, e);
        });
    }
    with_app(|app| app.save());
    relayout_all();
}

/// タブを動かす（同じ枠の中でも、別の枠へでも）
pub fn move_tab(key: &str, from_pane: u64, drop: Drop) {
    let moved = with_app(|app| {
        // 1 枚だけの区画を自分の辺や中央へ落とすのは何もしない
        if let Target::Pane(p) = drop.target {
            if p == from_pane {
                let single = app
                    .frames
                    .iter()
                    .find_map(|f| f.root.pane(p))
                    .map(|pane| pane.tabs.len() <= 1)
                    .unwrap_or(true);
                if drop.dock == Dock::Center || single {
                    return None;
                }
            }
        }
        app.remove_key_everywhere(key);
        let f = app.frame_mut(drop.frame)?;
        if !f.root.insert(drop.target, key, drop.dock) {
            f.root.insert(Target::Root, key, Dock::Center);
        }
        f.maximized = None;
        let e = app.embedded.values().find(|e| e.key == key).map(|e| (e.hwnd, e.frame));
        app.save();
        Some(e)
    })
    .flatten();
    let Some(e) = moved else { return };
    if let Some((hwnd, old_frame)) = e {
        if old_frame != drop.frame {
            // 別の枠へ: 親を付け替える（借りずに）
            if let Some(mut e) = with_app(|app| app.embedded.remove(&hwnd)).flatten() {
                host::move_to_frame(&mut e, h(drop.frame));
                with_app(|app| app.embedded.insert(hwnd, e));
            }
        }
    }
    relayout_all();
}

// ---- 分離の窓のドラッグ（host の float_proc から） ----

pub fn float_moving(window: isize, pt: POINT) {
    let start = with_app(|app| match &app.mouse {
        Mouse::None => true,
        Mouse::Dragging(s) => !matches!(s.source, Source::Window { hwnd } if hwnd == window),
        _ => false,
    })
    .unwrap_or(false);
    if start {
        let s = drag::Session::new(Source::Window { hwnd: window });
        with_app(|app| app.mouse = Mouse::Dragging(s));
    }
    drag::update(pt);
}

pub fn float_move_ended(window: isize) {
    let ours = with_app(|app| matches!(&app.mouse, Mouse::Dragging(s) if matches!(s.source, Source::Window { hwnd } if hwnd == window)))
        .unwrap_or(false);
    if ours {
        // 本体の移動の処理が終わってから落とす
        unsafe {
            let _ = PostMessageW(Some(main_frame()), WM_APP_FLOAT_DROP, WPARAM(window as usize), LPARAM(0));
        }
    }
}

pub fn float_drop(window: isize) {
    let session = with_app(|app| match std::mem::replace(&mut app.mouse, Mouse::None) {
        Mouse::Dragging(s) => Some(s),
        other => {
            app.mouse = other;
            None
        }
    })
    .flatten();
    let Some(s) = session else { return };
    drag::hide_overlay();
    // Esc で取り消すと OS が窓を運び始めの位置へ戻す。戻っていたら落とさない
    if !drag::window_was_moved(&s) {
        return;
    }
    if let (Source::Window { hwnd }, Some(d)) = (&s.source, s.drop) {
        if *hwnd == window && unsafe { IsWindow(Some(h(window))).as_bool() } {
            add_window(h(window), h(d.frame), d.target, d.dock);
        }
    }
}

// ---- 終了と再開 ----

/// メイン窓の WM_CLOSE: 今の配置を保存してから、全部を本体の分離の窓に戻す（クロークで隠したまま）
pub fn before_host_close() {
    let list: Vec<Embedded> = with_app(|app| {
        if app.suspended {
            return Vec::new();
        }
        app.save();
        app.suspended = true;
        app.mouse = Mouse::None;
        app.embedded.drain().map(|(_, e)| e).collect()
    })
    .unwrap_or_default();
    drag::hide_overlay();
    if !list.is_empty() {
        tracing::info!("終了の前に {} 枚を本体の分離の窓に戻した。{}", list.len(), crate::check::summary_line());
    }
    for e in &list {
        host::release(e, None, true);
    }
}

pub fn schedule_resume() {
    unsafe {
        let f = main_frame();
        if IsWindow(Some(f)).as_bool() {
            SetTimer(Some(f), TIMER_RESUME, 1000, None);
        }
    }
}

/// 閉じるのが取り消されていたら、はめ込み直す
pub fn resume() {
    unsafe {
        let _ = KillTimer(Some(main_frame()), TIMER_RESUME);
    }
    let alive = main_window().map(|m| unsafe { IsWindow(Some(m)).as_bool() && IsWindowVisible(m).as_bool() }).unwrap_or(false);
    if !alive {
        return;
    }
    let resumed = with_app(|app| std::mem::replace(&mut app.suspended, false)).unwrap_or(false);
    if resumed {
        tracing::info!("閉じるのが取り消されたので、はめ込み直す");
        scan(true);
    }
}

/// 全部を本体の分離の窓に戻す（枠が壊れるとき・プラグインが外れるとき・配置の切り替え）
pub fn release_all(hide: bool) {
    let list: Vec<Embedded> = with_app(|app| app.embedded.drain().map(|(_, e)| e).collect()).unwrap_or_default();
    for e in &list {
        host::release(e, None, hide);
    }
}

/// 枠に入っているものだけを戻す（別窓を閉じるとき・枠が隠れたとき）
pub fn release_frame(frame: isize) {
    let list: Vec<Embedded> = with_app(|app| {
        let keys: Vec<isize> = app.embedded.iter().filter(|(_, e)| e.frame == frame).map(|(k, _)| *k).collect();
        keys.into_iter().filter_map(|k| app.embedded.remove(&k)).collect()
    })
    .unwrap_or_default();
    for e in &list {
        host::release(e, None, false);
    }
}

// ---- 別窓と配置の切り替え ----

pub fn open_float(rect: Option<[i32; 4]>, root: Node) -> Option<HWND> {
    let main = main_window()?;
    let r = rect.filter(|r| on_screen(r)).unwrap_or_else(|| default_float_rect(main));
    let hwnd = frame::create_float(main, r)?;
    with_app(|app| {
        app.frames.push(Frame {
            hwnd: raw(hwnd),
            kind: FrameKind::Float,
            root,
            maximized: None,
            layout: Layout::default(),
            tabs: Vec::new(),
        })
    });
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
    relayout(hwnd);
    Some(hwnd)
}

fn on_screen(r: &[i32; 4]) -> bool {
    let rect = RECT { left: r[0], top: r[1], right: r[0] + r[2], bottom: r[1] + r[3] };
    r[2] > 50 && r[3] > 50 && unsafe { !MonitorFromRect(&rect, MONITOR_DEFAULTTONULL).is_invalid() }
}

fn default_float_rect(main: HWND) -> [i32; 4] {
    let mut m = RECT::default();
    unsafe {
        let _ = GetWindowRect(main, &mut m);
    }
    [m.left + 80, m.top + 80, 800, 600]
}

/// 別窓の × : 中身を本体の分離の窓に戻し、配置から別窓を外す
pub fn close_float(frame: isize) {
    release_frame(frame);
    with_app(|app| {
        app.frames.retain(|f| f.hwnd != frame);
        app.save();
    });
    unsafe {
        let _ = DestroyWindow(h(frame));
    }
}

/// `new` なら空の別窓を 1 枚開く。偽なら起動時: 前回の別窓を開き直す（1 回だけ）
pub fn open_saved_or_new_float(new: bool) {
    if new {
        open_float(None, Node::default());
        save();
        return;
    }
    let windows = with_app(|app| {
        if app.frames.iter().any(|f| f.kind == FrameKind::Float) {
            return Vec::new();
        }
        app.store.current.windows.clone()
    })
    .unwrap_or_default();
    for w in windows {
        open_float(Some(w.rect), w.root);
    }
    scan(false);
}

/// 保存した配置へ切り替える
pub fn switch_to(index: usize) {
    let set = with_app(|app| app.store.saved.get(index).map(|n| n.layout.clone())).flatten();
    let Some(mut set) = set else {
        tracing::info!("配置 {} は保存されていない", index + 1);
        return;
    };
    set.renumber();
    apply_set(set);
    tracing::info!("配置 {} に切り替えた", index + 1);
}

pub fn apply_set(set: LayoutSet) {
    // 入れ替えの間は窓を隠しておく（新しい配置に無いものは、見回りの最後に出す）
    release_all(true);
    let floats: Vec<isize> = with_app(|app| {
        let floats = app.frames.iter().filter(|f| f.kind == FrameKind::Float).map(|f| f.hwnd).collect();
        app.frames.retain(|f| f.kind == FrameKind::Main);
        if let Some(f) = app.frames.first_mut() {
            f.root = set.main.clone();
            f.maximized = None;
        }
        floats
    })
    .unwrap_or_default();
    for f in floats {
        unsafe {
            let _ = DestroyWindow(h(f));
        }
    }
    for w in set.windows {
        open_float(Some(w.rect), w.root);
    }
    scan(true);
    relayout_all();
    save();
}

pub fn save_as(name: &str) {
    with_app(|app| {
        let layout = app.current_set();
        if let Some(n) = app.store.saved.iter_mut().find(|n| n.name == name) {
            n.layout = layout;
        } else {
            app.store.saved.push(Named { name: name.to_string(), layout });
        }
        app.save();
    });
    tracing::info!("配置「{name}」を保存した");
}

pub fn overwrite(index: usize) {
    with_app(|app| {
        let layout = app.current_set();
        if let Some(n) = app.store.saved.get_mut(index) {
            n.layout = layout;
        }
        app.save();
    });
}

pub fn delete_saved(index: usize) {
    with_app(|app| {
        if index < app.store.saved.len() {
            app.store.saved.remove(index);
        }
        app.save();
    });
}
