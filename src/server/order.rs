//! The user's arrangement of the session list.

use super::SessionId;
use super::host::HostId;

/// Sessions and hosts it doesn't name follow the named ones, by id, so new
/// ones land at the end of their group.
#[derive(Clone, Default)]
pub struct ListOrder {
    sessions: Vec<SessionId>,
    hosts: Vec<HostId>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Item {
    Session(SessionId),
    /// A host's whole group, dragged by its heading.
    Host(HostId),
}

/// One row of the list: a host's heading, or a session in its group.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Slot {
    /// `None` is this host's own group.
    pub group: Option<HostId>,
    /// `None` on a heading.
    pub session: Option<SessionId>,
}

/// Where a drop over a row lands: in that row's session's or group's
/// place, on the side the item was dragged toward.
struct Target<T> {
    at: T,
    after: bool,
}

fn rank<T: PartialEq + Copy>(list: &[T], id: T) -> usize {
    list.iter().position(|&x| x == id).unwrap_or(usize::MAX)
}

impl ListOrder {
    /// This host's sessions first, then each host's group.
    pub fn sort(&self, mut ids: Vec<(Option<HostId>, SessionId)>) -> Vec<SessionId> {
        ids.sort_by_key(|&(host, sid)| {
            (
                host.map(|h| (rank(&self.hosts, h), h)),
                rank(&self.sessions, sid),
                sid,
            )
        });
        ids.into_iter().map(|(_, sid)| sid).collect()
    }

    /// Moves `item` into place for a drop over row `over`. False when the
    /// drop leaves the order as it was.
    pub fn drop(&mut self, slots: &[Slot], item: Item, over: usize) -> bool {
        match item {
            Item::Session(sid) => {
                let Some(target) = session_target(slots, sid, over) else {
                    return false;
                };
                let mut list: Vec<SessionId> = slots.iter().filter_map(|s| s.session).collect();
                list.retain(|&s| s != sid);
                let Some(i) = list.iter().position(|&s| s == target.at) else {
                    return false;
                };
                list.insert(i + target.after as usize, sid);
                self.sessions = list;
            }
            Item::Host(host) => {
                let Some(target) = host_target(slots, host, over) else {
                    return false;
                };
                let mut list: Vec<HostId> = Vec::new();
                for h in slots.iter().filter_map(|s| s.group) {
                    if h != host && !list.contains(&h) {
                        list.push(h);
                    }
                }
                let Some(i) = list.iter().position(|&h| h == target.at) else {
                    return false;
                };
                list.insert(i + target.after as usize, host);
                self.hosts = list;
            }
        }
        true
    }
}

/// Over its own group's heading, a session goes first in the group.
fn session_target(slots: &[Slot], sid: SessionId, over: usize) -> Option<Target<SessionId>> {
    let from = slots.iter().position(|s| s.session == Some(sid))?;
    let group = slots[from].group;
    let slot = slots.get(over)?;
    if slot.group != group {
        return None;
    }
    let to = if slot.session.is_some() {
        over
    } else {
        over + 1
    };
    let at = slots.get(to).filter(|s| s.group == group)?.session?;
    if to == from {
        return None;
    }
    Some(Target {
        at,
        after: to > from,
    })
}

fn host_target(slots: &[Slot], host: HostId, over: usize) -> Option<Target<HostId>> {
    let at = slots.get(over)?.group?;
    if at == host {
        return None;
    }
    let from = slots.iter().position(|s| s.group == Some(host))?;
    let start = slots.iter().position(|s| s.group == Some(at))?;
    Some(Target {
        at,
        after: start > from,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(group: Option<HostId>, sid: SessionId) -> Slot {
        Slot {
            group,
            session: Some(sid),
        }
    }

    fn heading(host: HostId) -> Slot {
        Slot {
            group: Some(host),
            session: None,
        }
    }

    /// Local 0, 1, 2; host 7 with 3, 4; host 8 with 5.
    fn slots() -> Vec<Slot> {
        vec![
            session(None, 0),
            session(None, 1),
            session(None, 2),
            heading(7),
            session(Some(7), 3),
            session(Some(7), 4),
            heading(8),
            session(Some(8), 5),
        ]
    }

    fn sorted(order: &ListOrder) -> Vec<SessionId> {
        order.sort(vec![
            (Some(8), 5),
            (None, 2),
            (Some(7), 4),
            (None, 0),
            (Some(7), 3),
            (None, 1),
        ])
    }

    #[test]
    fn default_order_is_local_then_hosts_by_id() {
        assert_eq!(sorted(&ListOrder::default()), vec![0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn session_takes_the_place_of_the_row_dropped_on() {
        let mut order = ListOrder::default();
        assert!(order.drop(&slots(), Item::Session(0), 2));
        assert_eq!(sorted(&order), vec![1, 2, 0, 3, 4, 5]);
        let mut order = ListOrder::default();
        assert!(order.drop(&slots(), Item::Session(2), 0));
        assert_eq!(sorted(&order), vec![2, 0, 1, 3, 4, 5]);
    }

    #[test]
    fn session_stays_within_its_group() {
        let mut order = ListOrder::default();
        assert!(!order.drop(&slots(), Item::Session(0), 4));
        assert!(!order.drop(&slots(), Item::Session(3), 1));
        assert!(!order.drop(&slots(), Item::Session(3), 6));
        assert!(!order.drop(&slots(), Item::Session(0), 0));
        assert!(!order.drop(&slots(), Item::Session(0), 99));
        assert_eq!(sorted(&order), vec![0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn session_over_its_heading_goes_first() {
        let mut order = ListOrder::default();
        assert!(order.drop(&slots(), Item::Session(4), 3));
        assert_eq!(sorted(&order), vec![0, 1, 2, 4, 3, 5]);
    }

    #[test]
    fn host_group_moves_whole() {
        let mut order = ListOrder::default();
        assert!(order.drop(&slots(), Item::Host(7), 7));
        assert_eq!(sorted(&order), vec![0, 1, 2, 5, 3, 4]);
    }

    #[test]
    fn host_group_stays_below_local_sessions() {
        let mut order = ListOrder::default();
        assert!(!order.drop(&slots(), Item::Host(8), 0));
        assert!(!order.drop(&slots(), Item::Host(8), 7));
        assert_eq!(sorted(&order), vec![0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn new_sessions_and_hosts_land_last_in_their_group() {
        let mut order = ListOrder::default();
        order.drop(&slots(), Item::Session(2), 0);
        order.drop(&slots(), Item::Host(8), 4);
        let ids = vec![(None, 9), (Some(9), 10), (Some(7), 11), (None, 0)];
        let mut all = ids.clone();
        all.extend([
            (None, 1),
            (None, 2),
            (Some(7), 3),
            (Some(7), 4),
            (Some(8), 5),
        ]);
        assert_eq!(order.sort(all), vec![2, 0, 1, 9, 5, 3, 4, 11, 10]);
    }
}
