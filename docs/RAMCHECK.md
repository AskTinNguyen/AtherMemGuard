# Ather MemGuard: RAM and BIOS check (Part G)

**Owns:** `src/ramcheck.rs` (new), `src/smbios.rs` (new: reading and parsing the firmware
tables), `src/ramcheck_data.toml` (new, embedded: part-number rules, board vendors, CPU
cautions), `tests/ramcheck.rs` (new).
**Shared edits:** `Cargo.toml` (windows-sys features), `src/lib.rs` (two `pub mod` lines),
`src/bin/memguard.rs` (the `ram` subcommand only, under a `// ---- Part G` marker),
`src/mcp.rs` (register `memguard_ram`, under a `// ---- Part G` marker),
`docs/AI-AGENTS.md` (the new tool).
Do not edit `doctor.rs`, `tray.rs`, `src/ui/*`, `README.md` or `config.rs`: another builder is
changing them right now. The integrator adds the checkup row, the toast and the README section
from what you expose (see "For the integrator" below).

The ground rules in `docs/DESIGN.md` ("Ground rules for every builder") apply in full.

## What it answers

**Is this PC's RAM running slower than the kit was sold to run, and if so, what should the
person do about it?** Memory kits ship with a fast profile (XMP on Intel, EXPO or DOCP on AMD)
that is off until someone turns it on in the BIOS. Many PCs run for years at the slow default.
For Unreal work that costs compile, shader and cook time.

It **reports and explains. It never changes anything**: no BIOS writes, no registry writes, no
driver, no admin rights. The person does the BIOS change themselves, following the steps.

## Where the facts come from

No WMI, no PowerShell, no network. Read the raw SMBIOS table with
`GetSystemFirmwareTable('RSMB', 0)` and parse it in `src/smbios.rs`:

| SMBIOS type | Fields used |
|---|---|
| 0 BIOS | vendor, version, release date |
| 2 Baseboard | manufacturer, product (board model), version |
| 4 Processor | manufacturer, version string (model name), family |
| 16 Physical memory array | number of slots, maximum capacity |
| 17 Memory device, one per slot | device locator and bank locator (slot names), size (incl. extended size), memory type (DDR4 = 26, DDR5 = 34), speed (the module's maximum / rated speed as the BIOS reports it), configured memory speed (what it runs at now), manufacturer, part number, minimum / maximum / configured voltage, rank |

Empty slots are type 17 entries with size 0; keep them, the slot layout needs them.

CPU microcode revision: read-only from the registry,
`HKLM\HARDWARE\DESCRIPTION\System\CentralProcessor\0`, value `Update Revision` (8 bytes; the
revision is the upper 4 bytes on Intel) and `ProcessorNameString`.

The parser must be a pure function over bytes (`smbios::parse(&[u8]) -> Smbios`) so tests can
feed it recorded tables. Tolerate everything: short structures, missing strings, vendors that
leave "To Be Filled By O.E.M." or blanks, OEM PCs and laptops with soldered memory, virtual
machines with no type 17 at all.

## Working out the rated speed

Three sources, most trusted first; the report says which one was used (`confidence`):

1. **The part number, decoded** (`confidence = "part_number"`). Rules live in
   `src/ramcheck_data.toml` so they can grow without code changes. Cover at least:

| Vendor | Pattern | Example | Decodes to |
|---|---|---|---|
| G.Skill | `F{gen}-{speed}{J\|C}{cl}{t2}{letter}{size}G{X\|D}{count}-...` | `F5-6000J3038F16GX2-TZ5N` | DDR5-6000, CL30-38-38, 2 x 16 GB |
| G.Skill DDR4 | `F4-{speed}C{cl}{D\|Q}-{total}G...` | `F4-3600C16D-32GTZN` | DDR4-3600, CL16, 32 GB kit of 2 |
| Corsair | `CM{series}{total}GX{gen}M{count}{rev}{speed}C{cl}...` | `CMK32GX5M2B5600C36` | DDR5-5600, CL36, 32 GB kit of 2 |
| Kingston Fury | `KF{gen}{speed/100}C{cl}{...}-{size}` | `KF560C36BBEK2-32`, `KF432C16BB/16` | DDR5-6000 CL36; DDR4-3200 CL16 |
| Crucial | `CT{size}G{gen digits}...` / Ballistix `BL{size}G{speed/100}C{cl}...` | `BL16G36C16U4B` | DDR4-3600 CL16 |
| TeamGroup | `...{gen}{size}G{speed}HC{cl}...` | `FF3D516G6000HC38ADC01` | DDR5-6000 CL38 |
| ADATA / XPG | `AX{gen}U{speed}{cl}{size}G...` | `AX5U6000C3016G-DCLARBK` | DDR5-6000 CL30 |
| Patriot | `PV{...}{size}G{speed/10}C{cl}...` | `PVV532G600C36K` | DDR5-6000 CL36 |
| Samsung / SK hynix / Micron OEM | fixed tables of the common part numbers | `M323R2GA3BB0-CQK` | DDR5-4800 CL40 (no fast profile: JEDEC only) |

   A kit's secondary timings and voltage are mostly not in the part number. Give the first
   timing (CL) and any others the pattern really encodes; for voltage use the typical value for
   that generation and speed from a small table in the data file, and label it "typical"
   (DDR4: 1.35 V up to 3600, 1.4-1.45 V above; DDR5: 1.25 V up to 5600, 1.35 V for 6000-6400,
   1.4-1.45 V above). Never present a typical value as the kit's specification.
2. **The module's reported maximum speed** (SMBIOS type 17 `speed`), when it is higher than
   the configured speed (`confidence = "module_reported"`).
3. **Neither** (`confidence = "unknown"`): say the kit couldn't be identified, show the part
   number, and tell the person to look it up on the maker's site. Still report everything
   else.

JEDEC-only memory (OEM modules, most laptops) has no fast profile: if configured speed equals
the JEDEC speed for that part, the verdict is "running as fast as it can", not a problem.

## Verdicts

| Verdict | When | Shown as |
|---|---|---|
| `below_rated` | Configured speed is at least 200 MHz under the rated speed | A problem to act on |
| `at_rated` | Within 200 MHz of rated | Fine |
| `no_profile` | JEDEC-only kit, or soldered memory | Fine, with one line why |
| `mixed_kit` | Sticks with different part numbers or sizes | A caution: mixed sticks often can't run a fast profile; the lowest common speed applies |
| `unknown` | Rated speed couldn't be worked out | "Couldn't tell", with what to look up |
| `not_applicable` | Virtual machine or no memory devices reported | Hidden everywhere except `memguard ram` |

Plus independent findings, each with its own `severity` (`problem` / `caution` / `info`):

- **Slots.** On a board with four slots and two sticks, the sticks should be in the second
  and fourth slot from the CPU (usually named `DIMM_A2` and `DIMM_B2`, or `DIMMA2`/`DIMMB2`,
  `A2`/`B2`). Flag two sticks in `A1`+`B1`, and flag both sticks in the same channel
  (`A1`+`A2`), which halves memory bandwidth and is the more serious of the two. If the slot
  names can't be read that way, say nothing about slots rather than guess. One stick alone on
  a multi-slot desktop board: an `info` that a second matching stick would double bandwidth.
- **BIOS age.** Release date over 18 months old: `caution`, "newer BIOS versions often improve
  memory compatibility", with the board maker's support page (a URL template per vendor in
  the data file, falling back to a web search URL for "{maker} {board} BIOS"). There is no way
  to know the latest version offline; say "may be out of date", never "is out of date".
- **CPU cautions**, from the processor name:
  - Intel Core 13th and 14th gen desktop (i5/i7/i9 13xxx and 14xxx K, KF, KS and 65 W parts):
    microcode revision below `0x12B` is a `problem` ("update the BIOS before changing memory
    settings: versions before 0x12B can damage these CPUs"); always add the caution to keep
    System Agent (SA) voltage at or under about 1.25 V.
  - Intel Core Ultra 200 (Arrow Lake): caution, SA voltage at or under about 1.25 V.
  - AMD Ryzen 7000 / 8000 / 9000 (AM5): SoC voltage at or under 1.3 V; note that BIOS
    versions from mid-2023 on cap it, so an older BIOS is one more reason to update. Suggest
    6000 MHz as the usual sweet spot; kits rated higher than 6400 may need to run at 6000-6400.
  - AMD Ryzen on AM4 (DDR4): 3600 is the usual sweet spot; above 3800 often needs manual tuning.
  - Anything else: no CPU-specific caution.

## The instructions

When the verdict is `below_rated` (or `mixed_kit` with a decodable target), the report
includes steps written for this board's BIOS family, chosen by baseboard manufacturer:

| Maker | Enter BIOS | Where the setting is | Name of the setting |
|---|---|---|---|
| ASUS | Del or F2 at power-on | Press F7 for Advanced Mode, then Ai Tweaker (ROG: Extreme Tweaker) → Ai Overclock Tuner | XMP I / XMP II on Intel; DOCP or EXPO I / EXPO II on AMD |
| MSI | Del | Press F7 for Advanced, then OC → Extreme Memory Profile (or the XMP / A-XMP / EXPO button on the EZ Mode screen) | XMP on Intel; A-XMP or EXPO on AMD |
| Gigabyte / AORUS | Del | Tweaker → Extreme Memory Profile (X.M.P.) | XMP on Intel; EXPO or XMP on AMD |
| ASRock | Del or F2 | OC Tweaker → DRAM Profile Configuration → Load XMP Setting | XMP / EXPO |
| Others, OEM desktops, laptops | varies | "Look for XMP, EXPO, DOCP or Memory Profile; many OEM PCs and most laptops don't offer it" | |

The steps, in order, with this PC's own values filled in:

1. Before you start: save your work; a failed attempt only needs a BIOS reset, it can't
   damage the memory. If a CPU `problem` applies (Intel microcode), do the BIOS update first.
2. Restart and enter the BIOS ({key}).
3. Go to {path} and pick {profile name}. Profile 1 is the kit's rated speed
   ({rated speed}, CL{cl}, about {voltage} V).
4. Save and exit (F10). The PC may restart more than once while it trains the memory; a black
   screen for up to a minute or two on DDR5 is normal.
5. **If it doesn't boot or is unstable:** the BIOS falls back by itself after a few failed
   starts, or clear CMOS (the board's Clear CMOS button or jumper). Then try the fallback
   speed: one step down ({fallback}: 6000 → 5600, 3600 → 3200, and so on) with the same
   profile, by setting the memory speed manually after loading the profile.
6. **Check it worked:** Task Manager → Performance → Memory → Speed should read {rated}, and
   `memguard ram` will say so too.
7. **Check it's stable** before trusting it with a long cook: run a memory test for at least
   an hour (TestMem5, MemTest86 or Windows Memory Diagnostic for a quick pass), then a full
   build. Errors or crashes mean: go to the fallback speed.
8. The CPU cautions that apply to this PC, in plain words.

## API

```rust
pub struct RamReport {
    pub verdict: Verdict, pub headline: String,            // "Your RAM is running at 4800 MHz. The kit is rated for 6000 MHz."
    pub summary: String,                                   // two or three sentences, app voice
    pub sticks: Vec<Stick>, pub slots_total: usize,
    pub rated: Option<Rated>,                              // speed_mhz, cl, timings: Option<String>, voltage: Option<f32>, voltage_is_typical, profile_kind: "XMP"|"EXPO"|"JEDEC", confidence
    pub current_mhz: Option<u32>, pub current_voltage: Option<f32>,
    pub board: Board,                                      // maker, model, bios_vendor, bios_version, bios_date, bios_age_months
    pub cpu: Cpu,                                          // name, vendor, microcode: Option<u32>
    pub findings: Vec<RamFinding>,                         // id ("ram:speed", "ram:slots", "ram:bios", "ram:microcode", ...), severity, title, detail
    pub steps: Vec<String>,                                // empty unless there is something to do
    pub fallback_mhz: Option<u32>,
    pub fingerprint: String,                               // hash of part numbers + configured speed + BIOS version: changes when the situation does
}
pub fn check() -> RamReport;                               // reads this PC; never fails, never writes
pub fn check_from(smbios: &smbios::Smbios, cpu_name: &str, microcode: Option<u32>, today: (i32, u32, u32)) -> RamReport; // pure, for tests
pub fn render_text(r: &RamReport) -> String;               // what `memguard ram` prints
pub fn render_html(r: &RamReport) -> String;               // a self-contained page in the Ather look (colours from docs/UI.md "Theme"), for "Show me how"
pub fn needs_attention(r: &RamReport) -> bool;             // verdict below_rated, or any finding with severity problem
```

`memguard ram [--json] [--html <file>]` prints the report (and exits 0 always: it is a
report, not a test). MCP tool `memguard_ram`: the report as JSON, read-only; its description
tells agents to relay the headline and offer the steps, and that only the person can change
BIOS settings.

## Tests

`check_from` and `smbios::parse` are pure. Test with hand-built and recorded tables: a
two-stick DDR5 kit at 4800 rated 6000 (below_rated, ASUS steps, AM5 caution); the same at
6000 (at_rated); DDR4 at 2133 rated 3600; sticks in A1+B1; both in channel A; one stick;
mixed part numbers; OEM JEDEC-only modules; a laptop; a VM with no memory devices; an Intel
14900K with microcode 0x129 (problem) and 0x12B (caution only); a BIOS dated three years ago;
garbage and truncated tables (no panic). Decode every example part number in the table above,
plus at least thirty more real ones across vendors, and check that unknown patterns return
`unknown` rather than a wrong guess. `tests/ramcheck.rs` runs the real `check()` on this PC
and asserts only that it doesn't panic and that the fields are self-consistent.

## For the integrator (not the builder)

- Checkup window and `memguard doctor`: a THIS PC row when `needs_attention`, title = the
  headline, link "Show me how" opening the HTML report written to the data folder.
- Toast, once per `fingerprint` (remembered in `ui-state.toml`), checked a few minutes after
  start: Warning · "Your RAM is running below its rated speed" · "{current} MHz now, rated
  {rated} MHz. Turning on {XMP|EXPO} in the BIOS takes about five minutes." · button
  "Show me how".
- Settings window, GENERAL: toggle "Check memory speed" (hint "Tells you once if your RAM is
  running slower than it was sold to run."), key `general.check_ram_speed`, default on.
