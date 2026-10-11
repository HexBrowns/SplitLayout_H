//! 配置のファイル（`<データフォルダ>/Plugin/SplitLayout_H/layouts.json`）
//!
//! 下位互換の決まり（`au2-plugin-checklist`「設定ファイルの下位互換」）に従う:
//! キーは省略でき、知らないキーは無視する。一時ファイルに書いてから置き換える。
//! 読めなかったら `.broken` へ退避し、その起動の間は上書きしない（退避できなかったときも同じ）。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::Node;

pub const VERSION: u32 = 1;

/// 本体の外の別窓 1 枚
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct FloatWindow {
    /// 画面座標の x, y, 幅, 高さ
    pub rect: [i32; 4],
    pub root: Node,
}

impl Default for FloatWindow {
    fn default() -> Self {
        Self { rect: [200, 200, 800, 600], root: Node::default() }
    }
}

/// 主の枠と別窓をまとめた 1 つの配置
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LayoutSet {
    pub main: Node,
    pub windows: Vec<FloatWindow>,
}

impl LayoutSet {
    pub fn renumber(&mut self) {
        self.main.renumber();
        for w in &mut self.windows {
            w.root.renumber();
        }
    }

    pub fn titles(&self) -> Vec<String> {
        let mut t = self.main.titles();
        for w in &self.windows {
            t.extend(w.root.titles());
        }
        t
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Named {
    pub name: String,
    pub layout: LayoutSet,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Store {
    pub version: u32,
    /// 今の配置（終了時に書き、次の起動で戻す）
    pub current: LayoutSet,
    /// 名前を付けて保存した配置。先頭の 5 つが編集メニューの「配置 1〜5」
    pub saved: Vec<Named>,
    /// 鍵ごとの、本体の分離の窓の位置（x, y, 幅, 高さ）。題の変わる窓を次の起動で見分ける（`ident`）
    pub rects: std::collections::BTreeMap<String, [i32; 4]>,
}

impl Default for Store {
    fn default() -> Self {
        Self { version: VERSION, current: LayoutSet::default(), saved: Vec::new(), rects: Default::default() }
    }
}

pub fn path() -> PathBuf {
    aviutl2::config::app_data_path().join("Plugin").join("SplitLayout_H").join("layouts.json")
}

pub struct Loaded {
    pub store: Store,
    /// 偽ならこの起動の間は書かない（読めなかったファイルを守るため）
    pub writable: bool,
    /// ログに出す一言（読めなかったときなど）
    pub message: Option<String>,
}

pub fn load(path: &Path) -> Loaded {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Loaded { store: Store::default(), writable: true, message: None };
        }
        Err(e) => return broken(path, &format!("読めない: {e}")),
    };
    let text = match String::from_utf8(bytes) {
        Ok(t) => t,
        Err(_) => return broken(path, "UTF-8 でない"),
    };
    match serde_json::from_str::<Store>(&text) {
        Ok(mut store) => {
            store.current.renumber();
            for n in &mut store.saved {
                n.layout.renumber();
            }
            let message = (store.version > VERSION)
                .then(|| format!("新しい版（{}）の配置ファイルを読んだ。知らない項目は無視する", store.version));
            store.version = VERSION;
            Loaded { store, writable: true, message }
        }
        Err(e) => broken(path, &format!("解析できない: {e}")),
    }
}

fn broken(path: &Path, why: &str) -> Loaded {
    let backup = path.with_extension("json.broken");
    let moved = std::fs::copy(path, &backup).is_ok();
    let message = if moved {
        format!("配置ファイルが{why}。{} へ退避し、空の配置で始める", backup.display())
    } else {
        format!("配置ファイルが{why}。退避もできなかったので、この起動の間は保存しない")
    };
    Loaded { store: Store::default(), writable: moved, message: Some(message) }
}

pub fn save(path: &Path, store: &Store) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string_pretty(store).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Dock, Target};

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("split_layout_h_test_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn round_trip() {
        let dir = tmpdir("rt");
        let p = dir.join("layouts.json");
        let mut s = Store::default();
        s.current.main.insert(Target::Root, "プレビュー編集", Dock::Center);
        let id = s.current.main.first_pane_id();
        s.current.main.insert(Target::Pane(id), "レイヤー編集", Dock::Bottom);
        s.current.windows.push(FloatWindow { rect: [10, 20, 300, 400], root: Node::default() });
        s.saved.push(Named { name: "編集".into(), layout: s.current.clone() });
        save(&p, &s).unwrap();
        let l = load(&p);
        assert!(l.writable && l.message.is_none());
        assert_eq!(l.store.current.titles(), vec!["プレビュー編集", "レイヤー編集"]);
        assert_eq!(l.store.current.windows[0].rect, [10, 20, 300, 400]);
        assert_eq!(l.store.saved[0].name, "編集");
        assert!(!dir.join("layouts.json.tmp").exists());
    }

    #[test]
    fn missing_file_is_empty_and_writable() {
        let dir = tmpdir("missing");
        let l = load(&dir.join("layouts.json"));
        assert!(l.writable && l.message.is_none());
        assert!(l.store.current.main.is_empty_pane());
    }

    #[test]
    fn old_and_partial_files_load() {
        let dir = tmpdir("partial");
        let p = dir.join("layouts.json");
        // 版も別窓も保存した配置も無い、木だけのファイル。知らないキーも混ぜる
        std::fs::write(&p, r#"{"current":{"main":{"split":"v","ratio":0.6,"a":{"tabs":["A"]},"b":{"tabs":["B"],"active":1}}},"later":true}"#).unwrap();
        let l = load(&p);
        assert!(l.writable);
        assert_eq!(l.store.current.titles(), vec!["A", "B"]);
        assert!(l.store.current.windows.is_empty());
        assert_eq!(l.store.version, VERSION);
    }

    #[test]
    fn broken_file_is_backed_up_and_kept() {
        let dir = tmpdir("broken");
        let p = dir.join("layouts.json");
        std::fs::write(&p, "{ これは JSON ではない").unwrap();
        let l = load(&p);
        assert!(l.message.is_some());
        assert!(l.writable, "退避できたら書いてよい");
        assert!(dir.join("layouts.json.broken").exists());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{ これは JSON ではない", "元のファイルは上書きしない");
    }
}
