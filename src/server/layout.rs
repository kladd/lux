//! Binary layout tree of windows (REQ-WINDOW-001), scoped so a later phase
//! can give each session/screen its own tree (REQ-WINDOW-002). Pure geometry
//! and tree surgery; no PTY or rendering concerns.

use ratatui::layout::Rect;

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

pub struct Separator {
    pub rect: Rect,
    pub kind: SplitKind,
}

/// Split `area` into (first, second, separator) rectangles. When the area is
/// too small to hold two windows and a separator, `first` gets everything
/// and the other two rects are zero-sized.
pub fn split_areas(kind: SplitKind, ratio: f64, area: Rect) -> (Rect, Rect, Rect) {
    match kind {
        SplitKind::SideBySide => {
            if area.width < 3 {
                let empty = Rect::new(area.right(), area.y, 0, 0);
                return (area, empty, empty);
            }
            let avail = area.width - 1;
            let first_w = (f64::from(avail) * ratio).round().clamp(1.0, f64::from(avail - 1)) as u16;
            let first = Rect { width: first_w, ..area };
            let sep = Rect { x: area.x + first_w, width: 1, ..area };
            let second = Rect { x: sep.x + 1, width: avail - first_w, ..area };
            (first, second, sep)
        }
        SplitKind::Stacked => {
            if area.height < 3 {
                let empty = Rect::new(area.x, area.bottom(), 0, 0);
                return (area, empty, empty);
            }
            let avail = area.height - 1;
            let first_h = (f64::from(avail) * ratio).round().clamp(1.0, f64::from(avail - 1)) as u16;
            let first = Rect { height: first_h, ..area };
            let sep = Rect { y: area.y + first_h, height: 1, ..area };
            let second = Rect { y: sep.y + 1, height: avail - first_h, ..area };
            (first, second, sep)
        }
    }
}

/// Compute every window's rectangle and every separator for the whole tree
/// (REQ-WINDOW-011/012 geometry, computed once then drawn from state).
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
                seps.push(Separator { rect: sep, kind: s.kind });
            }
            walk(&s.first, first, rects, seps);
            walk(&s.second, second, rects, seps);
        }
    }
}

/// Window ids in in-order traversal; the prefix+`o` cycle order
/// (REQ-WINDOW-016).
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
/// `new_id` (REQ-WINDOW-005/006 tree change).
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
/// subtree takes the whole space (REQ-WINDOW-020). Returns `None` when the
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
/// `dir` (REQ-WINDOW-017): the deepest ancestor split with a sibling on
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
        SplitKind::Stacked => area.height.saturating_sub(1),
    };
    if avail < 2 {
        return true;
    }
    let first_size = (f64::from(avail) * s.ratio).round().clamp(1.0, f64::from(avail - 1));
    // The boundary moves in `dir`: right/down grow `first`, left/up shrink it.
    let delta = match dir {
        Dir::Right | Dir::Down => 1.0,
        Dir::Left | Dir::Up => -1.0,
    };
    let new_first = (first_size + delta).clamp(1.0, f64::from(avail - 1));
    s.ratio = new_first / f64::from(avail);
    true
}

/// The window spatially adjacent to `from` in `dir` (REQ-KEY-004): the
/// nearest window on that side whose perpendicular extent overlaps `from`,
/// ties broken by largest overlap. `None` at a screen edge, leaving focus
/// unchanged (REQ-KEY-005).
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
            Dir::Left | Dir::Right => {
                rect.bottom().min(from.bottom()).saturating_sub(rect.top().max(from.top()))
            }
            Dir::Up | Dir::Down => {
                rect.right().min(from.right()).saturating_sub(rect.left().max(from.left()))
            }
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
        assert_eq!(seps.len(), 2);
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
        // REQ-KEY-005: nothing at the screen edge.
        assert_eq!(spatial_neighbor(&rects, rects[0].1, Dir::Left), None);
        assert_eq!(spatial_neighbor(&rects, rects[0].1, Dir::Up), None);
        assert_eq!(spatial_neighbor(&rects, rects[1].1, Dir::Up), None);
        assert_eq!(spatial_neighbor(&rects, rects[2].1, Dir::Down), None);
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
