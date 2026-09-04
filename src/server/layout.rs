//! Binary layout tree of windows: pure geometry and tree surgery.

use ratatui::layout::{Position, Rect};

pub type WindowId = usize;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SplitKind {
    SideBySide,
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

/// The column between side-by-side windows. Stacked windows abut, so
/// there is no row separator.
pub struct Separator {
    pub rect: Rect,
}

/// Split `area` into (first, second, separator) rectangles. If the area
/// can't hold two windows, `first` takes it all and the others are empty.
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

/// Every window rectangle and separator in the tree.
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

/// Window ids in tree order. Focus cycling follows it.
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

/// Remove `target`'s leaf and let its sibling take the space. `None`
/// means the tree was only that leaf.
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

/// The split directly above `id`'s leaf, and which side the leaf is on.
pub fn parent_split(node: &Node, id: WindowId) -> Option<(SplitKind, Side)> {
    let Node::Split(s) = node else { return None };
    if matches!(*s.first, Node::Leaf(leaf) if leaf == id) {
        return Some((s.kind, Side::First));
    }
    if matches!(*s.second, Node::Leaf(leaf) if leaf == id) {
        return Some((s.kind, Side::Second));
    }
    parent_split(&s.first, id).or_else(|| parent_split(&s.second, id))
}

/// Nudge the boundary on `focused`'s `dir` side by one cell. Returns false
/// when no split has a sibling on that side.
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
        SplitKind::Stacked => area.height,
    };
    if avail < 2 {
        return true;
    }
    let first_size = (f64::from(avail) * s.ratio)
        .round()
        .clamp(1.0, f64::from(avail - 1));
    let delta = match dir {
        Dir::Right | Dir::Down => 1.0,
        Dir::Left | Dir::Up => -1.0,
    };
    let new_first = (first_size + delta).clamp(1.0, f64::from(avail - 1));
    s.ratio = new_first / f64::from(avail);
    true
}

/// One step down the tree. A path of these keeps addressing the same
/// split while a drag changes its ratio.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Side {
    First,
    Second,
}

/// The split whose boundary is under `pos`: the separator of a
/// side-by-side split, or the top row of a stacked split's lower half. A
/// parent's boundary wins where a child's crosses it.
pub fn boundary_at(node: &Node, area: Rect, pos: Position) -> Option<(Vec<Side>, SplitKind)> {
    let mut path = Vec::new();
    let mut node = node;
    let mut area = area;
    loop {
        let Node::Split(s) = node else { return None };
        let (first, second, sep) = split_areas(s.kind, s.ratio, area);
        let hit = match s.kind {
            SplitKind::SideBySide => {
                // One cell of slack on each side so clicks needn't be exact.
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

/// Drag the split at `path` toward `to` until a window would shrink below
/// `min` (cols, rows). A window already below `min` may grow but not shrink.
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

fn subtree_rects(s: &Split, area: Rect) -> Vec<(WindowId, Rect)> {
    let (first, second, _) = split_areas(s.kind, s.ratio, area);
    let mut rects = Vec::new();
    let mut seps = Vec::new();
    walk(&s.first, first, &mut rects, &mut seps);
    walk(&s.second, second, &mut rects, &mut seps);
    rects
}

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

/// Splits keep their kind and ratio, so the two windows trade sizes as
/// well as places.
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

/// Flip the orientation of `focused`'s parent split.
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

pub fn rebalance(node: &mut Node) {
    if let Node::Split(s) = node {
        s.ratio = 0.5;
        rebalance(&mut s.first);
        rebalance(&mut s.second);
    }
}

/// The nearest window on `from`'s `dir` side that overlaps it
/// perpendicularly, ties broken by most overlap.
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
    fn parent_split_names_the_leafs_split_and_side() {
        let mut tree = Node::Leaf(1);
        assert_eq!(parent_split(&tree, 1), None);
        split_leaf(&mut tree, 1, SplitKind::SideBySide, 2);
        split_leaf(&mut tree, 2, SplitKind::Stacked, 3);
        assert_eq!(
            parent_split(&tree, 1),
            Some((SplitKind::SideBySide, Side::First))
        );
        assert_eq!(
            parent_split(&tree, 2),
            Some((SplitKind::Stacked, Side::First))
        );
        assert_eq!(
            parent_split(&tree, 3),
            Some((SplitKind::Stacked, Side::Second))
        );
        assert_eq!(parent_split(&tree, 4), None);
    }

    #[test]
    fn compute_covers_all_leaves_without_overlap() {
        let mut tree = Node::Leaf(1);
        split_leaf(&mut tree, 1, SplitKind::SideBySide, 2);
        split_leaf(&mut tree, 2, SplitKind::Stacked, 3);
        let (rects, seps) = compute(&tree, area());
        assert_eq!(rects.len(), 3);
        assert_eq!(seps.len(), 1);
        let cells: u32 = rects.iter().map(|(_, r)| r.area()).sum::<u32>()
            + seps.iter().map(|s| s.rect.area()).sum::<u32>();
        assert_eq!(cells, area().area());
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
        // Right from 1: same distance, but 3 overlaps 12 rows to 2's 11.
        assert_eq!(spatial_neighbor(&rects, rects[0].1, Dir::Right), Some(3));
        assert_eq!(spatial_neighbor(&rects, rects[1].1, Dir::Down), Some(3));
        assert_eq!(spatial_neighbor(&rects, rects[2].1, Dir::Up), Some(2));
        assert_eq!(spatial_neighbor(&rects, rects[1].1, Dir::Left), Some(1));
        assert_eq!(spatial_neighbor(&rects, rects[0].1, Dir::Left), None);
        assert_eq!(spatial_neighbor(&rects, rects[0].1, Dir::Up), None);
        assert_eq!(spatial_neighbor(&rects, rects[1].1, Dir::Up), None);
        assert_eq!(spatial_neighbor(&rects, rects[2].1, Dir::Down), None);
    }

    #[test]
    fn rebalance_evens_every_split() {
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
        let mut tree = Node::Leaf(1);
        split_leaf(&mut tree, 1, SplitKind::SideBySide, 2);
        split_leaf(&mut tree, 2, SplitKind::Stacked, 3);
        assert!(swap_leaves(&mut tree, 1, 3));
        assert_eq!(leaves(&tree), vec![3, 2, 1]);
        assert!(swap_leaves(&mut tree, 3, 2));
        assert_eq!(leaves(&tree), vec![2, 3, 1]);
        assert!(!swap_leaves(&mut tree, 1, 9));
        assert!(!swap_leaves(&mut tree, 1, 1));
        assert_eq!(leaves(&tree), vec![2, 3, 1]);
    }

    #[test]
    fn swap_trades_sizes_with_positions() {
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
        assert_eq!(s.kind, SplitKind::SideBySide);
        assert!(rotate(&mut tree, 3));
        let Node::Split(s) = &tree else {
            panic!("root is a split")
        };
        let Node::Split(inner) = s.second.as_ref() else {
            panic!("inner is a split");
        };
        assert_eq!(inner.kind, SplitKind::Stacked);
        assert!(!rotate(&mut Node::Leaf(1), 1));
    }

    #[test]
    fn boundary_at_finds_separators_and_stacked_rows() {
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
        assert_eq!(boundary_at(&tree, area(), Position::new(1, 1)), None);
    }

    #[test]
    fn parent_boundary_wins_where_a_child_boundary_crosses_it() {
        let mut tree = Node::Leaf(1);
        split_leaf(&mut tree, 1, SplitKind::Stacked, 2);
        split_leaf(&mut tree, 2, SplitKind::SideBySide, 3);
        let (_, seps) = compute(&tree, area());
        let sep = seps[0].rect;
        assert_eq!(
            boundary_at(&tree, area(), Position::new(sep.x, sep.y)),
            Some((vec![], SplitKind::Stacked))
        );
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
        assert!(drag_boundary(
            &mut tree,
            area(),
            &[],
            Position::new(0, 5),
            (10, 3)
        ));
        assert_eq!(compute(&tree, area()).0[0].1.width, 10);
        assert!(!drag_boundary(
            &mut tree,
            area(),
            &[],
            Position::new(0, 5),
            (10, 3)
        ));
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
        // Keyboard resize has no floor, so it can push a window below the
        // drag minimum.
        let mut tree = Node::Leaf(1);
        split_leaf(&mut tree, 1, SplitKind::SideBySide, 2);
        for _ in 0..30 {
            resize_toward(&mut tree, area(), 1, Dir::Right);
        }
        assert!(compute(&tree, area()).0[1].1.width < 10);
        assert!(!drag_boundary(
            &mut tree,
            area(),
            &[],
            Position::new(79, 5),
            (10, 3)
        ));
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
        assert!(!resize_toward(&mut tree, area(), 1, Dir::Left));
        assert!(resize_toward(&mut tree, area(), 2, Dir::Left));
        assert_eq!(compute(&tree, area()).0[0].1.width, before);
    }
}
