//! Where the RAM in use is (docs/UI.md section 3): Unreal, each app group, other apps,
//! Windows' file cache, Windows and drivers, compressed memory and what is left.
//!
//! The engine decides who owns each process (it knows Unreal and the groups); `breakdown`
//! adds up their private RAM, so nothing is counted twice, and fills in the kernel parts.
//! `RamBreakdown::parts` is the one list of parts every view (flyout, `memguard status`)
//! shows.

use std::borrow::Cow;
use std::collections::HashMap;

use serde::Serialize;

use crate::win::{Proc, SysInfo};

fn gb(bytes: u64) -> f64 {
    (bytes as f64 / (1024.0 * 1024.0 * 1024.0) * 10.0).round() / 10.0
}

/// One app outside Unreal and the groups, its processes added up by name.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AppRam {
    /// Executable name without `.exe`, as the first process of that name spells it.
    pub name: String,
    pub processes: usize,
    pub ram_gb: f64,
}

/// A group's share of the RAM in use.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GroupRam {
    pub name: String,
    pub ram_gb: f64,
}

/// Where the RAM in use is. The parts plus `unattributed_gb` add up to `in_use_gb`.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RamBreakdown {
    /// Total RAM minus available RAM.
    pub in_use_gb: f64,
    /// Everything on the protect list and the compile workers.
    pub unreal_gb: f64,
    pub unreal_editor_gb: f64,
    pub unreal_workers_gb: f64,
    /// In the order of `Status::groups`.
    pub groups: Vec<GroupRam>,
    /// Apps in no group, Unreal or Windows.
    pub other_apps_gb: f64,
    pub other_apps_count: usize,
    /// The biggest other apps, largest first (at most `TOP_APPS`). Left out of status.json:
    /// it names what someone has open, and agents don't need it.
    #[serde(skip)]
    pub top_apps: Vec<AppRam>,
    /// Windows' file cache: file data Windows keeps in RAM for itself (its working set, not
    /// the standby list, which counts as available).
    pub file_cache_gb: f64,
    /// Windows processes and services, kernel memory and drivers.
    pub windows_gb: f64,
    /// The Memory Compression process: memory of other apps, packed smaller.
    pub compressed_gb: f64,
    /// What is left: memory several processes share, memory drivers lock, pages waiting to be
    /// written. Windows doesn't say whose it is.
    pub unattributed_gb: f64,
}

/// One part of the RAM in use besides Unreal, which every view shows on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RamPart {
    OtherApps,
    /// Index into `RamBreakdown::groups`.
    Group(usize),
    FileCache,
    Windows,
    Squeezed,
    Shared,
}

impl RamBreakdown {
    /// Every part besides Unreal with its GB, in a fixed order (other apps, the groups, then
    /// Windows' parts). They add up to `in_use_gb - unreal_gb`.
    pub fn parts(&self) -> Vec<(RamPart, f64)> {
        let mut v = vec![(RamPart::OtherApps, self.other_apps_gb)];
        v.extend(self.groups.iter().enumerate().map(|(i, g)| (RamPart::Group(i), g.ram_gb)));
        v.extend([
            (RamPart::FileCache, self.file_cache_gb),
            (RamPart::Windows, self.windows_gb),
            (RamPart::Squeezed, self.compressed_gb),
            (RamPart::Shared, self.unattributed_gb),
        ]);
        v
    }

    /// The part's name in the UI's words; a group is called by its name.
    pub fn label(&self, part: RamPart) -> String {
        match part {
            RamPart::OtherApps => "Other apps".into(),
            RamPart::Group(i) => self.groups.get(i).map(|g| g.name.clone()).unwrap_or_default(),
            RamPart::FileCache => "Windows file cache".into(),
            RamPart::Windows => "Windows and drivers".into(),
            RamPart::Squeezed => "Squeezed by Windows".into(),
            RamPart::Shared => "Shared or not itemized".into(),
        }
    }
}

/// How many other apps the breakdown names.
pub const TOP_APPS: usize = 5;

/// Processes that are part of Windows (lowercase, without `.exe`). Sorted in ASCII ordinal
/// order for `binary_search`. They can't be closed, so they count as "Windows and drivers",
/// not "Other apps".
const WINDOWS_PROCESSES: &[&str] = &[
    "applicationframehost", "audiodg", "backgroundtaskhost", "compattelrunner", "conhost", "crossdeviceservice",
    "csrss", "ctfmon", "dashost", "dllhost", "dwm", "explorer", "fontdrvhost", "lockapp", "lsaiso", "lsass",
    "mpdefendercoreservice", "msdtc", "msmpeng", "musnotification", "nissrv", "phoneexperiencehost", "registry",
    "runtimebroker", "searchfilterhost", "searchhost", "searchindexer", "searchprotocolhost", "secure system",
    "securityhealthservice", "services", "sgrmbroker", "shellexperiencehost", "sihost", "smartscreen", "smss",
    "spoolsv", "startmenuexperiencehost", "svchost", "system", "systemsettings", "taskhostw", "textinputhost",
    "tiworker", "trustedinstaller", "unsecapp", "useroobebroker", "vmcompute", "vmms", "widgets", "widgetservice",
    "wininit", "winlogon", "wlanext", "wmiprvse", "wudfhost",
];

/// Is this (lowercase, no `.exe`) process name part of Windows?
pub fn is_windows_process(lower: &str) -> bool {
    WINDOWS_PROCESSES.binary_search(&lower).is_ok()
}

/// Who a process's RAM counts for, as the engine decided it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    UnrealEditor,
    UnrealWorker,
    /// On the protect list, but neither an editor nor a worker.
    Unreal,
    /// Index into the groups.
    Group(usize),
    /// Neither: Windows, compressed memory or another app, by its name.
    Nobody,
}

fn breakdown_inner(
    sys: &SysInfo,
    procs: &[Proc],
    lowers: Option<&[String]>,
    owners: &[Owner],
    group_names: &[String],
) -> RamBreakdown {
    let in_use = sys.ram_total.saturating_sub(sys.ram_avail);
    let (mut editor, mut workers, mut ue_other) = (0u64, 0u64, 0u64);
    let mut group_ram = vec![0u64; group_names.len()];
    let (mut windows, mut compressed) = (0u64, 0u64);
    // By lowercase name: (display name, processes, bytes).
    let mut apps: HashMap<Cow<'_, str>, (&str, usize, u64)> = HashMap::new();
    for (i, (p, owner)) in procs.iter().zip(owners).enumerate() {
        match owner {
            Owner::UnrealEditor => editor += p.private,
            Owner::UnrealWorker => workers += p.private,
            Owner::Unreal => ue_other += p.private,
            Owner::Group(g) => group_ram[*g] += p.private,
            Owner::Nobody => {
                let lower: Cow<'_, str> = match lowers.and_then(|l| l.get(i)) {
                    Some(s) => Cow::Borrowed(s.as_str()),
                    None => Cow::Owned(p.name.to_ascii_lowercase()),
                };
                if lower == "memory compression" {
                    compressed += p.private;
                } else if p.pid == 4 || is_windows_process(lower.as_ref()) {
                    windows += p.private;
                } else if p.pid != 0 {
                    if let Some(a) = apps.get_mut(lower.as_ref()) {
                        a.1 += 1;
                        a.2 += p.private;
                    } else {
                        apps.insert(lower, (p.name.as_str(), 1, p.private));
                    }
                }
            }
        }
    }
    windows += sys.nonpaged_pool + sys.resident_paged_pool + sys.resident_system_code + sys.resident_drivers;
    let file_cache = sys.resident_system_cache;
    let other: u64 = apps.values().map(|a| a.2).sum();
    let unreal = editor + workers + ue_other;
    let known = unreal + group_ram.iter().sum::<u64>() + other + windows + compressed + file_cache;

    let mut top: Vec<(&str, usize, u64)> = apps.into_values().collect();
    let other_apps_count = top.len();
    top.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(b.0)));
    top.truncate(TOP_APPS);
    RamBreakdown {
        in_use_gb: gb(in_use),
        unreal_gb: gb(unreal),
        unreal_editor_gb: gb(editor),
        unreal_workers_gb: gb(workers),
        groups: group_names.iter().zip(&group_ram).map(|(n, &b)| GroupRam { name: n.clone(), ram_gb: gb(b) }).collect(),
        other_apps_gb: gb(other),
        other_apps_count,
        top_apps: top.into_iter().map(|(name, processes, b)| AppRam { name: name.to_string(), processes, ram_gb: gb(b) }).collect(),
        file_cache_gb: gb(file_cache),
        windows_gb: gb(windows),
        compressed_gb: gb(compressed),
        unattributed_gb: gb(in_use.saturating_sub(known)),
    }
}

/// The breakdown of `sys`'s RAM in use, with `owners[i]` the owner of `procs[i]` and
/// `group_names` the groups `Owner::Group` points into.
pub fn breakdown(sys: &SysInfo, procs: &[Proc], owners: &[Owner], group_names: &[String]) -> RamBreakdown {
    breakdown_inner(sys, procs, None, owners, group_names)
}

/// Same as `breakdown`, reusing pre-lowercased process names (`lowers[i]` = `procs[i].name.to_ascii_lowercase()`).
pub fn breakdown_with_lowers(
    sys: &SysInfo,
    procs: &[Proc],
    lowers: &[String],
    owners: &[Owner],
    group_names: &[String],
) -> RamBreakdown {
    breakdown_inner(sys, procs, Some(lowers), owners, group_names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parts_cover_everything_but_unreal_in_a_fixed_order() {
        let app = |name: &str, processes: usize, ram_gb: f64| AppRam { name: name.into(), processes, ram_gb };
        let group = |name: &str, ram_gb: f64| GroupRam { name: name.into(), ram_gb };
        let br = RamBreakdown {
            in_use_gb: 82.6,
            unreal_gb: 34.0,
            unreal_editor_gb: 30.1,
            unreal_workers_gb: 2.4,
            groups: vec![group("Browsers", 3.6), group("Chat", 0.9), group("Agents", 5.8), group("Background", 0.6)],
            other_apps_gb: 6.2,
            other_apps_count: 61,
            top_apps: vec![app("rider64", 1, 2.8), app("vmmem", 1, 1.9), app("devenv", 2, 0.7), app("Discord", 6, 0.3), app("Spotify", 5, 0.2)],
            file_cache_gb: 22.4,
            windows_gb: 5.3,
            compressed_gb: 1.6,
            unattributed_gb: 2.2,
        };
        let parts = br.parts();
        let labels: Vec<String> = parts.iter().map(|(p, _)| br.label(*p)).collect();
        assert_eq!(
            labels,
            ["Other apps", "Browsers", "Chat", "Agents", "Background", "Windows file cache", "Windows and drivers", "Squeezed by Windows", "Shared or not itemized"]
        );
        let sum: f64 = parts.iter().map(|(_, g)| g).sum::<f64>() + br.unreal_gb;
        assert!((sum - br.in_use_gb).abs() < 0.05, "{}", sum);
        assert_eq!(br.label(RamPart::Group(99)), "");
    }

    #[test]
    fn windows_processes_is_sorted_and_unique() {
        assert_eq!(WINDOWS_PROCESSES.len(), 56);
        assert!(WINDOWS_PROCESSES.windows(2).all(|w| w[0] < w[1]));
        for &name in WINDOWS_PROCESSES {
            assert_eq!(name, name.to_ascii_lowercase());
            assert!(is_windows_process(name), "missing {}", name);
        }
        assert!(!is_windows_process("chrome"));
        assert!(!is_windows_process(""));
    }

    #[test]
    fn breakdown_with_lowers_matches_breakdown() {
        let gb = 1024 * 1024 * 1024u64;
        let sys = SysInfo {
            ram_total: 64 * gb,
            ram_avail: 32 * gb,
            nonpaged_pool: gb / 4,
            resident_paged_pool: gb / 4,
            resident_system_code: gb / 8,
            resident_drivers: gb / 8,
            resident_system_cache: gb,
            ..SysInfo::default()
        };
        let proc_item = |pid: u32, name: &str, private: u64| Proc {
            pid,
            ppid: 1,
            name: name.into(),
            ram: private,
            private,
            committed: private,
            created: 1,
            queryable: pid != 0,
        };
        let procs = vec![
            proc_item(0, "[System Process]", gb),
            proc_item(4, "System", gb / 2),
            proc_item(10, "Memory Compression", 2 * gb),
            proc_item(20, "Svchost", gb / 2),
            proc_item(30, "UnrealEditor", 8 * gb),
            proc_item(40, "ShaderCompileWorker", 4 * gb),
            proc_item(50, "chrome", 2 * gb),
            proc_item(60, "Blender", 3 * gb),
            proc_item(70, "blender", gb),
        ];
        let owners = vec![
            Owner::Nobody,
            Owner::Nobody,
            Owner::Nobody,
            Owner::Nobody,
            Owner::UnrealEditor,
            Owner::UnrealWorker,
            Owner::Group(0),
            Owner::Nobody,
            Owner::Nobody,
        ];
        let group_names = vec!["Browsers".to_string()];
        let lowers: Vec<String> = procs.iter().map(|p| p.name.to_ascii_lowercase()).collect();
        let a = breakdown(&sys, &procs, &owners, &group_names);
        let b = breakdown_with_lowers(&sys, &procs, &lowers, &owners, &group_names);
        assert_eq!(a, b);
        assert_eq!(b.other_apps_count, 1);
        assert_eq!(b.top_apps[0].name, "Blender");
        assert_eq!(b.top_apps[0].processes, 2);
    }
}
