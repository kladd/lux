//! Binary layout tree of windows, scoped so a later phase
//! can give each session/screen its own tree. Pure geometry
//! and tree surgery; no PTY or rendering concerns.

use ratatui::layout::{Position, Rect};

pub type WindowId = usize;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SplitKind {
    /// Two windows side by side (prefix + `%`).
    SideBySide,
    /// Two windows stacked vertically (prefix + `"`).
    Stacked,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dir {
    Left,
    Down,
    Up,
    Right,
}

#[derive(Debug)]
pub enum Node {
    Leaf(WindowId),
    Split(Split),
}

#[derive(Debug)]
pub struct Split {
    pub kind: SplitKind,
    /// Fraction of the split axis (minus the separator) given to `first`.
    pub ratio: f64,
    pub first: Box<Node>,
    pub second: Box<Node>,
}

/// A vertical separator between side-by-side windows.
/// Stacked windows abut directly — the lower window's tab bar is the
/// boundary — so no horizontal separators exist.
pub struct Separator {
    pub rect: Rect,
}

/// Split `area` into (first, second, separator) rectangles. Only
/// side-by-side splits have a separator column; stacked
/// splits partition the area completely. When the area is
/// too small to hold two windows, `first` gets everything and the other
/// rects are zero-sized.
pub fn split_areas(kind: SplitKind, ratio: f64, area: Rect) -> (Rect, Rect, Rect) {
    match kind {
        SplitKind::SideBySide => {
            if area.width < 3 {
                let empty = Rect::new(area.right(), area.y, 0, 0);
                return (area, empty, empty);
            }
            let avail = area.width - 1;
            let first_w = (f64::from(avail) * ratio)
                .round()
                .clamp(1.0, f64::from(avail - 1)) as u16;
            let first = Rect {
                width: first_w,
                ..area
            };
            let sep = Rect {
                x: area.x + first_w,
                width: 1,
                ..area
            };
            let second = Rect {
                x: sep.x + 1,
                width: avail - first_w,
                ..area
            };
            (first, second, sep)
        }
        SplitKind::Stacked => {
            if area.height < 2 {
                let empty = Rect::new(area.x, area.bottom(), 0, 0);
                return (area, empty, empty);
            }
            let avail = area.height;
            let first_h = (f64::from(avail) * ratio)
                .round()
                .clamp(1.0, f64::from(avail - 1)) as u16;
            let first = Rect {
                height: first_h,
                ..area
            };
            let second = Rect {
                y: area.y + first_h,
                height: avail - first_h,
                ..area
            };
            let empty = Rect::new(area.x, area.y + first_h, 0, 0);
            (first, second, empty)
        }
    }
}

/// Compute every window's rectangle and every separator for the whole tree
/// (geometry, computed once then drawn from state).
pub fn compute(node: &Node, area: Rect) -> (Vec<(WindowId, Rect)>, Vec<Separator>) {
    let mut rects = Vec::new();
    let mut seps = Vec::new();
    walk(node, area, &mut rects, &mut seps);
    (rects, seps)
}

fn walk(node: &Node, area: Rect, rects: &mut Vec<(WindowId, Rect)>, seps: &mut Vec<Separator>) {
    match node {
        Node::Leaf(id) => rects.push((*id, area)),
        Node::Split(s) => {
            let (first, second, sep) = split_areas(s.kind, s.ratio, area);
            if sep.width > 0 && sep.height > 0 {
                seps.push(Separator { rect: sep });
            }
            walk(&s.first, first, rects, seps);
            walk(&s.second, second, rects, seps);
        }
    }
}

/// Window ids in in-order traversal; the prefix+`o` cycle order.
pub fn leaves(node: &Node) -> Vec<WindowId> {
    match node {
        Node::Leaf(id) => vec![*id],
        Node::Split(s) => {
            let mut ids = leaves(&s.first);
            ids.extend(leaves(&s.second));
            ids
        }
    }
}

pub fn contains(node: &Node, id: WindowId) -> bool {
    match node {
        Node::Leaf(leaf) => *leaf == id,
        Node::Split(s) => contains(&s.first, id) || contains(&s.second, id),
    }
}

/// Replace `target`'s leaf with a split holding it and a new leaf for
/// `new_id`.
pub fn split_leaf(node: &mut Node, target: WindowId, kind: SplitKind, new_id: WindowId) {
    match node {
        Node::Leaf(id) if *id == target => {
            *node = Node::Split(Split {
                kind,
                ratio: 0.5,
                first: Box::new(Node::Leaf(target)),
                second: Box::new(Node::Leaf(new_id)),
            });
        }
        Node::Leaf(_) => {}
        Node::Split(s) => {
            split_leaf(&mut s.first, target, kind, new_id);
            split_leaf(&mut s.second, target, kind, new_id);
        }
    }
}

/// Remove `target`'s leaf, collapsing its parent split so the sibling
/// subtree takes the whole space. Returns `None` when the
/// tree was just that leaf.
pub fn remove_leaf(node: Node, target: WindowId) -> Option<Node> {
    match node {
        Node::Leaf(id) if id == target => None,
        Node::Leaf(_) => Some(node),
        Node::Split(mut s) => match remove_leaf(*s.first, target) {
            None => Some(*s.second),
            Some(first) => match remove_leaf(*s.second, target) {
                None => Some(first),
                Some(second) => {
                    s.first = Box::new(first);
                    s.second = Box::new(second);
                    Some(Node::Split(s))
                }
            },
        },
    }
}

/// Move the boundary between `focused` and its adjacent sibling one cell in
/// `dir`: the deepest ancestor split with a sibling on
/// that side owns the boundary. Returns whether a boundary was found.
pub fn resize_toward(node: &mut Node, area: Rect, focused: WindowId, dir: Dir) -> bool {
    let Node::Split(s) = node else { return false };
    let (first_area, second_area, _) = split_areas(s.kind, s.ratio, area);
    let in_first = contains(&s.first, focused);
    let (child, child_area) = if in_first {
        (&mut s.first, first_area)
    } else {
        (&mut s.second, second_area)
    };
    if resize_toward(child, child_area, focused, dir) {
        return true;
    }
    let owns_boundary = match dir {
        Dir::Right => s.kind == SplitKind::SideBySide && in_first,
        Dir::Left => s.kind == SplitKind::SideBySide && !in_first,
        Dir::Down => s.kind == SplitKind::Stacked && in_first,
        Dir::Up => s.kind == SplitKind::Stacked && !in_first,
    };
    if !owns_boundary {
        return false;
    }
    let avail = match s.kind {
        SplitKind::SideBySide => area.width.saturating_sub(1),
        // Stacked windows abut, no separator row.
        SplitKind::Stacked => area.height,
    };
    if avail < 2 {
        return true;
    }
    let first_size = (f64::from(avail) * s.ratio)
        .round()
        .clamp(1.0, f64::from(avail - 1));
    // The boundary moves in `dir`: right/down grow `first`, left/up shrink it.
    let delta = match dir {
        Dir::Right | Dir::Down => 1.0,
        Dir::Left | Dir::Up => -1.0,
    };
    let new_first = (first_size + delta).clamp(1.0, f64::from(avail - 1));
    s.ratio = new_first / f64::from(avail);
    true
}

/// One step from a split to a child; a path of these addresses a split
/// stably while its ratio changes during a drag.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Side {
    First,
    Second,
}

/// The split whose draggable boundary lies under `pos`, as the path from
/// the root plus the split's kind: the separator column of a side-by-side
/// split, or the top row of a stacked split's lower half (the lower
/// window's tab bar). A parent's boundary wins where a child's crosses it.
pub fn boundary_at(node: &Node, area: Rect, pos: Position) -> Option<(Vec<Side>, SplitKind)> {
    let mut path = Vec::new();
    let mut node = node;
    let mut area = area;
    loop {
        let Node::Split(s) = node else { return None };
        let (first, second, sep) = split_areas(s.kind, s.ratio, area);
        let hit = match s.kind {
            SplitKind::SideBySide => {
                // Widen the hit zone by one cell on each side so the
                // separator doesn't have to be clicked pixel-perfectly.
                let hit_zone = Rect {
                    x: sep.x.saturating_sub(1),
                    width: (sep.width + 2).min(area.width),
                    ..sep
                };
                hit_zone.contains(pos) && area.contains(pos)
            }
            SplitKind::Stacked => second.height > 0 && pos.y == second.y && area.contains(pos),
        };
        if hit {
            return Some((path, s.kind));
        }
        (node, area) = if first.contains(pos) {
            path.push(Side::First);
            (s.first.as_ref(), first)
        } else if second.contains(pos) {
            path.push(Side::Second);
            (s.second.as_ref(), second)
        } else {
            return None;
        };
    }
}

/// Drag the boundary of the split at `path` toward the screen position
/// `to`, one cell at a time, stopping where any window would shrink below
/// `min` (columns, rows); a window already below the minimum may only
/// grow. Returns whether the boundary moved.
pub fn drag_boundary(
    node: &mut Node,
    area: Rect,
    path: &[Side],
    to: Position,
    min: (u16, u16),
) -> bool {
    let mut node = node;
    let mut area = area;
    for side in path {
        let Node::Split(s) = node else { return false };
        let (first, second, _) = split_areas(s.kind, s.ratio, area);
        (node, area) = match side {
            Side::First => (s.first.as_mut(), first),
            Side::Second => (s.second.as_mut(), second),
        };
    }
    let Node::Split(s) = node else { return false };
    let avail = match s.kind {
        SplitKind::SideBySide => area.width.saturating_sub(1),
        SplitKind::Stacked => area.height,
    };
    if avail < 2 {
        return false;
    }
    // The boundary sits at the first half's far edge.
    let target = match s.kind {
        SplitKind::SideBySide => to.x.saturating_sub(area.x),
        SplitKind::Stacked => to.y.saturating_sub(area.y),
    }
    .clamp(1, avail - 1);
    let mut size = (f64::from(avail) * s.ratio)
        .round()
        .clamp(1.0, f64::from(avail - 1)) as u16;
    let mut moved = false;
    while size != target {
        let next = if target > size { size + 1 } else { size - 1 };
        let before = subtree_rects(s, area);
        let prev_ratio = s.ratio;
        s.ratio = f64::from(next) / f64::from(avail);
        if !step_fits(&before, &subtree_rects(s, area), min) {
            s.ratio = prev_ratio;
            break;
        }
        size = next;
        moved = true;
    }
    moved
}

/// Every window rectangle under one split, at its current ratio.
fn subtree_rects(s: &Split, area: Rect) -> Vec<(WindowId, Rect)> {
    let (first, second, _) = split_areas(s.kind, s.ratio, area);
    let mut rects = Vec::new();
    let mut seps = Vec::new();
    walk(&s.first, first, &mut rects, &mut seps);
    walk(&s.second, second, &mut rects, &mut seps);
    rects
}

/// Whether a boundary step leaves every window at or above the minimum in
/// each dimension — or at least no smaller than it already was.
fn step_fits(
    before: &[(WindowId, Rect)],
    after: &[(WindowId, Rect)],
    (min_cols, min_rows): (u16, u16),
) -> bool {
    after.iter().all(|&(id, rect)| {
        let old = before
            .iter()
            .find(|(before_id, _)| *before_id == id)
            .map(|&(_, r)| r)
            .unwrap_or(rect);
        (rect.width >= min_cols || rect.width >= old.width)
            && (rect.height >= min_rows || rect.height >= old.height)
    })
}

/// Exchange the tree positions of leaves `a` and `b`. Every split's kind
/// and ratio stay put, so windows of different sizes trade sizes as they
/// trade places. Returns whether both leaves exist.
pub fn swap_leaves(node: &mut Node, a: WindowId, b: WindowId) -> bool {
    fn exchange(node: &mut Node, a: WindowId, b: WindowId) {
        match node {
            Node::Leaf(id) if *id == a => *id = b,
            Node::Leaf(id) if *id == b => *id = a,
            Node::Leaf(_) => {}
            Node::Split(s) => {
                exchange(&mut s.first, a, b);
                exchange(&mut s.second, a, b);
            }
        }
    }
    if a == b || !contains(node, a) || !contains(node, b) {
        return false;
    }
    exchange(node, a, b);
    true
}

/// Flip the orientation of the split immediately containing `focused` —
/// the direct parent of that leaf. Returns whether the leaf has an
/// enclosing split.
pub fn rotate(node: &mut Node, focused: WindowId) -> bool {
    let Node::Split(s) = node else { return false };
    let is_parent = [&s.first, &s.second]
        .into_iter()
        .any(|child| matches!(child.as_ref(), Node::Leaf(id) if *id == focused));
    if is_parent {
        s.kind = match s.kind {
            SplitKind::SideBySide => SplitKind::Stacked,
            SplitKind::Stacked => SplitKind::SideBySide,
        };
        return true;
    }
    rotate(&mut s.first, focused) || rotate(&mut s.second, focused)
}

/// Reset every split in the tree to an even division between its two
/// children.
pub fn rebalance(node: &mut Node) {
    if let Node::Split(s) = node {
        s.ratio = 0.5;
        rebalance(&mut s.first);
        rebalance(&mut s.second);
    }
}

/// The window spatially adjacent to `from` in `dir`: the
/// nearest window on that side whose perpendicular extent overlaps `from`,
/// ties broken by largest overlap. `None` at a screen edge, leaving focus
/// unchanged.
pub fn spatial_neighbor(rects: &[(WindowId, Rect)], from: Rect, dir: Dir) -> Option<WindowId> {
    let mut best: Option<(WindowId, u16, u16)> = None;
    for &(id, rect) in rects {
        let dist = match dir {
            Dir::Right if rect.left() >= from.right() => rect.left() - from.right(),
            Dir::Left if rect.right() <= from.left() => from.left() - rect.right(),
            Dir::Down if rect.top() >= from.bottom() => rect.top() - from.bottom(),
            Dir::Up if rect.bottom() <= from.top() => from.top() - rect.bottom(),
            _ => continue,
        };
        let overlap = match dir {
            Dir::Left | Dir::Right => rect
                .bottom()
                .min(from.bottom())
                .saturating_sub(rect.top().max(from.top())),
            Dir::Up | Dir::Down => rect
                .right()
                .min(from.right())
                .saturating_sub(rect.left().max(from.left())),
        };
        if overlap == 0 {
            continue;
        }
        let better = match best {
            None => true,
            Some((_, best_dist, best_overlap)) => {
                dist < best_dist || (dist == best_dist && overlap > best_overlap)
            }
        };
        if better {
            best = Some((id, dist, overlap));
        }
    }
    best.map(|(id, _, _)| id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect::new(0, 0, 80, 24)
    }

    #[test]
    fn split_areas_partition_the_area() {
        let (first, second, sep) = split_areas(SplitKind::SideBySide, 0.5, area());
        assert_eq!(first.width + sep.width + second.width, 80);
        assert_eq!(first.height, 24);
        assert_eq!(second.height, 24);
        assert_eq!(sep.x, first.right());
        assert_eq!(second.x, sep.right());
        // Stacked windows abut with no separator row.
        let (first, second, sep) = split_areas(SplitKind::Stacked, 0.5, area());
        assert_eq!(first.height + second.height, 24);
        assert_eq!(second.y, first.bottom());
        assert_eq!(sep.area(), 0);
    }

    #[test]
    fn split_and_remove_round_trip() {
        let mut tree = Node::Leaf(1);
        split_leaf(&mut tree, 1, SplitKind::SideBySide, 2);
        split_leaf(&mut tree, 2, SplitKind::Stacked, 3);
        assert_eq!(leaves(&tree), vec![1, 2, 3]);

        let tree = remove_leaf(tree, 2).unwrap();
        assert_eq!(leaves(&tree), vec![1, 3]);
        let tree = remove_leaf(tree, 1).unwrap();
        assert_eq!(leaves(&tree), vec![3]);
        assert!(remove_leaf(tree, 3).is_none());
    }

    #[test]
    fn compute_covers_all_leaves_without_overlap() {
        let mut tree = Node::Leaf(1);
        split_leaf(&mut tree, 1, SplitKind::SideBySide, 2);
        split_leaf(&mut tree, 2, SplitKind::Stacked, 3);
        let (rects, seps) = compute(&tree, area());
        assert_eq!(rects.len(), 3);
        // Only the side-by-side split has a separator.
        assert_eq!(seps.len(), 1);
        let cells: u32 = rects.iter().map(|(_, r)| r.area() as u32).sum::<u32>()
            + seps.iter().map(|s| s.rect.area() as u32).sum::<u32>();
        assert_eq!(cells, area().area() as u32);
    }

    #[test]
    fn spatial_neighbor_picks_nearest_overlapping_window() {
        // ┌───────┬───────┐
        // │       │   2   │
        // │   1   ├───────┤
        // │       │   3   │
        // └───────┴───────┘
        let rects = [
            (1, Rect::new(0, 0, 40, 24)),
            (2, Rect::new(41, 0, 39, 11)),
            (3, Rect::new(41, 12, 39, 12)),
        ];
        // From 1, right: tie on distance; 3 overlaps 12 rows vs 2's 11.
        assert_eq!(spatial_neighbor(&rects, rects[0].1, Dir::Right), Some(3));
        assert_eq!(spatial_neighbor(&rects, rects[1].1, Dir::Down), Some(3));
        assert_eq!(spatial_neighbor(&rects, rects[2].1, Dir::Up), Some(2));
        assert_eq!(spatial_neighbor(&rects, rects[1].1, Dir::Left), Some(1));
        // Nothing at the screen edge.
        assert_eq!(spatial_neighbor(&rects, rects[0].1, Dir::Left), None);
        assert_eq!(spatial_neighbor(&rects, rects[0].1, Dir::Up), None);
        assert_eq!(spatial_neighbor(&rects, rects[1].1, Dir::Up), None);
        assert_eq!(spatial_neighbor(&rects, rects[2].1, Dir::Down), None);
    }

    #[test]
    fn rebalance_evens_every_split() {
        // Every split at every depth returns to an even
        // division — the layout matches a freshly built identical tree.
        let mut tree = Node::Leaf(1);
        split_leaf(&mut tree, 1, SplitKind::SideBySide, 2);
        split_leaf(&mut tree, 2, SplitKind::Stacked, 3);
        let mut even = Node::Leaf(1);
        split_leaf(&mut even, 1, SplitKind::SideBySide, 2);
        split_leaf(&mut even, 2, SplitKind::Stacked, 3);
        for _ in 0..5 {
            assert!(resize_toward(&mut tree, area(), 1, Dir::Right));
            assert!(resize_toward(&mut tree, area(), 2, Dir::Down));
        }
        assert_ne!(compute(&tree, area()).0, compute(&even, area()).0);
        rebalance(&mut tree);
        assert_eq!(compute(&tree, area()).0, compute(&even, area()).0);
    }

    #[test]
    fn swap_exchanges_two_leaf_positions() {
        // 1 | (2 / 3): swapping 1 and 3 leaves the splits alone and
        // trades the leaves' places, even across subtrees.
        let mut tree = Node::Leaf(1);
        split_leaf(&mut tree, 1, SplitKind::SideBySide, 2);
        split_leaf(&mut tree, 2, SplitKind::Stacked, 3);
        assert!(swap_leaves(&mut tree, 1, 3));
        assert_eq!(leaves(&tree), vec![3, 2, 1]);
        assert!(swap_leaves(&mut tree, 3, 2));
        assert_eq!(leaves(&tree), vec![2, 3, 1]);
        // A missing leaf or a self-swap changes nothing.
        assert!(!swap_leaves(&mut tree, 1, 9));
        assert!(!swap_leaves(&mut tree, 1, 1));
        assert_eq!(leaves(&tree), vec![2, 3, 1]);
    }

    #[test]
    fn swap_trades_sizes_with_positions() {
        // An uneven split's larger share stays on its side; the windows
        // trade sizes as they trade places.
        let mut tree = Node::Leaf(1);
        split_leaf(&mut tree, 1, SplitKind::SideBySide, 2);
        for _ in 0..5 {
            assert!(resize_toward(&mut tree, area(), 1, Dir::Right));
        }
        let before: std::collections::HashMap<_, _> =
            compute(&tree, area()).0.into_iter().collect();
        assert!(swap_leaves(&mut tree, 1, 2));
        let after: std::collections::HashMap<_, _> = compute(&tree, area()).0.into_iter().collect();
        assert_eq!(before[&1].width, after[&2].width);
        assert_eq!(before[&2].width, after[&1].width);
    }

    #[test]
    fn rotate_flips_the_enclosing_split() {
        // 1 | (2 / 3): window 3's parent is the inner stack; rotating from
        // it turns the stack side-by-side and back.
        let mut tree = Node::Leaf(1);
        split_leaf(&mut tree, 1, SplitKind::SideBySide, 2);
        split_leaf(&mut tree, 2, SplitKind::Stacked, 3);
        assert!(rotate(&mut tree, 3));
        let Node::Split(s) = &tree else {
            panic!("root is a split")
        };
        let Node::Split(inner) = s.second.as_ref() else {
            panic!("inner is a split");
        };
        assert_eq!(inner.kind, SplitKind::SideBySide);
        // The root split is untouched.
        assert_eq!(s.kind, SplitKind::SideBySide);
        assert!(rotate(&mut tree, 3));
        let Node::Split(s) = &tree else {
            panic!("root is a split")
        };
        let Node::Split(inner) = s.second.as_ref() else {
            panic!("inner is a split");
        };
        assert_eq!(inner.kind, SplitKind::Stacked);
        // A lone leaf has no orientation to flip.
        assert!(!rotate(&mut Node::Leaf(1), 1));
    }

    #[test]
    fn boundary_at_finds_separators_and_stacked_rows() {
        // 1 | (2 / 3): the root's separator column and the inner stacked
        // boundary at window 3's tab bar row.
        let mut tree = Node::Leaf(1);
        split_leaf(&mut tree, 1, SplitKind::SideBySide, 2);
        split_leaf(&mut tree, 2, SplitKind::Stacked, 3);
        let (rects, seps) = compute(&tree, area());
        let sep = seps[0].rect;
        assert_eq!(
            boundary_at(&tree, area(), Position::new(sep.x, 5)),
            Some((vec![], SplitKind::SideBySide))
        );
        let three = rects.iter().find(|(id, _)| *id == 3).unwrap().1;
        assert_eq!(
            boundary_at(&tree, area(), Position::new(three.x + 1, three.y)),
            Some((vec![Side::Second], SplitKind::Stacked))
        );
        // Window content is no boundary.
        assert_eq!(boundary_at(&tree, area(), Position::new(1, 1)), None);
    }

    #[test]
    fn parent_boundary_wins_where_a_child_boundary_crosses_it() {
        // 1 / (2 | 3): the lower half's top row contains the inner
        // separator's first cell; the stacked boundary claims it.
        let mut tree = Node::Leaf(1);
        split_leaf(&mut tree, 1, SplitKind::Stacked, 2);
        split_leaf(&mut tree, 2, SplitKind::SideBySide, 3);
        let (_, seps) = compute(&tree, area());
        let sep = seps[0].rect;
        assert_eq!(
            boundary_at(&tree, area(), Position::new(sep.x, sep.y)),
            Some((vec![], SplitKind::Stacked))
        );
        // Below the top row the separator is its own boundary.
        assert_eq!(
            boundary_at(&tree, area(), Position::new(sep.x, sep.y + 1)),
            Some((vec![Side::Second], SplitKind::SideBySide))
        );
    }

    #[test]
    fn drag_moves_the_boundary_to_the_mouse() {
        let mut tree = Node::Leaf(1);
        split_leaf(&mut tree, 1, SplitKind::SideBySide, 2);
        assert!(drag_boundary(
            &mut tree,
            area(),
            &[],
            Position::new(30, 5),
            (10, 3)
        ));
        let (rects, seps) = compute(&tree, area());
        assert_eq!(seps[0].rect.x, 30);
        assert_eq!(rects[0].1.width, 30);
    }

    #[test]
    fn drag_stops_at_the_minimum_window_size() {
        let mut tree = Node::Leaf(1);
        split_leaf(&mut tree, 1, SplitKind::SideBySide, 2);
        // Dragging far past the floor stops where the left window
        // reaches the minimum width.
        assert!(drag_boundary(
            &mut tree,
            area(),
            &[],
            Position::new(0, 5),
            (10, 3)
        ));
        assert_eq!(compute(&tree, area()).0[0].1.width, 10);
        // Already at the floor: nothing moves.
        assert!(!drag_boundary(
            &mut tree,
            area(),
            &[],
            Position::new(0, 5),
            (10, 3)
        ));
        // Stacked: the lower window stops at the minimum height.
        let mut tree = Node::Leaf(1);
        split_leaf(&mut tree, 1, SplitKind::Stacked, 2);
        assert!(drag_boundary(
            &mut tree,
            area(),
            &[],
            Position::new(5, 24),
            (10, 3)
        ));
        assert_eq!(compute(&tree, area()).0[1].1.height, 3);
    }

    #[test]
    fn drag_never_shrinks_a_window_already_below_minimum() {
        // Keyboard resize can push a window below the drag floor; the
        // drag may not shrink it further but may still grow it.
        let mut tree = Node::Leaf(1);
        split_leaf(&mut tree, 1, SplitKind::SideBySide, 2);
        for _ in 0..30 {
            resize_toward(&mut tree, area(), 1, Dir::Right);
        }
        assert!(compute(&tree, area()).0[1].1.width < 10);
        // Toward the too-narrow window: no movement.
        assert!(!drag_boundary(
            &mut tree,
            area(),
            &[],
            Position::new(79, 5),
            (10, 3)
        ));
        // Away from it: the boundary moves freely.
        assert!(drag_boundary(
            &mut tree,
            area(),
            &[],
            Position::new(40, 5),
            (10, 3)
        ));
    }

    #[test]
    fn resize_moves_the_boundary_one_cell() {
        let mut tree = Node::Leaf(1);
        split_leaf(&mut tree, 1, SplitKind::SideBySide, 2);
        let before = compute(&tree, area()).0[0].1.width;
        assert!(resize_toward(&mut tree, area(), 1, Dir::Right));
        assert_eq!(compute(&tree, area()).0[0].1.width, before + 1);
        // Window 1 has no left sibling: no boundary to its left.
        assert!(!resize_toward(&mut tree, area(), 1, Dir::Left));
        // From the second window, the same boundary moves back left.
        assert!(resize_toward(&mut tree, area(), 2, Dir::Left));
        assert_eq!(compute(&tree, area()).0[0].1.width, before);
    }
}
