//! 本体の窓を扱う: 分離したパネルの窓を見つける・はめ込む・戻す・見張る・隠す
//!
//! 前提（`.claude/rules/au2-plugin-checklist.md`「本体の窓の構成」）:
//! 分離したパネルは、所有者がメイン窓のクラス `aviutl2Manager` の最上位の窓になる。
//! それを `WS_CHILD` にして `SetParent` で枠の子にすると、そのまま描かれて操作できる。
//!
//! 起動・終了・配置の切り替えの間は、分離の窓を DWM のクローク（表示の状態のまま画面に出さない）で隠し、
//! 窓がばらけて見えないようにする。本体が保存する位置と表示の状態は変わらない。
//!
//! ここの関数は `App` を借りない（窓のメッセージが返ってきても二重に借りないように）。

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};

use windows::core::{BOOL, PCWSTR};
use windows::Win32::Foundation::{HMODULE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DwmSetWindowAttribute, DWMWA_CLOAK, DWMWA_CLOAKED};
use windows::Win32::System::LibraryLoader::{
    GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetFocus, SetFocus};
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::app;

pub const HOST_CLASS: &str = "aviutl2Manager";
/// シーンのプレビュー（`[Window.scene.preview]`）の鍵。窓の題が表示中のシーン名になるので、題では覚えない
pub const SCENE_PREVIEW: &str = crate::ident::SCENE_PREVIEW;

const PANEL_SUBCLASS: usize = 0x534C_4801;
const FLOAT_SUBCLASS: usize = 0x534C_4802;
const MAIN_SUBCLASS: usize = 0x534C_4803;

/// はめ込んだ窓の元の状態（戻すときに使う）
#[derive(Clone, Debug)]
pub struct Embedded {
    pub hwnd: isize,
    /// 配置での鍵（ふつうは窓の題。シーンのプレビューは `SCENE_PREVIEW`）
    pub key: String,
    /// はめ込んでいる枠
    pub frame: isize,
    /// 中の窓の手掛かり（`fingerprint`。プラグインの窓だけ）
    pub fp: Option<String>,
    style: isize,
    owner: isize,
    rect: RECT,
}

thread_local! {
    /// 自分が窓を動かしている間は真（本体が動かしたのと区別する）
    static OUR_MOVE: Cell<bool> = const { Cell::new(false) };
    /// はめ込んだ窓ごとの、枠の中で割り当てた位置（枠のクライアント座標）
    static TARGETS: RefCell<HashMap<isize, RECT>> = RefCell::new(HashMap::new());
    /// ドラッグを見るためにサブクラス化した分離の窓
    static FLOAT_HOOKED: RefCell<HashSet<isize>> = RefCell::new(HashSet::new());
    /// クロークしている窓
    static CLOAKED: RefCell<HashSet<isize>> = RefCell::new(HashSet::new());
    static WIN_EVENT: Cell<isize> = const { Cell::new(0) };
    /// 最後にアクティブにした（とみなした）はめ込みの窓
    static ACTIVE_PANEL: Cell<isize> = const { Cell::new(0) };
    /// 裏のタブ・最大化で隠れた区画として、こちらが `SW_HIDE` で隠した窓（戻すときに表示へ戻す）
    static HIDDEN_BY_US: RefCell<HashSet<isize>> = RefCell::new(HashSet::new());
    /// 本体が隠した窓（「表示」メニューで消した）。こちらからは出さない
    static HOST_HIDDEN: RefCell<HashSet<isize>> = RefCell::new(HashSet::new());
}

/// 裏のタブは本当に隠す。画面の外に置くだけだと本体からは「出ている」ことになり、
/// 「ショートカットキー操作の再生はシーン再生を優先」がオンのとき、裏のシーンのプレビューで再生されてしまう
/// 出したら Some(true)、隠したら Some(false)、何もしなければ None
pub fn set_shown(hwnd: HWND, show: bool) -> Option<bool> {
    let key = raw(hwnd);
    unsafe {
        if show {
            if HIDDEN_BY_US.with(|s| s.borrow_mut().remove(&key)) {
                with_our_move(|| {
                    let _ = ShowWindow(hwnd, SW_SHOWNA);
                });
                return Some(true);
            }
        } else if IsWindowVisible(hwnd).as_bool() && !HOST_HIDDEN.with(|s| s.borrow().contains(&key)) {
            with_our_move(|| {
                let _ = ShowWindow(hwnd, SW_HIDE);
            });
            HIDDEN_BY_US.with(|s| {
                s.borrow_mut().insert(key);
            });
            return Some(false);
        }
        None
    }
}

/// 本体が隠した（「表示」で消した）窓か
pub fn host_hidden(hwnd: HWND) -> bool {
    HOST_HIDDEN.with(|s| s.borrow().contains(&raw(hwnd)))
}

fn forget_shown(hwnd: isize) -> bool {
    HOST_HIDDEN.with(|s| {
        s.borrow_mut().remove(&hwnd);
    });
    HIDDEN_BY_US.with(|s| s.borrow_mut().remove(&hwnd))
}

/// はめ込んだ窓をアクティブにする。最上位の窓だった頃はクリックで OS が `WM_ACTIVATE` を送り、焦点も移していた。
/// 子では起きないので、前の窓に非アクティブ、新しい窓にアクティブを知らせ、焦点を移す
/// （知らせないと、本体が再生のショートカットの宛先を決められないらしい。推測。空白キーで再生できなかった）
pub fn activate_panel(hwnd: HWND) {
    let key = raw(hwnd);
    let prev = ACTIVE_PANEL.with(|c| c.replace(key));
    unsafe {
        if prev != key {
            if prev != 0 && IsWindow(Some(h(prev))).as_bool() {
                SendMessageW(h(prev), WM_ACTIVATE, Some(WPARAM(WA_INACTIVE as usize)), Some(LPARAM(key)));
            }
            SendMessageW(hwnd, WM_ACTIVATE, Some(WPARAM(WA_CLICKACTIVE as usize)), Some(LPARAM(prev)));
        }
        let focus = GetFocus();
        if focus != hwnd && !IsChild(hwnd, focus).as_bool() {
            let _ = SetFocus(Some(hwnd));
        }
    }
}

fn forget_active(hwnd: isize) {
    ACTIVE_PANEL.with(|c| {
        if c.get() == hwnd {
            c.set(0);
        }
    });
}

pub fn h(v: isize) -> HWND {
    HWND(v as *mut _)
}

pub fn raw(hwnd: HWND) -> isize {
    hwnd.0 as isize
}

pub fn class_of(hwnd: HWND) -> String {
    let mut buf = [0u16; 128];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

pub fn title_of(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetWindowTextW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

fn has_child(hwnd: HWND) -> bool {
    unsafe { GetWindow(hwnd, GW_CHILD).is_ok_and(|c| !c.0.is_null()) }
}

/// プラグインの窓の、中の窓（`register_window_client` した窓）の手掛かり「クラス|題」。
/// 本体は分離の窓の題を言語ファイルで訳すが、中の窓のクラスと題は変わらない（`ident` の 2）。
/// 子が無い・中の窓に題が無い（クラスだけでは同じものが多い）なら None
///
/// 中の窓はプラグインのスレッドの窓のことがある（aviutl2-eframe）。題は `InternalGetWindowText` で読み、窓へメッセージを送らない
/// （WinEvent の中、本体の処理の途中からも呼ぶので、よそのスレッドを待たない）
pub fn fingerprint(hwnd: HWND) -> Option<String> {
    let child = unsafe { GetWindow(hwnd, GW_CHILD).ok().filter(|c| !c.0.is_null())? };
    let mut buf = [0u16; 256];
    let n = unsafe { InternalGetWindowText(child, &mut buf) };
    let title = String::from_utf16_lossy(&buf[..n.max(0) as usize]);
    if title.is_empty() {
        return None;
    }
    Some(format!("{}|{}", class_of(child), title))
}

/// 見分けるための窓の情報（題・子の有無・位置）
pub fn describe(hwnd: HWND) -> crate::ident::Win {
    let mut r = RECT::default();
    unsafe {
        let _ = GetWindowRect(hwnd, &mut r);
    }
    crate::ident::Win {
        id: raw(hwnd),
        title: title_of(hwnd),
        child: has_child(hwnd),
        fp: fingerprint(hwnd),
        rect: [r.left, r.top, r.right - r.left, r.bottom - r.top],
    }
}

/// 配置での鍵（題とシーン名だけで決める簡易版。メニューの一覧用）。本体が自分で描く窓（子ウィンドウなし）で、題がシーン名のどれかなら、シーンのプレビュー
pub fn panel_key(hwnd: HWND, scene_names: &[String]) -> String {
    let title = title_of(hwnd);
    if !has_child(hwnd) && scene_names.iter().any(|s| *s == title) {
        SCENE_PREVIEW.to_string()
    } else {
        title
    }
}

/// タブに出す名前
pub fn label(key: &str, hwnd: Option<isize>) -> String {
    if key == SCENE_PREVIEW {
        match hwnd {
            Some(w) => format!("シーン: {}", title_of(h(w))),
            None => "シーンのプレビュー".to_string(),
        }
    } else {
        // はめ込んでいる間は窓の今の題（色設定は編集の対象で題が変わる）。窓が無ければ覚えている名前
        hwnd.map(|w| title_of(h(w))).filter(|t| !t.is_empty()).unwrap_or_else(|| key.to_string())
    }
}

pub fn with_our_move<R>(f: impl FnOnce() -> R) -> R {
    let prev = OUR_MOVE.with(|c| c.replace(true));
    let r = f();
    OUR_MOVE.with(|c| c.set(prev));
    r
}

/// こちらが窓を動かしている最中か（その間の WinEvent では何もしない。はめ込みの途中の `SetParent` で
/// 表示の通知が来て、子になりかけの窓をクロークしてしまうため。窓の結合テストで見つけた）
fn in_our_move() -> bool {
    OUR_MOVE.with(|c| c.get())
}

/// 前回その窓に割り当てた位置（同じなら動かさない）
pub fn target_of(hwnd: isize) -> Option<RECT> {
    TARGETS.with(|t| t.borrow().get(&hwnd).copied())
}

pub fn set_target(hwnd: isize, rect: RECT) {
    TARGETS.with(|t| {
        t.borrow_mut().insert(hwnd, rect);
    });
}

fn forget_target(hwnd: isize) {
    TARGETS.with(|t| {
        t.borrow_mut().remove(&hwnd);
    });
}

// ---- クローク ----

/// クロークは最上位の窓にしか掛け外しできない。**子になった窓のクロークは解けず（`E_HANDLE`）、
/// 描かれてはいてもマウスの当たり判定から外れたままになる**（入力が親へ素通りする。試験用の窓で確認）。
/// だから、はめ込む前（最上位のうち）に必ず解く
fn set_cloak(hwnd: HWND, on: bool) -> bool {
    let v = BOOL::from(on);
    unsafe { DwmSetWindowAttribute(hwnd, DWMWA_CLOAK, &v as *const BOOL as *const _, std::mem::size_of::<BOOL>() as u32).is_ok() }
}

/// DWM がこの窓をクロークしているか（自分が掛けたものに限らない）
pub fn is_cloaked(hwnd: HWND) -> bool {
    let mut v = 0u32;
    unsafe {
        DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut v as *mut u32 as *mut _, std::mem::size_of::<u32>() as u32).is_ok() && v != 0
    }
}

pub fn cloak(hwnd: HWND) {
    // 子の窓には掛からない（`E_HANDLE`）。掛かったときだけ一覧に入れる
    if set_cloak(hwnd, true) {
        CLOAKED.with(|c| {
            c.borrow_mut().insert(raw(hwnd));
        });
    }
}

pub fn uncloak(hwnd: HWND) {
    if !set_cloak(hwnd, false) {
        tracing::warn!("クロークを解けなかった: {}", title_of(hwnd));
    }
    CLOAKED.with(|c| {
        c.borrow_mut().remove(&raw(hwnd));
    });
}

pub fn cloaked() -> Vec<isize> {
    CLOAKED.with(|c| c.borrow().iter().copied().collect())
}

/// 壊れた窓を一覧から除く
fn forget_cloak(hwnd: isize) {
    CLOAKED.with(|c| {
        c.borrow_mut().remove(&hwnd);
    });
}

// ---- 見つける・はめ込む・戻す ----

/// 本体の分離したパネルの窓か（同じプロセス・クラス `aviutl2Manager`・所有者がメイン窓）
pub fn is_panel_window(hwnd: HWND, main: HWND) -> bool {
    unsafe {
        if hwnd == main || !IsWindow(Some(hwnd)).as_bool() {
            return false;
        }
        let mut pid = 0u32;
        let mut main_pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        GetWindowThreadProcessId(main, Some(&mut main_pid));
        pid == main_pid && class_of(hwnd) == HOST_CLASS && GetWindow(hwnd, GW_OWNER).ok() == Some(main)
    }
}

/// 本体の分離したパネルの窓をすべて（見えていないものも）。題の無いものは除く（分離の途中の一瞬の窓）
pub fn enum_panel_windows(main: HWND) -> Vec<HWND> {
    struct Ctx {
        main: HWND,
        out: Vec<HWND>,
    }
    unsafe extern "system" fn cb(hwnd: HWND, lp: LPARAM) -> windows::core::BOOL {
        let ctx = &mut *(lp.0 as *mut Ctx);
        if is_panel_window(hwnd, ctx.main) && !title_of(hwnd).is_empty() {
            ctx.out.push(hwnd);
        }
        true.into()
    }
    let mut ctx = Ctx { main, out: Vec::new() };
    let _ = unsafe { EnumWindows(Some(cb), LPARAM(&mut ctx as *mut Ctx as isize)) };
    ctx.out
}

/// 分離の窓を枠の子にはめ込む。失敗したら None（窓は変えない）
pub fn embed(hwnd: HWND, frame: HWND, key: String) -> Option<Embedded> {
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
        let owner = GetWindow(hwnd, GW_OWNER).map(raw).unwrap_or(0);
        let mut rect = RECT::default();
        let _ = GetWindowRect(hwnd, &mut rect);
        let fp = fingerprint(hwnd);
        let strip = (WS_POPUP | WS_CAPTION | WS_THICKFRAME | WS_SYSMENU | WS_MINIMIZEBOX | WS_MAXIMIZEBOX).0 as isize;
        let child = (style & !strip) | (WS_CHILD | WS_CLIPSIBLINGS).0 as isize;
        // クロークは最上位のうちに解く（子になってからでは解けず、入力が届かなくなる）。
        // 解いた瞬間に見えないよう、先に画面の外へ動かしておく（元の位置は上で控えた）
        if CLOAKED.with(|c| c.borrow().contains(&raw(hwnd))) || is_cloaked(hwnd) {
            with_our_move(|| {
                let _ = SetWindowPos(hwnd, None, -32000, -32000, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
            });
            uncloak(hwnd);
        }
        let ok = with_our_move(|| {
            SetWindowLongPtrW(hwnd, GWL_STYLE, child);
            if SetParent(hwnd, Some(frame)).is_err() {
                SetWindowLongPtrW(hwnd, GWL_STYLE, style);
                return false;
            }
            let _ = SetWindowPos(
                hwnd,
                None,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
            true
        });
        if is_cloaked(hwnd) {
            tracing::warn!("クロークが残ったままはめ込んだ（入力が届かない）: {}", title_of(hwnd));
        }
        if !ok {
            tracing::warn!("はめ込めなかった: {}", title_of(hwnd));
            with_our_move(|| {
                let _ = SetWindowPos(hwnd, None, rect.left, rect.top, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
            });
            return None;
        }
        let _ = SetWindowSubclass(hwnd, Some(panel_proc), PANEL_SUBCLASS, 0);
        Some(Embedded { hwnd: raw(hwnd), key, frame: raw(frame), fp, style, owner, rect })
    }
}

impl Embedded {
    /// はめ込む前の最上位の窓の位置（x, y, 幅, 高さ）。終了時はここへ戻すので、次の起動で本体はこの位置に窓を作る
    pub fn top_rect(&self) -> [i32; 4] {
        [self.rect.left, self.rect.top, self.rect.right - self.rect.left, self.rect.bottom - self.rect.top]
    }
}

/// はめ込んだ窓を別の枠へ移す（スタイルはそのまま）
pub fn move_to_frame(e: &mut Embedded, frame: HWND) {
    let hwnd = h(e.hwnd);
    with_our_move(|| unsafe {
        let _ = SetParent(hwnd, Some(frame));
    });
    e.frame = raw(frame);
}

/// 本体の分離の窓に戻す。`at` があれば、そこ（画面座標）を左上にして出す。
/// `hide` なら DWM のクロークで隠したまま戻す（終了・切り替えの間に窓がばらけて見えないように）
pub fn release(e: &Embedded, at: Option<POINT>, hide: bool) {
    let hwnd = h(e.hwnd);
    forget_target(e.hwnd);
    forget_active(e.hwnd);
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            return;
        }
        let _ = RemoveWindowSubclass(hwnd, Some(panel_proc), PANEL_SUBCLASS);
        // こちらが裏のタブとして隠した窓は、表示の状態へ戻す（本体が保存する表示の状態を変えない）
        let unhide = forget_shown(e.hwnd);
        let vis = WS_VISIBLE.0 as isize;
        let now_vis = if unhide { vis } else { GetWindowLongPtrW(hwnd, GWL_STYLE) & vis };
        let style = (e.style & !vis) | now_vis;
        let (x, y) = match at {
            Some(p) => (p.x, p.y),
            None => (e.rect.left, e.rect.top),
        };
        with_our_move(|| {
            let _ = SetParent(hwnd, None);
            if hide {
                cloak(hwnd);
            } else {
                uncloak(hwnd);
            }
            SetWindowLongPtrW(hwnd, GWL_STYLE, style);
            SetWindowLongPtrW(hwnd, GWLP_HWNDPARENT, e.owner);
            let _ = SetWindowPos(
                hwnd,
                None,
                x,
                y,
                e.rect.right - e.rect.left,
                e.rect.bottom - e.rect.top,
                SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
            if unhide {
                let _ = ShowWindow(hwnd, SW_SHOWNA);
            }
        });
    }
}

/// はめ込んだ窓の見張り: 本体が動かしたら枠の割り当てへ押し返す。クリックで焦点を移す。壊されたら枠に知らせる
unsafe extern "system" fn panel_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, id: usize, data: usize) -> LRESULT {
    crate::guard::run("panel_proc", || panel_proc_inner(hwnd, msg, wp, lp, id, data)).unwrap_or_else(|| DefSubclassProc(hwnd, msg, wp, lp))
}

unsafe fn panel_proc_inner(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _id: usize, _data: usize) -> LRESULT {
    let key = raw(hwnd);
    match msg {
        WM_WINDOWPOSCHANGING if !OUR_MOVE.with(|c| c.get()) => {
            let pos = &mut *(lp.0 as *mut WINDOWPOS);
            if !pos.flags.contains(SWP_NOMOVE) || !pos.flags.contains(SWP_NOSIZE) {
                if let Some(t) = TARGETS.with(|t| t.borrow().get(&key).copied()) {
                    pos.x = t.left;
                    pos.y = t.top;
                    pos.cx = t.right - t.left;
                    pos.cy = t.bottom - t.top;
                    pos.flags &= !(SWP_NOMOVE | SWP_NOSIZE);
                }
            }
            // 裏のタブとして隠している窓を、本体が出す・前面に出そうとした（「色を編集」で色設定を開くなど）。タブを前に出す
            if HIDDEN_BY_US.with(|s| s.borrow().contains(&key)) && !pos.flags.contains(SWP_HIDEWINDOW) {
                if pos.flags.contains(SWP_SHOWWINDOW) {
                    app::post_front(key, 1);
                } else if !pos.flags.contains(SWP_NOZORDER) {
                    app::post_front(key, 2);
                }
            }
            if pos.flags.contains(SWP_HIDEWINDOW) {
                // 本体が隠した（「表示」メニューで消した）
                HOST_HIDDEN.with(|s| {
                    s.borrow_mut().insert(key);
                });
                HIDDEN_BY_US.with(|s| {
                    s.borrow_mut().remove(&key);
                });
                app::post_refresh_for_panel(key);
            } else if pos.flags.contains(SWP_SHOWWINDOW) {
                // 本体が出した。裏のタブなら並べ直しで隠し直す
                HOST_HIDDEN.with(|s| {
                    s.borrow_mut().remove(&key);
                });
                app::post_refresh_for_panel(key);
            }
        }
        WM_MOUSEACTIVATE => activate_panel(hwnd),
        WM_SETFOCUS if HIDDEN_BY_US.with(|s| s.borrow().contains(&key)) => app::post_front(key, 3),
        WM_SHOWWINDOW if !OUR_MOVE.with(|c| c.get()) => app::post_refresh_for_panel(key),
        WM_SETTEXT => app::post_refresh_for_panel(key),
        WM_NCDESTROY => {
            let _ = RemoveWindowSubclass(hwnd, Some(panel_proc), PANEL_SUBCLASS);
            forget_target(key);
            forget_cloak(key);
            forget_active(key);
            forget_shown(key);
            app::post_panel_gone(key);
        }
        _ => {}
    }
    DefSubclassProc(hwnd, msg, wp, lp)
}

// ---- 分離の窓のドラッグ（本体の窓をタイトルバーで運んで、枠へ落とす） ----

/// 分離の窓をドラッグを見るためにサブクラス化する（何度呼んでもよい）
pub fn hook_float(hwnd: HWND) {
    let key = raw(hwnd);
    let new = FLOAT_HOOKED.with(|s| s.borrow_mut().insert(key));
    if new {
        unsafe {
            let _ = SetWindowSubclass(hwnd, Some(float_proc), FLOAT_SUBCLASS, 0);
        }
    }
}

unsafe extern "system" fn float_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, id: usize, data: usize) -> LRESULT {
    crate::guard::run("float_proc", || float_proc_inner(hwnd, msg, wp, lp, id, data)).unwrap_or_else(|| DefSubclassProc(hwnd, msg, wp, lp))
}

unsafe fn float_proc_inner(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _id: usize, _data: usize) -> LRESULT {
    let key = raw(hwnd);
    match msg {
        WM_MOVING => {
            // はめ込んだ後は来ない（タイトルバーを外すので）
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            app::float_moving(key, pt);
        }
        WM_EXITSIZEMOVE => {
            let r = DefSubclassProc(hwnd, msg, wp, lp);
            app::float_move_ended(key);
            return r;
        }
        // シーンのプレビューは題（シーン名）が後から変わる。変わったら鍵を見直す
        WM_SETTEXT => app::post_scan(),
        WM_NCDESTROY => {
            let _ = RemoveWindowSubclass(hwnd, Some(float_proc), FLOAT_SUBCLASS);
            FLOAT_HOOKED.with(|s| {
                s.borrow_mut().remove(&key);
            });
            forget_cloak(key);
        }
        _ => {}
    }
    DefSubclassProc(hwnd, msg, wp, lp)
}

// ---- メイン窓（終了の前に全部を戻す） ----

pub fn hook_main(main: HWND) -> bool {
    unsafe { SetWindowSubclass(main, Some(main_proc), MAIN_SUBCLASS, 0).as_bool() }
}

unsafe extern "system" fn main_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, id: usize, data: usize) -> LRESULT {
    crate::guard::run("main_proc", || main_proc_inner(hwnd, msg, wp, lp, id, data)).unwrap_or_else(|| DefSubclassProc(hwnd, msg, wp, lp))
}

unsafe fn main_proc_inner(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _id: usize, _data: usize) -> LRESULT {
    match msg {
        WM_CLOSE => {
            // 本体が分離の窓の位置を aviutl2.ini に書くより前に、元の最上位の窓へ戻しておく（クロークで隠したまま）
            app::before_host_close();
            let r = DefSubclassProc(hwnd, msg, wp, lp);
            // 保存の確認で取り消されたときに備えて、少し後にはめ込み直す（普通に閉じれば枠ごと消えて何も起きない）
            app::schedule_resume();
            return r;
        }
        WM_ENDSESSION if wp.0 != 0 => app::before_host_close(),
        WM_NCDESTROY => {
            let _ = RemoveWindowSubclass(hwnd, Some(main_proc), MAIN_SUBCLASS);
        }
        _ => {}
    }
    DefSubclassProc(hwnd, msg, wp, lp)
}

// ---- 分離の窓が現れたのを知る ----

/// `WINEVENT_INCONTEXT` で掛ける（本体が窓を出したその場で呼ばれる）。配置にある窓なら、画面に出る前にクロークする
pub fn install_win_event() {
    unsafe {
        let mut module = HMODULE::default();
        let found = GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(win_event as *const u16),
            &mut module,
        )
        .is_ok();
        let pid = windows::Win32::System::Threading::GetCurrentProcessId();
        let tid = windows::Win32::System::Threading::GetCurrentThreadId();
        let hook = if found {
            SetWinEventHook(EVENT_OBJECT_SHOW, EVENT_OBJECT_SHOW, Some(module), Some(win_event), pid, tid, WINEVENT_INCONTEXT)
        } else {
            HWINEVENTHOOK::default()
        };
        let hook = if hook.is_invalid() {
            tracing::warn!("WinEvent を同期で掛けられなかったので、非同期で掛ける（起動時に窓が一瞬ばらけて見える）");
            SetWinEventHook(EVENT_OBJECT_SHOW, EVENT_OBJECT_SHOW, None, Some(win_event), pid, tid, WINEVENT_OUTOFCONTEXT)
        } else {
            hook
        };
        WIN_EVENT.with(|c| c.set(hook.0 as isize));
    }
}

unsafe extern "system" fn win_event(
    _hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _thread: u32,
    _time: u32,
) {
    crate::guard::run("win_event", || win_event_inner(hwnd, id_object, id_child));
}

unsafe fn win_event_inner(hwnd: HWND, id_object: i32, id_child: i32) {
    if id_object != OBJID_WINDOW.0 || id_child != 0 || hwnd.0.is_null() || class_of(hwnd) != HOST_CLASS {
        return;
    }
    if in_our_move() {
        return;
    }
    if app::wants_window(hwnd, has_child(hwnd), &title_of(hwnd)) {
        cloak(hwnd);
    }
    app::post_scan();
}

/// 片付け（プラグインの Drop から。UI スレッド）
pub fn uninstall(main: Option<HWND>) {
    unsafe {
        let hook = WIN_EVENT.with(|c| c.replace(0));
        if hook != 0 {
            let _ = UnhookWinEvent(HWINEVENTHOOK(hook as *mut _));
        }
        let floats: Vec<isize> = FLOAT_HOOKED.with(|s| s.borrow_mut().drain().collect());
        for f in floats {
            if IsWindow(Some(h(f))).as_bool() {
                let _ = RemoveWindowSubclass(h(f), Some(float_proc), FLOAT_SUBCLASS);
            }
        }
        HIDDEN_BY_US.with(|s| s.borrow_mut().clear());
        HOST_HIDDEN.with(|s| s.borrow_mut().clear());
        TARGETS.with(|t| t.borrow_mut().clear());
        ACTIVE_PANEL.with(|c| c.set(0));
        // 隠したまま残っている窓があれば出す（終了時は本体が先に壊しているので何もしない）
        for c in CLOAKED.with(|c| c.borrow_mut().drain().collect::<Vec<_>>()) {
            if IsWindow(Some(h(c))).as_bool() {
                set_cloak(h(c), false);
            }
        }
        if let Some(m) = main {
            if IsWindow(Some(m)).as_bool() {
                let _ = RemoveWindowSubclass(m, Some(main_proc), MAIN_SUBCLASS);
            }
        }
    }
}
