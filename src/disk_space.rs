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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ext {
    pub lcn: u64,
    pub len: u64,
    pub rc: u32,
}

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
}

impl ItemSpace {
    /// `footprint - frees_alone`: what stays allocated if only this item goes.
    pub fn stays_shared(&self) -> u64 {
        self.footprint - self.frees_alone
    }
}

/// A set of items that reference the same clusters, and how much of that frees when exactly
/// these items go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharingGroup {
    pub items: Vec<usize>,
    pub shared: u64,
    pub frees_if_all_removed: u64,
}

/// Runs of clusters with the same references, aggregated: which items reference them how
/// often, and the volume's count.
#[derive(Debug, Clone)]
struct Segment {
    refs: Vec<(u16, u32)>,
    rc: u32,
    len: u64,
}

#[derive(Debug, Clone, Default)]
pub struct Sweep {
    pub items: Vec<ItemSpace>,
    exclusive: Vec<u64>,
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

/// Sweep the extents of `items` (all on one volume).
pub fn sweep(items: &[ItemExtents]) -> Sweep {
    let n = items.len();
    assert!(n <= u16::MAX as usize, "too many items for one sweep");
    // (position, +1 start / -1 end, item, rc)
    let mut ev: Vec<(u64, i8, u16, u32)> = Vec::with_capacity(items.iter().map(|i| i.shared.len() * 2).sum());
    for (i, it) in items.iter().enumerate() {
        for e in it.shared.iter().filter(|e| e.len > 0) {
            ev.push((e.lcn, 1, i as u16, e.rc));
            ev.push((e.lcn + e.len, -1, i as u16, e.rc));
        }
    }
    ev.sort_unstable_by_key(|e| (e.0, e.1));

    let mut out = Sweep { items: vec![ItemSpace::default(); n], exclusive: items.iter().map(|i| i.exclusive_clusters).collect(), ..Default::default() };
    let mut agg: HashMap<(Vec<(u16, u32)>, u32), u64> = HashMap::new();
    let mut mult: BTreeMap<u16, u32> = BTreeMap::new();
    let mut rcs: BTreeMap<u32, u32> = BTreeMap::new();
    let mut total_m = 0u32;
    let mut prev = 0u64;
    let mut k = 0usize;
    while k < ev.len() {
        let pos = ev[k].0;
        if total_m > 0 && pos > prev {
            let len = pos - prev;
            let known = rcs.range(1..).next_back().map(|(r, _)| *r);
            let unknown = rcs.contains_key(&0);
            if rcs.range(1..).count() > 1 {
                out.inconsistent += len;
            }
            // Unknown counts: assume nothing outside the scan refers to it (an upper bound).
            let rc = match (known, unknown) {
                (Some(r), false) => r,
                (Some(r), true) => r.max(total_m),
                (None, _) => total_m,
            };
            if unknown {
                out.unknown_rc += len;
            }
            if total_m > rc {
                out.anomalies += len;
            }
            let refs: Vec<(u16, u32)> = mult.iter().map(|(i, m)| (*i, *m)).collect();
            *agg.entry((refs, rc)).or_default() += len;
        }
        while k < ev.len() && ev[k].0 == pos {
            let (_, d, i, rc) = ev[k];
            if d > 0 {
                *mult.entry(i).or_default() += 1;
                *rcs.entry(rc).or_default() += 1;
                total_m += 1;
            } else {
                dec(&mut mult, i);
                dec(&mut rcs, rc);
                total_m -= 1;
            }
            k += 1;
        }
        prev = pos;
    }

    for ((refs, rc), len) in agg {
        let m_all: u32 = refs.iter().map(|r| r.1).sum();
        out.union += len;
        for &(i, m) in &refs {
            let s = &mut out.items[i as usize];
            s.footprint += len;
            if m >= rc {
                s.frees_alone += len;
            }
            if m_all < rc {
                s.shared_outside += len;
            }
            for &(j, _) in &refs {
                if j != i {
                    *s.shared_with.entry(j as usize).or_default() += len;
                }
            }
        }
        out.segments.push(Segment { refs, rc, len });
    }
    for (i, x) in out.exclusive.iter().enumerate() {
        out.items[i].footprint += x;
        out.items[i].frees_alone += x;
        out.union += x;
    }
    out
}

fn dec<K: Ord + Copy>(m: &mut BTreeMap<K, u32>, k: K) {
    if let Some(c) = m.get_mut(&k) {
        *c -= 1;
        if *c == 0 {
            m.remove(&k);
        }
    }
}

impl Sweep {
    /// Clusters freed by deleting exactly the items in `set` together: every reference to them
    /// is inside the set. The only way to get a total for several items.
    pub fn frees_together(&self, set: &[usize]) -> u64 {
        let inside = |i: u16| set.contains(&(i as usize));
        let excl: u64 = set.iter().filter_map(|i| self.exclusive.get(*i)).sum();
        excl + self
            .segments
            .iter()
            .filter(|s| s.refs.iter().filter(|(i, _)| inside(*i)).map(|r| r.1).sum::<u32>() >= s.rc)
            .map(|s| s.len)
            .sum::<u64>()
    }

    /// Distinct clusters the items in `set` use between them.
    pub fn union_of(&self, set: &[usize]) -> u64 {
        let excl: u64 = set.iter().filter_map(|i| self.exclusive.get(*i)).sum();
        excl + self.segments.iter().filter(|s| s.refs.iter().any(|(i, _)| set.contains(&(*i as usize)))).map(|s| s.len).sum::<u64>()
    }

    /// Sets of two or more items that reference the same clusters, largest first.
    pub fn sharing_groups(&self, at_least: u64) -> Vec<SharingGroup> {
        let mut groups: BTreeMap<Vec<usize>, (u64, u64)> = BTreeMap::new();
        for s in self.segments.iter().filter(|s| s.refs.len() >= 2) {
            let ids: Vec<usize> = s.refs.iter().map(|r| r.0 as usize).collect();
            let m: u32 = s.refs.iter().map(|r| r.1).sum();
            let e = groups.entry(ids).or_default();
            e.0 += s.len;
            if m >= s.rc {
                e.1 += s.len;
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
    /// The numbers come from a cached scan older than `fresh_sec`.
    pub stale: bool,
}

/// The confidence rules (proposal 3.3).
pub fn confidence(f: &ScanFacts) -> Confidence {
    if f.unmeasurable {
        Confidence::Unknown
    } else if f.block_cloning && f.files_without_refcount > 0 {
        Confidence::UpperBound
    } else if f.in_flux || f.anomalies || f.stale {
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

/// Normalise a path for comparisons: backslashes, no trailing separator, lower case.
pub fn norm(p: &str) -> String {
    let mut s = p.replace('/', "\\").to_lowercase();
    for prefix in [r"\\?\unc\", r"\\?\"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = if prefix.ends_with("unc\\") { format!(r"\\{}", rest) } else { rest.to_string() };
        }
    }
    while s.ends_with('\\') && !s.ends_with(":\\") && s.len() > 1 {
        s.pop();
    }
    s
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

    fn e(lcn: u64, len: u64, rc: u32) -> Ext {
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
}
