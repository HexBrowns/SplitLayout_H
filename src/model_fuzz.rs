//! 配置の木のランダム操作テスト（cargo test）
//!
//! 実際の操作（落とす・外す・タブを動かす・区切りを動かす・タブを切り替える・並べ替える・鍵の書き換え）を
//! 乱数で何千回も続け、そのたびに木（`Node::check`）と割り付け（`Layout::check`）の整合性を確かめる。
//! 区切りは「接していない区画の幅を保つ」ことも確かめる。

use crate::model::*;

/// 再現できる乱数（xorshift64*）
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
    fn unit(&mut self) -> f32 {
        (self.next() % 10_000) as f32 / 10_000.0
    }
}

const DOCKS: [Dock; 5] = [Dock::Center, Dock::Left, Dock::Right, Dock::Top, Dock::Bottom];

fn titles(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("パネル{i}")).collect()
}

fn random_area(rng: &mut Rng) -> Rect {
    // 小さい枠（区切りより狭い）から FHD の全画面まで
    let w = [3, 20, 120, 640, 1919][rng.below(5)] + rng.below(30) as i32;
    let h = [3, 20, 90, 480, 1000][rng.below(5)] + rng.below(30) as i32;
    Rect::new(0, 0, w, h)
}

fn check_all(n: &Node, rng: &mut Rng, step: usize, op: &str) {
    if let Err(e) = n.check() {
        panic!("{step} 手目（{op}）の後で木が崩れた: {e}\n{n:#?}");
    }
    for _ in 0..3 {
        let area = random_area(rng);
        let maxed = if rng.below(4) == 0 { n.panes().get(rng.below(n.panes().len())).map(|p| p.id) } else { None };
        let l = layout(n, area, 24, 4, maxed);
        if let Err(e) = l.check(area) {
            panic!("{step} 手目（{op}）の後で割り付けが崩れた（{area:?} 最大化 {maxed:?}）: {e}");
        }
        // 区画は木と同じ数だけ並ぶ
        assert_eq!(l.panes.len(), n.panes().len());
    }
}

/// 1 回のランダム操作。何をしたかを返す
fn random_op(n: &mut Node, rng: &mut Rng, pool: &[String]) -> String {
    let present = n.titles();
    let panes: Vec<u64> = n.panes().iter().map(|p| p.id).collect();
    match rng.below(9) {
        // 新しいパネルを落とす（区画か枠全体へ）
        0 | 1 => {
            let absent: Vec<&String> = pool.iter().filter(|t| !present.contains(t)).collect();
            let Some(t) = absent.get(rng.below(absent.len())).map(|s| s.to_string()) else { return "なし".into() };
            let dock = DOCKS[rng.below(5)];
            let target = if rng.below(4) == 0 { Target::Root } else { Target::Pane(panes[rng.below(panes.len())]) };
            assert!(n.insert(target, &t, dock), "落とす先が見つからない");
            format!("落とす {t} {target:?} {dock:?}")
        }
        // 外す
        2 => {
            let Some(t) = present.get(rng.below(present.len())).cloned() else { return "なし".into() };
            assert!(n.remove_title(&t));
            format!("外す {t}")
        }
        // タブを動かす（外して別の所へ落とす。app::move_tab と同じ順）
        3 | 4 => {
            let Some(t) = present.get(rng.below(present.len())).cloned() else { return "なし".into() };
            let from = n.pane_of(&t).unwrap();
            let to = panes[rng.below(panes.len())];
            let dock = DOCKS[rng.below(5)];
            let single = n.pane(from).unwrap().tabs.len() <= 1;
            if to == from && (dock == Dock::Center || single) {
                return "動かさない（自分の区画）".into();
            }
            n.remove_title(&t);
            if !n.insert(Target::Pane(to), &t, dock) {
                n.insert(Target::Root, &t, Dock::Center);
            }
            format!("動かす {t} → {to} {dock:?}")
        }
        // 区切りを動かす
        5 => {
            let area = random_area(rng);
            let l = layout(n, area, 24, 4, None);
            let Some(sp) = l.splitters.get(rng.below(l.splitters.len())).copied() else { return "なし".into() };
            let span = match sp.axis {
                Axis::Horizontal => sp.span.w(),
                Axis::Vertical => sp.span.h(),
            };
            let r = rng.unit();
            assert!(n.set_ratio_keeping(sp.id, r, span, 4));
            format!("区切り {} を {r}", sp.id)
        }
        // タブを切り替える
        6 => {
            let p = panes[rng.below(panes.len())];
            let len = n.pane(p).unwrap().tabs.len();
            n.set_active(p, rng.below(len.max(1)));
            "切り替え".into()
        }
        // 見出しの中で並べ替える
        7 => {
            let p = panes[rng.below(panes.len())];
            let len = n.pane(p).unwrap().tabs.len();
            if len < 2 {
                return "なし".into();
            }
            n.move_tab(p, rng.below(len), rng.below(len));
            "並べ替え".into()
        }
        // 鍵の書き換え（シーンのプレビューの移し替え）
        _ => {
            let Some(t) = present.get(rng.below(present.len())).cloned() else { return "なし".into() };
            let new = format!("{t}*");
            if present.contains(&new) {
                return "なし".into();
            }
            n.rename_title(&t, &new);
            format!("書き換え {t} → {new}")
        }
    }
}

#[test]
fn random_operations_keep_tree_and_layout_consistent() {
    let pool = titles(60);
    for seed in 1..=40u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let mut n = Node::default();
        for step in 0..400 {
            let op = random_op(&mut n, &mut rng, &pool);
            check_all(&n, &mut rng, step, &op);
        }
        // 保存して読み直しても同じ
        let json = serde_json::to_string(&n).unwrap();
        let mut back: Node = serde_json::from_str(&json).unwrap();
        back.renumber();
        assert_eq!(back.titles(), n.titles(), "seed {seed}");
        back.check().unwrap();
    }
}

#[test]
fn splitter_keeps_untouched_panes_under_random_trees() {
    // ランダムな木で、どの区切りを動かしても、区切りに接していない区画の幅（その向きの長さ）は 1px 以内で変わらない
    let pool = titles(30);
    let area = Rect::new(0, 0, 1916, 996);
    for seed in 1..=60u64 {
        let mut rng = Rng(seed.wrapping_mul(0xD1B5_4A32_D192_ED03));
        let mut n = Node::default();
        for _ in 0..25 {
            let absent: Vec<&String> = pool.iter().filter(|t| !n.titles().contains(t)).collect();
            let t = absent[rng.below(absent.len())].clone();
            let panes: Vec<u64> = n.panes().iter().map(|p| p.id).collect();
            n.insert(Target::Pane(panes[rng.below(panes.len())]), &t, DOCKS[1 + rng.below(4)]);
        }
        let before = layout(&n, area, 24, 4, None);
        for sp in before.splitters.clone() {
            let span = match sp.axis {
                Axis::Horizontal => sp.span.w(),
                Axis::Vertical => sp.span.h(),
            };
            let mut moved = n.clone();
            moved.set_ratio_keeping(sp.id, 0.05 + rng.unit() * 0.9, span, 4);
            let after = layout(&moved, area, 24, 4, None);
            after.check(area).unwrap();
            // 吸収する側が最小（5%）まで縮んで割合が止まったら、接していない区画も縮み始めてよい
            let clamped = ratios(&moved).iter().zip(ratios(&n)).any(|(a, b)| (*a == 0.05 || *a == 0.95) && *a != b);
            for pb in &before.panes {
                let pa = after.pane_box(pb.id).unwrap();
                // 区切りの全体（span）の外にある区画は、まったく動かない
                let inside = pb.rect.l >= sp.span.l && pb.rect.r <= sp.span.r && pb.rect.t >= sp.span.t && pb.rect.b <= sp.span.b;
                if !inside {
                    assert_eq!(pa.rect, pb.rect, "seed {seed}: 分割の外の区画が動いた");
                    continue;
                }
                // 区切りに接していない区画は、区切りの向きの長さを保つ
                let touches = match sp.axis {
                    Axis::Horizontal => pb.rect.r == sp.rect.l || pb.rect.l == sp.rect.r,
                    Axis::Vertical => pb.rect.b == sp.rect.t || pb.rect.t == sp.rect.b,
                };
                if !touches {
                    let (lb, la) = match sp.axis {
                        Axis::Horizontal => (pb.rect.w(), pa.rect.w()),
                        Axis::Vertical => (pb.rect.h(), pa.rect.h()),
                    };
                    assert!((lb - la).abs() <= 1 || clamped, "seed {seed}: 接していない区画の長さが {lb} → {la}");
                }
            }
        }
    }
}

/// 木の中の分割の割合を、たどる順に
fn ratios(n: &Node) -> Vec<f32> {
    match n {
        Node::Split(s) => {
            let mut v = vec![s.ratio];
            v.extend(ratios(&s.a));
            v.extend(ratios(&s.b));
            v
        }
        Node::Pane(_) => Vec::new(),
    }
}

#[test]
fn layout_of_large_tree_is_fast() {
    // 区画 30・タブ 60 の木の割り付け 1 回の時間（実機の並べ直しの、計算の部分）
    let pool = titles(60);
    let mut rng = Rng(7);
    let mut n = Node::default();
    for t in &pool {
        let panes: Vec<u64> = n.panes().iter().map(|p| p.id).collect();
        let dock = if n.panes().len() < 30 { DOCKS[1 + rng.below(4)] } else { Dock::Center };
        n.insert(Target::Pane(panes[rng.below(panes.len())]), t, dock);
    }
    assert!(n.panes().len() >= 25);
    let area = Rect::new(0, 0, 1916, 996);
    let start = std::time::Instant::now();
    let rounds = 2000;
    for _ in 0..rounds {
        let l = layout(&n, area, 24, 4, None);
        std::hint::black_box(&l);
    }
    let per = start.elapsed() / rounds;
    eprintln!("区画 {} の割り付け 1 回 {per:?}", n.panes().len());
    assert!(per < std::time::Duration::from_millis(1), "割り付けが遅い: {per:?}");
}
