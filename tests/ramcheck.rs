//! RAM and BIOS check. Everything here is pure (`check_from`, `smbios::parse`) except
//! `real_check_on_this_pc`, which only reads this PC's firmware tables and registry.

use athermemguard::ramcheck::{self, check_from, needs_attention, render_html, render_text, RamReport, Severity, Verdict};
use athermemguard::smbios::{self, Baseboard, Bios, MemoryArray, MemoryDevice, Processor, Smbios};

const TODAY: (i32, u32, u32) = (2026, 11, 14);
const DDR4: u8 = smbios::MEM_DDR4;
const DDR5: u8 = smbios::MEM_DDR5;

// ---------------------------------------------------------------- building tables

fn stick(slot: &str, part: &str, mem_type: u8, size_gb: u64, speed: u32, configured: u32, mv: u16) -> MemoryDevice {
    MemoryDevice {
        handle: 0,
        array_handle: 0x10,
        size_mb: Some(size_gb * 1024),
        form_factor: smbios::FORM_DIMM,
        device_locator: slot.into(),
        bank_locator: String::new(),
        memory_type: mem_type,
        type_detail: 0,
        speed,
        configured_speed: configured,
        manufacturer: String::new(),
        part_number: part.into(),
        rank: 1,
        min_voltage_mv: mv,
        max_voltage_mv: mv,
        configured_voltage_mv: mv,
    }
}

fn empty(slot: &str) -> MemoryDevice {
    MemoryDevice {
        array_handle: 0x10,
        size_mb: Some(0),
        device_locator: slot.into(),
        manufacturer: "Unknown".into(),
        part_number: "Unknown".into(),
        ..Default::default()
    }
}

fn pc(maker: &str, model: &str, bios_date: &str, devices: Vec<MemoryDevice>) -> Smbios {
    Smbios {
        version: (3, 6),
        bios: Some(Bios { vendor: "American Megatrends Inc.".into(), version: "1813".into(), release_date: bios_date.into() }),
        board: Some(Baseboard { manufacturer: maker.into(), product: model.into(), version: "Rev 1.xx".into() }),
        processors: vec![],
        arrays: vec![MemoryArray { handle: 0x10, location: 3, usage: 3, slots: devices.len() as u16, max_capacity_bytes: 0 }],
        devices,
    }
}

/// Four slots, two sticks in the given slots (others empty), names DIMM_A1..DIMM_B2.
fn four_slots(used: [&str; 2], part: &str, mem_type: u8, size: u64, speed: u32, configured: u32, mv: u16) -> Vec<MemoryDevice> {
    ["DIMM_A1", "DIMM_A2", "DIMM_B1", "DIMM_B2"]
        .iter()
        .map(|s| if used.contains(s) { stick(s, part, mem_type, size, speed, configured, mv) } else { empty(s) })
        .collect()
}

const ASUS: &str = "ASUSTeK COMPUTER INC.";
const MSI: &str = "Micro-Star International Co., Ltd.";
const R7800X3D: &str = "AMD Ryzen 7 7800X3D 8-Core Processor";

fn am5_ddr5(configured: u32) -> RamReport {
    let devs = four_slots(["DIMM_A2", "DIMM_B2"], "F5-6000J3038F16G", DDR5, 16, 4800, configured, if configured > 4800 { 1350 } else { 1100 });
    check_from(&pc(ASUS, "ROG STRIX B650E-F GAMING WIFI", "10/05/2023", devs), R7800X3D, None, TODAY)
}

fn has(r: &RamReport, id: &str, sev: Severity) -> bool {
    r.findings.iter().any(|f| f.id == id && f.severity == sev)
}

fn steps(r: &RamReport) -> String {
    r.steps.join("\n")
}

// ---------------------------------------------------------------- scenarios

#[test]
fn ddr5_kit_at_4800_rated_6000_on_am5() {
    let r = am5_ddr5(4800);
    assert_eq!(r.verdict, Verdict::BelowRated);
    assert_eq!(r.headline, "Your RAM is running at 4800 MHz. The kit is rated for 6000 MHz.");
    let rated = r.rated.as_ref().unwrap();
    assert_eq!((rated.speed_mhz, rated.cl, rated.timings.as_deref()), (6000, Some(30), Some("30-38-38")));
    assert_eq!(rated.profile_kind, "EXPO");
    assert_eq!(rated.confidence, "part_number");
    assert_eq!(rated.voltage, Some(1.35));
    assert!(rated.voltage_is_typical);
    assert_eq!(r.current_mhz, Some(4800));
    assert_eq!(r.fallback_mhz, Some(5600));
    assert!(needs_attention(&r));
    assert!(has(&r, "ram:speed", Severity::Problem));
    assert!(has(&r, "ram:cpu", Severity::Caution), "AM5 caution: {:?}", r.findings);
    assert!(r.findings.iter().any(|f| f.detail.contains("SoC voltage at or under 1.3 V")));
    assert!(!r.findings.iter().any(|f| f.id == "ram:slots"), "A2 + B2 is right");
    // ASUS steps, ROG path, AMD DDR5 profile name, this kit's values, the fallback, the CPU caution.
    let s = steps(&r);
    assert_eq!(r.steps.len(), 8, "{}", s);
    assert!(s.contains("press Del or F2"), "{}", s);
    assert!(s.contains("Extreme Tweaker → Ai Overclock Tuner"), "{}", s);
    assert!(s.contains("EXPO I"), "{}", s);
    assert!(s.contains("DDR5-6000, CL30-38-38, about 1.35 V (typical"), "{}", s);
    assert!(s.contains("5600 MHz"), "{}", s);
    assert!(s.contains("Task Manager → Performance → Memory → Speed should read 6000 MHz"), "{}", s);
    assert!(s.contains("SoC voltage"), "{}", s);
    assert!(!s.contains("XMP I"), "{}", s);
}

#[test]
fn same_kit_at_6000_is_fine() {
    let r = am5_ddr5(6000);
    assert_eq!(r.verdict, Verdict::AtRated);
    assert_eq!(r.headline, "Your RAM is running at its rated 6000 MHz.");
    assert!(r.steps.is_empty());
    assert!(!has(&r, "ram:speed", Severity::Problem));
    // The BIOS is from 2023: still worth a caution, but not "needs attention".
    assert!(has(&r, "ram:bios", Severity::Caution));
    assert!(!needs_attention(&r));
    assert_ne!(r.fingerprint, am5_ddr5(4800).fingerprint);
    assert_eq!(r.fingerprint, am5_ddr5(6000).fingerprint);
}

#[test]
fn ddr4_at_2133_rated_3600_on_msi_intel() {
    let devs = four_slots(["DIMM_A2", "DIMM_B2"], "F4-3600C16-16GTZNC", DDR4, 16, 2133, 2133, 1200);
    let r = check_from(&pc(MSI, "MAG Z690 TOMAHAWK WIFI (MS-7D32)", "05/10/2026", devs), "12th Gen Intel(R) Core(TM) i7-12700K", None, TODAY);
    assert_eq!(r.verdict, Verdict::BelowRated);
    let rated = r.rated.as_ref().unwrap();
    assert_eq!((rated.speed_mhz, rated.cl, rated.profile_kind.as_str()), (3600, Some(16), "XMP"));
    assert_eq!(rated.voltage, Some(1.35));
    assert_eq!(r.fallback_mhz, Some(3200));
    let s = steps(&r);
    assert!(s.contains("press Del as it starts"), "{}", s);
    assert!(s.contains("OC → Extreme Memory Profile"), "{}", s);
    assert!(s.contains("and pick XMP."), "{}", s);
    assert!(s.contains("DDR4-3600, CL16"), "{}", s);
    assert!(s.contains("3200 MHz"), "{}", s);
    // A 12th gen CPU has no specific caution, so no CPU step.
    assert!(!s.contains("For this CPU"), "{}", s);
    assert_eq!(r.steps.len(), 7);
    assert!(!r.findings.iter().any(|f| f.id == "ram:cpu" || f.id == "ram:bios"));
}

#[test]
fn sticks_in_a1_b1_get_a_caution() {
    let devs = four_slots(["DIMM_A1", "DIMM_B1"], "F5-6000J3038F16G", DDR5, 16, 4800, 6000, 1350);
    let r = check_from(&pc(ASUS, "TUF GAMING B650-PLUS", "06/01/2026", devs), R7800X3D, None, TODAY);
    let f = r.findings.iter().find(|f| f.id == "ram:slots").expect("slot finding");
    assert_eq!(f.severity, Severity::Caution);
    assert!(f.detail.contains("DIMM_A2 and DIMM_B2"), "{}", f.detail);
}

#[test]
fn both_sticks_in_channel_a_is_a_problem() {
    let devs = four_slots(["DIMM_A1", "DIMM_A2"], "F5-6000J3038F16G", DDR5, 16, 4800, 6000, 1350);
    let r = check_from(&pc(ASUS, "TUF GAMING B650-PLUS", "06/01/2026", devs), R7800X3D, None, TODAY);
    let f = r.findings.iter().find(|f| f.id == "ram:slots").expect("slot finding");
    assert_eq!(f.severity, Severity::Problem);
    assert!(f.detail.contains("halve memory bandwidth"));
    assert!(needs_attention(&r));
}

#[test]
fn slot_names_that_dont_parse_get_no_slot_advice() {
    let names = ["ChannelA-DIMM0", "ChannelA-DIMM1", "ChannelB-DIMM0", "ChannelB-DIMM1"];
    let devs: Vec<MemoryDevice> = names
        .iter()
        .enumerate()
        .map(|(i, n)| if i < 2 { stick(n, "F5-6000J3038F16G", DDR5, 16, 4800, 6000, 1350) } else { empty(n) })
        .collect();
    let r = check_from(&pc(ASUS, "TUF GAMING B650-PLUS", "06/01/2026", devs), R7800X3D, None, TODAY);
    assert!(!r.findings.iter().any(|f| f.id == "ram:slots"), "{:?}", r.findings);
}

#[test]
fn one_stick_suggests_a_second() {
    let devs = vec![empty("DIMM_A1"), stick("DIMM_A2", "CMK16GX4M2B3200C16", DDR4, 8, 2133, 3200, 1350), empty("DIMM_B1"), empty("DIMM_B2")];
    let r = check_from(&pc("Gigabyte Technology Co., Ltd.", "B550 AORUS ELITE V2", "02/01/2026", devs), "AMD Ryzen 5 5600X 6-Core Processor", None, TODAY);
    assert_eq!(r.verdict, Verdict::AtRated);
    assert!(has(&r, "ram:slots", Severity::Info));
    assert!(r.findings.iter().any(|f| f.id == "ram:cpu" && f.title.contains("3600")), "AM4 note: {:?}", r.findings);
}

#[test]
fn mixed_part_numbers() {
    let devs = vec![
        empty("DIMM_A1"),
        stick("DIMM_A2", "CMK16GX4M2B3200C16", DDR4, 8, 2133, 2133, 1200),
        empty("DIMM_B1"),
        stick("DIMM_B2", "F4-3600C16-16GTZNC", DDR4, 16, 2133, 2133, 1200),
    ];
    let r = check_from(&pc("Gigabyte Technology Co., Ltd.", "Z790 AORUS ELITE AX", "02/01/2026", devs), "13th Gen Intel(R) Core(TM) i5-13600K", Some(0x12F), TODAY);
    assert_eq!(r.verdict, Verdict::MixedKit);
    assert_eq!(r.headline, "Your RAM sticks don't match.");
    assert!(has(&r, "ram:mixed", Severity::Caution));
    // Every stick decodes, so the lowest common rated speed is the target, with steps.
    assert_eq!(r.rated.as_ref().unwrap().speed_mhz, 3200);
    let s = steps(&r);
    assert!(s.contains("Tweaker → Extreme Memory Profile (X.M.P.)"), "{}", s);
    assert!(s.contains("System Agent"), "{}", s);
    assert!(!needs_attention(&r));
}

#[test]
fn oem_jedec_modules_have_no_profile() {
    let devs = vec![
        stick("DIMM1", "M378A1K43EB2-CWE", DDR4, 8, 3200, 3200, 1200),
        stick("DIMM2", "M378A1K43EB2-CWE", DDR4, 8, 3200, 3200, 1200),
    ];
    let r = check_from(&pc("Dell Inc.", "0K3CM7", "04/02/2026", devs), "Intel(R) Core(TM) i7-10700 CPU @ 2.90GHz", None, TODAY);
    assert_eq!(r.verdict, Verdict::NoProfile);
    assert_eq!(r.headline, "Your RAM is running at 3200 MHz, as fast as it's made to run.");
    let rated = r.rated.as_ref().unwrap();
    assert_eq!((rated.speed_mhz, rated.profile_kind.as_str(), rated.voltage_is_typical), (3200, "JEDEC", false));
    assert_eq!(rated.voltage, Some(1.2));
    assert!(r.steps.is_empty());
    assert_eq!(r.fallback_mhz, None);
    assert!(!needs_attention(&r));
}

#[test]
fn laptop_with_so_dimms_held_back_by_the_cpu() {
    let mut devs = vec![
        stick("DIMM 0", "M471A2K43DB1-CWE", DDR4, 16, 3200, 2666, 1200),
        stick("DIMM 0", "M471A2K43DB1-CWE", DDR4, 16, 3200, 2666, 1200),
    ];
    for d in &mut devs {
        d.form_factor = smbios::FORM_SODIMM;
    }
    let r = check_from(&pc("LENOVO", "20XW0055GE", "01/15/2026", devs), "11th Gen Intel(R) Core(TM) i5-1135G7 @ 2.40GHz", None, TODAY);
    assert_eq!(r.verdict, Verdict::NoProfile);
    assert!(has(&r, "ram:speed", Severity::Info));
    assert!(r.steps.is_empty());
    assert!(!r.findings.iter().any(|f| f.id == "ram:slots"));
}

#[test]
fn laptop_with_soldered_memory() {
    let mut devs = vec![stick("Controller0-ChannelA", "MT62F1G32D4DR-031 WT:B", smbios::MEM_LPDDR5, 8, 6400, 6400, 500)];
    devs[0].form_factor = smbios::FORM_ROW_OF_CHIPS;
    devs.push(devs[0].clone());
    let r = check_from(&pc("Dell Inc.", "0XYZ12", "03/01/2026", devs), "Intel(R) Core(TM) Ultra 7 155H", None, TODAY);
    assert_eq!(r.verdict, Verdict::NoProfile);
    assert!(r.summary.contains("soldered"));
    assert!(r.steps.is_empty());
    assert!(!r.findings.iter().any(|f| f.id == "ram:cpu"), "155H is not a desktop Core Ultra 200");
}

#[test]
fn virtual_machine_without_memory_devices() {
    let s = Smbios {
        bios: Some(Bios { vendor: "VMware, Inc.".into(), version: "VMW71.00V".into(), release_date: "11/12/2020".into() }),
        ..Default::default()
    };
    let r = check_from(&s, "", None, TODAY);
    assert_eq!(r.verdict, Verdict::NotApplicable);
    assert!(r.findings.is_empty(), "{:?}", r.findings);
    assert!(r.steps.is_empty());
    assert!(!needs_attention(&r));
    // Hyper-V lists memory devices with nothing useful in them.
    let mut hv = pc("Microsoft Corporation", "Virtual Machine", "11/12/2020", vec![stick("M0001", "None", 0, 4, 0, 0, 0)]);
    hv.devices[0].manufacturer = "Microsoft Corporation".into();
    let r = check_from(&hv, "Intel(R) Xeon(R) CPU", None, TODAY);
    assert_eq!(r.verdict, Verdict::NotApplicable);
}

#[test]
fn intel_14900k_microcode() {
    let devs = four_slots(["DIMM_A2", "DIMM_B2"], "F5-6000J3038F16G", DDR5, 16, 4800, 4800, 1100);
    let table = pc(ASUS, "PRIME Z790-P WIFI", "08/20/2026", devs);
    let name = "Intel(R) Core(TM) i9-14900K";

    let old = check_from(&table, name, Some(0x129), TODAY);
    let f = old.findings.iter().find(|f| f.id == "ram:microcode").expect("microcode finding");
    assert_eq!(f.severity, Severity::Problem);
    assert!(f.detail.contains("0x129") && f.detail.contains("0x12B"), "{}", f.detail);
    assert!(old.steps[0].contains("Update the BIOS first"), "{}", old.steps[0]);
    assert!(steps(&old).contains("XMP I") && steps(&old).contains("Ai Tweaker"), "{}", steps(&old));
    assert!(needs_attention(&old));

    let fixed = check_from(&table, name, Some(0x12B), TODAY);
    assert!(!fixed.findings.iter().any(|f| f.id == "ram:microcode"));
    assert!(has(&fixed, "ram:cpu", Severity::Caution), "SA voltage caution always applies");
    assert!(!fixed.steps[0].contains("Update the BIOS"));

    let unknown = check_from(&table, name, None, TODAY);
    assert!(has(&unknown, "ram:microcode", Severity::Caution));

    // An HX laptop part and a 12th gen part get none of it.
    for other in ["Intel(R) Core(TM) i9-14900HX", "12th Gen Intel(R) Core(TM) i9-12900K", "Intel(R) Core(TM) i3-13100"] {
        let r = check_from(&table, other, Some(0x100), TODAY);
        assert!(!r.findings.iter().any(|f| f.id == "ram:microcode" || f.id == "ram:cpu"), "{}: {:?}", other, r.findings);
    }
    // 13th gen 65 W and KS parts are covered.
    for covered in ["13th Gen Intel(R) Core(TM) i7-13700", "Intel(R) Core(TM) i9-14900KS", "13th Gen Intel(R) Core(TM) i5-13600KF"] {
        let r = check_from(&table, covered, Some(0x100), TODAY);
        assert!(has(&r, "ram:microcode", Severity::Problem), "{}", covered);
    }
}

#[test]
fn bios_three_years_old() {
    let devs = four_slots(["DIMM_A2", "DIMM_B2"], "F5-6000J3038F16G", DDR5, 16, 6000, 6000, 1350);
    let r = check_from(&pc(MSI, "MAG B650 TOMAHAWK WIFI (MS-7D75)", "11/01/2023", devs), R7800X3D, None, TODAY);
    assert_eq!(r.board.bios_age_months, Some(36));
    assert_eq!(r.board.bios_date, "2023-11-01");
    let f = r.findings.iter().find(|f| f.id == "ram:bios").expect("bios finding");
    assert_eq!(f.severity, Severity::Caution);
    assert_eq!(f.title, "The BIOS may be out of date");
    assert!(!f.detail.contains("is out of date"));
    assert!(f.detail.contains("https://www.msi.com/Motherboard/MAG-B650-TOMAHAWK-WIFI/support"), "{}", f.detail);

    // A maker without a support template gets a web search.
    let devs = four_slots(["DIMM_A2", "DIMM_B2"], "F5-6000J3038F16G", DDR5, 16, 6000, 6000, 1350);
    let r = check_from(&pc("Gigabyte Technology Co., Ltd.", "B650 AORUS ELITE AX", "11/01/2023", devs), R7800X3D, None, TODAY);
    let f = r.findings.iter().find(|f| f.id == "ram:bios").unwrap();
    assert!(f.detail.contains("https://www.bing.com/search?q=Gigabyte%20B650%20AORUS%20ELITE%20AX%20BIOS"), "{}", f.detail);

    // 18 months or less: nothing.
    let devs = four_slots(["DIMM_A2", "DIMM_B2"], "F5-6000J3038F16G", DDR5, 16, 6000, 6000, 1350);
    let r = check_from(&pc(MSI, "X", "06/01/2025", devs), R7800X3D, None, TODAY);
    assert!(!r.findings.iter().any(|f| f.id == "ram:bios"));
}

#[test]
fn unknown_part_number_says_what_to_look_up() {
    let devs = four_slots(["DIMM_A2", "DIMM_B2"], "XYZ-SUPERRAM-9000", DDR5, 16, 4800, 4800, 1100);
    let r = check_from(&pc(ASUS, "TUF GAMING B650-PLUS", "06/01/2026", devs), R7800X3D, None, TODAY);
    assert_eq!(r.verdict, Verdict::Unknown);
    assert!(r.rated.is_none());
    let f = r.findings.iter().find(|f| f.id == "ram:speed").unwrap();
    assert!(f.detail.contains("XYZ-SUPERRAM-9000"), "{}", f.detail);
    assert!(r.steps.is_empty());
}

#[test]
fn module_reported_speed_is_the_second_source() {
    let devs = four_slots(["DIMM_A2", "DIMM_B2"], "XYZ-SUPERRAM", DDR4, 8, 3600, 2133, 1200);
    let r = check_from(&pc(MSI, "B450 TOMAHAWK MAX (MS-7C02)", "06/01/2026", devs), "AMD Ryzen 5 3600 6-Core Processor", None, TODAY);
    assert_eq!(r.verdict, Verdict::BelowRated);
    let rated = r.rated.as_ref().unwrap();
    assert_eq!((rated.speed_mhz, rated.confidence.as_str(), rated.cl), (3600, "module_reported", None));
    assert!(steps(&r).contains("A-XMP"), "MSI on AMD with DDR4: {}", steps(&r));

    // A module reporting a standard speed above what it runs at is held back by the platform.
    let devs = four_slots(["DIMM_A2", "DIMM_B2"], "XYZ-SUPERRAM", DDR4, 8, 3200, 2666, 1200);
    let r = check_from(&pc(MSI, "B450 TOMAHAWK MAX", "06/01/2026", devs), "Intel(R) Core(TM) i5-10400", None, TODAY);
    assert_eq!(r.verdict, Verdict::NoProfile);
}

#[test]
fn clock_reported_instead_of_data_rate_is_doubled() {
    let devs = four_slots(["DIMM_A2", "DIMM_B2"], "F4-3600C16-16GTZNC", DDR4, 16, 1067, 1067, 1200);
    let r = check_from(&pc(MSI, "X", "06/01/2026", devs), "", None, TODAY);
    assert_eq!(r.current_mhz, Some(2134));
    assert_eq!(r.verdict, Verdict::BelowRated);
}

#[test]
fn html_escapes_firmware_strings_and_is_self_contained() {
    let devs = four_slots(["DIMM_A2", "DIMM_B2"], "<script>alert('x')</script>", DDR5, 16, 4800, 4800, 1100);
    let r = check_from(&pc("Evil & Co <b>", "<img src=x onerror=alert(1)>", "06/01/2020", devs), R7800X3D, None, TODAY);
    let h = render_html(&r);
    assert!(h.starts_with("<!doctype html>"));
    assert!(!h.contains("<script"), "scripts must never appear");
    assert!(!h.contains("<img"));
    assert!(h.contains("&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt;"));
    assert!(h.contains("Evil &amp; Co &lt;b&gt;"));
    for external in ["<link", "<iframe", "@import", "url(", "src=\""] {
        assert!(!h.contains(external), "{}", external);
    }
    // The theme.
    for colour in ["rgb(14,14,13)", "rgb(26,27,25)", "rgb(242,239,232)", "rgb(150,150,142)", "rgb(212,255,0)", "rgb(255,184,48)", "rgb(255,120,110)"] {
        assert!(h.contains(colour), "{}", colour);
    }
    assert!(h.contains("Bahnschrift SemiBold Condensed"));

    let full = render_html(&am5_ddr5(4800));
    assert!(full.contains("<h1>Your RAM is running at 4800 MHz. The kit is rated for 6000 MHz.</h1>"));
    assert!(full.contains("<ol class=\"steps\">"));
    assert_eq!(full.matches("<li>").count(), 8);
    assert!(full.contains("<table>") && full.contains("F5-6000J3038F16G"));
    assert!(full.contains("<a href=\"https://www.asus.com/supportonly/ROG%20STRIX%20B650E-F%20GAMING%20WIFI/helpdesk_bios/\">"), "{}", full);
}

#[test]
fn text_report_has_the_headline_rows_and_steps() {
    let r = am5_ddr5(4800);
    let t = render_text(&r);
    assert!(t.starts_with("RAM AND BIOS CHECK\nYour RAM is running at 4800 MHz."));
    assert!(t.contains("DIMM_A1  empty"));
    assert!(t.contains("Rated:  DDR5-6000 CL30-38-38, about 1.35 V (typical), EXPO profile (from the part number)"), "{}", t);
    assert!(t.contains("  1. Before you start"));
    assert!(t.contains("  8. For this CPU"));
    let json = serde_json::to_value(&r).unwrap();
    assert_eq!(json["verdict"], "below_rated");
    assert_eq!(json["rated"]["confidence"], "part_number");
    assert_eq!(json["findings"][0]["severity"], "problem");
}

// ---------------------------------------------------------------- part numbers

/// One stick of `part` running at the JEDEC default, so a fast kit reads as below rated.
fn decode(part: &str, mem_type: u8) -> RamReport {
    let configured = if mem_type == DDR5 { 4000 } else { 2133 };
    let devs = vec![stick("DIMM_A2", part, mem_type, 16, configured, configured, 1200), stick("DIMM_B2", part, mem_type, 16, configured, configured, 1200)];
    check_from(&pc(MSI, "X", "06/01/2026", devs), "Intel(R) Core(TM) i7-12700K", None, TODAY)
}

/// (part number, DDR generation, rated speed, CL, standard speed (JEDEC) only)
const PARTS: &[(&str, u8, u32, Option<u32>, bool)] = &[
    // The examples in docs/RAMCHECK.md
    ("F5-6000J3038F16GX2-TZ5N", 5, 6000, Some(30), false),
    ("F4-3600C16D-32GTZN", 4, 3600, Some(16), false),
    ("CMK32GX5M2B5600C36", 5, 5600, Some(36), false),
    ("KF560C36BBEK2-32", 5, 6000, Some(36), false),
    ("KF432C16BB/16", 4, 3200, Some(16), false),
    ("BL16G36C16U4B", 4, 3600, Some(16), false),
    ("FF3D516G6000HC38ADC01", 5, 6000, Some(38), false),
    ("AX5U6000C3016G-DCLARBK", 5, 6000, Some(30), false),
    ("PVV532G600C36K", 5, 6000, Some(36), false),
    ("M323R2GA3BB0-CQK", 5, 4800, Some(40), true),
    // G.Skill (kits, and the per-stick numbers the modules report)
    ("F5-6000J3038F16G", 5, 6000, Some(30), false),
    ("F5-6000J3636F16GX2-TZ5RK", 5, 6000, Some(36), false),
    ("F5-6400J3239G16GX2-TZ5RK", 5, 6400, Some(32), false),
    ("F5-6000J3040G32GX2-TZ5NR", 5, 6000, Some(30), false),
    ("F5-5600J3636C16GX2-FX5", 5, 5600, Some(36), false),
    ("F4-3200C16D-16GVKB", 4, 3200, Some(16), false),
    ("F4-3600C18D-32GTZN", 4, 3600, Some(18), false),
    ("F4-3200C14D-16GTZ", 4, 3200, Some(14), false),
    ("F4-3200C16-8GVKB", 4, 3200, Some(16), false),
    ("F4-3600C16-16GTZNC", 4, 3600, Some(16), false),
    // Corsair
    ("CMH128GX5M2B6400C42", 5, 6400, Some(42), false),
    ("CMK16GX4M2B3200C16", 4, 3200, Some(16), false),
    ("CMK32GX4M2D3600C18", 4, 3600, Some(18), false),
    ("CMW32GX4M2E3200C16", 4, 3200, Some(16), false),
    ("CMK16GX4M2A2666C16", 4, 2666, Some(16), false),
    ("CMT32GX5M2B5600C36", 5, 5600, Some(36), false),
    ("CMH32GX5M2B6000C30", 5, 6000, Some(30), false),
    ("CMK32GX5M2B5200C40", 5, 5200, Some(40), false),
    ("CMSX16GX4M2A3200C22", 4, 3200, Some(22), true),
    // Kingston
    ("KF432C16BBK2/32", 4, 3200, Some(16), false),
    ("KF436C17BBK2/16", 4, 3600, Some(17), false),
    ("KF552C40BBK2-32", 5, 5200, Some(40), false),
    ("KF560C40BBK2-32", 5, 6000, Some(40), false),
    ("KF564C32RSK2-32", 5, 6400, Some(32), false),
    ("KF3200C16D4/8GX", 4, 3200, Some(16), false),
    ("KHX3200C16D4/8GX", 4, 3200, Some(16), false),
    ("KF432S20IB/16", 4, 3200, Some(20), false),
    ("HX432C16FB3K2/16", 4, 3200, Some(16), false),
    ("HX426C16FB3/8", 4, 2666, Some(16), false),
    ("KVR32N22S8/8", 4, 3200, Some(22), true),
    ("KVR26N19S8/8", 4, 2666, Some(19), true),
    ("KVR48U40BS8-16", 5, 4800, Some(40), true),
    // Crucial
    ("BL8G32C16U4B", 4, 3200, Some(16), false),
    ("BL2K8G32C16U4B", 4, 3200, Some(16), false),
    ("BLM16G40C18U4B", 4, 4000, Some(18), false),
    ("CT16G4DFRA32A", 4, 3200, None, true),
    ("CT16G4DFD832A", 4, 3200, None, true),
    ("CT16G4SFRA32A", 4, 3200, None, true),
    ("CT8G4DFS8266", 4, 2666, None, true),
    ("CT16G48C40U5", 5, 4800, Some(40), true),
    ("CT32G56C46U5", 5, 5600, Some(46), true),
    ("CP2K16G60C36U5B", 5, 6000, Some(36), false),
    ("CP16G56C46U5", 5, 5600, Some(46), true),
    // TeamGroup
    ("TLZGD416G3200HC16CDC01", 4, 3200, Some(16), false),
    ("FF3D532G6000HC30DC01", 5, 6000, Some(30), false),
    ("TEAMGROUP-UD4-3600", 4, 3600, None, false),
    ("TEAMGROUP-UD4-3200", 4, 3200, None, true),
    ("UD5-6000", 5, 6000, None, false),
    // ADATA / XPG
    ("AX5U5200C3816G-DCLARBK", 5, 5200, Some(38), false),
    ("AX5U6400C3216G-DCLARBK", 5, 6400, Some(32), false),
    ("AX4U320016G16A-DT60", 4, 3200, Some(16), false),
    ("AX4U360016G18A-DT60", 4, 3600, Some(18), false),
    // Patriot
    ("PSD516G480081", 5, 4800, None, true),
    ("PSD48G266681", 4, 2666, None, true),
    // Samsung OEM
    ("M323R4GA3BB0-CQK", 5, 4800, Some(40), true),
    ("M323R2GA3DB0-CWM", 5, 5600, Some(46), true),
    ("M425R1GB4BB0-CQK", 5, 4800, Some(40), true),
    ("M378A1K43EB2-CWE", 4, 3200, None, true),
    ("M378A2K43EB1-CWE", 4, 3200, None, true),
    ("M471A1K43DB1-CWE", 4, 3200, None, true),
    ("M378A1K43CB2-CTD", 4, 2666, None, true),
    // SK hynix OEM
    ("HMA81GU6CJR8N-VK", 4, 2666, None, true),
    ("HMA82GS6CJR8N-XN", 4, 3200, None, true),
    ("HMAA4GS6AJR8N-XN", 4, 3200, None, true),
    ("HMA851U6CJR6N-VK", 4, 2666, None, true),
    // Micron OEM
    ("MTA8ATF1G64AZ-2G6E1", 4, 2666, None, true),
    ("MTA16ATF2G64HZ-3G2J1", 4, 3200, None, true),
    ("MTA8ATF1G64HZ-3G2E1", 4, 3200, None, true),
    ("MTC8C1084S1UC48BA1", 5, 4800, None, true),
];

#[test]
fn part_numbers_decode() {
    for &(part, gen, speed, cl, jedec) in PARTS {
        let mem_type = if gen == 5 { DDR5 } else { DDR4 };
        let r = decode(part, mem_type);
        let rated = r.rated.as_ref().unwrap_or_else(|| panic!("{} should decode", part));
        assert_eq!(rated.confidence, "part_number", "{}", part);
        assert_eq!(rated.speed_mhz, speed, "{}", part);
        assert_eq!(rated.cl, cl, "{}", part);
        assert_eq!(rated.profile_kind == "JEDEC", jedec, "{}: {}", part, rated.profile_kind);
        assert_eq!(r.verdict, if jedec { Verdict::NoProfile } else { Verdict::BelowRated }, "{}", part);
        // Case and padding don't matter.
        let r2 = decode(&format!("  {}  ", part.to_lowercase()), mem_type);
        assert_eq!(r2.rated.as_ref().map(|x| x.speed_mhz), Some(speed), "{} lower-cased", part);
    }
    assert!(PARTS.len() >= 40, "{} cases", PARTS.len());
}

#[test]
fn unknown_part_numbers_are_unknown_not_guessed() {
    let unknown = [
        "PVS416G360C8K",      // Patriot Viper Steel DDR4: CL scheme not covered, so not guessed
        "BLS8G4D240FSB",      // Ballistix Sport: different scheme
        "99U5429-007.A00LF",  // Kingston internal OEM number
        "HMCG78AGBUA081N",    // SK hynix DDR5: speed not in the number we know
        "M378A1K43EB2-CXX",   // Samsung with an unknown speed code
        "CMK32GX5M2",         // cut off
        "F5-6000",            // cut off
        "KF560C",             // cut off
        "ABCDEFGH",
        "Undefined",
        "0000000000000000",
        "To Be Filled By O.E.M.",
        "",
    ];
    for part in unknown {
        let r = decode(part, DDR5);
        assert!(r.rated.as_ref().is_none_or(|x| x.confidence != "part_number"), "{} must not decode: {:?}", part, r.rated);
    }
    // A decode that disagrees with the firmware's memory type is not used.
    let r = decode("F5-6000J3038F16G", DDR4);
    assert!(r.rated.is_none(), "{:?}", r.rated);
    let r = decode("F4-3600C16-16GTZNC", DDR5);
    assert!(r.rated.is_none(), "{:?}", r.rated);
}

// ---------------------------------------------------------------- raw tables

fn structure(kind: u8, formatted: &[u8], strings: &[&str]) -> Vec<u8> {
    let mut v = vec![kind, (formatted.len() + 4) as u8, 0, 0];
    v.extend_from_slice(formatted);
    if strings.is_empty() {
        v.extend_from_slice(&[0, 0]);
    } else {
        for s in strings {
            v.extend_from_slice(s.as_bytes());
            v.push(0);
        }
        v.push(0);
    }
    v
}

#[allow(clippy::too_many_arguments)]
fn type17(slot: &str, bank: &str, size_mb: u32, part: &str, maker: &str, speed: u16, conf: u16, mv: u16, ext_speed: u32) -> Vec<u8> {
    let mut f = vec![0u8; 0x5C - 4];
    let mut put16 = |off: usize, v: u16| f[off - 4..off - 2].copy_from_slice(&v.to_le_bytes());
    put16(0x04, 0x0040); // array handle
    put16(0x06, 0xFFFE);
    put16(0x08, 64);
    put16(0x0A, 64);
    put16(0x0C, if size_mb == 0 { 0 } else if size_mb >= 0x7FFF { 0x7FFF } else { size_mb as u16 });
    put16(0x13, 0x0080);
    put16(0x15, speed);
    put16(0x20, conf);
    put16(0x22, mv);
    put16(0x24, mv);
    put16(0x26, mv);
    f[0x0E - 4] = smbios::FORM_DIMM;
    f[0x10 - 4] = 1; // device locator
    f[0x11 - 4] = 2; // bank locator
    f[0x12 - 4] = if size_mb == 0 { 2 } else { DDR5 };
    f[0x17 - 4] = 3; // manufacturer
    f[0x1A - 4] = 4; // part number
    f[0x1B - 4] = if size_mb == 0 { 0 } else { 2 };
    f[0x1C - 4..0x20 - 4].copy_from_slice(&size_mb.to_le_bytes());
    f[0x54 - 4..0x58 - 4].copy_from_slice(&ext_speed.to_le_bytes());
    f[0x58 - 4..0x5C - 4].copy_from_slice(&ext_speed.to_le_bytes());
    structure(17, &f, &[slot, bank, maker, part])
}

/// A table laid out like this development PC's (MSI Z890, Core Ultra 9 285K, two 64 GB
/// Corsair DDR5-6400 sticks in DIMMA2 and DIMMB2), without serial numbers.
fn recorded_table() -> Vec<u8> {
    let mut t = Vec::new();
    t.extend(structure(0, &[1, 2, 0x00, 0xF0, 3, 0xFF], &["American Megatrends International, LLC.", "1.AA0", "03/11/2026"]));
    t.extend(structure(2, &[1, 2, 3, 0], &["Micro-Star International Co., Ltd.", "MPG Z890 CARBON WIFI (MS-7E17)", "1.0"]));
    let mut p = vec![0u8; 0x30 - 4];
    p[0x04 - 4] = 1; // socket
    p[0x05 - 4] = 3;
    p[0x06 - 4] = 0xFE;
    p[0x07 - 4] = 2; // manufacturer
    p[0x10 - 4] = 3; // version
    p[0x28 - 4..0x2A - 4].copy_from_slice(&0x0101u16.to_le_bytes());
    t.extend(structure(4, &p, &["LGA1851", "Intel(R) Corporation", "Intel(R) Core(TM) Ultra 9 285K"]));
    let mut a = vec![0u8; 0x17 - 4];
    a[0] = 3; // location: board
    a[1] = 3; // use: system memory
    a[2] = 3;
    a[3..7].copy_from_slice(&0x8000_0000u32.to_le_bytes());
    a[7..9].copy_from_slice(&0xFFFEu16.to_le_bytes());
    a[9..11].copy_from_slice(&4u16.to_le_bytes());
    a[11..19].copy_from_slice(&(256u64 << 30).to_le_bytes());
    let mut arr = structure(16, &a, &[]);
    arr[2..4].copy_from_slice(&0x0040u16.to_le_bytes());
    t.extend(arr);
    t.extend(type17("DIMMA1", "BANK 0", 0, "Unknown", "Unknown", 0, 0, 0, 0));
    t.extend(type17("DIMMA2", "BANK 0", 65536, "CMH128GX5M2B6400C42  ", "Corsair", 6400, 6400, 1350, 0));
    t.extend(type17("DIMMB1", "BANK 1", 0, "Unknown", "Unknown", 0, 0, 0, 0));
    // This one reports its speeds through the extended fields, as SMBIOS 3.3 allows.
    t.extend(type17("DIMMB2", "BANK 1", 65536, "CMH128GX5M2B6400C42", "Corsair", 0xFFFF, 0xFFFF, 1350, 6400));
    t.extend(structure(127, &[], &[]));
    let mut raw = vec![0u8, 3, 8, 0];
    raw.extend_from_slice(&(t.len() as u32).to_le_bytes());
    raw.extend(t);
    raw
}

#[test]
fn recorded_table_parses_like_this_pc() {
    let raw = recorded_table();
    let s = smbios::parse(&raw);
    assert_eq!(s.version, (3, 8));
    assert_eq!(s.bios.as_ref().unwrap().version, "1.AA0");
    assert_eq!(s.board.as_ref().unwrap().product, "MPG Z890 CARBON WIFI (MS-7E17)");
    assert_eq!(s.processors[0].family, 0x0101);
    assert_eq!(s.arrays[0].slots, 4);
    assert_eq!(s.arrays[0].max_capacity_bytes, 256 << 30);
    assert_eq!(s.devices.len(), 4);
    let d = &s.devices[1];
    assert_eq!((d.size_mb, d.speed, d.configured_speed, d.configured_voltage_mv, d.rank), (Some(65536), 6400, 6400, 1350, 2));
    assert_eq!(d.part_number, "CMH128GX5M2B6400C42");
    assert_eq!((s.devices[3].speed, s.devices[3].configured_speed), (6400, 6400));
    assert!(!s.devices[0].populated());

    let r = check_from(&s, "", Some(0x121), TODAY);
    assert_eq!(r.verdict, Verdict::AtRated);
    assert_eq!(r.headline, "Your RAM is running at its rated 6400 MHz.");
    assert_eq!(r.slots_total, 4);
    assert_eq!(r.board.maker, "MSI");
    assert_eq!(r.board.bios_age_months, Some(8));
    assert_eq!(r.cpu.name, "Intel(R) Core(TM) Ultra 9 285K");
    assert_eq!(r.cpu.vendor, "Intel");
    assert_eq!(r.rated.as_ref().unwrap().cl, Some(42));
    assert_eq!(r.sticks[1].size_gb, Some(64.0));
    assert!(has(&r, "ram:cpu", Severity::Caution), "Arrow Lake SA caution");
    assert!(!r.findings.iter().any(|f| f.id == "ram:slots" || f.id == "ram:bios"));
    assert!(!needs_attention(&r));
}

#[test]
fn garbage_and_truncated_tables_never_panic() {
    let raw = recorded_table();
    for n in 0..=raw.len() {
        let s = smbios::parse(&raw[..n]);
        let r = check_from(&s, "", None, TODAY);
        let _ = render_text(&r);
        let _ = render_html(&r);
    }
    // Corrupt bytes one at a time.
    for i in 8..raw.len() {
        let mut bad = raw.clone();
        bad[i] ^= 0xA5;
        let _ = check_from(&smbios::parse(&bad), "", None, TODAY);
    }
    let mut seed: u32 = 12345;
    for len in [0usize, 1, 7, 8, 9, 64, 512, 4096] {
        let junk: Vec<u8> = (0..len)
            .map(|_| {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                (seed >> 16) as u8
            })
            .collect();
        let r = check_from(&smbios::parse(&junk), "garbage \u{0}\u{7f}", Some(0), (0, 0, 0));
        let _ = render_html(&r);
    }
    // Processors and boards with nothing in them.
    let s = Smbios { processors: vec![Processor::default()], board: Some(Baseboard::default()), ..Default::default() };
    assert_eq!(check_from(&s, "", None, TODAY).verdict, Verdict::NotApplicable);
}

// ---------------------------------------------------------------- the real PC

#[test]
fn real_check_on_this_pc() {
    let r = ramcheck::check();
    assert!(!r.headline.is_empty() && !r.summary.is_empty());
    let populated: Vec<_> = r.sticks.iter().filter(|s| s.populated).collect();
    if !r.sticks.is_empty() {
        assert_eq!(r.slots_total, r.sticks.len());
    }
    if r.verdict != Verdict::NotApplicable {
        assert!(!populated.is_empty());
    }
    assert_eq!(r.current_mhz, populated.iter().filter_map(|s| s.configured_mhz).min());
    for s in &r.sticks {
        if !s.populated {
            assert!(s.configured_mhz.is_none() && s.rated_mhz.is_none());
        }
    }
    match r.verdict {
        Verdict::BelowRated => {
            let rated = r.rated.as_ref().unwrap();
            assert!(r.current_mhz.unwrap() + 200 <= rated.speed_mhz);
            assert!(!r.steps.is_empty());
            assert!(needs_attention(&r));
        }
        Verdict::AtRated => {
            let rated = r.rated.as_ref().unwrap();
            assert!(r.current_mhz.unwrap() + 200 > rated.speed_mhz);
            assert!(r.steps.is_empty());
        }
        Verdict::Unknown => assert!(r.steps.is_empty()),
        _ => {}
    }
    if let Some(rated) = &r.rated {
        assert!(["part_number", "module_reported"].contains(&rated.confidence.as_str()));
        assert!(["XMP", "EXPO", "JEDEC"].contains(&rated.profile_kind.as_str()));
    }
    assert_eq!(r.fingerprint.len(), 16);
    assert_eq!(r.fingerprint, ramcheck::check().fingerprint, "the same PC gives the same fingerprint");
    assert!(render_text(&r).contains(&r.headline));
    assert!(!render_html(&r).contains("<script"));
}
