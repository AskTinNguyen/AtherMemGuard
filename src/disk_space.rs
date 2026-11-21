//! Space accounting without double counting (proposal section 3). Pure: no Win32 here.
//!
//! The scanner (`disk_win`) hands this module, per volume, each item's extents with the
//! volume-wide reference count ReFS reports for them (`FSCTL_GET_RETRIEVAL_POINTERS_AND_REFCOUNT`).
//! Extents with count 1 are exclusive by definition, so the scanner only counts them; the
//! rest are swept here. For every run of clusters the sweep knows `m[i]` (references from item
//! *i*), `M` (from every scanned item) and `R` (the volume's count):
//!
//! - in item *i*'s footprint if `m[i] > 0`;
//! - freed by deleting item *i* alone if `m[i] == R`;
//! - freed by deleting the set *S* if the sum of `m[i]` over *S* equals `R`;
//! - `R - M` references are outside the scan: that space stays whatever is picked.
//!
//! There is deliberately no `Add` or `Sum` for "frees" numbers: a total for several items only
//! comes out of [`Sweep::frees_together`], which works on the cluster table.

use std::collections::{BTreeMap, HashMap};

use serde::Serialize;

/// One run of clusters a file uses. `rc` is the volume's reference count for it; 0 means the
/// count could not be read (no refcount FSCTL), which makes every "frees" an upper bound.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ext {
    pub lcn: u64,
    pub len: u32,
    pub rc: u32,
}
const _: () = assert!(std::mem::size_of::<Ext>() == 16);

/// What the scanner collected for one item on one volume.
#[derive(Debug, Clone, Default)]
pub struct ItemExtents {
    /// Clusters in extents with reference count 1 that belong to files counted for this item.
    pub exclusive_clusters: u64,
    /// Every other extent (count above 1, or unknown).
    pub shared: Vec<Ext>,
}

/// One item's space after the sweep, in clusters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ItemSpace {
    pub footprint: u64,
    pub frees_alone: u64,
    /// Clusters this item uses that something outside the scan also references.
    pub shared_outside: u64,
    /// Clusters this item shares with each other scanned item (these overlap each other).
    pub shared_with: BTreeMap<usize, u64>,
    /// Clusters touching this item where the scan saw more references than the volume counts
    /// (or more scanned hard links than `links`).
    pub anomalies: u64,
    /// Clusters touching this item where two files reported different reference counts.
    pub inconsistent: u64,
    /// Clusters touching this item whose reference count could not be read (`rc == 0`).
    pub unknown_rc: u64,
}

impl ItemSpace {
    /// `footprint - frees_alone`: what stays allocated if only this item goes.
    pub fn stays_shared(&self) -> u64 {
        self.footprint - self.frees_alone
    }
}

/// A file with multiple hard links (`links >= 2`): all hard links share the same
/// file-extent references (`exts` with raw volume `rc`). The file releases its extents
/// only when all `links` names are deleted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkedFile {
    pub links: u32,
    /// `(item_index, link_count_in_item)`.
    pub items: Vec<(usize, u32)>,
    /// Raw extents from `FSCTL_GET_RETRIEVAL_POINTERS_AND_REFCOUNT` (`rc == 0` = unknown).
    pub exts: Vec<Ext>,
}

/// A set of items that reference the same clusters, and how much of that frees when exactly
/// these items go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharingGroup {
    pub items: Vec<usize>,
    pub shared: u64,
    pub frees_if_all_removed: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct LinkSig {
    links: u32,
    /// Sorted `(item_index, link_count_in_item)`, merged per item.
    items: Vec<(u16, u32)>,
    link_anomaly: bool,
}

/// Runs of clusters with the same references, aggregated: which items reference them how
/// often, which multi-link signatures cover them, and the volume's count.
#[derive(Debug, Clone, Copy)]
struct Segment {
    refs_start: u32,
    refs_end: u32,
    sig_start: u32,
    sig_end: u32,
    rc: u32,
    len: u64,
}

impl Segment {
    #[inline]
    fn refs<'a>(&self, refs_arena: &'a [(u16, u32)]) -> &'a [(u16, u32)] {
        &refs_arena[self.refs_start as usize..self.refs_end as usize]
    }

    #[inline]
    fn sig_refs<'a>(&self, sigs_arena: &'a [(usize, u32)]) -> &'a [(usize, u32)] {
        &sigs_arena[self.sig_start as usize..self.sig_end as usize]
    }

    fn released(
        &self,
        refs_arena: &[(u16, u32)],
        sigs_arena: &[(usize, u32)],
        sigs: &[LinkSig],
        inside: impl Fn(u16) -> bool,
    ) -> u32 {
        let single: u32 = self.refs(refs_arena).iter().filter(|(i, _)| inside(*i)).map(|r| r.1).sum();
        let linked: u32 = self
            .sig_refs(sigs_arena)
            .iter()
            .filter_map(|&(sid, k)| {
                let sig = &sigs[sid];
                let in_set: u32 = sig.items.iter().filter(|(i, _)| inside(*i)).map(|(_, c)| *c).sum();
                (in_set >= sig.links).then_some(k)
            })
            .sum();
        single + linked
    }

    fn participants_into(
        &self,
        refs_arena: &[(u16, u32)],
        sigs_arena: &[(usize, u32)],
        sigs: &[LinkSig],
        out: &mut Vec<u16>,
    ) {
        out.clear();
        let refs = self.refs(refs_arena);
        let sig_refs = self.sig_refs(sigs_arena);
        out.extend(refs.iter().map(|r| r.0));
        if !sig_refs.is_empty() {
            for &(sid, _) in sig_refs {
                out.extend(sigs[sid].items.iter().map(|(i, _)| *i));
            }
            out.sort_unstable();
            out.dedup();
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Sweep {
    pub items: Vec<ItemSpace>,
    exclusive: Vec<u64>,
    sigs: Vec<LinkSig>,
    refs_arena: Vec<(u16, u32)>,
    sigs_arena: Vec<(usize, u32)>,
    segments: Vec<Segment>,
    /// Distinct clusters used by all items together.
    pub union: u64,
    /// Clusters where the scan saw more references than the volume counts (the tree changed
    /// under the scan, or the model is wrong). Anything above 0 downgrades to `estimate`.
    pub anomalies: u64,
    /// Clusters where two files reported different counts for the same cluster.
    pub inconsistent: u64,
    /// Clusters whose count could not be read: their "frees" are upper bounds.
    pub unknown_rc: u64,
}

const START_BIT: u32 = 1 << 31;
const LINKED_BIT: u32 = 1 << 30;
const ID_MASK: u32 = (1 << 30) - 1;

#[repr(C)]
#[derive(Clone, Copy)]
struct Event {
    pos: u64,
    tag_id: u32, // bit 31: 1=start(+1), 0=end(-1); bit 30: is_linked; bits 0..30: id
    rc: u32,
}
const _: () = assert!(std::mem::size_of::<Event>() == 16);

#[inline]
fn cmp_event(a: &Event, b: &Event) -> std::cmp::Ordering {
    a.pos.cmp(&b.pos).then(a.tag_id.cmp(&b.tag_id))
}

#[inline]
fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

fn hash_link_sig(links: u32, items: &[(u16, u32)], link_anomaly: bool) -> u64 {
    let mut h = splitmix64((links as u64) ^ ((link_anomaly as u64) << 32) ^ ((items.len() as u64) << 40));
    for &(item, cnt) in items {
        h = splitmix64(h ^ ((item as u64) << 32) ^ (cnt as u64));
    }
    h
}

fn hash_seg_key(refs: &[(u16, u32)], sig_refs: &[(usize, u32)], rc: u32) -> u64 {
    let mut h = splitmix64((rc as u64) ^ ((refs.len() as u64) << 32) ^ ((sig_refs.len() as u64) << 48));
    for &(item, m) in refs {
        h = splitmix64(h ^ ((item as u64) << 32) ^ (m as u64));
    }
    for &(sid, c) in sig_refs {
        h = splitmix64(h ^ (sid as u64).rotate_left(17) ^ (c as u64));
    }
    h
}

fn par_merge(a: &[Event], b: &[Event], dst: &mut [Event], threads: usize) {
    debug_assert_eq!(a.len() + b.len(), dst.len());
    if a.len() + b.len() <= 32_768 || threads <= 1 || a.is_empty() || b.is_empty() {
        let (mut ia, mut ib, mut id) = (0, 0, 0);
        while ia < a.len() && ib < b.len() {
            if cmp_event(&a[ia], &b[ib]).is_le() {
                dst[id] = a[ia];
                ia += 1;
            } else {
                dst[id] = b[ib];
                ib += 1;
            }
            id += 1;
        }
        if ia < a.len() {
            dst[id..].copy_from_slice(&a[ia..]);
        } else if ib < b.len() {
            dst[id..].copy_from_slice(&b[ib..]);
        }
        return;
    }
    let (a, b) = if a.len() < b.len() { (b, a) } else { (a, b) };
    let ma = a.len() / 2;
    let pivot = a[ma];
    let mb = b.partition_point(|x| cmp_event(x, &pivot).is_lt());
    let (a_lo, a_hi) = a.split_at(ma);
    let (b_lo, b_hi) = b.split_at(mb);
    let (dst_lo, dst_hi) = dst.split_at_mut(ma + mb);
    let t_lo = threads / 2;
    let t_hi = threads - t_lo;
    std::thread::scope(|s| {
        s.spawn(|| par_merge(a_lo, b_lo, dst_lo, t_lo));
        par_merge(a_hi, b_hi, dst_hi, t_hi);
    });
}

fn sort_events(ev: &mut Vec<Event>) {
    const PAR_SORT_THRESHOLD: usize = 65_536;
    if ev.len() <= PAR_SORT_THRESHOLD {
        ev.sort_unstable_by(cmp_event);
        return;
    }
    let avail = std::thread::available_parallelism().map_or(4, |n| n.get());
    let chunks = if avail >= 8 && ev.len() >= 524_288 {
        8
    } else if avail >= 4 {
        4
    } else if avail >= 2 {
        2
    } else {
        1
    };
    if chunks <= 1 {
        ev.sort_unstable_by(cmp_event);
        return;
    }
    let len = ev.len();
    let chunk_size = len.div_ceil(chunks);
    let mut bounds: Vec<(usize, usize)> = Vec::with_capacity(chunks);
    let mut start = 0usize;
    while start < len {
        let end = (start + chunk_size).min(len);
        bounds.push((start, end));
        start = end;
    }
    std::thread::scope(|s| {
        for chunk in ev.chunks_mut(chunk_size) {
            s.spawn(move || chunk.sort_unstable_by(cmp_event));
        }
    });
    if bounds.len() <= 1 {
        return;
    }
    let mut tmp = vec![Event { pos: 0, tag_id: 0, rc: 0 }; len];
    let mut src_is_ev = true;
    while bounds.len() > 1 {
        let mut next_bounds: Vec<(usize, usize)> = Vec::with_capacity(bounds.len().div_ceil(2));
        let (src, dst): (&[Event], &mut [Event]) = if src_is_ev { (ev.as_slice(), tmp.as_mut_slice()) } else { (tmp.as_slice(), ev.as_mut_slice()) };
        let pairs = bounds.len() / 2;
        let threads_per_pair = (chunks / pairs.max(1)).max(1);
        std::thread::scope(|s| {
            let mut rem_dst = dst;
            let mut idx = 0usize;
            while idx + 1 < bounds.len() {
                let (s0, e0) = bounds[idx];
                let (s1, e1) = bounds[idx + 1];
                let a = &src[s0..e0];
                let b = &src[s1..e1];
                let (pair_dst, rest) = rem_dst.split_at_mut(a.len() + b.len());
                rem_dst = rest;
                next_bounds.push((s0, e1));
                if idx + 2 >= bounds.len() {
                    par_merge(a, b, pair_dst, threads_per_pair);
                } else {
                    s.spawn(move || par_merge(a, b, pair_dst, threads_per_pair));
                }
                idx += 2;
            }
            if idx < bounds.len() {
                let (s0, e0) = bounds[idx];
                rem_dst.copy_from_slice(&src[s0..e0]);
                next_bounds.push((s0, e0));
            }
        });
        bounds = next_bounds;
        src_is_ev = !src_is_ev;
    }
    if !src_is_ev {
        *ev = tmp;
    }
}

#[derive(Default)]
struct RcTracker {
    zero_cnt: u32,
    inline: [(u32, u32); 4],
    inline_len: u8,
    overflow: Vec<(u32, u32)>,
    max_nz: u32,
}

impl RcTracker {
    #[inline]
    fn inc(&mut self, rc: u32) {
        if rc == 0 {
            self.zero_cnt += 1;
            return;
        }
        if rc > self.max_nz {
            self.max_nz = rc;
        }
        let len = self.inline_len as usize;
        for i in 0..len {
            if self.inline[i].0 == rc {
                self.inline[i].1 += 1;
                return;
            }
        }
        for (r, c) in &mut self.overflow {
            if *r == rc {
                *c += 1;
                return;
            }
        }
        if len < 4 {
            self.inline[len] = (rc, 1);
            self.inline_len += 1;
        } else {
            self.overflow.push((rc, 1));
        }
    }

    #[inline]
    fn dec(&mut self, rc: u32) {
        if rc == 0 {
            self.zero_cnt -= 1;
            return;
        }
        let len = self.inline_len as usize;
        for i in 0..len {
            if self.inline[i].0 == rc {
                self.inline[i].1 -= 1;
                if self.inline[i].1 == 0 {
                    if let Some(moved) = self.overflow.pop() {
                        self.inline[i] = moved;
                    } else {
                        self.inline_len -= 1;
                        let last = self.inline_len as usize;
                        if i != last {
                            self.inline[i] = self.inline[last];
                        }
                    }
                    if rc == self.max_nz {
                        self.recompute_max();
                    }
                }
                return;
            }
        }
        if let Some(pos) = self.overflow.iter().position(|(r, _)| *r == rc) {
            self.overflow[pos].1 -= 1;
            if self.overflow[pos].1 == 0 {
                self.overflow.swap_remove(pos);
                if rc == self.max_nz {
                    self.recompute_max();
                }
            }
        }
    }

    #[cold]
    fn recompute_max(&mut self) {
        let mut m = 0u32;
        for &(r, _) in &self.inline[..self.inline_len as usize] {
            m = m.max(r);
        }
        for &(r, _) in &self.overflow {
            m = m.max(r);
        }
        self.max_nz = m;
    }

    #[inline]
    fn unknown(&self) -> bool {
        self.zero_cnt > 0
    }

    #[inline]
    fn is_inconsistent(&self) -> bool {
        (self.inline_len as usize + self.overflow.len()) > 1
    }

    #[inline]
    fn known_max(&self) -> Option<u32> {
        (self.max_nz > 0).then_some(self.max_nz)
    }
}

/// Sweep the extents of `items` (all on one volume).
pub fn sweep(items: &[ItemExtents]) -> Sweep {
    sweep_with_links(items, &[])
}

/// Sweep the extents of `items` together with multi-link files (`linked`) on one volume.
pub fn sweep_with_links(items: &[ItemExtents], linked: &[LinkedFile]) -> Sweep {
    let n = items.len();
    assert!(n <= u16::MAX as usize, "too many items for one sweep");

    let mut sigs: Vec<LinkSig> = Vec::new();
    let mut linked_sigs: Vec<Option<usize>> = Vec::new();
    let mut linked_ev_cap = 0usize;
    if !linked.is_empty() {
        linked_sigs.reserve(linked.len());
        let mut lf_counts = vec![0u32; n];
        let mut lf_touched: Vec<u16> = Vec::with_capacity(8);
        let mut lf_items_scratch: Vec<(u16, u32)> = Vec::with_capacity(8);
        let mut sig_hashes: Vec<u64> = Vec::new();
        let mut sig_buckets: Vec<u32> = vec![0u32; (linked.len().next_power_of_two() * 2).max(16)];
        for lf in linked {
            if lf.items.is_empty() {
                linked_sigs.push(None);
                continue;
            }
            lf_touched.clear();
            for &(item_idx, cnt) in &lf.items {
                assert!(item_idx < n, "linked file item index out of range");
                if cnt > 0 {
                    if lf_counts[item_idx] == 0 {
                        lf_touched.push(item_idx as u16);
                    }
                    lf_counts[item_idx] += cnt;
                }
            }
            if lf_touched.is_empty() {
                linked_sigs.push(None);
                continue;
            }
            if lf_touched.len() > 1 {
                lf_touched.sort_unstable();
            }
            lf_items_scratch.clear();
            let mut seen_links = 0u32;
            for &idx in &lf_touched {
                let c = lf_counts[idx as usize];
                lf_counts[idx as usize] = 0;
                seen_links += c;
                lf_items_scratch.push((idx, c));
            }
            let links = lf.links.max(1);
            let link_anomaly = seen_links > links;
            let h = hash_link_sig(links, &lf_items_scratch, link_anomaly);
            let mask = sig_buckets.len() - 1;
            let mut slot = (h as usize) & mask;
            let mut found = None;
            loop {
                let entry = sig_buckets[slot];
                if entry == 0 {
                    break;
                }
                let sid = (entry - 1) as usize;
                if sig_hashes[sid] == h
                    && sigs[sid].links == links
                    && sigs[sid].link_anomaly == link_anomaly
                    && sigs[sid].items.as_slice() == lf_items_scratch.as_slice()
                {
                    found = Some(sid);
                    break;
                }
                slot = (slot + 1) & mask;
            }
            let sid = match found {
                Some(id) => id,
                None => {
                    let id = sigs.len();
                    assert!(id < (1 << 30), "too many distinct link signatures for one sweep");
                    sigs.push(LinkSig { links, items: lf_items_scratch.clone(), link_anomaly });
                    sig_hashes.push(h);
                    sig_buckets[slot] = (id + 1) as u32;
                    if sigs.len() * 10 >= sig_buckets.len() * 7 {
                        let new_cap = sig_buckets.len() * 2;
                        let mut new_buckets = vec![0u32; new_cap];
                        let nmask = new_cap - 1;
                        for (i, &sh) in sig_hashes.iter().enumerate() {
                            let mut s = (sh as usize) & nmask;
                            while new_buckets[s] != 0 {
                                s = (s + 1) & nmask;
                            }
                            new_buckets[s] = (i + 1) as u32;
                        }
                        sig_buckets = new_buckets;
                    }
                    id
                }
            };
            linked_ev_cap += lf.exts.len() * 2;
            linked_sigs.push(Some(sid));
        }
    }

    let ev_cap: usize = items.iter().map(|i| i.shared.len() * 2).sum::<usize>() + linked_ev_cap;
    let mut out = Sweep {
        items: vec![ItemSpace::default(); n],
        exclusive: items.iter().map(|i| i.exclusive_clusters).collect(),
        sigs,
        ..Default::default()
    };
    if ev_cap == 0 {
        for (i, &x) in out.exclusive.iter().enumerate() {
            out.items[i].footprint = x;
            out.items[i].frees_alone = x;
            out.union += x;
        }
        return out;
    }

    let mut ev: Vec<Event> = Vec::with_capacity(ev_cap);
    for (i, it) in items.iter().enumerate() {
        let start_tag = START_BIT | (i as u32);
        let end_tag = i as u32;
        for e in it.shared.iter().filter(|e| e.len > 0) {
            let end = e.lcn.saturating_add(e.len as u64);
            if end > e.lcn {
                ev.push(Event { pos: e.lcn, tag_id: start_tag, rc: e.rc });
                ev.push(Event { pos: end, tag_id: end_tag, rc: e.rc });
            }
        }
    }
    if !linked.is_empty() {
        for (lf, sid) in linked.iter().zip(linked_sigs) {
            let Some(sid) = sid else { continue };
            let start_tag = START_BIT | LINKED_BIT | (sid as u32);
            let end_tag = LINKED_BIT | (sid as u32);
            for e in lf.exts.iter().filter(|e| e.len > 0) {
                let end = e.lcn.saturating_add(e.len as u64);
                if end > e.lcn {
                    ev.push(Event { pos: e.lcn, tag_id: start_tag, rc: e.rc });
                    ev.push(Event { pos: end, tag_id: end_tag, rc: e.rc });
                }
            }
        }
    }
    if ev.is_empty() {
        for (i, &x) in out.exclusive.iter().enumerate() {
            out.items[i].footprint = x;
            out.items[i].frees_alone = x;
            out.union += x;
        }
        return out;
    }
    sort_events(&mut ev);

    let mut mult: Vec<u32> = vec![0; n];
    let mut item_pos: Vec<u32> = vec![u32::MAX; n];
    let mut active_items: Vec<u16> = Vec::with_capacity(n.min(256));

    let num_sigs = out.sigs.len();
    let mut sig_cnt: Vec<u32> = if num_sigs > 0 { vec![0; num_sigs] } else { Vec::new() };
    let mut sig_pos: Vec<u32> = if num_sigs > 0 { vec![u32::MAX; num_sigs] } else { Vec::new() };
    let mut active_sigs: Vec<u32> = Vec::new();
    let mut active_link_anomalies = 0u32;

    let mut rcs = RcTracker::default();
    let mut single_total_m = 0u32;
    let mut linked_total_k = 0u32;

    let mut refs_arena: Vec<(u16, u32)> = Vec::new();
    let mut sigs_arena: Vec<(usize, u32)> = Vec::new();
    let mut segments: Vec<Segment> = Vec::new();
    let mut seg_hashes: Vec<u64> = Vec::new();
    let mut seg_buckets: Vec<u32> = vec![0u32; 256];

    let mut scratch_parts: Vec<u16> = Vec::new();
    let mut scratch_refs: Vec<(u16, u32)> = Vec::with_capacity(32);
    let mut scratch_sigs: Vec<(usize, u32)> = Vec::new();

    let mut prev = 0u64;
    let mut k = 0usize;
    while k < ev.len() {
        let pos = ev[k].pos;
        let seen_file_refs = single_total_m + linked_total_k;
        if seen_file_refs > 0 && pos > prev {
            let len = pos - prev;
            let known = rcs.known_max();
            let unknown = rcs.unknown();
            let is_inconsistent = rcs.is_inconsistent();
            // Unknown counts: assume nothing outside the scan refers to it (an upper bound).
            let rc = match (known, unknown) {
                (Some(r), false) => r,
                (Some(r), true) => r.max(seen_file_refs),
                (None, _) => seen_file_refs,
            };
            let is_anomaly = (seen_file_refs > rc) || (active_link_anomalies > 0);
            if is_inconsistent || unknown || is_anomaly {
                let parts: &[u16] = if active_sigs.is_empty() {
                    &active_items
                } else {
                    scratch_parts.clear();
                    scratch_parts.extend_from_slice(&active_items);
                    for &sid in &active_sigs {
                        scratch_parts.extend(out.sigs[sid as usize].items.iter().map(|(i, _)| *i));
                    }
                    scratch_parts.sort_unstable();
                    scratch_parts.dedup();
                    &scratch_parts
                };
                if is_inconsistent {
                    out.inconsistent += len;
                    for &i in parts {
                        out.items[i as usize].inconsistent += len;
                    }
                }
                if unknown {
                    out.unknown_rc += len;
                    for &i in parts {
                        out.items[i as usize].unknown_rc += len;
                    }
                }
                if is_anomaly {
                    out.anomalies += len;
                    for &i in parts {
                        out.items[i as usize].anomalies += len;
                    }
                }
            }
            scratch_refs.clear();
            for &i in &active_items {
                scratch_refs.push((i, mult[i as usize]));
            }
            if scratch_refs.len() > 1 {
                scratch_refs.sort_unstable_by_key(|r| r.0);
            }
            scratch_sigs.clear();
            if !active_sigs.is_empty() {
                for &sid in &active_sigs {
                    scratch_sigs.push((sid as usize, sig_cnt[sid as usize]));
                }
                if scratch_sigs.len() > 1 {
                    scratch_sigs.sort_unstable_by_key(|r| r.0);
                }
            }
            let h = hash_seg_key(&scratch_refs, &scratch_sigs, rc);
            let mask = seg_buckets.len() - 1;
            let mut slot = (h as usize) & mask;
            let mut hit = None;
            loop {
                let entry = seg_buckets[slot];
                if entry == 0 {
                    break;
                }
                let idx = (entry - 1) as usize;
                if seg_hashes[idx] == h {
                    let s = &segments[idx];
                    if s.rc == rc
                        && &refs_arena[s.refs_start as usize..s.refs_end as usize] == scratch_refs.as_slice()
                        && &sigs_arena[s.sig_start as usize..s.sig_end as usize] == scratch_sigs.as_slice()
                    {
                        hit = Some(idx);
                        break;
                    }
                }
                slot = (slot + 1) & mask;
            }
            if let Some(idx) = hit {
                segments[idx].len += len;
            } else {
                let refs_start = refs_arena.len() as u32;
                refs_arena.extend_from_slice(&scratch_refs);
                let refs_end = refs_arena.len() as u32;
                let sig_start = sigs_arena.len() as u32;
                sigs_arena.extend_from_slice(&scratch_sigs);
                let sig_end = sigs_arena.len() as u32;
                let idx = segments.len();
                segments.push(Segment { refs_start, refs_end, sig_start, sig_end, rc, len });
                seg_hashes.push(h);
                seg_buckets[slot] = (idx + 1) as u32;
                if segments.len() * 10 >= seg_buckets.len() * 7 {
                    let new_cap = seg_buckets.len() * 2;
                    let mut new_buckets = vec![0u32; new_cap];
                    let nmask = new_cap - 1;
                    for (i, &sh) in seg_hashes.iter().enumerate() {
                        let mut s = (sh as usize) & nmask;
                        while new_buckets[s] != 0 {
                            s = (s + 1) & nmask;
                        }
                        new_buckets[s] = (i + 1) as u32;
                    }
                    seg_buckets = new_buckets;
                }
            }
        }
        while k < ev.len() && ev[k].pos == pos {
            let tag = ev[k].tag_id;
            let rc = ev[k].rc;
            let is_start = (tag & START_BIT) != 0;
            let is_linked = (tag & LINKED_BIT) != 0;
            let id = (tag & ID_MASK) as usize;
            if is_start {
                if is_linked {
                    if sig_cnt[id] == 0 {
                        sig_pos[id] = active_sigs.len() as u32;
                        active_sigs.push(id as u32);
                        if out.sigs[id].link_anomaly {
                            active_link_anomalies += 1;
                        }
                    }
                    sig_cnt[id] += 1;
                    linked_total_k += 1;
                } else {
                    if mult[id] == 0 {
                        item_pos[id] = active_items.len() as u32;
                        active_items.push(id as u16);
                    }
                    mult[id] += 1;
                    single_total_m += 1;
                }
                rcs.inc(rc);
            } else {
                if is_linked {
                    sig_cnt[id] -= 1;
                    linked_total_k -= 1;
                    if sig_cnt[id] == 0 {
                        let p = sig_pos[id] as usize;
                        let last = active_sigs.pop().unwrap();
                        if p < active_sigs.len() {
                            active_sigs[p] = last;
                            sig_pos[last as usize] = p as u32;
                        }
                        sig_pos[id] = u32::MAX;
                        if out.sigs[id].link_anomaly {
                            active_link_anomalies -= 1;
                        }
                    }
                } else {
                    mult[id] -= 1;
                    single_total_m -= 1;
                    if mult[id] == 0 {
                        let p = item_pos[id] as usize;
                        let last = active_items.pop().unwrap();
                        if p < active_items.len() {
                            active_items[p] = last;
                            item_pos[last as usize] = p as u32;
                        }
                        item_pos[id] = u32::MAX;
                    }
                }
                rcs.dec(rc);
            }
            k += 1;
        }
        prev = pos;
    }

    for seg in &segments {
        seg.participants_into(&refs_arena, &sigs_arena, &out.sigs, &mut scratch_parts);
        let outside = seg.released(&refs_arena, &sigs_arena, &out.sigs, |_| true) < seg.rc;
        out.union += seg.len;
        for &i in &scratch_parts {
            let frees_alone = seg.released(&refs_arena, &sigs_arena, &out.sigs, |x| x == i) >= seg.rc;
            let s = &mut out.items[i as usize];
            s.footprint += seg.len;
            if frees_alone {
                s.frees_alone += seg.len;
            }
            if outside {
                s.shared_outside += seg.len;
            }
            for &j in &scratch_parts {
                if j != i {
                    *s.shared_with.entry(j as usize).or_default() += seg.len;
                }
            }
        }
    }
    out.refs_arena = refs_arena;
    out.sigs_arena = sigs_arena;
    out.segments = segments;
    for (i, x) in out.exclusive.iter().enumerate() {
        out.items[i].footprint += x;
        out.items[i].frees_alone += x;
        out.union += x;
    }
    out
}

fn with_in_set<R>(exclusive: &[u64], set: &[usize], f: impl FnOnce(&[bool], u64) -> R) -> R {
    let n = exclusive.len();
    let mut excl = 0u64;
    if n <= 256 {
        let mut stack = [false; 256];
        for &idx in set {
            if idx < n && !stack[idx] {
                stack[idx] = true;
                excl += exclusive[idx];
            }
        }
        f(&stack[..n], excl)
    } else {
        let mut heap = vec![false; n];
        for &idx in set {
            if idx < n && !heap[idx] {
                heap[idx] = true;
                excl += exclusive[idx];
            }
        }
        f(&heap, excl)
    }
}

impl Sweep {
    /// Clusters freed by deleting exactly the items in `set` together: every reference to them
    /// is inside the set. The only way to get a total for several items.
    pub fn frees_together(&self, set: &[usize]) -> u64 {
        with_in_set(&self.exclusive, set, |mask, excl| {
            if self.segments.is_empty() {
                return excl;
            }
            let inside = |i: u16| mask[i as usize];
            excl + self
                .segments
                .iter()
                .filter(|s| s.released(&self.refs_arena, &self.sigs_arena, &self.sigs, inside) >= s.rc)
                .map(|s| s.len)
                .sum::<u64>()
        })
    }

    /// Distinct clusters the items in `set` use between them.
    pub fn union_of(&self, set: &[usize]) -> u64 {
        with_in_set(&self.exclusive, set, |mask, excl| {
            if self.segments.is_empty() {
                return excl;
            }
            excl + self
                .segments
                .iter()
                .filter(|s| {
                    s.refs(&self.refs_arena).iter().any(|(i, _)| mask[*i as usize])
                        || s.sig_refs(&self.sigs_arena).iter().any(|&(sid, _)| self.sigs[sid].items.iter().any(|(i, _)| mask[*i as usize]))
                })
                .map(|s| s.len)
                .sum::<u64>()
        })
    }

    /// Sets of two or more items that reference the same clusters, largest first. `at_least` is in clusters.
    pub fn sharing_groups(&self, at_least: u64) -> Vec<SharingGroup> {
        let mut groups: HashMap<Vec<usize>, (u64, u64)> = HashMap::new();
        let mut parts_buf: Vec<u16> = Vec::new();
        let mut ids_buf: Vec<usize> = Vec::new();
        for s in &self.segments {
            s.participants_into(&self.refs_arena, &self.sigs_arena, &self.sigs, &mut parts_buf);
            if parts_buf.len() < 2 {
                continue;
            }
            ids_buf.clear();
            ids_buf.extend(parts_buf.iter().map(|&i| i as usize));
            let m = s.released(&self.refs_arena, &self.sigs_arena, &self.sigs, |_| true);
            if let Some(e) = groups.get_mut(ids_buf.as_slice()) {
                e.0 += s.len;
                if m >= s.rc {
                    e.1 += s.len;
                }
            } else {
                groups.insert(ids_buf.clone(), (s.len, if m >= s.rc { s.len } else { 0 }));
            }
        }
        let mut v: Vec<SharingGroup> = groups
            .into_iter()
            .filter(|(_, (shared, _))| *shared >= at_least)
            .map(|(items, (shared, frees))| SharingGroup { items, shared, frees_if_all_removed: frees })
            .collect();
        v.sort_by(|a, b| b.shared.cmp(&a.shared).then(a.items.cmp(&b.items)));
        v
    }
}

// ---------------------------------------------------------------- confidence

/// How far a "frees" number can be trusted (proposal 3.3), weakest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// Could not be measured (network path, cloud sync root, access denied). The number is null.
    Unknown,
    /// Block cloning is on but reference counts could not be read: frees at most this.
    UpperBound,
    /// From an old scan, or the tree changed during the scan: "about".
    Estimate,
    /// Cut short by the budget, or some files could not be opened: frees at least this.
    LowerBound,
    Exact,
}

impl Confidence {
    pub fn name(self) -> &'static str {
        match self {
            Confidence::Unknown => "unknown",
            Confidence::UpperBound => "upper_bound",
            Confidence::Estimate => "estimate",
            Confidence::LowerBound => "lower_bound",
            Confidence::Exact => "exact",
        }
    }

    /// The word to put in front of a number: "", "at least", "about", "at most".
    pub fn qualifier(self) -> &'static str {
        match self {
            Confidence::Exact => "",
            Confidence::LowerBound => "at least ",
            Confidence::Estimate => "about ",
            Confidence::UpperBound => "at most ",
            Confidence::Unknown => "",
        }
    }

    /// May this number be compared with a need (is it safe to rely on as a floor)?
    pub fn usable_as_floor(self) -> bool {
        matches!(self, Confidence::Exact | Confidence::LowerBound)
    }
}

/// The facts the confidence of one item's numbers follows from.
#[derive(Debug, Clone, Copy, Default)]
pub struct ScanFacts {
    /// The root is a network path, inside a cloud sync root, or could not be opened.
    pub unmeasurable: bool,
    /// The volume shares clusters between files (ReFS block cloning).
    pub block_cloning: bool,
    /// Files whose extents were read without reference counts.
    pub files_without_refcount: u64,
    /// Level 2 did not visit every file (budget, or stopped for a build).
    pub partial: bool,
    /// Files or folders that could not be opened or read.
    pub failures: u64,
    /// A file in the item was written during the scan, or Unreal is working in it.
    pub in_flux: bool,
    /// The sweep found more references than the volume counts.
    pub anomalies: bool,
    /// Two files reported different counts for the same cluster.
    pub inconsistent: bool,
    /// The numbers come from a cached scan older than `fresh_sec`.
    pub stale: bool,
}

/// The confidence rules (proposal 3.3).
pub fn confidence(f: &ScanFacts) -> Confidence {
    if f.unmeasurable {
        Confidence::Unknown
    } else if f.block_cloning && f.files_without_refcount > 0 {
        Confidence::UpperBound
    } else if f.in_flux || f.anomalies || f.inconsistent || f.stale {
        Confidence::Estimate
    } else if f.partial || f.failures > 0 {
        Confidence::LowerBound
    } else {
        Confidence::Exact
    }
}

/// A total's confidence is the weakest of its members'. A set with an `upper_bound` or
/// `unknown` member has no total at all (`None`).
pub fn total_confidence(members: &[Confidence]) -> Option<Confidence> {
    let weakest = members.iter().copied().min()?;
    (weakest >= Confidence::Estimate).then_some(weakest)
}

// ---------------------------------------------------------------- items and ownership

/// Normalise a path into `out` for comparisons without allocating when `out` has capacity.
pub fn norm_into(p: &str, out: &mut String) {
    out.clear();
    if p.is_ascii() {
        for &b in p.as_bytes() {
            let b = if b == b'/' { b'\\' } else { b.to_ascii_lowercase() };
            out.push(b as char);
        }
    } else {
        out.push_str(&p.to_lowercase());
        for idx in 0..out.len() {
            if out.as_bytes()[idx] == b'/' {
                out.replace_range(idx..idx + 1, r"\");
            }
        }
    }
    if out.starts_with(r"\\?\unc\") {
        out.replace_range(..8, r"\\");
    } else if out.starts_with(r"\\?\") {
        out.drain(..4);
    }
    let prefix_len = if out.starts_with(r"\\") { 2 } else { 0 };
    if out.len() > prefix_len && out.as_bytes()[prefix_len..].windows(2).any(|w| w == b"\\\\") {
        let mut cleaned = String::with_capacity(out.len());
        cleaned.push_str(&out[..prefix_len]);
        let mut prev_sep = prefix_len == 2;
        for ch in out[prefix_len..].chars() {
            if ch == '\\' {
                if !prev_sep {
                    cleaned.push('\\');
                    prev_sep = true;
                }
            } else {
                prev_sep = false;
                cleaned.push(ch);
            }
        }
        out.clear();
        out.push_str(&cleaned);
    }
    while out.ends_with('\\') && !out.ends_with(":\\") && out.len() > 1 {
        out.pop();
    }
}

/// Normalise a path for comparisons: backslashes, no trailing separator, lower case.
pub fn norm(p: &str) -> String {
    let mut out = String::with_capacity(p.len());
    norm_into(p, &mut out);
    out
}

/// Is `inner` the same as, or inside, `outer` (both normalised)?
pub fn is_within(inner: &str, outer: &str) -> bool {
    inner == outer || (inner.starts_with(outer) && (outer.ends_with('\\') || inner[outer.len()..].starts_with('\\')))
}

/// Which item owns `path`: the deepest root that contains it (rule 5). `roots` are
/// `(item index, normalised root)`.
pub fn owner(path: &str, roots: &[(usize, String)]) -> Option<usize> {
    let p = norm(path);
    roots.iter().filter(|(_, r)| is_within(&p, r)).max_by_key(|(_, r)| r.len()).map(|(i, _)| *i)
}

/// For each item root, the other roots strictly inside it: those folders are counted for the
/// inner item only, so the outer one skips them. Exact duplicates are dropped (the first one
/// wins) and reported in the second value.
pub fn nested_roots(roots: &[(usize, String)]) -> (BTreeMap<usize, Vec<String>>, Vec<(usize, String)>) {
    let mut seen: Vec<String> = Vec::new();
    let mut dups = Vec::new();
    let mut kept: Vec<(usize, String)> = Vec::new();
    for (i, r) in roots {
        let n = norm(r);
        if seen.contains(&n) {
            dups.push((*i, r.clone()));
            continue;
        }
        seen.push(n.clone());
        kept.push((*i, n));
    }
    let mut out: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for (i, outer) in &kept {
        for (j, inner) in &kept {
            if i != j && inner != outer && is_within(inner, outer) {
                out.entry(*i).or_default().push(inner.clone());
            }
        }
    }
    (out, dups)
}

/// Bytes to GB with two decimals, as every disk number is reported.
pub fn gb2(bytes: u64) -> f64 {
    (bytes as f64 / (1u64 << 30) as f64 * 100.0).round() / 100.0
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    fn e(lcn: u64, len: u32, rc: u32) -> Ext {
        Ext { lcn, len, rc }
    }
    fn item(excl: u64, shared: &[Ext]) -> ItemExtents {
        ItemExtents { exclusive_clusters: excl, shared: shared.to_vec() }
    }

    /// Two lanes cloned from one project that isn't scanned: everything they share with it
    /// stays, whatever is deleted.
    #[test]
    fn clones_of_an_unscanned_tree_free_nothing() {
        // clusters 0..100 referenced by project (outside), lane a, lane b: rc 3
        let a = item(10, &[e(0, 100, 3)]);
        let b = item(5, &[e(0, 100, 3)]);
        let s = sweep(&[a, b]);
        assert_eq!(s.items[0].footprint, 110);
        assert_eq!(s.items[0].frees_alone, 10);
        assert_eq!(s.items[0].shared_outside, 100);
        assert_eq!(s.items[0].shared_with[&1], 100);
        assert_eq!(s.items[0].stays_shared(), 100);
        assert_eq!(s.frees_together(&[0, 1]), 15);
        assert_eq!(s.union, 115);
        assert_eq!(s.anomalies, 0);
    }

    /// Two lanes that share clusters only with each other: alone each frees its own part,
    /// together they also free the shared part (the sum of singles under-counts).
    #[test]
    fn pair_shared_only_with_each_other_frees_together() {
        let a = item(7, &[e(1000, 40, 2), e(0, 100, 3)]);
        let b = item(50, &[e(1000, 40, 2), e(0, 100, 3)]);
        let s = sweep(&[a, b]);
        assert_eq!(s.items[0].frees_alone, 7);
        assert_eq!(s.items[1].frees_alone, 50);
        assert_eq!(s.frees_together(&[0, 1]), 97);
        assert!(s.frees_together(&[0, 1]) >= s.items[0].frees_alone + s.items[1].frees_alone);
        let g = s.sharing_groups(1);
        assert_eq!(g[0], SharingGroup { items: vec![0, 1], shared: 140, frees_if_all_removed: 40 });
    }

    /// git LFS store and the working tree inside one item: the clone is counted once in the
    /// footprint and frees, because both references are inside.
    #[test]
    fn clones_inside_one_item_count_once_and_free() {
        let a = item(0, &[e(0, 100, 2), e(0, 100, 2)]);
        let s = sweep(&[a]);
        assert_eq!(s.items[0].footprint, 100);
        assert_eq!(s.items[0].frees_alone, 100);
        assert_eq!(s.union, 100);
    }

    #[test]
    fn partial_overlaps_split_into_runs() {
        // a: 0..100 rc2 shared with b at 50..150 (b's own extent rc2)
        let a = item(0, &[e(0, 50, 1), e(50, 50, 2)]);
        let b = item(0, &[e(50, 50, 2), e(100, 50, 1)]);
        let s = sweep(&[a, b]);
        assert_eq!(s.items[0].footprint, 100);
        assert_eq!(s.items[1].footprint, 100);
        assert_eq!(s.items[0].frees_alone, 50);
        assert_eq!(s.frees_together(&[0, 1]), 150);
        assert_eq!(s.union, 150);
    }

    #[test]
    fn more_references_than_the_count_is_an_anomaly() {
        let a = item(0, &[e(0, 10, 2)]);
        let b = item(0, &[e(0, 10, 2)]);
        let c = item(0, &[e(0, 10, 2)]);
        let s = sweep(&[a, b, c]);
        assert_eq!(s.anomalies, 10);
        let f = ScanFacts { block_cloning: true, anomalies: s.anomalies > 0, ..Default::default() };
        assert_eq!(confidence(&f), Confidence::Estimate);
    }

    #[test]
    fn different_counts_for_one_cluster_take_the_larger() {
        let a = item(0, &[e(0, 10, 2)]);
        let b = item(0, &[e(0, 10, 3)]);
        let s = sweep(&[a, b]);
        assert_eq!(s.inconsistent, 10);
        assert_eq!(s.frees_together(&[0, 1]), 0);
    }

    #[test]
    fn unknown_counts_assume_no_outside_referrer() {
        // Without counts, a lane looks fully exclusive: v1's over-statement, labelled upper_bound.
        let a = item(0, &[e(0, 100, 0)]);
        let s = sweep(&[a]);
        assert_eq!(s.items[0].frees_alone, 100);
        assert_eq!(s.unknown_rc, 100);
        let f = ScanFacts { block_cloning: true, files_without_refcount: 3, ..Default::default() };
        assert_eq!(confidence(&f), Confidence::UpperBound);
    }

    /// The invariants of 3.7 over random synthetic volumes: frees_together >= sum of
    /// frees_alone, frees_together <= union footprint, footprint >= frees_alone.
    #[test]
    fn invariants_hold_for_random_volumes() {
        let mut x: u64 = 0x2545_F491_4F6C_DD1D;
        let mut rnd = |n: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % n
        };
        for _ in 0..200 {
            let n_items = 1 + rnd(5) as usize;
            // Build a volume: clusters 0..400 in runs of 10, each run referenced by some items
            // (possibly several times) plus some outside references.
            let mut items: Vec<ItemExtents> = vec![ItemExtents::default(); n_items];
            for run in 0..40u64 {
                let mut refs: Vec<usize> = Vec::new();
                for i in 0..n_items {
                    for _ in 0..rnd(3) {
                        refs.push(i);
                    }
                }
                if refs.is_empty() {
                    continue;
                }
                let outside = rnd(2) as u32;
                let rc = refs.len() as u32 + outside;
                for i in refs {
                    if rc == 1 {
                        items[i].exclusive_clusters += 10;
                    } else {
                        items[i].shared.push(e(run * 10, 10, rc));
                    }
                }
            }
            let s = sweep(&items);
            assert_eq!(s.anomalies, 0);
            let all: Vec<usize> = (0..n_items).collect();
            let sum_alone: u64 = s.items.iter().map(|i| i.frees_alone).sum();
            assert!(s.frees_together(&all) >= sum_alone);
            assert!(s.frees_together(&all) <= s.union);
            assert_eq!(s.union_of(&all), s.union);
            for (i, it) in s.items.iter().enumerate() {
                assert!(it.footprint >= it.frees_alone);
                assert_eq!(s.frees_together(&[i]), it.frees_alone);
            }
            // Subsets: monotone.
            if n_items >= 2 {
                assert!(s.frees_together(&[0, 1]) >= s.frees_together(&[0]));
                assert!(s.frees_together(&[0, 1]) <= s.union_of(&[0, 1]));
            }
        }
    }

    #[test]
    fn confidence_rules_and_totals() {
        let base = ScanFacts { block_cloning: true, ..Default::default() };
        assert_eq!(confidence(&base), Confidence::Exact);
        assert_eq!(confidence(&ScanFacts { partial: true, ..base }), Confidence::LowerBound);
        assert_eq!(confidence(&ScanFacts { failures: 2, ..base }), Confidence::LowerBound);
        assert_eq!(confidence(&ScanFacts { in_flux: true, ..base }), Confidence::Estimate);
        assert_eq!(confidence(&ScanFacts { in_flux: true, partial: true, ..base }), Confidence::Estimate);
        assert_eq!(confidence(&ScanFacts { stale: true, ..base }), Confidence::Estimate);
        assert_eq!(confidence(&ScanFacts { unmeasurable: true, ..base }), Confidence::Unknown);
        // NTFS has no block cloning, so missing counts don't matter there.
        assert_eq!(confidence(&ScanFacts { files_without_refcount: 9, ..Default::default() }), Confidence::Exact);
        use Confidence::*;
        assert_eq!(total_confidence(&[Exact, LowerBound]), Some(LowerBound));
        assert_eq!(total_confidence(&[Exact, Estimate, LowerBound]), Some(Estimate));
        assert_eq!(total_confidence(&[Exact, UpperBound]), None);
        assert_eq!(total_confidence(&[Unknown]), None);
        assert_eq!(total_confidence(&[]), None);
        assert!(Exact.usable_as_floor() && LowerBound.usable_as_floor() && !Estimate.usable_as_floor());
    }

    #[test]
    fn deepest_root_owns_a_file() {
        let roots = vec![(0, norm(r"D:\s2")), (1, norm(r"D:\s2\Saved")), (2, norm(r"D:\s2\Saved\Logs"))];
        assert_eq!(owner(r"D:\s2\Content\a.uasset", &roots), Some(0));
        assert_eq!(owner(r"D:\s2\Saved\Config\x.ini", &roots), Some(1));
        assert_eq!(owner(r"d:\S2\saved\logs\S2.log", &roots), Some(2));
        assert_eq!(owner(r"D:\s2x\a", &roots), None);
        assert_eq!(owner(r"D:\s2\SavedGames\a", &roots), Some(0));
    }

    #[test]
    fn nested_roots_and_duplicates() {
        let roots = vec![
            (0, r"D:\lanes\a".to_string()),
            (1, r"D:\lanes\a\Intermediate".to_string()),
            (2, r"d:\LANES\a\".to_string()),
            (3, r"D:\lanes\ab".to_string()),
        ];
        let (nested, dups) = nested_roots(&roots);
        assert_eq!(nested.get(&0), Some(&vec![r"d:\lanes\a\intermediate".to_string()]));
        assert!(!nested.contains_key(&3));
        assert_eq!(dups, vec![(2, r"d:\LANES\a\".to_string())]);
        assert_eq!(norm(r"\\?\D:\x\"), r"d:\x");
        assert_eq!(norm(r"D:\"), r"d:\");
        assert!(is_within(r"d:\x", r"d:\"));
    }

    /// Adding an item whose root lies inside another changes nothing for the union: the
    /// scanner gives those files to the inner item only, so the set's numbers are the same.
    #[test]
    fn splitting_an_item_keeps_set_totals() {
        let whole = sweep(&[item(30, &[e(0, 20, 2), e(100, 10, 3)])]);
        // Same clusters, split between an outer and an inner item.
        let split = sweep(&[item(20, &[e(0, 20, 2)]), item(10, &[e(100, 10, 3)])]);
        assert_eq!(whole.union, split.union);
        assert_eq!(whole.frees_together(&[0]), split.frees_together(&[0, 1]));
    }

    #[test]
    fn hard_linked_and_single_link_sharing_block_cloned_extent() {
        // Case (a): 1 hard-linked file (links=2: item 0, item 1) + 1 single-link file (item 2)
        // sharing block-cloned extent (0, 100, rc=2).
        let items = vec![item(0, &[]), item(0, &[]), item(0, &[e(0, 100, 2)])];
        let linked = vec![LinkedFile { links: 2, items: vec![(0, 1), (1, 1)], exts: vec![e(0, 100, 2)] }];
        let s = sweep_with_links(&items, &linked);
        assert_eq!(s.inconsistent, 0);
        assert_eq!(s.anomalies, 0);
        assert_eq!(s.items[0].frees_alone, 0);
        assert_eq!(s.items[1].frees_alone, 0);
        assert_eq!(s.items[2].frees_alone, 0);
        assert_eq!(s.frees_together(&[0, 1]), 0);
        assert_eq!(s.frees_together(&[0, 2]), 0);
        assert_eq!(s.frees_together(&[0, 1, 2]), 100);
        let g = s.sharing_groups(1);
        assert_eq!(g, vec![SharingGroup { items: vec![0, 1, 2], shared: 100, frees_if_all_removed: 100 }]);
    }

    #[test]
    fn multiple_hard_linked_files_sharing_block_cloned_extent() {
        // Case (b1): F1 (links=2 in [0, 1]) + F2 (links=3 in [0, 2, 2]) sharing (0, 100, rc=2).
        let items = vec![item(0, &[]), item(0, &[]), item(0, &[])];
        let linked = vec![
            LinkedFile { links: 2, items: vec![(0, 1), (1, 1)], exts: vec![e(0, 100, 2)] },
            LinkedFile { links: 3, items: vec![(0, 1), (2, 2)], exts: vec![e(0, 100, 2)] },
        ];
        let s = sweep_with_links(&items, &linked);
        assert_eq!(s.anomalies, 0);
        assert_eq!(s.inconsistent, 0);
        assert_eq!(s.frees_together(&[0, 1]), 0);
        assert_eq!(s.frees_together(&[0, 2]), 0);
        assert_eq!(s.frees_together(&[0, 1, 2]), 100);

        // Case (b2): F1 (links=2 in [0, 1]) + F2 (links=2: 1 in item 0, 1 outside) sharing (0, 100, rc=2).
        let items2 = vec![item(0, &[]), item(0, &[])];
        let linked2 = vec![
            LinkedFile { links: 2, items: vec![(0, 1), (1, 1)], exts: vec![e(0, 100, 2)] },
            LinkedFile { links: 2, items: vec![(0, 1)], exts: vec![e(0, 100, 2)] },
        ];
        let s2 = sweep_with_links(&items2, &linked2);
        assert_eq!(s2.anomalies, 0);
        assert_eq!(s2.inconsistent, 0);
        assert_eq!(s2.frees_together(&[0, 1]), 0);
        assert_eq!(s2.items[0].shared_outside, 100);
        assert_eq!(s2.items[1].shared_outside, 100);
        assert_eq!(s2.sharing_groups(1)[0].frees_if_all_removed, 0);
    }

    #[test]
    fn hard_linked_files_same_item_vs_outside_release_rules() {
        // Case (c): F1 (links=2, both in item 0) + F2 (links=2, 1 in item 0, 1 outside) on rc=2.
        let items = vec![item(0, &[])];
        let linked = vec![
            LinkedFile { links: 2, items: vec![(0, 2)], exts: vec![e(0, 100, 2)] },
            LinkedFile { links: 2, items: vec![(0, 1)], exts: vec![e(0, 100, 2)] },
        ];
        let s = sweep_with_links(&items, &linked);
        assert_eq!(s.items[0].frees_alone, 0);
        assert_eq!(s.items[0].shared_outside, 100);

        // F1 (links=2, both in item 0, rc=1): deleting item 0 frees all 100 clusters.
        let s_alone = sweep_with_links(&items, &[LinkedFile { links: 2, items: vec![(0, 2)], exts: vec![e(0, 100, 1)] }]);
        assert_eq!(s_alone.items[0].frees_alone, 100);
        assert_eq!(s_alone.items[0].footprint, 100);
        assert_eq!(s_alone.items[0].shared_outside, 0);
    }

    #[test]
    fn anomalies_inconsistent_and_unknown_rc_isolated_per_item() {
        // Case (d): items 0 & 1 have inconsistent/unknown extents while item 2 is clean.
        let items = vec![item(0, &[e(0, 10, 2)]), item(0, &[e(0, 10, 3)]), item(0, &[e(100, 10, 2)])];
        let s = sweep(&items);
        assert_eq!(s.items[0].inconsistent, 10);
        assert_eq!(s.items[1].inconsistent, 10);
        assert_eq!(s.items[2].inconsistent, 0);
        assert_eq!(s.items[0].anomalies, 0);
        assert_eq!(s.items[2].anomalies, 0);

        // ScanFacts::inconsistent downgrades confidence to Estimate.
        let f = ScanFacts { block_cloning: true, inconsistent: true, ..Default::default() };
        assert_eq!(confidence(&f), Confidence::Estimate);
        assert_eq!(total_confidence(&[Confidence::Exact, confidence(&f)]), Some(Confidence::Estimate));

        // Unknown rc on a LinkedFile propagates to every participating item.
        let s_unk = sweep_with_links(&[item(0, &[]), item(0, &[]), item(0, &[e(200, 5, 1)])], &[LinkedFile { links: 2, items: vec![(0, 1), (1, 1)], exts: vec![e(0, 10, 0)] }]);
        assert_eq!(s_unk.items[0].unknown_rc, 10);
        assert_eq!(s_unk.items[1].unknown_rc, 10);
        assert_eq!(s_unk.items[2].unknown_rc, 0);
    }

    #[test]
    fn ext_and_event_are_packed_16_bytes() {
        assert_eq!(std::mem::size_of::<Ext>(), 16);
        assert_eq!(std::mem::size_of::<Event>(), 16);
    }

    #[test]
    fn split_extents_preserve_sweep_results_and_u64_max_boundary_does_not_wrap() {
        // Equivalent unsplit vs u32::MAX-chunked extent behavior (using a conceptual split at 1_000_000).
        let unsplit = sweep_with_links(
            &[item(0, &[e(100, 2_000_000, 2)]), item(0, &[e(100, 2_000_000, 2)])],
            &[LinkedFile { links: 2, items: vec![(0, 1), (1, 1)], exts: vec![e(5_000_000, 1_500_000, 2)] }],
        );
        let split = sweep_with_links(
            &[
                item(0, &[e(100, 1_000_000, 2), e(1_000_100, 1_000_000, 2)]),
                item(0, &[e(100, 1_000_000, 2), e(1_000_100, 1_000_000, 2)]),
            ],
            &[LinkedFile {
                links: 2,
                items: vec![(0, 1), (1, 1)],
                exts: vec![e(5_000_000, 750_000, 2), e(5_750_000, 750_000, 2)],
            }],
        );
        assert_eq!(unsplit.items, split.items);
        assert_eq!(unsplit.union, split.union);
        assert_eq!(unsplit.frees_together(&[0, 1]), split.frees_together(&[0, 1]));
        assert_eq!(unsplit.sharing_groups(1), split.sharing_groups(1));
        assert_eq!(unsplit.segments.len(), split.segments.len());

        // u64::MAX saturation boundary: must not panic or wrap.
        let boundary = sweep(&[item(0, &[e(u64::MAX - 5, 10, 2)]), item(0, &[e(u64::MAX - 5, 10, 2)])]);
        assert_eq!(boundary.union, 5);
        assert_eq!(boundary.frees_together(&[0, 1]), 5);
    }

    #[test]
    fn rc_tracker_overflow_and_unwinding_handles_many_rcs_and_zero() {
        // 6 distinct non-zero rcs + rc=0 overlapping on [0, 10), then unwinding in arbitrary order.
        let mut items = vec![item(0, &[e(0, 50, 0), e(0, 40, 2), e(0, 30, 5), e(0, 20, 9)])];
        items.push(item(0, &[e(0, 10, 3), e(0, 25, 7), e(0, 35, 11)]));
        let s = sweep(&items);
        // [0, 10): unknown=true, rcs={2,3,5,7,9,11} -> inconsistent=true, max=11
        // [10, 20): unknown=true, rcs={2,5,7,9,11} -> inconsistent=true, max=11
        // [20, 25): unknown=true, rcs={2,5,7,11} -> inconsistent=true, max=11
        // [25, 30): unknown=true, rcs={2,5,11} -> inconsistent=true, max=11
        // [30, 35): unknown=true, rcs={2,11} -> inconsistent=true, max=11
        // [35, 40): unknown=true, rcs={2}, item 0 holds both active extents (seen=2 == rc=2) -> frees_alone!
        // [40, 50): unknown=true, only rc=0 -> seen=1, rc=1 -> frees_alone!
        assert_eq!(s.unknown_rc, 50);
        assert_eq!(s.inconsistent, 35);
        assert_eq!(s.union, 50);
        assert_eq!(s.items[0].frees_alone, 15); // [35, 50)
    }

    #[test]
    fn parallel_sort_sweep_matches_naive_oracle_above_threshold() {
        // Build > 65_536 events (e.g. 50_000 extents = 100_000 events) shuffled across 4 items.
        let mut items = vec![ItemExtents::default(); 4];
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut rnd = |m: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % m
        };
        for idx in 0..50_000u64 {
            let item_idx = (idx % 4) as usize;
            let lcn = rnd(10_000) * 3;
            let len = 1 + rnd(12) as u32;
            let rc = 2 + (idx % 3) as u32;
            items[item_idx].shared.push(e(lcn, len, rc));
        }
        let s = sweep(&items);
        // Verify invariants against brute-force cluster union on [0, 30_050).
        let mut seen = vec![false; 30_050];
        for it in &items {
            for ex in &it.shared {
                for c in ex.lcn..ex.lcn + ex.len as u64 {
                    seen[c as usize] = true;
                }
            }
        }
        let expected_union = seen.iter().filter(|&&b| b).count() as u64;
        assert_eq!(s.union, expected_union);
        assert_eq!(s.union_of(&[0, 1, 2, 3]), expected_union);
        assert!(s.frees_together(&[0, 1, 2, 3]) <= expected_union);
    }

    #[test]
    fn norm_into_matches_norm_and_reuses_capacity() {
        let cases = [
            "D:/a/b",
            "D:\\",
            r"\\?\UNC\Server/Share/",
            r"\\?\C:/a//b\",
            r"C:\Proj\ΣΕΛΙΔΑ/Foo/",
            r"\\?\",
            "/",
        ];
        let mut buf = String::with_capacity(128);
        let initial_cap = buf.capacity();
        for c in cases {
            norm_into(c, &mut buf);
            assert_eq!(buf, norm(c), "case: {}", c);
            assert_eq!(buf.capacity(), initial_cap, "unexpected realloc for {}", c);
        }
    }

    #[test]
    fn frees_together_and_union_of_deduplicate_repeated_indices() {
        // Exclusive-only sweep (segments empty).
        let sw_excl = sweep(&[item(10, &[]), item(20, &[])]);
        assert_eq!(sw_excl.frees_together(&[0, 0]), sw_excl.frees_together(&[0]));
        assert_eq!(sw_excl.frees_together(&[0, 0]), 10);
        assert_eq!(sw_excl.union_of(&[0, 0]), sw_excl.union_of(&[0]));
        assert_eq!(sw_excl.union_of(&[0, 0]), 10);
        assert_eq!(sw_excl.frees_together(&[0, 1, 0, 1]), sw_excl.frees_together(&[0, 1]));
        assert_eq!(sw_excl.union_of(&[0, 1, 0, 1]), sw_excl.union_of(&[0, 1]));

        // Mixed exclusive + shared + linked sweep (segments non-empty).
        let sw_mixed = sweep_with_links(
            &[item(10, &[e(100, 5, 2)]), item(20, &[e(100, 5, 2)])],
            &[LinkedFile { links: 2, items: vec![(0, 1), (1, 1)], exts: vec![e(200, 7, 2)] }],
        );
        assert_eq!(sw_mixed.frees_together(&[0, 0]), sw_mixed.frees_together(&[0]));
        assert_eq!(sw_mixed.frees_together(&[0, 0]), 10);
        assert_eq!(sw_mixed.union_of(&[0, 0]), sw_mixed.union_of(&[0]));
        assert_eq!(sw_mixed.union_of(&[0, 0]), 22);
        assert_eq!(sw_mixed.frees_together(&[0, 1, 0, 1]), sw_mixed.frees_together(&[0, 1]));
        assert_eq!(sw_mixed.union_of(&[0, 1, 0, 1]), sw_mixed.union_of(&[0, 1]));
    }

    #[test]
    fn t5_sweep_edge_zero_len_u64_max_dup_ext_and_par_merge_odd_chunks() {
        let empty = sweep(&[]);
        assert_eq!(empty.union, 0);

        let items = vec![
            ItemExtents {
                exclusive_clusters: 3,
                shared: vec![
                    Ext { lcn: 100, len: 10, rc: 2 },
                    Ext { lcn: 100, len: 10, rc: 2 },
                    Ext { lcn: 200, len: 0, rc: 2 },
                    Ext { lcn: u64::MAX - 2, len: 10, rc: 2 },
                ],
            },
            ItemExtents {
                exclusive_clusters: 0,
                shared: vec![Ext { lcn: 100, len: 10, rc: 2 }],
            },
        ];
        let sw = sweep_with_links(&items, &[]);
        assert_eq!(sw.items[0].footprint, 15);
        assert_eq!(sw.items[0].frees_alone, 13);
        assert_eq!(sw.items[1].footprint, 10);
        assert_eq!(sw.items[1].frees_alone, 0);
        assert_eq!(sw.union, 15);
        assert_eq!(sw.anomalies, 10);

        // >524,288 events (odd number of chunks in parallel sort/merge) vs naive oracle
        let n_ext = 270_000u64;
        let mut s0 = Vec::with_capacity(n_ext as usize);
        let mut s1 = Vec::with_capacity(2048);
        for i in 0..n_ext {
            s0.push(Ext { lcn: i * 4, len: 2, rc: if i < 1000 { 2 } else { 1 } });
            if i < 1000 {
                s1.push(Ext { lcn: i * 4, len: 2, rc: 2 });
            }
        }
        let big = sweep(&[
            ItemExtents { exclusive_clusters: 5, shared: s0 },
            ItemExtents { exclusive_clusters: 7, shared: s1 },
        ]);
        assert_eq!(big.items[0].footprint, 5 + n_ext * 2);
        assert_eq!(big.items[0].frees_alone, 5 + (n_ext - 1000) * 2);
        assert_eq!(big.items[1].footprint, 7 + 2000);
        assert_eq!(big.items[1].frees_alone, 7);
        assert_eq!(big.union, 5 + 7 + n_ext * 2);
    }

    #[test]
    fn t5_rc_tracker_overflow_unwind_and_norm_double_slash() {
        let mut rc = RcTracker::default();
        for v in [2u32, 3, 4, 5, 6, 7] {
            rc.inc(v);
        }
        assert!(rc.is_inconsistent());
        assert_eq!(rc.known_max(), Some(7));
        for v in [7u32, 6, 5, 4, 3, 2] {
            rc.dec(v);
        }
        assert!(!rc.is_inconsistent());
        assert_eq!(rc.known_max(), None);

        // UNC prefix must keep leading \\ while regular paths collapse consecutive separators
        assert_eq!(norm(r"\\server\share\a\b\"), r"\\server\share\a\b");
        assert_eq!(norm(r"\\server\\share\\a\b\"), r"\\server\share\a\b");
        assert_eq!(norm(r"E:/Caches/\pip\cache"), r"e:\caches\pip\cache");
    }
}
