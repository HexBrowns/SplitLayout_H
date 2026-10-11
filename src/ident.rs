//! 本体の分離の窓を、配置のどのタブ（鍵）として扱うかを決める（窓に依存しない。cargo test で確かめる）
//!
//! 本体の窓には題のほかに見分ける手がかりが無い（プロパティ・追加領域・ID は起動ごとに変わるか空。2026-10-11 に本番で確認）。
//! ところが題は変わることがある（シーンのプレビューはシーン名、色設定は「対象 [項目]」）。そこで次の順に決める。
//!
//! 1. この起動の間に一度はめ込んだ窓は、そのときの鍵（題が変わっても、外して入れ直しても同じタブ）
//! 2. 本体が自分で描く窓（子ウィンドウなし）で、題がシーン名なら、シーンのプレビュー
//! 3. 題が配置の鍵にあれば、その鍵
//! 4. 本体が自分で描く窓なら、位置で見分ける。終了時は窓を元の位置に戻してから本体に保存させるので、
//!    次の起動で本体は同じ位置に窓を作る。その位置を鍵ごとに覚えておき、一致する鍵が 1 つだけなら、その鍵
//! 5. どれでもなければ題

use std::collections::{BTreeMap, HashMap, HashSet};

pub const SCENE_PREVIEW: &str = "@scene.preview";

#[derive(Clone, Debug)]
pub struct Win {
    pub id: isize,
    pub title: String,
    /// 子ウィンドウを持つ（プラグインの窓）
    pub child: bool,
    /// 画面座標の x, y, 幅, 高さ
    pub rect: [i32; 4],
}

pub struct Known<'a> {
    pub seen: &'a HashMap<isize, String>,
    pub scenes: &'a [String],
    /// 配置にある鍵（どの枠のものも）
    pub layout_keys: &'a [String],
    /// 今はめ込んでいる窓の鍵
    pub embedded_keys: &'a HashSet<String>,
    pub rects: &'a BTreeMap<String, [i32; 4]>,
}

/// 窓ごとの鍵（wins と同じ順）
pub fn resolve(wins: &[Win], k: &Known) -> Vec<String> {
    let mut out: Vec<Option<String>> = wins
        .iter()
        .map(|w| {
            if let Some(key) = k.seen.get(&w.id) {
                return Some(key.clone());
            }
            if !w.child && k.scenes.iter().any(|s| *s == w.title) {
                return Some(SCENE_PREVIEW.to_string());
            }
            if k.layout_keys.iter().any(|key| *key == w.title) {
                return Some(w.title.clone());
            }
            None
        })
        .collect();
    let taken: HashSet<String> = out.iter().flatten().cloned().collect();
    // 位置で見分ける（本体が描く窓だけ。同じ位置の窓や鍵が 2 つ以上なら、決めない）
    let open: Vec<usize> = (0..wins.len()).filter(|i| out[*i].is_none() && !wins[*i].child).collect();
    for &i in &open {
        let w = &wins[i];
        let same_rect = open.iter().filter(|j| wins[**j].rect == w.rect).count();
        if same_rect != 1 {
            continue;
        }
        let candidates: Vec<&String> = k
            .layout_keys
            .iter()
            .filter(|key| !taken.contains(*key) && !k.embedded_keys.contains(*key))
            .filter(|key| k.rects.get(*key) == Some(&w.rect))
            .collect();
        if candidates.len() == 1 {
            out[i] = Some(candidates[0].clone());
        }
    }
    out.into_iter().zip(wins).map(|(o, w)| o.unwrap_or_else(|| w.title.clone())).collect()
}

/// 本体が描く窓を、区画の中央に落としたとき、その区画の灰色のタブ（窓の無いタブ）として入れるか。
/// 入れるなら、そのタブの鍵を返す。一度別のタブになってしまった窓（題が変わっていた）を、手で戻すための逃げ道
pub fn placeholder_to_fill(win: &Win, win_key: &str, active_key: Option<&str>, layout_keys: &[String], live_titles: &[String], embedded_keys: &HashSet<String>) -> Option<String> {
    let active = active_key?;
    if win.child || active == win_key || layout_keys.iter().any(|k| k == win_key) {
        return None;
    }
    // 灰色: はめ込まれておらず、同じ題の分離の窓も無い
    let gray = !embedded_keys.contains(active) && !live_titles.iter().any(|t| t == active) && active != SCENE_PREVIEW;
    gray.then(|| active.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(id: isize, title: &str, child: bool, rect: [i32; 4]) -> Win {
        Win { id, title: title.into(), child, rect }
    }

    fn keys(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn title_scene_and_seen() {
        let seen: HashMap<isize, String> = [(3, "色設定".to_string())].into();
        let layout = keys(&["プレビュー編集", "色設定", SCENE_PREVIEW]);
        let k = Known {
            seen: &seen,
            scenes: &keys(&["シーン1"]),
            layout_keys: &layout,
            embedded_keys: &HashSet::new(),
            rects: &BTreeMap::new(),
        };
        let wins = [
            win(1, "プレビュー編集", false, [0, 0, 10, 10]),
            win(2, "シーン1", false, [0, 0, 10, 10]),
            win(3, "PDF_H [背景色]", false, [0, 0, 10, 10]),
            win(4, "シーン1", true, [0, 0, 10, 10]), // プラグインの窓はシーン名でもシーンのプレビューにしない
        ];
        assert_eq!(resolve(&wins, &k), keys(&["プレビュー編集", SCENE_PREVIEW, "色設定", "シーン1"]));
    }

    #[test]
    fn rect_identifies_renamed_host_window() {
        let layout = keys(&["色設定", "テキスト編集"]);
        let rects: BTreeMap<String, [i32; 4]> =
            [("色設定".to_string(), [100, 100, 300, 400]), ("テキスト編集".to_string(), [50, 50, 300, 400])].into();
        let k = Known {
            seen: &HashMap::new(),
            scenes: &[],
            layout_keys: &layout,
            embedded_keys: &HashSet::new(),
            rects: &rects,
        };
        let wins = [win(1, "PDF_H [背景色]", false, [100, 100, 300, 400]), win(2, "テキスト編集", false, [50, 50, 300, 400])];
        assert_eq!(resolve(&wins, &k), keys(&["色設定", "テキスト編集"]));
        // プラグインの窓は位置で見分けない
        let wins = [win(1, "何か", true, [100, 100, 300, 400])];
        assert_eq!(resolve(&wins, &k), keys(&["何か"]));
    }

    #[test]
    fn rect_ambiguity_falls_back_to_title() {
        let layout = keys(&["A", "B"]);
        let rects: BTreeMap<String, [i32; 4]> = [("A".to_string(), [1, 1, 5, 5]), ("B".to_string(), [1, 1, 5, 5])].into();
        let k = Known {
            seen: &HashMap::new(),
            scenes: &[],
            layout_keys: &layout,
            embedded_keys: &HashSet::new(),
            rects: &rects,
        };
        // 同じ位置の鍵が 2 つ
        assert_eq!(resolve(&[win(1, "X [y]", false, [1, 1, 5, 5])], &k), keys(&["X [y]"]));
        // 同じ位置の窓が 2 つ
        let rects1: BTreeMap<String, [i32; 4]> = [("A".to_string(), [1, 1, 5, 5])].into();
        let k1 = Known { rects: &rects1, ..k };
        let wins = [win(1, "X", false, [1, 1, 5, 5]), win(2, "Y", false, [1, 1, 5, 5])];
        assert_eq!(resolve(&wins, &k1), keys(&["X", "Y"]));
    }

    #[test]
    fn rect_skips_keys_already_taken() {
        let layout = keys(&["色設定"]);
        let rects: BTreeMap<String, [i32; 4]> = [("色設定".to_string(), [1, 1, 5, 5])].into();
        let embedded: HashSet<String> = ["色設定".to_string()].into();
        let k = Known { seen: &HashMap::new(), scenes: &[], layout_keys: &layout, embedded_keys: &embedded, rects: &rects };
        assert_eq!(resolve(&[win(1, "X [y]", false, [1, 1, 5, 5])], &k), keys(&["X [y]"]));
    }

    #[test]
    fn placeholder_is_filled_only_by_host_window() {
        let layout = keys(&["色設定", "ログ表示"]);
        let none = HashSet::new();
        let w = win(1, "PDF_H [背景色]", false, [0, 0, 1, 1]);
        assert_eq!(placeholder_to_fill(&w, "PDF_H [背景色]", Some("色設定"), &layout, &[], &none).as_deref(), Some("色設定"));
        // 灰色でない（同じ題の窓がある）
        assert_eq!(placeholder_to_fill(&w, "PDF_H [背景色]", Some("ログ表示"), &layout, &keys(&["ログ表示"]), &none), None);
        // プラグインの窓は入れ替えない
        let p = win(2, "何か", true, [0, 0, 1, 1]);
        assert_eq!(placeholder_to_fill(&p, "何か", Some("色設定"), &layout, &[], &none), None);
        // 題が配置にある窓は、その題のタブとして入る
        let l = win(3, "ログ表示", false, [0, 0, 1, 1]);
        assert_eq!(placeholder_to_fill(&l, "ログ表示", Some("色設定"), &layout, &[], &none), None);
    }
}
