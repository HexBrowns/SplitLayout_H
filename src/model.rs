//! 配置の木と、その操作・矩形の計算・当たり判定（窓に依存しない。cargo test で確かめる）
//!
//! 木は二分木。**分割**（左右か上下と割合）と **区画**（タブの並び）からなる。
//! 区画と分割には実行時だけの番号（`id`）を振る。ドラッグの途中で木が組み変わっても、対象を番号で引き直せるようにするため。
//! 番号は保存しない。

use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// 分割の向き。`Horizontal` は a が左・b が右、`Vertical` は a が上・b が下
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Axis {
    #[serde(rename = "h")]
    Horizontal,
    #[serde(rename = "v")]
    Vertical,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Split {
    #[serde(rename = "split")]
    pub axis: Axis,
    /// a の取り分（0〜1）
    pub ratio: f32,
    pub a: Box<Node>,
    pub b: Box<Node>,
    #[serde(skip, default = "next_id")]
    pub id: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pane {
    /// パネルの窓の題（「プレビュー編集」など）
    #[serde(default)]
    pub tabs: Vec<String>,
    #[serde(default)]
    pub active: usize,
    #[serde(skip, default = "next_id")]
    pub id: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Node {
    Split(Split),
    Pane(Pane),
}

/// 分割の中で動いた辺
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Edge {
    Start,
    End,
}

/// 落とす位置
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dock {
    Center,
    Left,
    Right,
    Top,
    Bottom,
}

/// 落とす先
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// 区画（中央でタブにまとめる、辺でその区画を分割する）
    Pane(u64),
    /// 枠全体の辺（枠全体を分割する）
    Root,
}

/// 区画の辺へ落としたときの新しい側の取り分
const PANE_SPLIT: f32 = 0.5;
/// 枠全体の辺へ落としたときの新しい側の取り分
const ROOT_SPLIT: f32 = 0.3;
const RATIO_MIN: f32 = 0.05;
const RATIO_MAX: f32 = 0.95;

impl Pane {
    pub fn new(tabs: Vec<String>) -> Self {
        Self { tabs, active: 0, id: next_id() }
    }

    pub fn active_title(&self) -> Option<&str> {
        self.tabs.get(self.active.min(self.tabs.len().saturating_sub(1))).map(|s| s.as_str())
    }

    fn fix_active(&mut self) {
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len().saturating_sub(1);
        }
    }
}

impl Default for Node {
    fn default() -> Self {
        Node::Pane(Pane::new(Vec::new()))
    }
}

impl Node {
    pub fn is_empty_pane(&self) -> bool {
        matches!(self, Node::Pane(p) if p.tabs.is_empty())
    }

    /// 番号を振り直す（読み込んだ木や、保存した配置から写した木に使う）
    pub fn renumber(&mut self) {
        match self {
            Node::Split(s) => {
                s.id = next_id();
                s.a.renumber();
                s.b.renumber();
            }
            Node::Pane(p) => {
                p.id = next_id();
                p.fix_active();
            }
        }
    }

    pub fn panes(&self) -> Vec<&Pane> {
        let mut out = Vec::new();
        self.walk_panes(&mut |p| out.push(p));
        out
    }

    fn walk_panes<'a>(&'a self, f: &mut impl FnMut(&'a Pane)) {
        match self {
            Node::Split(s) => {
                s.a.walk_panes(f);
                s.b.walk_panes(f);
            }
            Node::Pane(p) => f(p),
        }
    }

    pub fn titles(&self) -> Vec<String> {
        self.panes().iter().flat_map(|p| p.tabs.iter().cloned()).collect()
    }

    pub fn contains(&self, title: &str) -> bool {
        self.panes().iter().any(|p| p.tabs.iter().any(|t| t == title))
    }

    pub fn pane(&self, id: u64) -> Option<&Pane> {
        self.panes().into_iter().find(|p| p.id == id)
    }

    pub fn pane_mut(&mut self, id: u64) -> Option<&mut Pane> {
        match self {
            Node::Split(s) => match s.a.pane_mut(id) {
                Some(p) => Some(p),
                None => s.b.pane_mut(id),
            },
            Node::Pane(p) if p.id == id => Some(p),
            Node::Pane(_) => None,
        }
    }

    pub fn first_pane_id(&self) -> u64 {
        match self {
            Node::Split(s) => s.a.first_pane_id(),
            Node::Pane(p) => p.id,
        }
    }

    pub fn pane_of(&self, title: &str) -> Option<u64> {
        self.panes().into_iter().find(|p| p.tabs.iter().any(|t| t == title)).map(|p| p.id)
    }

    /// 題をどの区画からも取り除き、空いた区画を畳む。取り除いたら真
    pub fn remove_title(&mut self, title: &str) -> bool {
        let removed = self.remove_title_inner(title);
        if removed {
            self.collapse();
        }
        removed
    }

    fn remove_title_inner(&mut self, title: &str) -> bool {
        match self {
            Node::Split(s) => {
                let a = s.a.remove_title_inner(title);
                let b = s.b.remove_title_inner(title);
                a || b
            }
            Node::Pane(p) => {
                let before = p.tabs.len();
                if let Some(i) = p.tabs.iter().position(|t| t == title) {
                    if i < p.active {
                        p.active -= 1;
                    }
                }
                p.tabs.retain(|t| t != title);
                p.fix_active();
                before != p.tabs.len()
            }
        }
    }

    /// 空の区画を持つ分割を、もう一方で置き換える（根が空の区画になるのはよい）
    pub fn collapse(&mut self) {
        if let Node::Split(s) = self {
            s.a.collapse();
            s.b.collapse();
            let replacement = if s.a.is_empty_pane() {
                Some(std::mem::take(&mut *s.b))
            } else if s.b.is_empty_pane() {
                Some(std::mem::take(&mut *s.a))
            } else {
                None
            };
            if let Some(n) = replacement {
                *self = n;
            }
        }
    }

    /// 題を落とす。先に、同じ題をどこからも取り除いておくこと（呼ぶ側の責任）。
    /// 落とす先が見つからなければ偽（木は変えない）
    pub fn insert(&mut self, target: Target, title: &str, dock: Dock) -> bool {
        match target {
            Target::Root => {
                if self.is_empty_pane() || dock == Dock::Center {
                    let id = self.first_pane_id();
                    return self.insert(Target::Pane(id), title, Dock::Center);
                }
                let old = std::mem::take(self);
                *self = wrap(old, title, dock, ROOT_SPLIT);
                true
            }
            Target::Pane(id) => self.insert_at_pane(id, title, dock),
        }
    }

    fn insert_at_pane(&mut self, id: u64, title: &str, dock: Dock) -> bool {
        match self {
            Node::Split(s) => s.a.insert_at_pane(id, title, dock) || s.b.insert_at_pane(id, title, dock),
            Node::Pane(p) if p.id == id => {
                if dock == Dock::Center || p.tabs.is_empty() {
                    p.tabs.push(title.to_string());
                    p.active = p.tabs.len() - 1;
                } else {
                    let old = std::mem::take(self);
                    *self = wrap(old, title, dock, PANE_SPLIT);
                }
                true
            }
            Node::Pane(_) => false,
        }
    }

    /// タブの鍵を書き換える（前の版の配置の移し替え）。書き換えたら真
    pub fn rename_title(&mut self, from: &str, to: &str) -> bool {
        match self {
            Node::Split(s) => {
                let a = s.a.rename_title(from, to);
                let b = s.b.rename_title(from, to);
                a || b
            }
            Node::Pane(p) => {
                let mut hit = false;
                for t in p.tabs.iter_mut().filter(|t| *t == from) {
                    *t = to.to_string();
                    hit = true;
                }
                hit
            }
        }
    }

    pub fn set_active(&mut self, pane: u64, index: usize) {
        if let Some(p) = self.pane_mut(pane) {
            if index < p.tabs.len() {
                p.active = index;
            }
        }
    }

    /// 題のタブを前に出す
    pub fn activate_title(&mut self, title: &str) {
        if let Some(id) = self.pane_of(title) {
            if let Some(p) = self.pane_mut(id) {
                if let Some(i) = p.tabs.iter().position(|t| t == title) {
                    p.active = i;
                }
            }
        }
    }

    pub fn set_ratio(&mut self, split: u64, ratio: f32) -> bool {
        match self {
            Node::Split(s) if s.id == split => {
                s.ratio = ratio.clamp(RATIO_MIN, RATIO_MAX);
                true
            }
            Node::Split(s) => s.a.set_ratio(split, ratio) || s.b.set_ratio(split, ratio),
            Node::Pane(_) => false,
        }
    }

    /// 区切りを動かす。**区切りに接している区画だけが大きさを変え、ほかの区画は画素の幅を保つ**。
    /// `span` はその分割の全体の長さ（区切りの向きに沿って）。
    /// 丸めの誤差が溜まらないよう、呼ぶ側はドラッグを始めたときの木に対して毎回呼ぶこと
    pub fn set_ratio_keeping(&mut self, split: u64, ratio: f32, span: i32, gap: i32) -> bool {
        match self {
            Node::Split(s) if s.id == split => {
                let avail = (span - gap).max(1) as f32;
                let r = ratio.clamp(RATIO_MIN, RATIO_MAX);
                let a_old = (avail * s.ratio).round();
                let a_new = (avail * r).round();
                s.ratio = r;
                s.a.keep(s.axis, Edge::End, a_old, a_new, gap);
                s.b.keep(s.axis, Edge::Start, avail - a_old, avail - a_new, gap);
                true
            }
            Node::Split(s) => s.a.set_ratio_keeping(split, ratio, span, gap) || s.b.set_ratio_keeping(split, ratio, span, gap),
            Node::Pane(_) => false,
        }
    }

    /// 長さが old から new に変わった木の中で、動いた辺（edge）に接していない区画の長さを保つよう割合を直す
    fn keep(&mut self, axis: Axis, edge: Edge, old: f32, new: f32, gap: i32) {
        let Node::Split(t) = self else { return };
        if t.axis != axis {
            // 向きが違う分割は、両方の子が動いた辺に接している
            t.a.keep(axis, edge, old, new, gap);
            t.b.keep(axis, edge, old, new, gap);
            return;
        }
        let g = gap as f32;
        let avail_old = (old - g).max(1.0);
        let avail_new = (new - g).max(1.0);
        let a_old = (avail_old * t.ratio).round();
        let b_old = avail_old - a_old;
        match edge {
            // 終わりの辺（右・下）が動いた: a を保ち、b が吸収する
            Edge::End => t.ratio = (a_old / avail_new).clamp(RATIO_MIN, RATIO_MAX),
            // 始まりの辺（左・上）が動いた: b を保ち、a が吸収する
            Edge::Start => t.ratio = ((avail_new - b_old) / avail_new).clamp(RATIO_MIN, RATIO_MAX),
        }
        let a_new = (avail_new * t.ratio).round();
        t.a.keep(axis, edge, a_old, a_new, gap);
        t.b.keep(axis, edge, b_old, avail_new - a_new, gap);
    }

    /// 区画の中でタブの順を変える
    pub fn move_tab(&mut self, pane: u64, from: usize, to: usize) {
        if let Some(p) = self.pane_mut(pane) {
            if from < p.tabs.len() && to < p.tabs.len() && from != to {
                let t = p.tabs.remove(from);
                p.tabs.insert(to, t);
                p.active = to;
            }
        }
    }
}

impl Node {
    /// 木の整合性を確かめる（テストと、実機での自己点検に使う）。崩れていれば理由
    pub fn check(&self) -> Result<(), String> {
        let mut titles = std::collections::HashSet::new();
        let mut ids = std::collections::HashSet::new();
        self.check_inner(true, &mut titles, &mut ids)
    }

    fn check_inner(
        &self,
        is_root: bool,
        titles: &mut std::collections::HashSet<String>,
        ids: &mut std::collections::HashSet<u64>,
    ) -> Result<(), String> {
        match self {
            Node::Split(s) => {
                if !ids.insert(s.id) {
                    return Err(format!("分割の番号 {} が重複", s.id));
                }
                if !(RATIO_MIN..=RATIO_MAX).contains(&s.ratio) || s.ratio.is_nan() {
                    return Err(format!("割合 {} が範囲の外", s.ratio));
                }
                s.a.check_inner(false, titles, ids)?;
                s.b.check_inner(false, titles, ids)
            }
            Node::Pane(p) => {
                if !ids.insert(p.id) {
                    return Err(format!("区画の番号 {} が重複", p.id));
                }
                if p.tabs.is_empty() && !is_root {
                    return Err("空の区画が残っている".into());
                }
                if !p.tabs.is_empty() && p.active >= p.tabs.len() {
                    return Err(format!("前のタブ {} がタブの数 {} 以上", p.active, p.tabs.len()));
                }
                for t in &p.tabs {
                    if !titles.insert(t.clone()) {
                        return Err(format!("タブ「{t}」が重複"));
                    }
                }
                Ok(())
            }
        }
    }
}

impl Layout {
    /// 割り付けの整合性: 見えている区画と区切りが area の中に収まり、重ならず、合わせて area をちょうど埋める
    pub fn check(&self, area: Rect) -> Result<(), String> {
        let mut rects: Vec<Rect> = self.panes.iter().filter(|p| p.visible).map(|p| p.rect).collect();
        rects.extend(self.splitters.iter().map(|s| s.rect));
        let mut sum: i64 = 0;
        for (i, a) in rects.iter().enumerate() {
            if a.w() < 0 || a.h() < 0 {
                return Err(format!("矩形の幅か高さが負 {a:?}"));
            }
            if a.is_empty() {
                continue;
            }
            if a.l < area.l || a.t < area.t || a.r > area.r || a.b > area.b {
                return Err(format!("{a:?} が {area:?} からはみ出す"));
            }
            for b in rects.iter().skip(i + 1).filter(|b| !b.is_empty()) {
                if a.l < b.r && b.l < a.r && a.t < b.b && b.t < a.b {
                    return Err(format!("{a:?} と {b:?} が重なる"));
                }
            }
            sum += a.w() as i64 * a.h() as i64;
        }
        let whole = area.w().max(0) as i64 * area.h().max(0) as i64;
        if sum != whole {
            return Err(format!("区画と区切りの面積の和 {sum} が全体 {whole} と合わない（隙間がある）"));
        }
        for p in self.panes.iter().filter(|p| p.visible) {
            if p.header.t != p.rect.t || p.body.b != p.rect.b || p.header.b != p.body.t {
                return Err(format!("見出しと中身が区画を分けていない {p:?}"));
            }
        }
        Ok(())
    }
}

/// 元の木と新しい区画を、dock の向きで分割にまとめる。`share` は新しい側の取り分
fn wrap(old: Node, title: &str, dock: Dock, share: f32) -> Node {
    let new = Node::Pane(Pane::new(vec![title.to_string()]));
    let (axis, new_first) = match dock {
        Dock::Left => (Axis::Horizontal, true),
        Dock::Right => (Axis::Horizontal, false),
        Dock::Top => (Axis::Vertical, true),
        Dock::Bottom | Dock::Center => (Axis::Vertical, false),
    };
    let (a, b, ratio) = if new_first { (new, old, share) } else { (old, new, 1.0 - share) };
    Node::Split(Split { axis, ratio, a: Box::new(a), b: Box::new(b), id: next_id() })
}

// ---- 矩形 ----

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub l: i32,
    pub t: i32,
    pub r: i32,
    pub b: i32,
}

impl Rect {
    pub const fn new(l: i32, t: i32, r: i32, b: i32) -> Self {
        Self { l, t, r, b }
    }
    pub fn w(&self) -> i32 {
        self.r - self.l
    }
    pub fn h(&self) -> i32 {
        self.b - self.t
    }
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.l && x < self.r && y >= self.t && y < self.b
    }
    pub fn is_empty(&self) -> bool {
        self.w() <= 0 || self.h() <= 0
    }
    pub fn inflate(&self, d: i32) -> Rect {
        Rect::new(self.l - d, self.t - d, self.r + d, self.b + d)
    }
    pub fn center(&self) -> (i32, i32) {
        ((self.l + self.r) / 2, (self.t + self.b) / 2)
    }
    fn centered(cx: i32, cy: i32, size: i32) -> Rect {
        Rect::new(cx - size / 2, cy - size / 2, cx - size / 2 + size, cy - size / 2 + size)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct PaneBox {
    pub id: u64,
    /// 見出しと中身を合わせた矩形
    pub rect: Rect,
    pub header: Rect,
    pub body: Rect,
    /// 最大化で隠れているときは偽
    pub visible: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct SplitterBox {
    pub id: u64,
    pub axis: Axis,
    /// 区切りの帯
    pub rect: Rect,
    /// 分割全体（割合を計算するため）
    pub span: Rect,
}

#[derive(Clone, Debug, Default)]
pub struct Layout {
    pub panes: Vec<PaneBox>,
    pub splitters: Vec<SplitterBox>,
}

/// 木を area に割り付ける。`maximized` の区画があれば、その区画だけを area いっぱいにする
pub fn layout(root: &Node, area: Rect, header_h: i32, gap: i32, maximized: Option<u64>) -> Layout {
    let mut out = Layout::default();
    let max = maximized.filter(|id| root.pane(*id).is_some());
    place(root, area, header_h, gap, max, &mut out);
    out
}

fn pane_box(id: u64, rect: Rect, header_h: i32, visible: bool) -> PaneBox {
    let hh = header_h.min(rect.h().max(0));
    PaneBox {
        id,
        rect,
        header: Rect::new(rect.l, rect.t, rect.r, rect.t + hh),
        body: Rect::new(rect.l, rect.t + hh, rect.r, rect.b),
        visible,
    }
}

fn place(node: &Node, area: Rect, header_h: i32, gap: i32, max: Option<u64>, out: &mut Layout) {
    match node {
        Node::Pane(p) => {
            let (rect, visible) = match max {
                None => (area, true),
                Some(id) if id == p.id => (area, true),
                Some(_) => (Rect::default(), false),
            };
            out.panes.push(pane_box(p.id, rect, header_h, visible));
        }
        Node::Split(s) => {
            if max.is_some() {
                // 最大化中は分割を無視して、全体の矩形をそのまま子へ渡す
                place(&s.a, area, header_h, gap, max, out);
                place(&s.b, area, header_h, gap, max, out);
                return;
            }
            let (ra, sep, rb) = split_rect(area, s.axis, s.ratio, gap);
            out.splitters.push(SplitterBox { id: s.id, axis: s.axis, rect: sep, span: area });
            place(&s.a, ra, header_h, gap, max, out);
            place(&s.b, rb, header_h, gap, max, out);
        }
    }
}

/// 分割の矩形を a・区切り・b に分ける
pub fn split_rect(area: Rect, axis: Axis, ratio: f32, gap: i32) -> (Rect, Rect, Rect) {
    // 枠が区切りより細いときは、区切りを枠の長さまでに縮める（幅が負の区画を作らない）
    let gap = gap.min(match axis {
        Axis::Horizontal => area.w(),
        Axis::Vertical => area.h(),
    }).max(0);
    match axis {
        Axis::Horizontal => {
            let avail = (area.w() - gap).max(0);
            let wa = (avail as f32 * ratio).round() as i32;
            let x = area.l + wa;
            (
                Rect::new(area.l, area.t, x, area.b),
                Rect::new(x, area.t, x + gap, area.b),
                Rect::new(x + gap, area.t, area.r, area.b),
            )
        }
        Axis::Vertical => {
            let avail = (area.h() - gap).max(0);
            let ha = (avail as f32 * ratio).round() as i32;
            let y = area.t + ha;
            (
                Rect::new(area.l, area.t, area.r, y),
                Rect::new(area.l, y, area.r, y + gap),
                Rect::new(area.l, y + gap, area.r, area.b),
            )
        }
    }
}

/// 区切りをドラッグした位置から割合を出す
pub fn ratio_at(sp: &SplitterBox, x: i32, y: i32, gap: i32) -> f32 {
    let (pos, start, len) = match sp.axis {
        Axis::Horizontal => (x, sp.span.l, sp.span.w()),
        Axis::Vertical => (y, sp.span.t, sp.span.h()),
    };
    let avail = (len - gap).max(1);
    ((pos - start - gap / 2) as f32 / avail as f32).clamp(RATIO_MIN, RATIO_MAX)
}

impl Layout {
    pub fn pane_at(&self, x: i32, y: i32) -> Option<&PaneBox> {
        self.panes.iter().find(|p| p.visible && p.rect.contains(x, y))
    }

    /// 区切りは掴みやすいように `grip` だけ太らせて判定する
    pub fn splitter_at(&self, x: i32, y: i32, grip: i32) -> Option<&SplitterBox> {
        self.splitters.iter().find(|s| {
            let r = match s.axis {
                Axis::Horizontal => Rect::new(s.rect.l - grip, s.rect.t, s.rect.r + grip, s.rect.b),
                Axis::Vertical => Rect::new(s.rect.l, s.rect.t - grip, s.rect.r, s.rect.b + grip),
            };
            r.contains(x, y)
        })
    }

    pub fn pane_box(&self, id: u64) -> Option<&PaneBox> {
        self.panes.iter().find(|p| p.id == id)
    }
}

// ---- ガイド ----

/// 区画の中央に出す 5 つのガイド（中央と上下左右）
pub fn pane_guides(body: Rect, size: i32) -> Vec<(Dock, Rect)> {
    let (cx, cy) = body.center();
    let step = size + size / 4;
    vec![
        (Dock::Center, Rect::centered(cx, cy, size)),
        (Dock::Left, Rect::centered(cx - step, cy, size)),
        (Dock::Right, Rect::centered(cx + step, cy, size)),
        (Dock::Top, Rect::centered(cx, cy - step, size)),
        (Dock::Bottom, Rect::centered(cx, cy + step, size)),
    ]
}

/// 枠全体の辺に出す 4 つのガイド
pub fn root_guides(area: Rect, size: i32) -> Vec<(Dock, Rect)> {
    let (cx, cy) = area.center();
    let m = size / 2;
    vec![
        (Dock::Left, Rect::new(area.l + m, cy - size / 2, area.l + m + size, cy - size / 2 + size)),
        (Dock::Right, Rect::new(area.r - m - size, cy - size / 2, area.r - m, cy - size / 2 + size)),
        (Dock::Top, Rect::new(cx - size / 2, area.t + m, cx - size / 2 + size, area.t + m + size)),
        (Dock::Bottom, Rect::new(cx - size / 2, area.b - m - size, cx - size / 2 + size, area.b - m)),
    ]
}

/// 落としたときに新しい区画が占める範囲（予告の矩形）
pub fn preview_rect(rect: Rect, dock: Dock, root: bool) -> Rect {
    let share = if root { ROOT_SPLIT } else { PANE_SPLIT };
    let w = (rect.w() as f32 * share) as i32;
    let h = (rect.h() as f32 * share) as i32;
    match dock {
        Dock::Center => rect,
        Dock::Left => Rect::new(rect.l, rect.t, rect.l + w, rect.b),
        Dock::Right => Rect::new(rect.r - w, rect.t, rect.r, rect.b),
        Dock::Top => Rect::new(rect.l, rect.t, rect.r, rect.t + h),
        Dock::Bottom => Rect::new(rect.l, rect.b - h, rect.r, rect.b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(tabs: &[&str]) -> Node {
        Node::Pane(Pane::new(tabs.iter().map(|s| s.to_string()).collect()))
    }

    #[test]
    fn insert_into_empty_root_makes_tab() {
        let mut n = Node::default();
        assert!(n.insert(Target::Root, "プレビュー編集", Dock::Left));
        assert_eq!(n.titles(), vec!["プレビュー編集"]);
        assert!(matches!(n, Node::Pane(_)));
    }

    #[test]
    fn edge_drop_splits_pane() {
        let mut n = pane(&["A"]);
        let id = n.first_pane_id();
        assert!(n.insert(Target::Pane(id), "B", Dock::Right));
        match &n {
            Node::Split(s) => {
                assert_eq!(s.axis, Axis::Horizontal);
                assert_eq!(s.a.titles(), vec!["A"]);
                assert_eq!(s.b.titles(), vec!["B"]);
                assert!((s.ratio - 0.5).abs() < 1e-6);
            }
            _ => panic!("分割になっていない"),
        }
        let top = n.pane_of("A").unwrap();
        assert!(n.insert(Target::Pane(top), "C", Dock::Top));
        assert_eq!(n.titles(), vec!["C", "A", "B"]);
    }

    #[test]
    fn center_drop_adds_tab_and_activates() {
        let mut n = pane(&["A", "B"]);
        let id = n.first_pane_id();
        n.insert(Target::Pane(id), "C", Dock::Center);
        let p = n.pane(id).unwrap();
        assert_eq!(p.tabs, vec!["A", "B", "C"]);
        assert_eq!(p.active_title(), Some("C"));
    }

    #[test]
    fn root_edge_uses_root_share() {
        let mut n = pane(&["A"]);
        n.insert(Target::Root, "B", Dock::Left);
        match &n {
            Node::Split(s) => {
                assert_eq!(s.a.titles(), vec!["B"]);
                assert!((s.ratio - ROOT_SPLIT).abs() < 1e-6);
            }
            _ => panic!(),
        }
        let mut n = pane(&["A"]);
        n.insert(Target::Root, "B", Dock::Bottom);
        match &n {
            Node::Split(s) => {
                assert_eq!(s.axis, Axis::Vertical);
                assert_eq!(s.b.titles(), vec!["B"]);
                assert!((s.ratio - (1.0 - ROOT_SPLIT)).abs() < 1e-6);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn remove_collapses_empty_panes() {
        let mut n = pane(&["A"]);
        let id = n.first_pane_id();
        n.insert(Target::Pane(id), "B", Dock::Right);
        let b = n.pane_of("B").unwrap();
        n.insert(Target::Pane(b), "C", Dock::Bottom);
        assert!(n.remove_title("B"));
        assert_eq!(n.titles(), vec!["A", "C"]);
        assert!(n.remove_title("A"));
        assert!(matches!(n, Node::Pane(_)));
        assert_eq!(n.titles(), vec!["C"]);
        assert!(n.remove_title("C"));
        assert!(n.is_empty_pane());
        assert!(!n.remove_title("C"));
    }

    #[test]
    fn remove_keeps_active_on_same_tab() {
        let mut n = pane(&["A", "B", "C"]);
        let id = n.first_pane_id();
        n.set_active(id, 2);
        n.remove_title("A");
        assert_eq!(n.pane(id).unwrap().active_title(), Some("C"));
        n.remove_title("C");
        assert_eq!(n.pane(id).unwrap().active_title(), Some("B"));
    }

    #[test]
    fn move_to_own_edge_then_back_is_stable() {
        // 1 枚だけの区画のタブを自分の辺へ落とす操作は、呼ぶ側が「取り除く→落とす」で行う。
        // 取り除くと区画が消えるので、落とす先が無くなり insert は偽を返す（木は空のまま変えない）
        let mut n = pane(&["A"]);
        let mut s = n.clone();
        let id = n.first_pane_id();
        n.insert(Target::Pane(id), "B", Dock::Right);
        let b = n.pane_of("B").unwrap();
        n.remove_title("B");
        assert!(!n.insert(Target::Pane(b), "B", Dock::Left));
        assert_eq!(n.titles(), vec!["A"]);
        s.renumber();
        assert_eq!(s.titles(), vec!["A"]);
    }

    #[test]
    fn layout_splits_area_and_reserves_headers() {
        let mut n = pane(&["A"]);
        let id = n.first_pane_id();
        n.insert(Target::Pane(id), "B", Dock::Right);
        let l = layout(&n, Rect::new(0, 0, 1004, 500), 22, 4, None);
        assert_eq!(l.panes.len(), 2);
        assert_eq!(l.splitters.len(), 1);
        let a = l.panes[0];
        let b = l.panes[1];
        assert_eq!(a.rect, Rect::new(0, 0, 500, 500));
        assert_eq!(l.splitters[0].rect, Rect::new(500, 0, 504, 500));
        assert_eq!(b.rect, Rect::new(504, 0, 1004, 500));
        assert_eq!(a.header, Rect::new(0, 0, 500, 22));
        assert_eq!(a.body, Rect::new(0, 22, 500, 500));
        assert_eq!(l.pane_at(700, 100).unwrap().id, b.id);
        assert!(l.pane_at(502, 100).is_none());
        assert_eq!(l.splitter_at(498, 100, 3).unwrap().id, l.splitters[0].id);
    }

    #[test]
    fn layout_maximized_shows_one_pane() {
        let mut n = pane(&["A"]);
        let id = n.first_pane_id();
        n.insert(Target::Pane(id), "B", Dock::Bottom);
        let b = n.pane_of("B").unwrap();
        let area = Rect::new(0, 0, 800, 600);
        let l = layout(&n, area, 20, 4, Some(b));
        assert!(l.splitters.is_empty());
        let pb = l.pane_box(b).unwrap();
        assert!(pb.visible);
        assert_eq!(pb.rect, area);
        assert!(!l.pane_box(id).unwrap().visible);
        // 無い区画を最大化しても普通に並ぶ
        let l = layout(&n, area, 20, 4, Some(999_999));
        assert_eq!(l.splitters.len(), 1);
    }

    /// 区画の幅（画素）を左から並べる
    fn widths(n: &Node, w: i32) -> Vec<(String, i32)> {
        let l = layout(n, Rect::new(0, 0, w, 100), 20, 4, None);
        let mut v: Vec<(String, i32, i32)> = l
            .panes
            .iter()
            .map(|p| (n.pane(p.id).unwrap().tabs[0].clone(), p.rect.l, p.rect.w()))
            .collect();
        v.sort_by_key(|x| x.1);
        v.into_iter().map(|(t, _, w)| (t, w)).collect()
    }

    #[test]
    fn dragging_splitter_keeps_other_panes() {
        // A | B | C | D（右へ入れ子）。A と B の間を動かしても、C と D の幅は変わらない
        let mut n = pane(&["A"]);
        let a = n.first_pane_id();
        n.insert(Target::Pane(a), "B", Dock::Right);
        let b = n.pane_of("B").unwrap();
        n.insert(Target::Pane(b), "C", Dock::Right);
        let c = n.pane_of("C").unwrap();
        n.insert(Target::Pane(c), "D", Dock::Right);
        let w = 1204;
        let before = widths(&n, w);
        let l = layout(&n, Rect::new(0, 0, w, 100), 20, 4, None);
        // 一番外の分割（A と残り）
        let outer = l.splitters.iter().find(|s| s.span.w() == w).copied().unwrap();
        let mut moved = n.clone();
        assert!(moved.set_ratio_keeping(outer.id, 0.6, outer.span.w(), 4));
        let after = widths(&moved, w);
        assert!(after[0].1 > before[0].1, "A が広がる: {before:?} -> {after:?}");
        assert!(after[1].1 < before[1].1, "B が縮む");
        assert!((after[2].1 - before[2].1).abs() <= 1, "C は保つ: {before:?} -> {after:?}");
        assert!((after[3].1 - before[3].1).abs() <= 1, "D は保つ: {before:?} -> {after:?}");
        assert_eq!(after.iter().map(|x| x.1).sum::<i32>() + 4 * 3, w);
    }

    #[test]
    fn dragging_splitter_moves_both_perpendicular_children() {
        // A | (B / C)。A と右の間を動かすと、B と C は両方とも幅が変わる（どちらも区切りに接している）
        let mut n = pane(&["A"]);
        let a = n.first_pane_id();
        n.insert(Target::Pane(a), "B", Dock::Right);
        let b = n.pane_of("B").unwrap();
        n.insert(Target::Pane(b), "C", Dock::Bottom);
        let l = layout(&n, Rect::new(0, 0, 1004, 400), 20, 4, None);
        let outer = l.splitters.iter().find(|s| s.axis == Axis::Horizontal).copied().unwrap();
        let mut moved = n.clone();
        moved.set_ratio_keeping(outer.id, 0.3, outer.span.w(), 4);
        let l2 = layout(&moved, Rect::new(0, 0, 1004, 400), 20, 4, None);
        let bw = l2.pane_box(moved.pane_of("B").unwrap()).unwrap().rect.w();
        let cw = l2.pane_box(moved.pane_of("C").unwrap()).unwrap().rect.w();
        assert_eq!(bw, cw);
        assert!(bw > 500);
    }

    #[test]
    fn rename_title_in_nested_panes() {
        let mut n = pane(&["A", "シーン1"]);
        let id = n.first_pane_id();
        n.insert(Target::Pane(id), "B", Dock::Bottom);
        assert!(n.rename_title("シーン1", "@scene.preview"));
        assert_eq!(n.titles(), vec!["A", "@scene.preview", "B"]);
        assert!(!n.rename_title("シーン1", "x"));
    }

    #[test]
    fn ratio_follows_cursor_and_clamps() {
        let sp = SplitterBox {
            id: 1,
            axis: Axis::Horizontal,
            rect: Rect::new(500, 0, 504, 100),
            span: Rect::new(0, 0, 1004, 100),
        };
        assert!((ratio_at(&sp, 252, 0, 4) - 0.25).abs() < 0.01);
        assert!((ratio_at(&sp, -50, 0, 4) - RATIO_MIN).abs() < 1e-6);
        assert!((ratio_at(&sp, 5000, 0, 4) - RATIO_MAX).abs() < 1e-6);
    }

    #[test]
    fn guides_do_not_overlap() {
        let g = pane_guides(Rect::new(0, 0, 400, 400), 32);
        for (i, (_, a)) in g.iter().enumerate() {
            for (_, b) in g.iter().skip(i + 1) {
                let overlap = a.l < b.r && b.l < a.r && a.t < b.b && b.t < a.b;
                assert!(!overlap, "{a:?} と {b:?} が重なる");
            }
        }
        assert_eq!(preview_rect(Rect::new(0, 0, 100, 100), Dock::Right, false), Rect::new(50, 0, 100, 100));
    }

    #[test]
    fn json_round_trip_and_unknown_keys() {
        let mut n = pane(&["A"]);
        let id = n.first_pane_id();
        n.insert(Target::Pane(id), "B", Dock::Bottom);
        let s = serde_json::to_string(&n).unwrap();
        assert!(!s.contains("\"id\""), "番号は保存しない: {s}");
        let back: Node = serde_json::from_str(&s).unwrap();
        assert_eq!(back.titles(), vec!["A", "B"]);
        let v: Node = serde_json::from_str(r#"{"tabs":["X"],"active":5,"future":1}"#).unwrap();
        let mut v = v;
        v.renumber();
        assert_eq!(v.panes()[0].active, 0);
    }
}
