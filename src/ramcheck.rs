//! RAM and BIOS check: is this PC's memory running slower than the kit was sold to run, are
//! the sticks in the right slots, does the BIOS look old, and which CPU cautions apply. Then,
//! in plain steps, what to do in the BIOS.
//!
//! It reports and explains only. Nothing here writes anything: no BIOS, registry or files
//! (the CLI writes the HTML page only where it's told to). No WMI, PowerShell or network.
//!
//! Facts come from the raw SMBIOS table (`smbios.rs`) and, read-only, the CPU's name and
//! microcode revision from `HKLM\HARDWARE\DESCRIPTION\System\CentralProcessor\0`.
//! Knowledge (part-number rules, voltages, board menus, CPU cautions) lives in
//! `ramcheck_data.toml` so it can grow without code changes.

use std::collections::HashMap;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use crate::smbios::{self, MemoryDevice, Smbios};

// ---------------------------------------------------------------- report types

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Running at least 200 MHz under the kit's rated speed: something to act on.
    BelowRated,
    /// Within 200 MHz of rated.
    AtRated,
    /// Standard-speed (JEDEC) memory or soldered memory: there's no fast profile to turn on.
    NoProfile,
    /// Sticks with different part numbers or sizes.
    MixedKit,
    /// The rated speed couldn't be worked out.
    Unknown,
    /// A virtual machine, or no memory devices reported.
    NotApplicable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Problem,
    Caution,
    Info,
}

#[derive(Debug, Clone, Serialize)]
pub struct RamReport {
    pub verdict: Verdict,
    pub headline: String,
    pub summary: String,
    /// Every slot the firmware lists, empty ones included.
    pub sticks: Vec<Stick>,
    pub slots_total: usize,
    pub rated: Option<Rated>,
    pub current_mhz: Option<u32>,
    pub current_voltage: Option<f32>,
    pub board: Board,
    pub cpu: Cpu,
    pub findings: Vec<RamFinding>,
    /// Empty unless there is something to do.
    pub steps: Vec<String>,
    pub fallback_mhz: Option<u32>,
    /// Changes when the situation does (part numbers, configured speed, BIOS version).
    pub fingerprint: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Stick {
    /// Slot name as the firmware reports it (device locator), e.g. "DIMMA2".
    pub slot: String,
    pub bank: String,
    pub populated: bool,
    pub size_gb: Option<f64>,
    /// "DDR4", "DDR5", "LPDDR5"...; empty when unknown.
    pub kind: String,
    /// "DIMM", "SO-DIMM", "soldered" or "".
    pub form: String,
    pub manufacturer: String,
    pub part_number: String,
    /// The module's maximum speed as the BIOS reports it.
    pub module_mhz: Option<u32>,
    /// What it runs at now.
    pub configured_mhz: Option<u32>,
    pub voltage: Option<f32>,
    pub rank: Option<u8>,
    /// Rated speed decoded from this stick's part number.
    pub rated_mhz: Option<u32>,
    /// Which part-number rule decoded it (the vendor's name).
    pub decoded_as: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Rated {
    pub speed_mhz: u32,
    pub cl: Option<u32>,
    /// e.g. "30-38-38" when the part number encodes more than CL.
    pub timings: Option<String>,
    pub voltage: Option<f32>,
    /// True when `voltage` is the usual value for kits like this, not this kit's specification.
    pub voltage_is_typical: bool,
    /// "XMP", "EXPO" or "JEDEC" (standard speed, no profile).
    pub profile_kind: String,
    /// "part_number" or "module_reported".
    pub confidence: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Board {
    pub maker: String,
    pub model: String,
    pub bios_vendor: String,
    pub bios_version: String,
    /// ISO date (yyyy-mm-dd) when it parses, else as the firmware wrote it.
    pub bios_date: String,
    pub bios_age_months: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Cpu {
    pub name: String,
    /// "Intel", "AMD" or "".
    pub vendor: String,
    pub microcode: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RamFinding {
    pub id: String,
    pub severity: Severity,
    pub title: String,
    pub detail: String,
}

// ---------------------------------------------------------------- data file

const DATA_TOML: &str = include_str!("ramcheck_data.toml");

#[derive(Deserialize)]
struct Data {
    part: Vec<PartRule>,
    jedec: JedecTables,
    voltage: Vec<VoltageRow>,
    jedec_ids: HashMap<String, String>,
    board: Vec<BoardRule>,
    cpu: Vec<CpuRule>,
}

#[derive(Deserialize)]
struct PartRule {
    vendor: String,
    patterns: Vec<String>,
    gen: Option<u8>,
    #[serde(default)]
    gen_from_speed: bool,
    speed_scale: Option<u32>,
    #[serde(default)]
    speed_map: HashMap<String, u32>,
    #[serde(default)]
    codes: HashMap<String, [u32; 2]>,
    timings: Option<String>,
    profile: String,
    #[serde(skip)]
    compiled: Vec<Vec<Tok>>,
}

#[derive(Deserialize)]
struct JedecTables {
    ddr4: JedecGen,
    ddr5: JedecGen,
}

#[derive(Deserialize)]
struct JedecGen {
    max_speed: u32,
    volts: f32,
    cl: HashMap<String, u32>,
    range: [u32; 2],
    ladder: Vec<u32>,
}

#[derive(Deserialize)]
struct VoltageRow {
    gen: u8,
    up_to: u32,
    volts: f32,
}

#[derive(Deserialize)]
struct BoardRule {
    #[allow(dead_code)]
    id: String,
    name: String,
    #[serde(rename = "match")]
    matches: Vec<String>,
    enter: String,
    path: String,
    #[serde(default)]
    model_paths: Vec<ModelPath>,
    profile_intel: String,
    profile_amd_ddr5: String,
    profile_amd_ddr4: String,
    support: Option<String>,
}

#[derive(Deserialize)]
struct ModelPath {
    contains: String,
    path: String,
}

#[derive(Deserialize)]
struct CpuRule {
    #[allow(dead_code)]
    id: String,
    pattern: String,
    models: Vec<[u32; 2]>,
    suffixes: Vec<String>,
    #[allow(dead_code)]
    platform: String,
    findings: Vec<CpuFindingRule>,
    #[serde(skip)]
    compiled: Vec<Tok>,
}

#[derive(Deserialize)]
struct CpuFindingRule {
    id: String,
    severity: String,
    microcode_below: Option<u32>,
    #[serde(default)]
    microcode_unknown: bool,
    title: String,
    detail: String,
}

fn data() -> &'static Data {
    static DATA: OnceLock<Data> = OnceLock::new();
    DATA.get_or_init(|| {
        // The file is part of the build and a unit test checks it, so a failure here is a
        // bug in the data, not something a user can cause.
        let mut d: Data = toml::from_str(DATA_TOML).expect("ramcheck_data.toml is valid");
        for r in &mut d.part {
            r.compiled = r.patterns.iter().map(|p| compile(p).expect("part pattern compiles")).collect();
        }
        for c in &mut d.cpu {
            c.compiled = compile(&c.pattern).expect("cpu pattern compiles");
        }
        d
    })
}

impl JedecTables {
    fn for_gen(&self, gen: u8) -> Option<&JedecGen> {
        match gen {
            4 => Some(&self.ddr4),
            5 => Some(&self.ddr5),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------- pattern matcher

#[derive(Debug, Clone)]
enum Class {
    Digit,
    Alpha,
    Alnum,
    Set(Vec<char>),
}

impl Class {
    fn has(&self, c: char) -> bool {
        match self {
            Class::Digit => c.is_ascii_digit(),
            Class::Alpha => c.is_ascii_alphabetic(),
            Class::Alnum => c.is_ascii_alphanumeric(),
            Class::Set(v) => v.contains(&c),
        }
    }
}

#[derive(Debug, Clone)]
enum Tok {
    Lit(char),
    Any,
    Cap { name: Option<String>, class: Class, min: usize, max: usize },
}

/// Compile a pattern (syntax in `ramcheck_data.toml`).
fn compile(p: &str) -> Result<Vec<Tok>, String> {
    let chars: Vec<char> = p.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' => {
                toks.push(Tok::Any);
                i += 1;
            }
            '{' => {
                let end = chars[i..].iter().position(|&c| c == '}').ok_or_else(|| format!("unclosed {{ in {}", p))? + i;
                let body: String = chars[i + 1..end].iter().collect();
                let (name, spec) = body.split_once(':').ok_or_else(|| format!("no : in {{{}}}", body))?;
                let name = if name.is_empty() { None } else { Some(name.to_string()) };
                let (class, rest) = if let Some(s) = spec.strip_prefix('[') {
                    let close = s.find(']').ok_or_else(|| format!("unclosed [ in {}", p))?;
                    let set = expand_set(&s[..close]);
                    (Class::Set(set), &s[close + 1..])
                } else {
                    let mut it = spec.chars();
                    let class = match it.next() {
                        Some('d') => Class::Digit,
                        Some('a') => Class::Alpha,
                        Some('w') => Class::Alnum,
                        _ => return Err(format!("unknown class in {{{}}}", body)),
                    };
                    (class, &spec[1..])
                };
                let default = if matches!(class, Class::Set(_)) { (1, 1) } else { (1, usize::MAX) };
                let (min, max) = parse_count(rest, default).ok_or_else(|| format!("bad count in {{{}}}", body))?;
                toks.push(Tok::Cap { name, class, min, max });
                i = end + 1;
            }
            c => {
                toks.push(Tok::Lit(c.to_ascii_uppercase()));
                i += 1;
            }
        }
    }
    Ok(toks)
}

fn expand_set(s: &str) -> Vec<char> {
    let c: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        if i + 2 < c.len() && c[i + 1] == '-' {
            let (a, b) = (c[i] as u32, c[i + 2] as u32);
            out.extend((a..=b).filter_map(char::from_u32).map(|x| x.to_ascii_uppercase()));
            i += 3;
        } else {
            out.push(c[i].to_ascii_uppercase());
            i += 1;
        }
    }
    out
}

fn parse_count(s: &str, default: (usize, usize)) -> Option<(usize, usize)> {
    if s.is_empty() {
        return Some(default);
    }
    match s.split_once('-') {
        Some((a, b)) => {
            let (a, b) = (a.parse().ok()?, b.parse().ok()?);
            (a <= b).then_some((a, b))
        }
        None => s.parse().ok().map(|n| (n, n)),
    }
}

/// Match the whole of `s` (already upper-cased). Returns the named captures.
fn match_pattern(toks: &[Tok], s: &str) -> Option<HashMap<String, String>> {
    let chars: Vec<char> = s.chars().collect();
    let mut caps = Vec::new();
    if match_from(toks, &chars, 0, &mut caps) {
        Some(caps.into_iter().collect())
    } else {
        None
    }
}

fn match_from(toks: &[Tok], s: &[char], pos: usize, caps: &mut Vec<(String, String)>) -> bool {
    let Some((tok, rest)) = toks.split_first() else {
        return pos == s.len();
    };
    match tok {
        Tok::Lit(c) => s.get(pos) == Some(c) && match_from(rest, s, pos + 1, caps),
        Tok::Any => (pos..=s.len()).any(|p| match_from(rest, s, p, caps)),
        Tok::Cap { name, class, min, max } => {
            let avail = s[pos..].iter().take_while(|&&c| class.has(c)).count().min(*max);
            if avail < *min {
                return false;
            }
            for n in (*min..=avail).rev() {
                let mark = caps.len();
                if let Some(name) = name {
                    caps.push((name.clone(), s[pos..pos + n].iter().collect()));
                }
                if match_from(rest, s, pos + n, caps) {
                    return true;
                }
                caps.truncate(mark);
            }
            false
        }
    }
}

// ---------------------------------------------------------------- part numbers

/// What a part number says about the kit.
#[derive(Debug, Clone, PartialEq)]
struct Decoded {
    vendor: String,
    gen: Option<u8>,
    speed_mhz: u32,
    cl: Option<u32>,
    timings: Option<String>,
    jedec: bool,
}

fn normalize_part(part: &str) -> String {
    part.trim().to_ascii_uppercase()
}

/// Decode a part number with the first matching rule. `None` when no rule matches or the
/// result doesn't make sense (speed out of range for the generation).
fn decode_part(part: &str) -> Option<Decoded> {
    let p = normalize_part(part);
    if p.is_empty() {
        return None;
    }
    let d = data();
    for rule in &d.part {
        for toks in &rule.compiled {
            let Some(caps) = match_pattern(toks, &p) else { continue };
            if let Some(dec) = decode_with(rule, &caps, d) {
                return Some(dec);
            }
        }
    }
    None
}

fn decode_with(rule: &PartRule, caps: &HashMap<String, String>, d: &Data) -> Option<Decoded> {
    let num = |k: &str| caps.get(k).and_then(|v| v.parse::<u32>().ok());
    let (mut speed, mut cl) = (None, num("cl"));
    if let Some(raw) = caps.get("speed") {
        speed = match rule.speed_map.get(raw) {
            Some(&v) => Some(v),
            None => raw.parse::<u32>().ok().map(|v| v * rule.speed_scale.unwrap_or(1)),
        };
    }
    if let Some(code) = caps.get("code") {
        let [s, c] = *rule.codes.get(code)?;
        speed = Some(s);
        if c > 0 {
            cl = Some(c);
        }
    }
    let speed = speed?;
    let mut gen = num("gen").map(|g| g as u8).or(rule.gen);
    if gen.is_none() && rule.gen_from_speed {
        gen = match speed {
            4400.. => Some(5),
            2133.. => Some(4),
            _ => None,
        };
    }
    if let Some(j) = gen.and_then(|g| d.jedec.for_gen(g)) {
        if speed < j.range[0] || speed > j.range[1] {
            return None;
        }
    } else if !(1600..=12000).contains(&speed) {
        return None;
    }
    let timings = rule.timings.as_ref().and_then(|t| {
        let cl = caps.get("cl")?;
        let t2 = caps.get("t2")?;
        Some(t.replace("{cl}", cl).replace("{t2}", t2))
    });
    let jedec = match rule.profile.as_str() {
        "jedec" => true,
        "xmp" => false,
        _ => gen.and_then(|g| d.jedec.for_gen(g)).is_some_and(|j| {
            speed <= j.max_speed
                && match j.cl.get(&speed.to_string()) {
                    None => false, // not a standard speed: must be a profile
                    Some(&std_cl) => cl.is_none_or(|c| c >= std_cl),
                }
        }),
    };
    Some(Decoded { vendor: rule.vendor.clone(), gen, speed_mhz: speed, cl, timings, jedec })
}

// ---------------------------------------------------------------- helpers

const PLACEHOLDERS: &[&str] = &[
    "TO BE FILLED BY O.E.M.",
    "DEFAULT STRING",
    "NOT SPECIFIED",
    "NOT AVAILABLE",
    "UNKNOWN",
    "NONE",
    "N/A",
    "NA",
    "UNDEFINED",
    "SYSTEM MANUFACTURER",
    "SYSTEM PRODUCT NAME",
    "BASE BOARD MANUFACTURER",
    "BASE BOARD PRODUCT NAME",
    "OEM",
    "O.E.M.",
    "0",
    "00000000",
    "0000",
    "MODULEPARTNUMBER00",
    "MODULE PART NUMBER",
];

/// Firmware text with placeholders and padding removed.
fn clean(s: &str) -> String {
    let t: String = s.chars().filter(|c| !c.is_control()).collect::<String>().trim().to_string();
    let up = t.to_ascii_uppercase();
    if PLACEHOLDERS.contains(&up.as_str()) || up.chars().all(|c| c == '0' || c == ' ' || c == 'F') && !up.is_empty() && up.len() > 3 {
        String::new()
    } else {
        t
    }
}

fn mem_kind(t: u8) -> &'static str {
    match t {
        smbios::MEM_DDR3 => "DDR3",
        smbios::MEM_DDR4 => "DDR4",
        smbios::MEM_LPDDR3 => "LPDDR3",
        smbios::MEM_LPDDR4 => "LPDDR4",
        smbios::MEM_LPDDR4X => "LPDDR4X",
        smbios::MEM_DDR5 => "DDR5",
        smbios::MEM_LPDDR5 => "LPDDR5",
        _ => "",
    }
}

fn mem_gen(t: u8) -> Option<u8> {
    match t {
        smbios::MEM_DDR4 | smbios::MEM_LPDDR4 | smbios::MEM_LPDDR4X => Some(4),
        smbios::MEM_DDR5 | smbios::MEM_LPDDR5 => Some(5),
        _ => None,
    }
}

fn is_lpddr(t: u8) -> bool {
    matches!(t, smbios::MEM_LPDDR3 | smbios::MEM_LPDDR4 | smbios::MEM_LPDDR4X | smbios::MEM_LPDDR5)
}

fn is_soldered(d: &MemoryDevice) -> bool {
    matches!(d.form_factor, smbios::FORM_CHIP | smbios::FORM_ROW_OF_CHIPS | smbios::FORM_DIE) || is_lpddr(d.memory_type)
}

fn form_name(d: &MemoryDevice) -> &'static str {
    if is_soldered(d) {
        "soldered"
    } else {
        match d.form_factor {
            smbios::FORM_DIMM => "DIMM",
            smbios::FORM_SODIMM => "SO-DIMM",
            _ => "",
        }
    }
}

/// Speeds in MT/s. Some firmware reports the clock (half the data rate) instead: a DDR4
/// value under 1600 or a DDR5 value under 3200 can only be that, so it's doubled.
fn norm_speed(v: u32, gen: Option<u8>) -> Option<u32> {
    match (v, gen) {
        (0 | 0xFFFF, _) => None,
        (v, Some(4)) if v < 1600 => Some(v * 2),
        (v, Some(5)) if v < 3200 => Some(v * 2),
        (v, _) => Some(v),
    }
}

fn maker_name(raw: &str) -> String {
    let c = clean(raw);
    let up = c.to_ascii_uppercase().replace(' ', "");
    match data().jedec_ids.get(&up) {
        Some(n) => n.clone(),
        None => c,
    }
}

/// "03/11/2026" (or "03/11/26", or "2026-03-11") -> (2026, 3, 11).
fn parse_date(s: &str) -> Option<(i32, u32, u32)> {
    let s = s.trim();
    let (y, m, d) = if let Some((a, rest)) = s.split_once('/') {
        let (b, c) = rest.split_once('/')?;
        let mut y: i32 = c.trim().get(..4.min(c.trim().len()))?.parse().ok()?;
        if c.trim().len() == 2 {
            y += if y < 80 { 2000 } else { 1900 };
        }
        (y, a.trim().parse().ok()?, b.trim().parse().ok()?)
    } else {
        let mut it = s.split('-');
        (it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.get(..2)?.parse().ok()?)
    };
    ((1990..=2100).contains(&y) && (1..=12).contains(&m) && (1..=31).contains(&d)).then_some((y, m, d))
}

fn months_between(from: (i32, u32, u32), to: (i32, u32, u32)) -> Option<u32> {
    let mut m = (to.0 - from.0) * 12 + to.1 as i32 - from.1 as i32;
    if to.2 < from.2 {
        m -= 1;
    }
    (m >= 0).then_some(m as u32)
}

fn fnv(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{:016x}", h)
}

/// Upper-cased CPU name with (R), (TM), "CPU", "PRO" and extra spaces removed, plus a
/// trailing space so patterns can end on a word.
fn normalize_cpu(name: &str) -> String {
    let up = name.to_ascii_uppercase().replace("(R)", " ").replace("(TM)", " ").replace("®", " ").replace("™", " ");
    let words: Vec<&str> = up.split_whitespace().filter(|w| *w != "CPU" && *w != "PRO").collect();
    format!("{} ", words.join(" "))
}

fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

/// "MPG Z890 CARBON WIFI (MS-7E17)" -> "MPG Z890 CARBON WIFI".
fn model_without_code(model: &str) -> String {
    match model.find('(') {
        Some(i) if i > 0 => model[..i].trim().to_string(),
        _ => model.trim().to_string(),
    }
}

fn find_board(maker_raw: &str) -> Option<&'static BoardRule> {
    let up = maker_raw.to_ascii_uppercase();
    data().board.iter().find(|b| b.matches.iter().any(|m| up.contains(m.as_str())))
}

fn looks_virtual(s: &Smbios) -> bool {
    const VM: &[&str] = &["VMWARE", "VIRTUALBOX", "INNOTEK", "QEMU", "PARALLELS", "BOCHS", "SEABIOS", "VIRTUAL MACHINE", "KVM", "XEN"];
    let mut hay = String::new();
    if let Some(b) = &s.bios {
        hay.push_str(&b.vendor);
        hay.push(' ');
        hay.push_str(&b.version);
        hay.push(' ');
    }
    if let Some(b) = &s.board {
        hay.push_str(&b.manufacturer);
        hay.push(' ');
        hay.push_str(&b.product);
        hay.push(' ');
    }
    for d in &s.devices {
        hay.push_str(&d.manufacturer);
        hay.push(' ');
    }
    let hay = hay.to_ascii_uppercase();
    hay.split(|c: char| !c.is_ascii_alphanumeric() && c != ' ').any(|part| VM.iter().any(|v| part.contains(v)))
}

/// "DIMM_A2", "DIMMA2", "A2", "DDR5_A2" -> ('A', 2). Anything else -> None.
fn slot_id(name: &str) -> Option<(char, u8)> {
    let mut s = name.trim().to_ascii_uppercase();
    for p in ["DIMM", "DDR5", "DDR4"] {
        if let Some(r) = s.strip_prefix(p) {
            s = r.to_string();
        }
    }
    let s: String = s.chars().filter(|c| !matches!(c, '_' | '-' | ' ')).collect();
    let c: Vec<char> = s.chars().collect();
    match c.as_slice() {
        [ch @ ('A' | 'B'), n @ ('1' | '2')] => Some((*ch, *n as u8 - b'0')),
        _ => None,
    }
}

fn mhz(v: u32) -> String {
    format!("{} MHz", v)
}

fn volts(v: f32) -> String {
    format!("{:.2} V", v)
}

fn gb(v: f64) -> String {
    if (v - v.round()).abs() < 0.05 {
        format!("{} GB", v.round() as u64)
    } else {
        format!("{:.1} GB", v)
    }
}

// ---------------------------------------------------------------- the check

/// Read this PC and report. Never fails, never writes.
pub fn check() -> RamReport {
    let table = smbios::read();
    let (name, microcode) = read_cpu_registry();
    check_from(&table, &name, microcode, today())
}

/// The check over given facts: pure, for tests. `cpu_name` may be empty (the SMBIOS
/// processor name is used then); `today` is (year, month, day).
pub fn check_from(sm: &Smbios, cpu_name: &str, microcode: Option<u32>, today: (i32, u32, u32)) -> RamReport {
    let d = data();

    // System memory slots only (skip arrays for video memory, flash and the like).
    let system_arrays: Vec<u16> = sm.arrays.iter().filter(|a| a.usage == 0x03).map(|a| a.handle).collect();
    let other_arrays: Vec<u16> = sm.arrays.iter().filter(|a| a.usage != 0x03 && a.usage != 0).map(|a| a.handle).collect();
    let devices: Vec<&MemoryDevice> = sm
        .devices
        .iter()
        .filter(|dev| system_arrays.contains(&dev.array_handle) || !other_arrays.contains(&dev.array_handle))
        .collect();
    let slots_total = if devices.is_empty() { sm.arrays.iter().filter(|a| a.usage == 0x03).map(|a| a.slots as usize).sum() } else { devices.len() };

    // Board and BIOS.
    let raw_maker = sm.board.as_ref().map(|b| clean(&b.manufacturer)).unwrap_or_default();
    let board_rule = find_board(&raw_maker);
    let bios = sm.bios.clone().unwrap_or_default();
    let bios_parsed = parse_date(&bios.release_date);
    let board = Board {
        maker: board_rule.map(|b| b.name.clone()).unwrap_or_else(|| raw_maker.clone()),
        model: sm.board.as_ref().map(|b| clean(&b.product)).unwrap_or_default(),
        bios_vendor: clean(&bios.vendor),
        bios_version: clean(&bios.version),
        bios_date: bios_parsed.map(|(y, m, dd)| format!("{:04}-{:02}-{:02}", y, m, dd)).unwrap_or_else(|| clean(&bios.release_date)),
        bios_age_months: bios_parsed.and_then(|p| months_between(p, today)),
    };

    // CPU.
    let name = if cpu_name.trim().is_empty() {
        sm.processors.iter().map(|p| clean(&p.version)).find(|v| !v.is_empty()).unwrap_or_default()
    } else {
        cpu_name.trim().to_string()
    };
    let name_up = name.to_ascii_uppercase();
    let proc_maker = sm.processors.first().map(|p| p.manufacturer.to_ascii_uppercase()).unwrap_or_default();
    let vendor = if name_up.contains("INTEL") || proc_maker.contains("INTEL") {
        "Intel"
    } else if name_up.contains("AMD") || name_up.contains("RYZEN") || proc_maker.contains("AMD") || proc_maker.contains("ADVANCED MICRO") {
        "AMD"
    } else {
        ""
    };
    let cpu = Cpu { name: name.clone(), vendor: vendor.to_string(), microcode };

    // Sticks.
    let mut sticks = Vec::new();
    let mut decodes: Vec<Option<Decoded>> = Vec::new();
    for dev in &devices {
        let gen = mem_gen(dev.memory_type);
        let populated = dev.populated();
        let part = clean(&dev.part_number);
        let decoded = if populated {
            decode_part(&part).filter(|dc| match (dc.gen, gen) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            })
        } else {
            None
        };
        let v = dev.configured_voltage_mv;
        sticks.push(Stick {
            slot: clean(&dev.device_locator),
            bank: clean(&dev.bank_locator),
            populated,
            size_gb: dev.size_mb.filter(|&m| m > 0).map(|m| m as f64 / 1024.0),
            kind: if populated { mem_kind(dev.memory_type).to_string() } else { String::new() },
            form: if populated { form_name(dev).to_string() } else { String::new() },
            manufacturer: if populated { maker_name(&dev.manufacturer) } else { String::new() },
            part_number: part,
            module_mhz: if populated { norm_speed(dev.speed, gen) } else { None },
            configured_mhz: if populated { norm_speed(dev.configured_speed, gen) } else { None },
            voltage: (populated && v > 0 && v < 5000).then(|| v as f32 / 1000.0),
            rank: (populated && dev.rank > 0).then_some(dev.rank),
            rated_mhz: decoded.as_ref().map(|x| x.speed_mhz),
            decoded_as: decoded.as_ref().map(|x| x.vendor.clone()),
        });
        decodes.push(decoded);
    }
    let pop: Vec<usize> = (0..sticks.len()).filter(|&i| sticks[i].populated).collect();
    let pop_devs: Vec<&MemoryDevice> = pop.iter().map(|&i| devices[i]).collect();
    let gen = pop_devs.iter().find_map(|dv| mem_gen(dv.memory_type));
    let current_mhz = pop.iter().filter_map(|&i| sticks[i].configured_mhz).min();
    let current_voltage = pop.iter().find_map(|&i| sticks[i].voltage);
    let soldered = !pop_devs.is_empty() && pop_devs.iter().all(|dv| is_soldered(dv));
    let sodimm = !pop_devs.is_empty() && pop_devs.iter().all(|dv| dv.form_factor == smbios::FORM_SODIMM);

    let mut parts: Vec<String> = pop.iter().map(|&i| sticks[i].part_number.to_ascii_uppercase()).filter(|p| !p.is_empty()).collect();
    parts.sort();
    parts.dedup();
    let mut sizes: Vec<u64> = pop_devs.iter().filter_map(|dv| dv.size_mb).collect();
    sizes.sort();
    sizes.dedup();
    let mixed = !soldered && (parts.len() > 1 || sizes.len() > 1);

    // Rated speed: the part number (lowest when sticks differ), else what the modules report.
    let all_decoded = !pop.is_empty() && pop.iter().all(|&i| decodes[i].is_some());
    let mut rated = None;
    if all_decoded {
        let best = pop.iter().filter_map(|&i| decodes[i].as_ref()).min_by_key(|x| x.speed_mhz).cloned();
        if let Some(dc) = best {
            let g = dc.gen.or(gen);
            rated = Some(make_rated(d, dc.speed_mhz, dc.cl, dc.timings.clone(), g, dc.jedec, vendor, "part_number"));
        }
    } else if !mixed {
        let module = pop.iter().filter_map(|&i| sticks[i].module_mhz).min();
        if let (Some(m), Some(c)) = (module, current_mhz) {
            if m > c {
                // A module that reports more than the standard maximum must have a profile;
                // one that reports a standard speed is most likely held back by the CPU or board.
                let jedec = gen.and_then(|g| d.jedec.for_gen(g)).is_some_and(|j| m <= j.max_speed);
                rated = Some(make_rated(d, m, None, None, gen, jedec, vendor, "module_reported"));
            }
        }
    }

    let is_vm = looks_virtual(sm);
    let verdict = if pop.is_empty() || (is_vm && pop.iter().all(|&i| decodes[i].is_none())) {
        Verdict::NotApplicable
    } else if soldered {
        Verdict::NoProfile
    } else if mixed {
        Verdict::MixedKit
    } else {
        match (&rated, current_mhz) {
            (None, _) | (_, None) => Verdict::Unknown,
            (Some(r), _) if r.profile_kind == "JEDEC" => Verdict::NoProfile,
            (Some(r), Some(c)) if c + 200 <= r.speed_mhz => Verdict::BelowRated,
            _ => Verdict::AtRated,
        }
    };

    let fallback_mhz = rated.as_ref().filter(|r| r.profile_kind != "JEDEC").and_then(|r| {
        let g = gen?;
        d.jedec.for_gen(g)?.ladder.iter().rev().find(|&&s| s < r.speed_mhz).copied()
    });

    let below = matches!((&rated, current_mhz), (Some(r), Some(c)) if c + 200 <= r.speed_mhz && r.profile_kind != "JEDEC");
    let wants_steps = verdict == Verdict::BelowRated || (verdict == Verdict::MixedKit && all_decoded && below);

    // Findings.
    let mut findings = Vec::new();
    let part_shown = parts.first().cloned().unwrap_or_default();
    let decoded_vendor = pop.iter().find_map(|&i| sticks[i].decoded_as.clone());
    match verdict {
        Verdict::BelowRated => {
            let r = rated.as_ref().expect("below_rated has a rating");
            let c = current_mhz.unwrap_or(0);
            findings.push(RamFinding {
                id: "ram:speed".into(),
                severity: Severity::Problem,
                title: "Your RAM is running below its rated speed".into(),
                detail: format!(
                    "{} now; {} {}. Turning on {} in the BIOS lets it run at its rated speed, which speeds up compiles, shaders and cooks.",
                    mhz(c),
                    rated_source(r, &part_shown, decoded_vendor.as_deref()),
                    mhz(r.speed_mhz),
                    r.profile_kind
                ),
            });
        }
        Verdict::MixedKit => {
            let list: Vec<String> = pop
                .iter()
                .map(|&i| {
                    let s = &sticks[i];
                    format!("{} {}", s.size_gb.map(gb).unwrap_or_else(|| "?".into()), if s.part_number.is_empty() { "(no part number)" } else { &s.part_number })
                })
                .collect();
            let target = match (&rated, all_decoded) {
                (Some(r), true) => format!(" The slowest stick is rated for {}, so that is the most to aim for.", mhz(r.speed_mhz)),
                _ => String::new(),
            };
            findings.push(RamFinding {
                id: "ram:mixed".into(),
                severity: Severity::Caution,
                title: "Your memory sticks don't match".into(),
                detail: format!(
                    "Installed: {}. Mixed sticks often can't run a fast profile reliably; the lowest common speed applies.{}",
                    list.join(", "),
                    target
                ),
            });
        }
        Verdict::Unknown => {
            let detail = if rated.is_some() {
                "Couldn't read the speed the memory runs at now.".to_string()
            } else if part_shown.is_empty() {
                "The firmware doesn't report a part number for the memory, so its rated speed is unknown. The sticks' label shows the rated speed (for example DDR5-6000).".to_string()
            } else {
                format!(
                    "Couldn't tell what speed the kit is rated for from its part number, {}. Look it up on the memory maker's site{}: if the rated speed is higher than {}, a fast profile in the BIOS is worth turning on.",
                    part_shown,
                    pop.first().map(|&i| &sticks[i].manufacturer).filter(|m| !m.is_empty()).map(|m| format!(" ({})", m)).unwrap_or_default(),
                    current_mhz.map(mhz).unwrap_or_else(|| "its current speed".into())
                )
            };
            findings.push(RamFinding { id: "ram:speed".into(), severity: Severity::Info, title: "Couldn't identify the memory kit".into(), detail });
        }
        Verdict::NoProfile => {
            if let (Some(r), Some(c)) = (&rated, current_mhz) {
                if c + 200 <= r.speed_mhz {
                    findings.push(RamFinding {
                        id: "ram:speed".into(),
                        severity: Severity::Info,
                        title: "Memory runs below its standard speed".into(),
                        detail: format!(
                            "The modules are standard {} memory but run at {}. That is usually the most this CPU or board supports for this memory, not a setting to change.",
                            mhz(r.speed_mhz),
                            mhz(c)
                        ),
                    });
                }
            }
        }
        _ => {}
    }

    // Slots.
    if verdict != Verdict::NotApplicable && !soldered {
        if let Some(f) = slot_finding(&sticks, slots_total, sodimm) {
            findings.push(f);
        }
    }

    // BIOS age.
    if let Some(age) = board.bios_age_months.filter(|&a| a > 18 && verdict != Verdict::NotApplicable) {
        let link = support_url(board_rule, &board, vendor);
        findings.push(RamFinding {
            id: "ram:bios".into(),
            severity: Severity::Caution,
            title: "The BIOS may be out of date".into(),
            detail: format!(
                "This BIOS ({}) is from {}, {} months ago. Newer BIOS versions often improve memory compatibility. Check for an update on the board maker's support page: {}",
                if board.bios_version.is_empty() { "version unknown".to_string() } else { board.bios_version.clone() },
                board.bios_date,
                age,
                link
            ),
        });
    }

    // CPU cautions.
    let cpu_rule = cpu_rule_for(&name);
    let mut microcode_problem = false;
    if verdict != Verdict::NotApplicable {
        if let Some(rule) = cpu_rule {
            for fr in &rule.findings {
                let applies = match (fr.microcode_below, fr.microcode_unknown) {
                    (Some(min), _) => microcode.is_some_and(|m| m < min),
                    (None, true) => microcode.is_none(),
                    _ => true,
                };
                if !applies {
                    continue;
                }
                let severity = match fr.severity.as_str() {
                    "problem" => Severity::Problem,
                    "caution" => Severity::Caution,
                    _ => Severity::Info,
                };
                if fr.id == "ram:microcode" && severity == Severity::Problem {
                    microcode_problem = true;
                }
                findings.push(RamFinding {
                    id: fr.id.clone(),
                    severity,
                    title: fr.title.clone(),
                    detail: fr.detail.replace("{microcode}", &microcode.map(|m| format!("0x{:X}", m)).unwrap_or_else(|| "unknown".into())),
                });
            }
        }
    }

    // Steps.
    let steps = if wants_steps {
        let r = rated.as_ref().expect("steps need a rating");
        build_steps(r, board_rule, &board, vendor, gen, fallback_mhz, microcode_problem, &findings, sodimm)
    } else {
        Vec::new()
    };

    let (headline, summary) = words(verdict, rated.as_ref(), current_mhz, &findings, soldered, is_vm && pop.is_empty(), all_decoded);

    let mut fp = String::new();
    for s in &sticks {
        fp.push_str(&s.part_number);
        fp.push('|');
    }
    fp.push_str(&format!("{:?}|{}", current_mhz, board.bios_version));

    RamReport {
        verdict,
        headline,
        summary,
        sticks,
        slots_total,
        rated,
        current_mhz,
        current_voltage,
        board,
        cpu,
        findings,
        steps,
        fallback_mhz,
        fingerprint: fnv(&fp),
    }
}

#[allow(clippy::too_many_arguments)]
fn make_rated(d: &Data, speed: u32, cl: Option<u32>, timings: Option<String>, gen: Option<u8>, jedec: bool, cpu_vendor: &str, confidence: &str) -> Rated {
    let (voltage, typical) = if jedec {
        (gen.and_then(|g| d.jedec.for_gen(g)).map(|j| j.volts), false)
    } else {
        (gen.and_then(|g| d.voltage.iter().find(|v| v.gen == g && speed <= v.up_to)).map(|v| v.volts), true)
    };
    let profile_kind = if jedec {
        "JEDEC"
    } else if cpu_vendor == "AMD" && gen == Some(5) {
        "EXPO"
    } else {
        "XMP"
    };
    Rated {
        speed_mhz: speed,
        cl,
        timings,
        voltage,
        voltage_is_typical: typical && voltage.is_some(),
        profile_kind: profile_kind.into(),
        confidence: confidence.into(),
    }
}

fn rated_source(r: &Rated, part: &str, vendor: Option<&str>) -> String {
    if r.confidence == "part_number" {
        match vendor {
            Some(v) => format!("the {} part number {} says the kit is rated for", v, part),
            None => format!("the part number {} says the kit is rated for", part),
        }
    } else {
        "the modules report a rated speed of".to_string()
    }
}

fn cpu_rule_for(name: &str) -> Option<&'static CpuRule> {
    let n = normalize_cpu(name);
    data().cpu.iter().find(|r| {
        let Some(caps) = match_pattern(&r.compiled, &n) else { return false };
        let model: u32 = caps.get("model").and_then(|m| m.parse().ok()).unwrap_or(0);
        let suffix = caps.get("suffix").map(String::as_str).unwrap_or("");
        r.models.iter().any(|[a, b]| (*a..=*b).contains(&model)) && r.suffixes.iter().any(|s| s == suffix)
    })
}

fn slot_finding(sticks: &[Stick], slots_total: usize, sodimm: bool) -> Option<RamFinding> {
    let pop: Vec<&Stick> = sticks.iter().filter(|s| s.populated).collect();
    if pop.len() == 1 && slots_total >= 2 && !sodimm && pop[0].form == "DIMM" {
        return Some(RamFinding {
            id: "ram:slots".into(),
            severity: Severity::Info,
            title: "One memory stick".into(),
            detail: "There's one stick in a board with more slots. A second matching stick would run in dual channel and double memory bandwidth.".into(),
        });
    }
    if slots_total != 4 || sticks.len() != 4 || pop.len() != 2 {
        return None;
    }
    // Only when every slot name reads cleanly as A1, A2, B1, B2.
    let mut all: Vec<(char, u8)> = sticks.iter().filter_map(|s| slot_id(&s.slot)).collect();
    all.sort();
    if all != [('A', 1), ('A', 2), ('B', 1), ('B', 2)] {
        return None;
    }
    let mut used: Vec<(char, u8)> = pop.iter().filter_map(|s| slot_id(&s.slot)).collect();
    used.sort();
    let names: Vec<&str> = pop.iter().map(|s| s.slot.as_str()).collect();
    let a2 = sticks.iter().find(|s| slot_id(&s.slot) == Some(('A', 2))).map(|s| s.slot.clone()).unwrap_or_default();
    let b2 = sticks.iter().find(|s| slot_id(&s.slot) == Some(('B', 2))).map(|s| s.slot.clone()).unwrap_or_default();
    match used.as_slice() {
        [('A', 2), ('B', 2)] => None,
        [(x, _), (y, _)] if x == y => Some(RamFinding {
            id: "ram:slots".into(),
            severity: Severity::Problem,
            title: "Both sticks are on the same memory channel".into(),
            detail: format!(
                "The sticks are in {} and {}, which share one channel and halve memory bandwidth. Move them to {} and {} (the second and fourth slots from the CPU), with the PC off and unplugged.",
                names[0], names[1], a2, b2
            ),
        }),
        _ => Some(RamFinding {
            id: "ram:slots".into(),
            severity: Severity::Caution,
            title: "The sticks aren't in the recommended slots".into(),
            detail: format!(
                "The sticks are in {} and {}. With two sticks, boards are tuned for {} and {} (the second and fourth slots from the CPU), which run fast profiles more reliably. Move them with the PC off and unplugged.",
                names[0], names[1], a2, b2
            ),
        }),
    }
}

fn support_url(rule: Option<&BoardRule>, board: &Board, cpu_vendor: &str) -> String {
    let model = model_without_code(&board.model);
    if let (Some(t), false) = (rule.and_then(|r| r.support.as_ref()), model.is_empty()) {
        let slug: String = model.split_whitespace().collect::<Vec<_>>().join("-");
        return t
            .replace("{model_slug}", &url_encode(&slug))
            .replace("{model}", &url_encode(&model))
            .replace("{cpu_vendor}", if cpu_vendor.is_empty() { "Intel" } else { cpu_vendor });
    }
    let q = format!("{} {} BIOS", board.maker, model);
    format!("https://www.bing.com/search?q={}", url_encode(q.trim()))
}

#[allow(clippy::too_many_arguments)]
fn build_steps(
    r: &Rated,
    rule: Option<&BoardRule>,
    board: &Board,
    cpu_vendor: &str,
    gen: Option<u8>,
    fallback: Option<u32>,
    microcode_problem: bool,
    findings: &[RamFinding],
    laptop: bool,
) -> Vec<String> {
    let mut steps = Vec::new();
    let mut first = "Before you start: save your work and close your apps. A failed attempt only needs a BIOS reset; it can't damage the memory.".to_string();
    if microcode_problem {
        first.push_str(" Update the BIOS first (see the microcode note below): do that before changing any memory setting.");
    }
    steps.push(first);

    let profile = match rule {
        Some(b) if cpu_vendor == "AMD" && gen == Some(4) => b.profile_amd_ddr4.clone(),
        Some(b) if cpu_vendor == "AMD" => b.profile_amd_ddr5.clone(),
        Some(b) => b.profile_intel.clone(),
        None => r.profile_kind.clone(),
    };
    let mut spec = format!("DDR{}-{}", gen.map(|g| g.to_string()).unwrap_or_default(), r.speed_mhz);
    if gen.is_none() {
        spec = mhz(r.speed_mhz);
    }
    if let Some(cl) = r.cl {
        spec.push_str(&format!(", CL{}", r.timings.clone().unwrap_or_else(|| cl.to_string())));
    }
    if let Some(v) = r.voltage {
        if r.voltage_is_typical {
            spec.push_str(&format!(", about {} (typical for kits like this; the profile sets the exact value)", volts(v)));
        } else {
            spec.push_str(&format!(", {}", volts(v)));
        }
    }
    match rule {
        Some(b) => {
            steps.push(format!("Restart the PC and press {} as it starts, to enter the {} BIOS.", b.enter, b.name));
            let path = b
                .model_paths
                .iter()
                .find(|m| board.model.to_ascii_uppercase().contains(&m.contains.to_ascii_uppercase()))
                .map(|m| m.path.clone())
                .unwrap_or_else(|| b.path.clone())
                .replace("{profile}", &r.profile_kind);
            steps.push(format!(
                "{} and pick {}. If there's a choice of profiles, Profile 1 is the kit's rated speed ({}).",
                path, profile, spec
            ));
        }
        None => {
            steps.push("Restart the PC and enter the BIOS: usually Del, F2 or F10 as it starts (the start screen often says which).".into());
            let note = if laptop {
                " Most laptops don't offer it; if yours doesn't, there's nothing more to do."
            } else {
                " Many ready-made PCs don't offer it; if yours doesn't, there's nothing more to do."
            };
            steps.push(format!(
                "Look for XMP, EXPO, DOCP or Memory Profile, often under an Advanced, Overclocking or Memory section, and turn on {}. Profile 1 is the kit's rated speed ({}).{}",
                profile, spec, note
            ));
        }
    }
    steps.push("Save and exit (F10). The PC may restart more than once while it trains the memory; a black screen for up to a minute or two is normal, especially with DDR5.".into());
    let fb = match fallback {
        Some(f) => format!(
            "Then try one step down: load the same profile and set the memory speed to {} by hand.",
            mhz(f)
        ),
        None => "Then try the same profile with the memory speed set one step lower by hand.".into(),
    };
    steps.push(format!(
        "If it doesn't start or isn't stable: the BIOS goes back to safe settings by itself after a few failed starts, or clear CMOS (the board's Clear CMOS button or jumper). {}",
        fb
    ));
    steps.push(format!(
        "Check it worked: Task Manager → Performance → Memory → Speed should read {}, and `memguard ram` will say so too.",
        mhz(r.speed_mhz)
    ));
    steps.push(format!(
        "Check it's stable before trusting it with a long cook: run a memory test for at least an hour (TestMem5 or MemTest86; Windows Memory Diagnostic for a quick pass), then a full build. Errors or crashes mean: go to the slower speed{}.",
        fallback.map(|f| format!(" ({})", mhz(f))).unwrap_or_default()
    ));
    let cautions: Vec<String> = findings
        .iter()
        .filter(|f| f.id == "ram:cpu" || f.id == "ram:microcode")
        .map(|f| format!("{}: {}", f.title, f.detail))
        .collect();
    if !cautions.is_empty() {
        steps.push(format!("For this CPU: {}", cautions.join(" ")));
    }
    steps
}

fn words(
    verdict: Verdict,
    rated: Option<&Rated>,
    current: Option<u32>,
    findings: &[RamFinding],
    soldered: bool,
    vm: bool,
    all_decoded: bool,
) -> (String, String) {
    let cur = current.map(mhz).unwrap_or_else(|| "an unknown speed".into());
    let others = findings.iter().filter(|f| f.id != "ram:speed" && f.id != "ram:mixed" && f.severity != Severity::Info).count();
    let tail = if others > 0 { " There are other notes below." } else { "" };
    match verdict {
        Verdict::BelowRated => {
            let r = rated.expect("rated");
            (
                format!("Your RAM is running at {}. The kit is rated for {}.", cur, mhz(r.speed_mhz)),
                format!(
                    "The kit's fast profile ({}) is switched off in the BIOS, so the memory runs at its slower default. Turning it on takes about five minutes and is safe to try: if it doesn't work, the BIOS goes back by itself. The steps below say how.",
                    r.profile_kind
                ),
            )
        }
        Verdict::AtRated => {
            let r = rated.expect("rated");
            let src = if r.confidence == "part_number" { "the kit's part number" } else { "what the modules report" };
            (
                format!("Your RAM is running at its rated {}.", mhz(r.speed_mhz)),
                format!("The memory runs at the speed {} says it's rated for. Nothing to change.{}", src, tail),
            )
        }
        Verdict::NoProfile => {
            let why = if soldered {
                "The memory is soldered to the board, so there's no fast profile to turn on."
            } else {
                "This is standard-speed memory with no fast profile to turn on."
            };
            (format!("Your RAM is running at {}, as fast as it's made to run.", cur), format!("{} Nothing to change.{}", why, tail))
        }
        Verdict::MixedKit => {
            let s = match (rated, all_decoded) {
                (Some(r), true) if current.is_some_and(|c| c + 200 <= r.speed_mhz) => format!(
                    "The sticks don't match, and they run at {}. The slowest one is rated for {}, so a fast profile may work at that speed; the steps below say how, but mixed sticks may need the slower fallback.",
                    cur,
                    mhz(r.speed_mhz)
                ),
                _ => format!("The sticks don't match. Mixed sticks often can't run a fast profile reliably; they run at {} now.", cur),
            };
            ("Your RAM sticks don't match.".into(), format!("{}{}", s, tail))
        }
        Verdict::Unknown => (
            format!("Your RAM is running at {}. Couldn't tell what speed the kit is rated for.", cur),
            format!("The part number didn't match anything known, so the rated speed is unknown. The note below says what to look up.{}", tail),
        ),
        Verdict::NotApplicable => (
            if vm { "This looks like a virtual machine: there are no memory sticks to check.".into() } else { "No memory sticks reported, so there's nothing to check.".into() },
            "The firmware doesn't list any memory modules. That's normal for virtual machines and some locked-down PCs.".into(),
        ),
    }
}

/// Should the checkup and the toast mention it: running below rated, or any problem.
pub fn needs_attention(r: &RamReport) -> bool {
    r.verdict == Verdict::BelowRated || r.findings.iter().any(|f| f.severity == Severity::Problem)
}

// ---------------------------------------------------------------- reading this PC

/// CPU name and microcode revision from the registry (read-only).
fn read_cpu_registry() -> (String, Option<u32>) {
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ};
    let Ok(key) = winreg::RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey_with_flags(r"HARDWARE\DESCRIPTION\System\CentralProcessor\0", KEY_READ) else {
        return (String::new(), None);
    };
    let name: String = key.get_value("ProcessorNameString").unwrap_or_default();
    let microcode = key.get_raw_value("Update Revision").ok().and_then(|v| microcode_from_bytes(&v.bytes));
    (name.trim().to_string(), microcode)
}

/// `Update Revision` is 8 bytes on most systems with the revision in the upper 4 (Intel);
/// some report 4 bytes, and AMD uses the lower 4. Take the upper half when it's set.
fn microcode_from_bytes(b: &[u8]) -> Option<u32> {
    let mut buf = [0u8; 8];
    let n = b.len().min(8);
    buf[..n].copy_from_slice(&b[..n]);
    let v = u64::from_le_bytes(buf);
    let (hi, lo) = ((v >> 32) as u32, v as u32);
    let r = if hi != 0 { hi } else { lo };
    (r != 0).then_some(r)
}

fn today() -> (i32, u32, u32) {
    let s = crate::win::local_time_string();
    let mut it = s.get(..10).unwrap_or("").split('-');
    let y = it.next().and_then(|x| x.parse().ok()).unwrap_or(2000);
    let m = it.next().and_then(|x| x.parse().ok()).unwrap_or(1);
    let d = it.next().and_then(|x| x.parse().ok()).unwrap_or(1);
    (y, m, d)
}

// ---------------------------------------------------------------- rendering

fn verdict_word(v: Verdict) -> &'static str {
    match v {
        Verdict::BelowRated => "Below rated speed",
        Verdict::AtRated => "At rated speed",
        Verdict::NoProfile => "As fast as it's made to run",
        Verdict::MixedKit => "Mixed sticks",
        Verdict::Unknown => "Couldn't tell",
        Verdict::NotApplicable => "Nothing to check",
    }
}

fn severity_word(s: Severity) -> &'static str {
    match s {
        Severity::Problem => "problem",
        Severity::Caution => "caution",
        Severity::Info => "info",
    }
}

fn rated_line(r: &Rated, sticks: &[Stick]) -> String {
    let gen = sticks.iter().find(|s| s.populated).map(|s| s.kind.clone()).unwrap_or_default();
    let mut s = if gen.is_empty() { mhz(r.speed_mhz) } else { format!("{}-{}", gen, r.speed_mhz) };
    if let Some(cl) = r.cl {
        s.push_str(&format!(" CL{}", r.timings.clone().unwrap_or_else(|| cl.to_string())));
    }
    if let Some(v) = r.voltage {
        s.push_str(&format!(", {}{}", if r.voltage_is_typical { "about " } else { "" }, volts(v)));
        if r.voltage_is_typical {
            s.push_str(" (typical)");
        }
    }
    s.push_str(&format!(
        ", {} ({})",
        if r.profile_kind == "JEDEC" { "standard speed, no profile".to_string() } else { format!("{} profile", r.profile_kind) },
        match r.confidence.as_str() {
            "part_number" => "from the part number",
            "module_reported" => "as the modules report it",
            _ => "unknown source",
        }
    ));
    s
}

fn board_line(b: &Board) -> String {
    let mut s = format!("{} {}", b.maker, b.model).trim().to_string();
    if s.is_empty() {
        s = "Unknown board".into();
    }
    if !b.bios_version.is_empty() || !b.bios_date.is_empty() {
        s.push_str(&format!(", BIOS {}", if b.bios_version.is_empty() { "?" } else { &b.bios_version }));
        if !b.bios_date.is_empty() {
            s.push_str(&format!(" from {}", b.bios_date));
        }
        if let Some(a) = b.bios_age_months {
            s.push_str(&format!(" ({} months old)", a));
        }
    }
    s
}

fn cpu_line(c: &Cpu) -> String {
    let mut s = if c.name.is_empty() { "Unknown CPU".to_string() } else { c.name.clone() };
    if let Some(m) = c.microcode {
        s.push_str(&format!(", microcode 0x{:X}", m));
    }
    s
}

fn stick_cells(s: &Stick) -> [String; 5] {
    if !s.populated {
        return [s.slot.clone(), "empty".into(), String::new(), String::new(), String::new()];
    }
    let what = [s.size_gb.map(gb).unwrap_or_default(), s.kind.clone(), s.form.clone()].iter().filter(|x| !x.is_empty()).cloned().collect::<Vec<_>>().join(" ");
    let part = [s.manufacturer.clone(), s.part_number.clone()].iter().filter(|x| !x.is_empty()).cloned().collect::<Vec<_>>().join(" ");
    let rated = match (s.rated_mhz, s.module_mhz) {
        (Some(r), _) => mhz(r),
        (None, Some(m)) => format!("{} (module)", mhz(m)),
        _ => "?".into(),
    };
    let now = match (s.configured_mhz, s.voltage) {
        (Some(c), Some(v)) => format!("{} at {}", mhz(c), volts(v)),
        (Some(c), None) => mhz(c),
        _ => "?".into(),
    };
    [s.slot.clone(), what, part, rated, now]
}

/// What `memguard ram` prints.
pub fn render_text(r: &RamReport) -> String {
    let mut o = String::new();
    o.push_str("RAM AND BIOS CHECK\n");
    o.push_str(&format!("{}\n{}\n\n", r.headline, r.summary));
    if !r.sticks.is_empty() {
        o.push_str(&format!("Memory slots ({}):\n", r.slots_total));
        let rows: Vec<[String; 5]> = r.sticks.iter().map(stick_cells).collect();
        let w: Vec<usize> = (0..5).map(|i| rows.iter().map(|row| row[i].chars().count()).max().unwrap_or(0)).collect();
        o.push_str(&format!("  {:<w0$}  {:<w1$}  {:<w2$}  {:<w3$}  {}\n", "Slot", "Stick", "Part", "Rated", "Now", w0 = w[0].max(4), w1 = w[1].max(5), w2 = w[2].max(4), w3 = w[3].max(5)));
        for row in &rows {
            let line = format!("  {:<w0$}  {:<w1$}  {:<w2$}  {:<w3$}  {}", row[0], row[1], row[2], row[3], row[4], w0 = w[0].max(4), w1 = w[1].max(5), w2 = w[2].max(4), w3 = w[3].max(5));
            o.push_str(line.trim_end());
            o.push('\n');
        }
        o.push('\n');
    }
    match &r.rated {
        Some(rt) => o.push_str(&format!("Rated:  {}\n", rated_line(rt, &r.sticks))),
        None => o.push_str("Rated:  unknown\n"),
    }
    if let Some(c) = r.current_mhz {
        o.push_str(&format!("Now:    {}{}\n", mhz(c), r.current_voltage.map(|v| format!(" at {}", volts(v))).unwrap_or_default()));
    }
    o.push_str(&format!("Board:  {}\n", board_line(&r.board)));
    o.push_str(&format!("CPU:    {}\n", cpu_line(&r.cpu)));
    if !r.findings.is_empty() {
        o.push_str("\nNotes:\n");
        for f in &r.findings {
            o.push_str(&format!("  [{}] {}\n", severity_word(f.severity), f.title));
            o.push_str(&format!("      {}\n", f.detail));
        }
    }
    if !r.steps.is_empty() {
        o.push_str("\nWhat to do (in the BIOS; Ather MemGuard never changes it for you):\n");
        for (i, s) in r.steps.iter().enumerate() {
            o.push_str(&format!("  {}. {}\n", i + 1, s));
        }
    }
    o
}

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            c if c.is_control() => {}
            c => o.push(c),
        }
    }
    o
}

/// Escape, then show `backticks` as code.
fn esc_rich(s: &str) -> String {
    let e = esc(s);
    let mut o = String::new();
    for (i, part) in e.split('`').enumerate() {
        if i % 2 == 1 {
            o.push_str(&format!("<code>{}</code>", part));
        } else {
            o.push_str(part);
        }
    }
    o
}

/// A self-contained page (inline CSS, no scripts, nothing external) in the Ather look.
pub fn render_html(r: &RamReport) -> String {
    let verdict_class = match r.verdict {
        Verdict::BelowRated => "bad",
        Verdict::MixedKit | Verdict::Unknown => "warn",
        _ if r.findings.iter().any(|f| f.severity == Severity::Problem) => "bad",
        _ => "ok",
    };
    let mut h = String::new();
    h.push_str("<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<title>RAM and BIOS check</title>\n<style>\n");
    h.push_str(CSS);
    h.push_str("</style>\n</head>\n<body>\n<main>\n");
    h.push_str("<p class=\"eyebrow\">Ather MemGuard · RAM and BIOS check</p>\n");
    h.push_str(&format!("<p class=\"pill {}\">{}</p>\n", verdict_class, esc(verdict_word(r.verdict))));
    h.push_str(&format!("<h1>{}</h1>\n<p class=\"summary\">{}</p>\n", esc(&r.headline), esc_rich(&r.summary)));

    h.push_str("<section class=\"card\">\n<h2>Memory</h2>\n");
    if !r.sticks.is_empty() {
        h.push_str("<div class=\"scroll\"><table>\n<thead><tr><th>Slot</th><th>Stick</th><th>Part number</th><th>Rated</th><th>Now</th></tr></thead>\n<tbody>\n");
        for s in &r.sticks {
            let c = stick_cells(s);
            let cls = if s.populated { "" } else { " class=\"empty\"" };
            h.push_str(&format!(
                "<tr{}><td>{}</td><td>{}</td><td class=\"mono\">{}</td><td>{}</td><td>{}</td></tr>\n",
                cls,
                esc(&c[0]),
                esc(&c[1]),
                esc(&c[2]),
                esc(&c[3]),
                esc(&c[4])
            ));
        }
        h.push_str("</tbody></table></div>\n");
    }
    h.push_str("<dl>\n");
    h.push_str(&format!("<dt>Rated</dt><dd>{}</dd>\n", esc(&r.rated.as_ref().map(|x| rated_line(x, &r.sticks)).unwrap_or_else(|| "Unknown".into()))));
    if let Some(c) = r.current_mhz {
        h.push_str(&format!("<dt>Now</dt><dd>{}{}</dd>\n", mhz(c), r.current_voltage.map(|v| format!(" at {}", volts(v))).unwrap_or_default()));
    }
    h.push_str(&format!("<dt>Board</dt><dd>{}</dd>\n", esc(&board_line(&r.board))));
    h.push_str(&format!("<dt>CPU</dt><dd>{}</dd>\n", esc(&cpu_line(&r.cpu))));
    h.push_str("</dl>\n</section>\n");

    if !r.findings.is_empty() {
        h.push_str("<section class=\"card\">\n<h2>Notes</h2>\n<ul class=\"findings\">\n");
        for f in &r.findings {
            h.push_str(&format!(
                "<li class=\"{}\"><span class=\"tag\">{}</span><strong>{}</strong><p>{}</p></li>\n",
                severity_word(f.severity),
                severity_word(f.severity),
                esc(&f.title),
                linkify(&esc_rich(&f.detail))
            ));
        }
        h.push_str("</ul>\n</section>\n");
    }

    if !r.steps.is_empty() {
        h.push_str("<section class=\"card\">\n<h2>What to do</h2>\n<p class=\"muted\">In the BIOS. Ather MemGuard never changes it for you.</p>\n<ol class=\"steps\">\n");
        for s in &r.steps {
            h.push_str(&format!("<li>{}</li>\n", esc_rich(s)));
        }
        h.push_str("</ol>\n</section>\n");
    }
    h.push_str("<p class=\"foot\">Read from this PC's firmware tables. Nothing was changed.</p>\n</main>\n</body>\n</html>\n");
    h
}

/// Turn already-escaped https:// URLs into links (text after them stays as is).
fn linkify(escaped: &str) -> String {
    let mut out = String::new();
    let mut rest = escaped;
    while let Some(i) = rest.find("https://") {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let end = tail.find(|c: char| c.is_whitespace() || c == '<').unwrap_or(tail.len());
        let url = &tail[..end];
        out.push_str(&format!("<a href=\"{}\">{}</a>", url, url));
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

const CSS: &str = r#":root { color-scheme: dark; }
* { box-sizing: border-box; }
body { margin: 0; background: rgb(14,14,13); color: rgb(242,239,232);
  font: 15px/1.55 "Segoe UI Variable Text", "Segoe UI", system-ui, sans-serif; }
main { max-width: 860px; margin: 0 auto; padding: 32px 16px 48px; }
.eyebrow { font-family: "Bahnschrift SemiBold", "Bahnschrift", "Segoe UI", sans-serif; text-transform: uppercase;
  letter-spacing: 3px; font-size: 12px; color: rgb(212,255,0); margin: 0 0 14px; }
h1 { font-family: "Bahnschrift SemiBold Condensed", "Bahnschrift Condensed", "Bahnschrift", "Segoe UI", sans-serif;
  font-stretch: condensed; text-transform: uppercase; font-weight: 600; font-size: 34px; line-height: 1.1; margin: 8px 0 12px; }
h2 { font-family: "Bahnschrift SemiBold", "Bahnschrift", "Segoe UI", sans-serif; text-transform: uppercase;
  letter-spacing: 3px; font-size: 13px; color: rgb(212,255,0); margin: 0 0 14px; font-weight: 600; }
.summary { color: rgb(208,206,199); margin: 0 0 24px; max-width: 70ch; }
.pill { display: inline-block; margin: 0; padding: 3px 10px; border-radius: 6px; font-size: 12px; font-weight: 600;
  text-transform: uppercase; letter-spacing: 1px; }
.pill.ok { background: rgb(212,255,0); color: rgb(14,14,13); }
.pill.warn { background: rgb(255,184,48); color: rgb(14,14,13); }
.pill.bad { background: rgb(255,120,110); color: rgb(14,14,13); }
.card { background: rgb(26,27,25); border: 1px solid rgb(50,52,46); border-radius: 8px; padding: 20px; margin: 0 0 16px; }
.scroll { overflow-x: auto; }
table { width: 100%; border-collapse: collapse; font-size: 14px; }
th { text-align: left; color: rgb(150,150,142); font-weight: 600; padding: 6px 10px 6px 0; border-bottom: 1px solid rgb(50,52,46); white-space: nowrap; }
td { padding: 7px 10px 7px 0; border-bottom: 1px solid rgb(37,38,35); vertical-align: top; }
tr.empty td { color: rgb(150,150,142); }
.mono, code { font-family: "Cascadia Mono", Consolas, monospace; font-size: 13px; }
code { background: rgb(37,38,35); padding: 1px 5px; border-radius: 4px; }
dl { display: grid; grid-template-columns: max-content 1fr; gap: 6px 16px; margin: 16px 0 0; }
dt { color: rgb(150,150,142); }
dd { margin: 0; }
.findings { list-style: none; padding: 0; margin: 0; }
.findings li { border-left: 3px solid rgb(150,150,142); padding: 2px 0 2px 14px; margin: 0 0 16px; }
.findings li.problem { border-color: rgb(255,120,110); }
.findings li.caution { border-color: rgb(255,184,48); }
.findings p { margin: 4px 0 0; color: rgb(208,206,199); overflow-wrap: anywhere; }
.tag { display: inline-block; font-size: 11px; text-transform: uppercase; letter-spacing: 1px; margin-right: 8px; color: rgb(150,150,142); }
li.problem .tag { color: rgb(255,120,110); }
li.caution .tag { color: rgb(255,184,48); }
.steps { margin: 0; padding-left: 22px; }
.steps li { margin: 0 0 10px; padding-left: 4px; }
.steps li::marker { color: rgb(212,255,0); font-weight: 700; }
.muted { color: rgb(150,150,142); margin: -6px 0 14px; }
a { color: rgb(212,255,0); }
.foot { color: rgb(150,150,142); font-size: 13px; margin-top: 24px; }
@media (max-width: 560px) { h1 { font-size: 26px; } dl { grid-template-columns: 1fr; } dt { margin-top: 6px; } }
"#;

// ---------------------------------------------------------------- unit tests (internals)

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_file_loads_and_every_pattern_compiles() {
        let d = data();
        assert!(d.part.len() >= 15);
        assert!(d.part.iter().all(|r| r.compiled.len() == r.patterns.len()));
        assert_eq!(d.board.len(), 4);
        assert!(d.cpu.len() >= 4);
    }

    #[test]
    fn matcher_basics() {
        let t = compile("F{gen:[45]}-{speed:d4}*").unwrap();
        let c = match_pattern(&t, "F5-6000J3038").unwrap();
        assert_eq!(c["gen"], "5");
        assert_eq!(c["speed"], "6000");
        assert!(match_pattern(&t, "F3-6000").is_none());
        let t = compile("*D{gen:[45]}{:d1-3}G{speed:d4}HC{cl:d2}*").unwrap();
        assert_eq!(match_pattern(&t, "FF3D516G6000HC38ADC01").unwrap()["cl"], "38");
        assert!(compile("{x:q}").is_err());
        assert!(compile("{x:d").is_err());
    }

    #[test]
    fn decodes_the_spec_examples() {
        let cases: &[(&str, u32, Option<u32>, bool)] = &[
            ("F5-6000J3038F16GX2-TZ5N", 6000, Some(30), false),
            ("F4-3600C16D-32GTZN", 3600, Some(16), false),
            ("CMK32GX5M2B5600C36", 5600, Some(36), false),
            ("KF560C36BBEK2-32", 6000, Some(36), false),
            ("KF432C16BB/16", 3200, Some(16), false),
            ("BL16G36C16U4B", 3600, Some(16), false),
            ("FF3D516G6000HC38ADC01", 6000, Some(38), false),
            ("AX5U6000C3016G-DCLARBK", 6000, Some(30), false),
            ("PVV532G600C36K", 6000, Some(36), false),
            ("M323R2GA3BB0-CQK", 4800, Some(40), true),
        ];
        for (p, speed, cl, jedec) in cases {
            let d = decode_part(p).unwrap_or_else(|| panic!("{} should decode", p));
            assert_eq!((d.speed_mhz, d.cl, d.jedec), (*speed, *cl, *jedec), "{}", p);
        }
        assert_eq!(decode_part("F5-6000J3038F16GX2-TZ5N").unwrap().timings.as_deref(), Some("30-38-38"));
    }

    #[test]
    fn microcode_bytes() {
        assert_eq!(microcode_from_bytes(&[0x21, 0x01, 0, 0]), Some(0x121));
        assert_eq!(microcode_from_bytes(&[0, 0, 0, 0, 0x2B, 0x01, 0, 0]), Some(0x12B));
        assert_eq!(microcode_from_bytes(&[]), None);
    }

    #[test]
    fn dates_and_slots() {
        assert_eq!(parse_date("03/11/2026"), Some((2026, 3, 11)));
        assert_eq!(parse_date("12/01/19"), Some((2019, 12, 1)));
        assert_eq!(parse_date("2023-07-04"), Some((2023, 7, 4)));
        assert_eq!(parse_date("garbage"), None);
        assert_eq!(months_between((2026, 3, 11), (2026, 11, 14)), Some(8));
        assert_eq!(slot_id("DIMM_A2"), Some(('A', 2)));
        assert_eq!(slot_id("DIMMB1"), Some(('B', 1)));
        assert_eq!(slot_id("ChannelA-DIMM0"), None);
        assert_eq!(slot_id("DIMM 1"), None);
    }

    #[test]
    fn html_escapes() {
        assert_eq!(esc("<a href=\"x\">&'"), "&lt;a href=&quot;x&quot;&gt;&amp;&#39;");
        assert_eq!(linkify("see https://x.test/a b"), "see <a href=\"https://x.test/a\">https://x.test/a</a> b");
    }
}
