//! Reading and parsing the firmware's SMBIOS tables (BIOS, board, CPU, memory slots).
//!
//! `read_raw` asks Windows for the raw table (`GetSystemFirmwareTable('RSMB', 0)`); no admin
//! rights, WMI or PowerShell needed. `parse` is a pure function over those bytes, so tests can
//! feed it recorded or hand-built tables. It never panics: short structures, missing strings,
//! placeholder text and tables cut off half way all give a partial result.
//!
//! Only the structure types the RAM check needs are decoded: 0 (BIOS), 2 (baseboard),
//! 4 (processor), 16 (physical memory array) and 17 (memory device).

use serde::Serialize;

/// SMBIOS memory types (type 17, offset 0x12) the RAM check cares about.
pub const MEM_DDR3: u8 = 24;
pub const MEM_DDR4: u8 = 26;
pub const MEM_LPDDR3: u8 = 29;
pub const MEM_LPDDR4: u8 = 30;
pub const MEM_LPDDR4X: u8 = 31;
pub const MEM_DDR5: u8 = 34;
pub const MEM_LPDDR5: u8 = 35;

/// Form factors (type 17, offset 0x0E).
pub const FORM_CHIP: u8 = 0x05;
pub const FORM_DIMM: u8 = 0x09;
pub const FORM_ROW_OF_CHIPS: u8 = 0x0B;
pub const FORM_SODIMM: u8 = 0x0D;
pub const FORM_DIE: u8 = 0x10;

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Smbios {
    /// SMBIOS version from the RSMB header (major, minor); (0, 0) when unknown.
    pub version: (u8, u8),
    pub bios: Option<Bios>,
    pub board: Option<Baseboard>,
    pub processors: Vec<Processor>,
    pub arrays: Vec<MemoryArray>,
    /// One per slot, empty slots included (size 0).
    pub devices: Vec<MemoryDevice>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Bios {
    pub vendor: String,
    pub version: String,
    /// As the firmware wrote it, normally `mm/dd/yyyy`.
    pub release_date: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Baseboard {
    pub manufacturer: String,
    pub product: String,
    pub version: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Processor {
    pub manufacturer: String,
    /// The model name, e.g. "Intel(R) Core(TM) i9-14900K".
    pub version: String,
    pub family: u16,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct MemoryArray {
    pub handle: u16,
    /// 0x03 = system board.
    pub location: u8,
    /// 0x03 = system memory (others: video, flash, cache...).
    pub usage: u8,
    pub slots: u16,
    /// 0 when unknown.
    pub max_capacity_bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct MemoryDevice {
    pub handle: u16,
    pub array_handle: u16,
    /// 0 = empty slot. `None` = the firmware says "unknown".
    pub size_mb: Option<u64>,
    pub form_factor: u8,
    pub device_locator: String,
    pub bank_locator: String,
    pub memory_type: u8,
    pub type_detail: u16,
    /// Maximum speed the module supports, as the BIOS reports it (MT/s); 0 = unknown.
    pub speed: u32,
    /// The speed it is configured to run at now (MT/s on SMBIOS 2.7+); 0 = unknown.
    pub configured_speed: u32,
    pub manufacturer: String,
    pub part_number: String,
    /// 0 = unknown.
    pub rank: u8,
    /// Millivolts; 0 = unknown.
    pub min_voltage_mv: u16,
    pub max_voltage_mv: u16,
    pub configured_voltage_mv: u16,
}

impl MemoryDevice {
    pub fn populated(&self) -> bool {
        matches!(self.size_mb, Some(n) if n > 0) || (self.size_mb.is_none() && !self.part_number.is_empty())
    }
}

/// Parse the buffer `GetSystemFirmwareTable('RSMB', 0)` returns: an 8-byte header
/// (calling method, major, minor, DMI revision, u32 length) followed by the structure table.
/// Anything that doesn't add up ends the walk; what was read so far is kept.
pub fn parse(raw: &[u8]) -> Smbios {
    if raw.len() < 8 {
        return Smbios::default();
    }
    let len = u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]) as usize;
    let table = &raw[8..];
    let table = &table[..len.min(table.len())];
    let mut s = parse_table(table);
    s.version = (raw[1], raw[2]);
    s
}

/// Parse a bare structure table (no RSMB header).
pub fn parse_table(table: &[u8]) -> Smbios {
    let mut s = Smbios::default();
    let mut pos = 0usize;
    while pos + 4 <= table.len() {
        let kind = table[pos];
        let flen = table[pos + 1] as usize;
        if flen < 4 || pos + flen > table.len() {
            break;
        }
        let formatted = &table[pos..pos + flen];
        // The string set follows the formatted area and ends with two NULs.
        let Some(strings_len) = find_double_nul(&table[pos + flen..]) else {
            // Strings cut off: decode what we can from the formatted part and stop.
            decode(&mut s, kind, formatted, &[]);
            break;
        };
        let strings = split_strings(&table[pos + flen..pos + flen + strings_len]);
        decode(&mut s, kind, formatted, &strings);
        pos += flen + strings_len + 2;
        if kind == 127 {
            break;
        }
    }
    s
}

fn find_double_nul(b: &[u8]) -> Option<usize> {
    b.windows(2).position(|w| w == [0, 0])
}

fn split_strings(b: &[u8]) -> Vec<String> {
    if b.is_empty() {
        return Vec::new();
    }
    b.split(|&c| c == 0).map(|s| String::from_utf8_lossy(s).trim().to_string()).collect()
}

struct F<'a> {
    b: &'a [u8],
    strings: &'a [String],
}

impl F<'_> {
    fn u8(&self, off: usize) -> Option<u8> {
        self.b.get(off).copied()
    }
    fn u16(&self, off: usize) -> Option<u16> {
        Some(u16::from_le_bytes([*self.b.get(off)?, *self.b.get(off + 1)?]))
    }
    fn u32(&self, off: usize) -> Option<u32> {
        Some(u32::from_le_bytes([*self.b.get(off)?, *self.b.get(off + 1)?, *self.b.get(off + 2)?, *self.b.get(off + 3)?]))
    }
    fn u64(&self, off: usize) -> Option<u64> {
        Some(u64::from(self.u32(off)?) | (u64::from(self.u32(off + 4)?) << 32))
    }
    /// The string a formatted-area byte points at (1-based; 0 = none).
    fn s(&self, off: usize) -> String {
        match self.u8(off) {
            Some(i) if i > 0 => self.strings.get(i as usize - 1).cloned().unwrap_or_default(),
            _ => String::new(),
        }
    }
}

fn decode(s: &mut Smbios, kind: u8, b: &[u8], strings: &[String]) {
    let f = F { b, strings };
    let handle = f.u16(2).unwrap_or(0);
    match kind {
        0 => {
            s.bios.get_or_insert(Bios { vendor: f.s(0x04), version: f.s(0x05), release_date: f.s(0x08) });
        }
        2 => {
            s.board.get_or_insert(Baseboard { manufacturer: f.s(0x04), product: f.s(0x05), version: f.s(0x06) });
        }
        4 => {
            let mut family = f.u8(0x06).unwrap_or(0) as u16;
            if family == 0xFE {
                family = f.u16(0x28).unwrap_or(family);
            }
            s.processors.push(Processor { manufacturer: f.s(0x07), version: f.s(0x10), family });
        }
        16 => {
            let max_kb = f.u32(0x07).unwrap_or(0);
            let max_capacity_bytes = if max_kb == 0x8000_0000 {
                f.u64(0x0F).unwrap_or(0)
            } else {
                u64::from(max_kb) * 1024
            };
            s.arrays.push(MemoryArray {
                handle,
                location: f.u8(0x04).unwrap_or(0),
                usage: f.u8(0x05).unwrap_or(0),
                slots: f.u16(0x0D).unwrap_or(0),
                max_capacity_bytes,
            });
        }
        17 => {
            let size_mb = match f.u16(0x0C) {
                None | Some(0xFFFF) => None,
                Some(0) => Some(0),
                Some(0x7FFF) => f.u32(0x1C).map(|x| u64::from(x & 0x7FFF_FFFF)),
                Some(v) if v & 0x8000 != 0 => Some(u64::from(v & 0x7FFF) / 1024), // KB units
                Some(v) => Some(u64::from(v)),
            };
            let speed = match f.u16(0x15).unwrap_or(0) {
                0xFFFF => f.u32(0x54).unwrap_or(0),
                v => u32::from(v),
            };
            let configured_speed = match f.u16(0x20).unwrap_or(0) {
                0xFFFF => f.u32(0x58).unwrap_or(0),
                v => u32::from(v),
            };
            s.devices.push(MemoryDevice {
                handle,
                array_handle: f.u16(0x04).unwrap_or(0),
                size_mb,
                form_factor: f.u8(0x0E).unwrap_or(0),
                device_locator: f.s(0x10),
                bank_locator: f.s(0x11),
                memory_type: f.u8(0x12).unwrap_or(0),
                type_detail: f.u16(0x13).unwrap_or(0),
                speed,
                configured_speed,
                manufacturer: f.s(0x17),
                part_number: f.s(0x1A),
                rank: f.u8(0x1B).map(|a| a & 0x0F).unwrap_or(0),
                min_voltage_mv: f.u16(0x22).unwrap_or(0),
                max_voltage_mv: f.u16(0x24).unwrap_or(0),
                configured_voltage_mv: f.u16(0x26).unwrap_or(0),
            });
        }
        _ => {}
    }
}

/// The raw RSMB buffer for this PC, or `None` if Windows won't give it (very old systems,
/// some virtual machines). Read-only; needs no special rights.
pub fn read_raw() -> Option<Vec<u8>> {
    let size = firmware_table(&mut []);
    if size == 0 || size > 16 * 1024 * 1024 {
        return None;
    }
    let mut buf = vec![0u8; size as usize];
    let got = firmware_table(&mut buf);
    if got == 0 || got as usize > buf.len() {
        return None;
    }
    buf.truncate(got as usize);
    Some(buf)
}

/// The one `unsafe` call in this module: `GetSystemFirmwareTable('RSMB', 0, buf, len)`.
/// Windows writes at most `buf.len()` bytes into `buf` and returns the size of the whole
/// table (which may be larger than `buf`, or 0 on failure). With an empty slice it only
/// reports the size and writes nothing. The pointer and length come from one live slice,
/// so the call can never write outside it.
fn firmware_table(buf: &mut [u8]) -> u32 {
    use windows_sys::Win32::System::SystemInformation::{GetSystemFirmwareTable, RSMB};
    let ptr = if buf.is_empty() { std::ptr::null_mut() } else { buf.as_mut_ptr() };
    // SAFETY: see the doc comment; `ptr` is null or valid for `buf.len()` bytes.
    unsafe { GetSystemFirmwareTable(RSMB, 0, ptr, buf.len() as u32) }
}

/// Read and parse this PC's table. Empty when it can't be read.
pub fn read() -> Smbios {
    read_raw().map(|r| parse(&r)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn bios_and_board_strings() {
        let mut t = structure(0, &[1, 2, 0, 0, 3, 0], &["American Megatrends", "1.AA0", "03/11/2026"]);
        t.extend(structure(2, &[1, 2, 3, 0], &["Micro-Star", "MPG Z890", "1.0"]));
        t.extend(structure(127, &[], &[]));
        let s = parse_table(&t);
        let b = s.bios.unwrap();
        assert_eq!((b.vendor.as_str(), b.version.as_str(), b.release_date.as_str()), ("American Megatrends", "1.AA0", "03/11/2026"));
        assert_eq!(s.board.unwrap().product, "MPG Z890");
    }

    #[test]
    fn garbage_never_panics() {
        for n in 0..300usize {
            let junk: Vec<u8> = (0..n).map(|i| (i * 37 % 251) as u8).collect();
            let _ = parse(&junk);
            let _ = parse_table(&junk);
        }
        let _ = parse(&[0, 3, 4, 0, 255, 255, 255, 255, 17, 200]);
        assert_eq!(parse(&[]), Smbios::default());
    }

    #[test]
    fn missing_string_index_is_empty() {
        let t = structure(2, &[5, 0, 0, 0], &["only one"]);
        assert_eq!(parse_table(&t).board.unwrap().manufacturer, "");
    }
}
