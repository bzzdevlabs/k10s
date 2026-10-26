//! OS update waves (docs/OS.md, "Updates"): a pure function that every Tier 1 node evaluates on its own view of the
//! membership, like placement, so no coordinator hands out turns.
//!
//! For a platform with target image `T`, the nodes of that platform (every known member that reports it, live or
//! still remembered as dead/left, so a node rebooting into `T` keeps its place) are ranked by `sha256(T | id)`. Wave
//! 0 is the first `wave` nodes, wave 1 the next `2·wave`, then `4·wave`, … A node may start when every node of the
//! earlier waves is *done*: live, running `T`, committed. A node that fell back from `T` reports it as failed,
//! which stops the rollout for everyone until the desired state names another image. While this node holds a copy
//! of a live member's record that does not say whether it reports an OS (relayed in the older form), it waits:
//! that member may rank first.

use crate::manifest::OsPlan;
use crate::member::{Member, NodeId, OsInfo};
use crate::sha256::Sha256;
use alloc::vec::Vec;

/// What this node should do about its OS image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Nothing to do: not an updatable Tier 1 node, no image for its platform, or it already runs the target.
    Idle,
    /// Install `image` now.
    Update { image: [u8; 32], wave: u32 },
    /// Earlier waves are not done yet (`pending` nodes).
    Wait { image: [u8; 32], wave: u32, pending: usize },
    /// `node` failed to commit the target: the rollout is stopped.
    Halted { image: [u8; 32], node: NodeId },
    /// The OS reports of `members` live members are not known here yet.
    Unknown { image: [u8; 32], members: usize },
}

fn rank(target: &[u8; 32], id: NodeId) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(target);
    h.update(&id.0.to_le_bytes());
    h.finalize()
}

/// The wave of the node at `rank` (0-based) with a first wave of `first` nodes, and the rank where it starts.
pub fn wave_of(rank: usize, first: u32) -> (u32, usize) {
    let first = first.max(1) as usize;
    let (mut wave, mut start, mut size) = (0u32, 0usize, first);
    while rank >= start.saturating_add(size) {
        start += size;
        size = size.saturating_mul(2);
        wave += 1;
    }
    (wave, start)
}

/// The step for node `me` given the plan and every member this node knows (any state, itself included).
pub fn step(plan: &OsPlan, members: &[Member], me: NodeId) -> Step {
    let Some(mine) = members.iter().find(|m| m.id == me).and_then(|m| m.os.as_ref()).filter(|o| o.updatable) else {
        return Step::Idle;
    };
    let Some(target) = plan.image_for(&mine.platform) else { return Step::Idle };
    let tid = OsInfo::id(target);
    if mine.image == tid {
        return Step::Idle;
    }
    let mut ranked: Vec<([u8; 32], &Member)> = members
        .iter()
        .filter(|m| m.os.as_ref().is_some_and(|o| o.updatable && o.platform == mine.platform))
        .map(|m| (rank(target, m.id), m))
        .collect();
    if let Some((_, m)) = ranked.iter().find(|(_, m)| m.os.as_ref().is_some_and(|o| o.failed == tid)) {
        return Step::Halted { image: *target, node: m.id };
    }
    let unknown = members.iter().filter(|m| m.ext && !m.ext_known && m.is_live()).count();
    if unknown > 0 {
        return Step::Unknown { image: *target, members: unknown };
    }
    ranked.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.id.cmp(&b.1.id)));
    let pos = ranked.iter().position(|(_, m)| m.id == me).expect("this node reports its OS");
    let (wave, start) = wave_of(pos, plan.wave);
    let done = |m: &Member| m.is_live() && m.os.as_ref().is_some_and(|o| o.image == tid && o.committed);
    let pending = ranked[..start].iter().filter(|(_, m)| !done(m)).count();
    if pending == 0 {
        Step::Update { image: *target, wave }
    } else {
        Step::Wait { image: *target, wave, pending }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::OsImage;
    use crate::member::{Capacity, MemberState};
    use alloc::format;
    use alloc::string::ToString;
    use alloc::vec;

    const OLD: [u8; 32] = [1; 32];
    const NEW: [u8; 32] = [2; 32];

    fn member(i: u64, image: [u8; 32]) -> Member {
        let a = core::net::SocketAddr::from(([10, 0, 0, i as u8], 7946));
        let os = OsInfo { platform: "x86_64-vm".into(), image: OsInfo::id(&image), committed: true, updatable: true, failed: [0; 8] };
        Member { os: Some(os), ..Member::new(NodeId(i), a, a, a, Capacity { mem_bytes: 1 << 30, cores: 1 }) }
    }

    fn plan(image: [u8; 32], wave: u32) -> OsPlan {
        OsPlan { images: vec![OsImage { platform: "x86_64-vm".to_string(), image }], wave }
    }

    #[test]
    fn waves_double() {
        let starts: Vec<(u32, usize)> = (0..20).map(|r| wave_of(r, 1)).collect();
        assert_eq!(starts[0], (0, 0));
        assert_eq!((starts[1], starts[2]), ((1, 1), (1, 1)));
        assert_eq!((starts[3], starts[6], starts[7], starts[14], starts[15], starts[19]), ((2, 3), (2, 3), (3, 7), (3, 7), (4, 15), (4, 15)));
        assert_eq!(wave_of(5, 3), (1, 3));
        assert!(wave_of(usize::MAX - 1, u32::MAX).0 < 64, "terminates without overflow");
    }

    /// Simulates a whole rollout: each round, every node whose step says `Update` installs the image.
    #[test]
    fn rollout_proceeds_wave_by_wave_and_stops_on_failure() {
        let mut ms: Vec<Member> = (1..=20).map(|i| member(i, OLD)).collect();
        let p = plan(NEW, 1);
        let mut sizes = Vec::new();
        while ms.iter().any(|m| m.os.as_ref().unwrap().image != OsInfo::id(&NEW)) {
            let go: Vec<NodeId> = ms.iter().filter(|m| matches!(step(&p, &ms, m.id), Step::Update { .. })).map(|m| m.id).collect();
            assert!(!go.is_empty(), "stalled");
            // While a wave reboots and has not committed, nobody else starts.
            for m in ms.iter_mut().filter(|m| go.contains(&m.id)) {
                m.state = MemberState::Dead;
            }
            assert!(ms.iter().all(|m| !matches!(step(&p, &ms, m.id), Step::Update { .. }) || go.contains(&m.id)));
            for m in ms.iter_mut().filter(|m| go.contains(&m.id)) {
                m.state = MemberState::Alive;
                let os = m.os.as_mut().unwrap();
                (os.image, os.committed) = (OsInfo::id(&NEW), false);
            }
            assert!(ms.iter().all(|m| !matches!(step(&p, &ms, m.id), Step::Update { .. })), "trial boots block");
            for m in ms.iter_mut().filter(|m| go.contains(&m.id)) {
                m.os.as_mut().unwrap().committed = true;
            }
            sizes.push(go.len());
        }
        assert_eq!(sizes, vec![1, 2, 4, 8, 5]);
        assert!(ms.iter().all(|m| step(&p, &ms, m.id) == Step::Idle));

        // A bad image: the first node falls back and reports it; nobody else starts.
        let bad = [3; 32];
        let p = plan(bad, 1);
        let first: Vec<NodeId> = ms.iter().filter(|m| matches!(step(&p, &ms, m.id), Step::Update { .. })).map(|m| m.id).collect();
        assert_eq!(first.len(), 1);
        let canary = ms.iter_mut().find(|m| m.id == first[0]).unwrap();
        canary.os.as_mut().unwrap().failed = OsInfo::id(&bad);
        for m in &ms {
            assert_eq!(step(&p, &ms, m.id), Step::Halted { image: bad, node: first[0] }, "{}", m.id);
        }
        // Another image restarts the rollout.
        assert!(ms.iter().any(|m| matches!(step(&plan([4; 32], 1), &ms, m.id), Step::Update { .. })));
    }

    #[test]
    fn idle_cases_and_platforms() {
        let mut ms: Vec<Member> = (1..=4).map(|i| member(i, OLD)).collect();
        ms[3].os.as_mut().unwrap().platform = "x86_64-metal".into();
        ms[2].os = None;
        ms[1].os.as_mut().unwrap().updatable = false;
        let p = plan(NEW, 1);
        assert_eq!(step(&p, &ms, NodeId(2)), Step::Idle, "not updatable");
        assert_eq!(step(&p, &ms, NodeId(3)), Step::Idle, "no OS report");
        assert_eq!(step(&p, &ms, NodeId(4)), Step::Idle, "no image for its platform");
        assert_eq!(step(&p, &ms, NodeId(1)), Step::Update { image: NEW, wave: 0 }, "alone on its platform");
        assert_eq!(step(&plan(OLD, 1), &ms, NodeId(1)), Step::Idle, "already there");
        assert_eq!(step(&OsPlan::default(), &ms, NodeId(1)), Step::Idle);
        assert_eq!(step(&p, &ms, NodeId(99)), Step::Idle);
        // A copy of a record without its extensions: that member may rank first, so wait for its report.
        ms[2].ext_known = false;
        assert_eq!(step(&p, &ms, NodeId(1)), Step::Unknown { image: NEW, members: 1 });
        ms[2].state = MemberState::Dead;
        assert_eq!(step(&p, &ms, NodeId(1)), Step::Update { image: NEW, wave: 0 });
        let _ = format!("{:?}", step(&p, &ms, NodeId(1)));
    }
}
