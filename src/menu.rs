//! 右クリックメニューと、配置の名前を聞く小さな窓

use std::cell::{Cell, RefCell};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetStockObject, DEFAULT_GUI_FONT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::app::{self, with_app};
use crate::frame;
use crate::host::{self, h, raw};
use crate::model::{Dock, Target};

const ID_ADD: usize = 1000;
const ID_SWITCH: usize = 2000;
const ID_OVERWRITE: usize = 3000;
const ID_DELETE: usize = 4000;
const ID_REMOVE_TAB: usize = 10;
const ID_MAXIMIZE: usize = 11;
const ID_SAVE_AS: usize = 12;
const ID_OPEN_FLOAT: usize = 13;
const ID_HELP: usize = 14;

const HELP: &str = "分割レイアウトの使い方\n\n\
1. 並べたいパネルを、本体の右クリック →「ウィンドウ配置」→「ウィンドウを分離」で分離する（1 回だけ。本体が覚えます）\n\
2. 分離した窓のタイトルバーを掴んで、この枠の上へ運ぶ。出てきたガイドの上で離すと、その向きに分割して入ります（中央はタブにまとめる）\n\
3. 枠に入ったパネルは、見出しのタブを掴んで動かせます。枠の外で離すと、本体の分離した窓に戻ります\n\
4. 区切りはドラッグで動かせます。見出しのダブルクリックで、その区画を広げる／戻す\n\
5. 右クリック →「配置」で、名前を付けて保存・切り替えができます";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// メニューの文字列を、メニューを壊すまで持っておく
struct Builder {
    strings: Vec<Vec<u16>>,
}

impl Builder {
    fn item(&mut self, menu: HMENU, id: usize, text: &str, enabled: bool) {
        self.strings.push(wide(text));
        let flags = if enabled { MF_STRING } else { MF_STRING | MF_GRAYED };
        unsafe {
            let _ = AppendMenuW(menu, flags, id, PCWSTR(self.strings.last().unwrap().as_ptr()));
        }
    }
    fn sub(&mut self, menu: HMENU, sub: HMENU, text: &str, enabled: bool) {
        self.strings.push(wide(text));
        let flags = if enabled { MF_POPUP } else { MF_POPUP | MF_GRAYED };
        unsafe {
            let _ = AppendMenuW(menu, flags, sub.0 as usize, PCWSTR(self.strings.last().unwrap().as_ptr()));
        }
    }
    fn sep(&mut self, menu: HMENU) {
        unsafe {
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        }
    }
}

pub fn show(hwnd: HWND, x: i32, y: i32) {
    // 区画と、今の状態を読む
    let Some(info) = with_app(|app| {
        let f = app.frame(raw(hwnd))?;
        let pane = f.layout.pane_at(x, y).map(|p| p.id);
        let active = pane.and_then(|id| f.root.pane(id)).and_then(|p| p.active_title().map(|s| s.to_string()));
        let active_label = active.as_ref().map(|k| f.tabs.iter().find(|t| t.key == *k).map(|t| t.label.clone()).unwrap_or_else(|| host::label(k, None)));
        let maximized = pane.is_some() && f.maximized == pane;
        let names: Vec<String> = app.store.saved.iter().map(|n| n.name.clone()).collect();
        let keys = app.all_keys();
        let embedded: Vec<isize> = app.embedded.keys().copied().collect();
        let frames: Vec<isize> = app.frames.iter().map(|f| f.hwnd).collect();
        Some((pane, active, active_label, maximized, names, keys, embedded, frames, app.main_window))
    })
    .flatten() else {
        return;
    };
    let (pane, active, active_label, maximized, names, keys, embedded, frames, main) = info;

    // 足せるパネル: 分離していて、どの枠にも入っておらず、配置にも無いもの（こちらの枠を中に持つ窓は除く）
    let scenes = app::scene_names();
    let addable: Vec<(HWND, String, bool)> = host::enum_panel_windows(h(main))
        .into_iter()
        .filter(|w| !embedded.contains(&raw(*w)))
        .filter(|w| !frames.iter().any(|f| unsafe { IsChild(*w, h(*f)).as_bool() }))
        .map(|w| (w, host::panel_key(w, &scenes), unsafe { IsWindowVisible(w).as_bool() }))
        .filter(|(_, k, _)| !keys.contains(k))
        .map(|(w, k, v)| (w, host::label(&k, Some(raw(w))), v))
        .collect();

    let mut b = Builder { strings: Vec::new() };
    let cmd = unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let add = CreatePopupMenu().unwrap_or_default();
        if addable.is_empty() {
            b.item(add, 0, "（分離した窓がありません。本体で「ウィンドウを分離」してください）", false);
        }
        for (i, (_, t, vis)) in addable.iter().enumerate() {
            let label = if *vis { t.clone() } else { format!("{t}（非表示）") };
            b.item(add, ID_ADD + i, &label, true);
        }
        b.sub(menu, add, "パネルを追加", true);
        match &active_label {
            Some(t) => b.item(menu, ID_REMOVE_TAB, &format!("「{t}」を外す（本体の分離した窓に戻す）"), true),
            None => b.item(menu, ID_REMOVE_TAB, "タブを外す", false),
        }
        b.item(menu, ID_MAXIMIZE, if maximized { "元に戻す" } else { "この区画を広げる" }, pane.is_some());
        b.sep(menu);

        let layouts = CreatePopupMenu().unwrap_or_default();
        b.item(layouts, ID_SAVE_AS, "名前を付けて保存…", true);
        b.sep(layouts);
        if names.is_empty() {
            b.item(layouts, 0, "（保存した配置はありません）", false);
        }
        for (i, n) in names.iter().enumerate() {
            let label = if i < 5 { format!("{}  {n}", i + 1) } else { n.clone() };
            b.item(layouts, ID_SWITCH + i, &label, true);
        }
        if !names.is_empty() {
            b.sep(layouts);
            let over = CreatePopupMenu().unwrap_or_default();
            let del = CreatePopupMenu().unwrap_or_default();
            for (i, n) in names.iter().enumerate() {
                b.item(over, ID_OVERWRITE + i, n, true);
                b.item(del, ID_DELETE + i, n, true);
            }
            b.sub(layouts, over, "今の配置で上書き", true);
            b.sub(layouts, del, "削除", true);
        }
        b.sub(menu, layouts, "配置", true);
        b.item(menu, ID_OPEN_FLOAT, "別窓を開く", true);
        b.sep(menu);
        b.item(menu, ID_HELP, "使い方", true);

        let mut pt = POINT { x, y };
        let _ = windows::Win32::Graphics::Gdi::ClientToScreen(hwnd, &mut pt);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, Some(0), hwnd, None);
        let _ = DestroyMenu(menu);
        cmd.0 as usize
    };

    match cmd {
        0 => {}
        ID_REMOVE_TAB => {
            if let Some(t) = active {
                app::remove_key(&t, None);
            }
        }
        ID_MAXIMIZE => {
            if let Some(p) = pane {
                frame::toggle_maximize(hwnd, p);
            }
        }
        ID_SAVE_AS => {
            if let Some(name) = ask_text(hwnd, "配置を名前を付けて保存", "") {
                let exists = names.contains(&name);
                if !exists || confirm(hwnd, &format!("「{name}」を今の配置で上書きしますか？")) {
                    app::save_as(&name);
                }
            }
        }
        ID_OPEN_FLOAT => {
            app::open_saved_or_new_float(true);
        }
        ID_HELP => unsafe {
            let text = wide(HELP);
            MessageBoxW(Some(root_of(hwnd)), PCWSTR(text.as_ptr()), w!("分割レイアウト"), MB_OK | MB_ICONINFORMATION);
        },
        c if (ID_ADD..ID_ADD + addable.len()).contains(&c) => {
            let (w, _, _) = &addable[c - ID_ADD];
            let target = pane.map(Target::Pane).unwrap_or(Target::Root);
            app::add_window(*w, hwnd, target, Dock::Center);
        }
        c if (ID_SWITCH..ID_SWITCH + names.len()).contains(&c) => app::switch_to(c - ID_SWITCH),
        c if (ID_OVERWRITE..ID_OVERWRITE + names.len()).contains(&c) => {
            let i = c - ID_OVERWRITE;
            if confirm(hwnd, &format!("「{}」を今の配置で上書きしますか？", names[i])) {
                app::overwrite(i);
            }
        }
        c if (ID_DELETE..ID_DELETE + names.len()).contains(&c) => {
            let i = c - ID_DELETE;
            if confirm(hwnd, &format!("保存した配置「{}」を削除しますか？", names[i])) {
                app::delete_saved(i);
            }
        }
        _ => {}
    }
}

fn root_of(hwnd: HWND) -> HWND {
    unsafe { GetAncestor(hwnd, GA_ROOT) }
}

fn confirm(hwnd: HWND, text: &str) -> bool {
    let t = wide(text);
    unsafe { MessageBoxW(Some(root_of(hwnd)), PCWSTR(t.as_ptr()), w!("分割レイアウト"), MB_YESNO | MB_ICONQUESTION) == IDYES }
}

// ---- 名前を聞く窓 ----

const INPUT_CLASS: PCWSTR = w!("SplitLayout_H_Input");
const ID_EDIT: i32 = 100;
const ID_OK: i32 = 1;
const ID_CANCEL: i32 = 2;

thread_local! {
    /// 0: 入力中、1: OK、2: 取り消し
    static INPUT_RESULT: Cell<u8> = const { Cell::new(0) };
    static INPUT_TEXT: RefCell<String> = const { RefCell::new(String::new()) };
}

unsafe extern "system" fn input_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    crate::guard::run("input_proc", || input_proc_inner(hwnd, msg, wp, lp)).unwrap_or_else(|| DefWindowProcW(hwnd, msg, wp, lp))
}

unsafe fn input_proc_inner(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND => {
            let id = (wp.0 & 0xFFFF) as i32;
            if id == ID_OK || id == ID_CANCEL {
                finish_input(hwnd, id == ID_OK);
                return LRESULT(0);
            }
        }
        WM_CLOSE => {
            finish_input(hwnd, false);
            return LRESULT(0);
        }
        _ => {}
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}

unsafe fn finish_input(hwnd: HWND, ok: bool) {
    if ok {
        if let Ok(edit) = GetDlgItem(Some(hwnd), ID_EDIT) {
            let mut buf = [0u16; 256];
            let n = GetWindowTextW(edit, &mut buf);
            INPUT_TEXT.with(|t| *t.borrow_mut() = String::from_utf16_lossy(&buf[..n.max(0) as usize]).trim().to_string());
        }
    }
    INPUT_RESULT.with(|r| r.set(if ok { 1 } else { 2 }));
}

/// 1 行の文字を聞く（モーダル）。取り消し・空なら None
fn ask_text(owner: HWND, title: &str, initial: &str) -> Option<String> {
    let owner = root_of(owner);
    unsafe {
        let instance = GetModuleHandleW(None).ok()?;
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(input_proc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).ok()?,
            hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH((windows::Win32::Graphics::Gdi::COLOR_BTNFACE.0 + 1) as isize as *mut _),
            lpszClassName: INPUT_CLASS,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let dpi = GetDpiForWindow(owner).max(96) as i32;
        let s = |v: i32| v * dpi / 96;
        let mut or = windows::Win32::Foundation::RECT::default();
        let _ = GetWindowRect(owner, &mut or);
        let (ww, wh) = (s(360), s(130));
        let x = (or.left + or.right - ww) / 2;
        let y = (or.top + or.bottom - wh) / 2;
        let t = wide(title);
        let dlg = CreateWindowExW(
            WS_EX_DLGMODALFRAME,
            INPUT_CLASS,
            PCWSTR(t.as_ptr()),
            WS_POPUP | WS_CAPTION | WS_SYSMENU,
            x,
            y,
            ww,
            wh,
            Some(owner),
            None,
            Some(instance.into()),
            None,
        )
        .ok()?;
        let font = GetStockObject(DEFAULT_GUI_FONT);
        let init = wide(initial);
        let edit = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            w!("EDIT"),
            PCWSTR(init.as_ptr()),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
            s(12),
            s(14),
            s(320),
            s(24),
            Some(dlg),
            Some(HMENU(ID_EDIT as isize as *mut _)),
            Some(instance.into()),
            None,
        )
        .ok();
        let ok = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("BUTTON"),
            w!("OK"),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_DEFPUSHBUTTON as u32),
            s(156),
            s(52),
            s(84),
            s(28),
            Some(dlg),
            Some(HMENU(ID_OK as isize as *mut _)),
            Some(instance.into()),
            None,
        )
        .ok();
        let cancel = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("BUTTON"),
            w!("取り消し"),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            s(248),
            s(52),
            s(84),
            s(28),
            Some(dlg),
            Some(HMENU(ID_CANCEL as isize as *mut _)),
            Some(instance.into()),
            None,
        )
        .ok();
        for c in [edit, ok, cancel].into_iter().flatten() {
            SendMessageW(c, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
        }
        INPUT_RESULT.with(|r| r.set(0));
        INPUT_TEXT.with(|t| t.borrow_mut().clear());
        let _ = EnableWindow(owner, false);
        let _ = ShowWindow(dlg, SW_SHOW);
        if let Some(e) = edit {
            let _ = SetFocus(Some(e));
        }
        let mut msg = MSG::default();
        while INPUT_RESULT.with(|r| r.get()) == 0 {
            if !GetMessageW(&mut msg, None, 0, 0).as_bool() {
                // WM_QUIT は外のループへ渡す
                PostQuitMessage(msg.wParam.0 as i32);
                break;
            }
            if msg.message == WM_KEYDOWN && msg.wParam.0 == 0x0D && IsChild(dlg, msg.hwnd).as_bool() {
                finish_input(dlg, true);
                continue;
            }
            if msg.message == WM_KEYDOWN && msg.wParam.0 == 0x1B && IsChild(dlg, msg.hwnd).as_bool() {
                finish_input(dlg, false);
                continue;
            }
            if !IsDialogMessageW(dlg, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        let _ = EnableWindow(owner, true);
        let _ = DestroyWindow(dlg);
        let _ = SetForegroundWindow(owner);
        let _ = UnregisterClassW(INPUT_CLASS, Some(instance.into()));
        let ok = INPUT_RESULT.with(|r| r.get()) == 1;
        let text = INPUT_TEXT.with(|t| t.borrow().clone());
        (ok && !text.is_empty()).then_some(text)
    }
}
