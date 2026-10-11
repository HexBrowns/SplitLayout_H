//! 自己点検と速度の記録（実機とテストで同じものを使う）
//!
//! 並べ直しのたびに（投げたメッセージで後回しにして、まとめて 1 回）次を確かめ、崩れていればログに `[WARN]` を出す
//! （同じ崩れは 1 回だけ）。実機で決まった手順を操作してもらえば、ログだけで画面の遷移の正否が分かる。
//!
//! - 木と割り付けの整合性（`Node::check` / `Layout::check`）
//! - はめ込んだ窓: 生きている・親が枠・子のスタイル・クロークが残っていない・鍵が木にある・鍵が重複しない
//! - 前のタブは表示されていて、位置が区画の中身と一致する。裏のタブ・最大化で隠れた区画は隠れている
//!   （本体が「表示」で消したものは除く）
//!
//! 並べ直しの時間も数え、重い回は警告し、終了時に回数・平均・最大を 1 行にまとめる。

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::app::{self, with_app};
use crate::frame;
use crate::host::{self, h};
use crate::model::{self, Node};

pub const WM_APP_CHECK: u32 = WM_APP + 8;
/// これより長い並べ直しは警告する（画面の書き換え 3 回分）
const SLOW: Duration = Duration::from_millis(50);

#[derive(Default)]
struct Stats {
    relayouts: u64,
    total: Duration,
    max: Duration,
    checks: u64,
    violations: u64,
    slow_warned: Option<Instant>,
}

thread_local! {
    static STATS: RefCell<Stats> = RefCell::new(Stats::default());
    static REPORTED: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
    static POSTED: Cell<bool> = const { Cell::new(false) };
}

/// 並べ直し 1 回の内訳（重かった回だけログに出す）
#[derive(Default, Clone, Copy)]
pub struct Breakdown {
    pub place: Duration,
    pub show: Duration,
    pub moved: usize,
    pub shown: usize,
    pub hidden: usize,
}

pub fn record_relayout(d: Duration, b: Breakdown) {
    let warn = STATS.with(|s| {
        let mut s = s.borrow_mut();
        s.relayouts += 1;
        s.total += d;
        s.max = s.max.max(d);
        if d > SLOW && s.slow_warned.is_none_or(|t| t.elapsed() > Duration::from_secs(10)) {
            s.slow_warned = Some(Instant::now());
            true
        } else {
            false
        }
    });
    if warn {
        tracing::warn!(
            "並べ直しに {} ms かかった（位置 {} ms で {} 枚・表示 {} ms で出す {} 枚 隠す {} 枚）",
            d.as_millis(),
            b.place.as_millis(),
            b.moved,
            b.show.as_millis(),
            b.shown,
            b.hidden
        );
    }
}

/// 自己点検を後回しで 1 回だけ頼む
pub fn post_check() {
    // 枠を作った直後（主の枠を覚える前）にも並べ直しが走る。宛先の無いうちは頼まない
    // （頼むとスレッド宛ての迷子のメッセージになり、「頼んだ」印が立ったまま点検が二度と走らない。窓の結合テストで見つけた）
    let frame = app::MAIN_FRAME.load(std::sync::atomic::Ordering::Acquire);
    if frame == 0 || POSTED.with(|c| c.replace(true)) {
        return;
    }
    unsafe {
        if PostMessageW(Some(h(frame)), WM_APP_CHECK, WPARAM(0), LPARAM(0)).is_err() {
            POSTED.with(|c| c.set(false));
        }
    }
}

/// 自己点検を行い、新しい崩れをログに出す
pub fn run_check() -> Vec<String> {
    POSTED.with(|c| c.set(false));
    let v = violations();
    STATS.with(|s| {
        let mut s = s.borrow_mut();
        s.checks += 1;
        s.violations += v.len() as u64;
    });
    for m in &v {
        let new = REPORTED.with(|r| r.borrow_mut().insert(m.clone()));
        if new {
            tracing::warn!("自己点検: {m}");
        }
    }
    v
}

pub fn summary_line() -> String {
    STATS.with(|s| {
        let s = s.borrow();
        let avg = if s.relayouts == 0 { 0.0 } else { s.total.as_secs_f64() * 1000.0 / s.relayouts as f64 };
        format!(
            "並べ直し {} 回（平均 {avg:.2} ms、最大 {:.2} ms）、自己点検 {} 回（崩れ {} 件）",
            s.relayouts,
            s.max.as_secs_f64() * 1000.0,
            s.checks,
            s.violations
        )
    })
}

#[cfg(test)]
pub fn checks() -> u64 {
    STATS.with(|s| s.borrow().checks)
}

#[cfg(test)]
pub fn max_relayout() -> Duration {
    STATS.with(|s| s.borrow().max)
}

/// 崩れの一覧（空なら正常）
pub fn violations() -> Vec<String> {
    struct F {
        hwnd: isize,
        root: Node,
        maximized: Option<u64>,
    }
    let Some((frames, embedded, suspended)) = with_app(|app| {
        let frames: Vec<F> =
            app.frames.iter().map(|f| F { hwnd: f.hwnd, root: f.root.clone(), maximized: f.maximized }).collect();
        let embedded: Vec<(isize, String, isize)> = app.embedded.values().map(|e| (e.hwnd, e.key.clone(), e.frame)).collect();
        (frames, embedded, app.suspended)
    }) else {
        return Vec::new();
    };
    if suspended {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut keys = HashSet::new();
    for (_, k, _) in &embedded {
        if !keys.insert(k.clone()) {
            out.push(format!("鍵「{k}」の窓が 2 枚はめ込まれている"));
        }
    }
    for f in &frames {
        let fh = h(f.hwnd);
        if let Err(e) = f.root.check() {
            out.push(format!("木が崩れた: {e}"));
        }
        let m = frame::metrics(fh);
        let client = frame::client_rect(fh);
        let layout = model::layout(&f.root, client, m.header, m.gap, f.maximized);
        if let Err(e) = layout.check(client) {
            out.push(format!("割り付けが崩れた: {e}"));
        }
        let frame_visible = unsafe { IsWindowVisible(fh).as_bool() };
        for (hwnd, key, _) in embedded.iter().filter(|e| e.2 == f.hwnd) {
            let w = h(*hwnd);
            unsafe {
                if !IsWindow(Some(w)).as_bool() {
                    out.push(format!("「{key}」: 壊れた窓が残っている"));
                    continue;
                }
                if GetParent(w).ok() != Some(fh) {
                    out.push(format!("「{key}」: 親が枠でない"));
                }
                let style = GetWindowLongPtrW(w, GWL_STYLE) as u32;
                if style & WS_CHILD.0 == 0 {
                    out.push(format!("「{key}」: 子のスタイルでない"));
                }
                if host::is_cloaked(w) {
                    out.push(format!("「{key}」: クロークが残っている（入力が届かない）"));
                }
                let Some(pane) = f.root.pane_of(key) else {
                    out.push(format!("「{key}」: 木に無いのにはめ込まれている"));
                    continue;
                };
                if !frame_visible || host::host_hidden(w) {
                    continue;
                }
                let pb = layout.pane_box(pane);
                let front = pb.is_some_and(|p| p.visible) && f.root.pane(pane).and_then(|p| p.active_title()) == Some(key.as_str());
                let shown = style & WS_VISIBLE.0 != 0;
                if front && !shown {
                    out.push(format!("「{key}」: 前のタブなのに隠れている"));
                }
                if !front && shown {
                    out.push(format!("「{key}」: 裏のタブなのに出ている"));
                }
                if front {
                    let body = pb.unwrap().body;
                    let mut r = RECT::default();
                    let _ = GetWindowRect(w, &mut r);
                    let mut tl = POINT { x: r.left, y: r.top };
                    let _ = ScreenToClient(fh, &mut tl);
                    let got = (tl.x, tl.y, r.right - r.left, r.bottom - r.top);
                    let want = (body.l, body.t, body.w(), body.h());
                    if got != want {
                        out.push(format!("「{key}」: 位置 {got:?} が区画 {want:?} と違う"));
                    }
                }
            }
        }
    }
    for c in host::cloaked() {
        if embedded.iter().any(|e| e.0 == c) {
            out.push("クロークの一覧にはめ込んだ窓が残っている".into());
        }
    }
    out
}
