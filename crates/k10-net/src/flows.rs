//! The flow table: 4-tuple → connection slot, open addressing with linear probing and backward-shift deletion,
//! indexed by a keyed hash (remote peers cannot aim collisions). Grows by doubling up to a fixed capacity.

use crate::siphash::{hash13, Key};
use crate::tcp::FlowKey;
use alloc::vec;
use alloc::vec::Vec;

pub(crate) struct FlowMap {
    slots: Vec<Option<(FlowKey, u32)>>,
    len: usize,
    max_len: usize,
    key: Key,
}

impl FlowMap {
    /// Holds at most `max_len` entries (the table stays at most half full).
    pub fn new(key: Key, max_len: usize) -> FlowMap {
        FlowMap { slots: vec![None; 64], len: 0, max_len, key }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    fn index(&self, k: &FlowKey) -> usize {
        hash13(&self.key, &k.bytes()) as usize & (self.slots.len() - 1)
    }

    pub fn get(&self, k: &FlowKey) -> Option<u32> {
        let mask = self.slots.len() - 1;
        let mut i = self.index(k);
        loop {
            match &self.slots[i] {
                None => return None,
                Some((key, v)) if key == k => return Some(*v),
                _ => i = (i + 1) & mask,
            }
        }
    }

    fn grow(&mut self) {
        let old = core::mem::replace(&mut self.slots, vec![None; 0]);
        self.slots = vec![None; old.len() * 2];
        self.len = 0;
        for (k, v) in old.into_iter().flatten() {
            self.insert_unchecked(k, v);
        }
    }

    fn insert_unchecked(&mut self, k: FlowKey, v: u32) {
        let mask = self.slots.len() - 1;
        let mut i = self.index(&k);
        while self.slots[i].is_some() {
            i = (i + 1) & mask;
        }
        self.slots[i] = Some((k, v));
        self.len += 1;
    }

    /// Inserts a new key; false when full or already present.
    pub fn insert(&mut self, k: FlowKey, v: u32) -> bool {
        if self.len >= self.max_len || self.get(&k).is_some() {
            return false;
        }
        if (self.len + 1) * 2 > self.slots.len() {
            self.grow();
        }
        self.insert_unchecked(k, v);
        true
    }

    pub fn remove(&mut self, k: &FlowKey) -> Option<u32> {
        let mask = self.slots.len() - 1;
        let mut i = self.index(k);
        loop {
            match &self.slots[i] {
                None => return None,
                Some((key, _)) if key == k => break,
                _ => i = (i + 1) & mask,
            }
        }
        let v = self.slots[i].take().map(|(_, v)| v);
        self.len -= 1;
        // Backward shift: move later entries of the probe run into the hole when their home allows it.
        let mut hole = i;
        let mut j = (i + 1) & mask;
        while let Some((key, _)) = &self.slots[j] {
            let home = self.index(key);
            let dist_j = j.wrapping_sub(home) & mask;
            let dist_hole = hole.wrapping_sub(home) & mask;
            if dist_hole < dist_j || home == hole {
                self.slots[hole] = self.slots[j].take();
                hole = j;
            }
            j = (j + 1) & mask;
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::net::{IpAddr, Ipv4Addr};

    fn k(p: u16) -> FlowKey {
        FlowKey {
            local: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
            remote: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)),
            lport: 80,
            rport: p,
        }
    }

    #[test]
    fn insert_get_remove_many() {
        let mut m = FlowMap::new(Key(1, 2), 5000);
        for p in 0..4000u16 {
            assert!(m.insert(k(p), p as u32));
        }
        assert!(!m.insert(k(5), 9));
        for p in (0..4000u16).step_by(3) {
            assert_eq!(m.remove(&k(p)), Some(p as u32));
        }
        for p in 0..4000u16 {
            let want = if p % 3 == 0 { None } else { Some(p as u32) };
            assert_eq!(m.get(&k(p)), want, "{p}");
        }
        assert_eq!(m.len(), 4000 - 1334);
        let mut small = FlowMap::new(Key(1, 2), 2);
        assert!(small.insert(k(1), 1) && small.insert(k(2), 2) && !small.insert(k(3), 3));
    }
}
