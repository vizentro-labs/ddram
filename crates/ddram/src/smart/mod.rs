// Copyright 2026 Vizentro Labs
// SPDX-License-Identifier: Apache-2.0

//! NVMe SMART / Health Information (Log Page 02h) parsing, OS readers, and
//! endurance budgeting for the write governor.
//!
//! The parser is portable and works on the raw 512-byte log page. Readers that
//! fetch the page from the device exist for Linux (admin passthrough ioctl),
//! Windows (`IOCTL_STORAGE_QUERY_PROPERTY`) and macOS (IOKit NVMe SMART
//! plug-in). See `docs/research/ssd-requirements.md` for field semantics.

use std::fmt;
use std::io;
use std::path::Path;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

pub const LOG_PAGE_LEN: usize = 512;

/// Bytes per NVMe "data unit" (1000 units of 512 bytes).
pub const DATA_UNIT_BYTES: u128 = 512_000;

/// Critical Warning bits (byte 0).
pub mod critical {
    pub const SPARE_BELOW_THRESHOLD: u8 = 1 << 0;
    pub const TEMPERATURE: u8 = 1 << 1;
    pub const RELIABILITY_DEGRADED: u8 = 1 << 2;
    pub const READ_ONLY: u8 = 1 << 3;
    pub const VOLATILE_BACKUP_FAILED: u8 = 1 << 4;
    pub const PMR_READ_ONLY: u8 = 1 << 5;
}

/// Decoded NVMe SMART / Health Information log page.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HealthLog {
    pub critical_warning: u8,
    /// Composite temperature in Kelvin.
    pub composite_temp_k: u16,
    pub available_spare_pct: u8,
    pub available_spare_threshold_pct: u8,
    /// Vendor estimate of life used; may exceed 100.
    pub percentage_used: u8,
    pub endurance_group_critical_warning: u8,
    pub data_units_read: u128,
    pub data_units_written: u128,
    pub host_read_commands: u128,
    pub host_write_commands: u128,
    pub controller_busy_minutes: u128,
    pub power_cycles: u128,
    pub power_on_hours: u128,
    pub unsafe_shutdowns: u128,
    pub media_errors: u128,
    pub error_log_entries: u128,
    pub warning_temp_minutes: u32,
    pub critical_temp_minutes: u32,
    /// Temperature sensors 1-8 in Kelvin (0 = not implemented).
    pub temp_sensors_k: [u16; 8],
    pub thermal_mgmt_t1_transitions: u32,
    pub thermal_mgmt_t2_transitions: u32,
    pub thermal_mgmt_t1_seconds: u32,
    pub thermal_mgmt_t2_seconds: u32,
}

fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn le128(b: &[u8], o: usize) -> u128 {
    u128::from_le_bytes(b[o..o + 16].try_into().unwrap())
}

impl HealthLog {
    /// Parse a raw log page (NVMe Base Spec, Figure "SMART / Health Information Log Page").
    pub fn parse(b: &[u8]) -> io::Result<HealthLog> {
        if b.len() < LOG_PAGE_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "SMART log shorter than 512 bytes",
            ));
        }
        // Some drives occasionally return an all-zero page (temperature 0 K);
        // that is a failed read, not a healthy drive (nvme-cli issue #1729).
        if b[..LOG_PAGE_LEN].iter().all(|&x| x == 0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "all-zero SMART log (invalid read)",
            ));
        }
        let mut temp_sensors_k = [0u16; 8];
        for (i, t) in temp_sensors_k.iter_mut().enumerate() {
            *t = le16(b, 200 + 2 * i);
        }
        Ok(HealthLog {
            critical_warning: b[0],
            composite_temp_k: le16(b, 1),
            available_spare_pct: b[3],
            available_spare_threshold_pct: b[4],
            percentage_used: b[5],
            endurance_group_critical_warning: b[6],
            data_units_read: le128(b, 32),
            data_units_written: le128(b, 48),
            host_read_commands: le128(b, 64),
            host_write_commands: le128(b, 80),
            controller_busy_minutes: le128(b, 96),
            power_cycles: le128(b, 112),
            power_on_hours: le128(b, 128),
            unsafe_shutdowns: le128(b, 144),
            media_errors: le128(b, 160),
            error_log_entries: le128(b, 176),
            warning_temp_minutes: le32(b, 192),
            critical_temp_minutes: le32(b, 196),
            temp_sensors_k,
            thermal_mgmt_t1_transitions: le32(b, 216),
            thermal_mgmt_t2_transitions: le32(b, 220),
            thermal_mgmt_t1_seconds: le32(b, 224),
            thermal_mgmt_t2_seconds: le32(b, 228),
        })
    }

    /// Serialize back to a 512-byte page (used for tests and fixtures).
    pub fn to_bytes(&self) -> [u8; LOG_PAGE_LEN] {
        let mut b = [0u8; LOG_PAGE_LEN];
        b[0] = self.critical_warning;
        b[1..3].copy_from_slice(&self.composite_temp_k.to_le_bytes());
        b[3] = self.available_spare_pct;
        b[4] = self.available_spare_threshold_pct;
        b[5] = self.percentage_used;
        b[6] = self.endurance_group_critical_warning;
        let w = |b: &mut [u8; LOG_PAGE_LEN], o: usize, v: u128| {
            b[o..o + 16].copy_from_slice(&v.to_le_bytes())
        };
        w(&mut b, 32, self.data_units_read);
        w(&mut b, 48, self.data_units_written);
        w(&mut b, 64, self.host_read_commands);
        w(&mut b, 80, self.host_write_commands);
        w(&mut b, 96, self.controller_busy_minutes);
        w(&mut b, 112, self.power_cycles);
        w(&mut b, 128, self.power_on_hours);
        w(&mut b, 144, self.unsafe_shutdowns);
        w(&mut b, 160, self.media_errors);
        w(&mut b, 176, self.error_log_entries);
        b[192..196].copy_from_slice(&self.warning_temp_minutes.to_le_bytes());
        b[196..200].copy_from_slice(&self.critical_temp_minutes.to_le_bytes());
        for (i, t) in self.temp_sensors_k.iter().enumerate() {
            b[200 + 2 * i..202 + 2 * i].copy_from_slice(&t.to_le_bytes());
        }
        b[216..220].copy_from_slice(&self.thermal_mgmt_t1_transitions.to_le_bytes());
        b[220..224].copy_from_slice(&self.thermal_mgmt_t2_transitions.to_le_bytes());
        b[224..228].copy_from_slice(&self.thermal_mgmt_t1_seconds.to_le_bytes());
        b[228..232].copy_from_slice(&self.thermal_mgmt_t2_seconds.to_le_bytes());
        b
    }

    pub fn composite_temp_c(&self) -> i32 {
        self.composite_temp_k as i32 - 273
    }

    pub fn bytes_written(&self) -> u128 {
        self.data_units_written * DATA_UNIT_BYTES
    }

    pub fn bytes_read(&self) -> u128 {
        self.data_units_read * DATA_UNIT_BYTES
    }

    /// True if any critical warning bit makes the drive unsuitable as a
    /// memory tier (spare exhausted, degraded, read-only, backup failed).
    pub fn is_unsafe_for_tiering(&self) -> bool {
        self.critical_warning
            & (critical::SPARE_BELOW_THRESHOLD
                | critical::RELIABILITY_DEGRADED
                | critical::READ_ONLY
                | critical::VOLATILE_BACKUP_FAILED)
            != 0
            || self.endurance_group_critical_warning != 0
    }

    pub fn warnings(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        let c = self.critical_warning;
        if c & critical::SPARE_BELOW_THRESHOLD != 0 {
            v.push("available spare below threshold");
        }
        if c & critical::TEMPERATURE != 0 {
            v.push("temperature outside threshold");
        }
        if c & critical::RELIABILITY_DEGRADED != 0 {
            v.push("NVM subsystem reliability degraded");
        }
        if c & critical::READ_ONLY != 0 {
            v.push("media placed in read-only mode");
        }
        if c & critical::VOLATILE_BACKUP_FAILED != 0 {
            v.push("volatile memory backup device failed");
        }
        if c & critical::PMR_READ_ONLY != 0 {
            v.push("persistent memory region read-only");
        }
        if self.percentage_used >= 100 {
            v.push("rated endurance consumed (percentage used >= 100)");
        }
        v
    }
}

impl fmt::Display for HealthLog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tb = |u: u128| u as f64 * DATA_UNIT_BYTES as f64 / 1e12;
        writeln!(
            f,
            "critical_warning          : {:#04x}",
            self.critical_warning
        )?;
        writeln!(
            f,
            "temperature               : {} C",
            self.composite_temp_c()
        )?;
        writeln!(
            f,
            "available_spare           : {}% (threshold {}%)",
            self.available_spare_pct, self.available_spare_threshold_pct
        )?;
        writeln!(f, "percentage_used           : {}%", self.percentage_used)?;
        writeln!(
            f,
            "data_units_read           : {} ({:.2} TB)",
            self.data_units_read,
            tb(self.data_units_read)
        )?;
        writeln!(
            f,
            "data_units_written        : {} ({:.2} TB)",
            self.data_units_written,
            tb(self.data_units_written)
        )?;
        writeln!(f, "host_read_commands        : {}", self.host_read_commands)?;
        writeln!(
            f,
            "host_write_commands       : {}",
            self.host_write_commands
        )?;
        writeln!(
            f,
            "controller_busy_time      : {} min",
            self.controller_busy_minutes
        )?;
        writeln!(f, "power_cycles              : {}", self.power_cycles)?;
        writeln!(f, "power_on_hours            : {}", self.power_on_hours)?;
        writeln!(f, "unsafe_shutdowns          : {}", self.unsafe_shutdowns)?;
        writeln!(f, "media_errors              : {}", self.media_errors)?;
        writeln!(f, "error_log_entries         : {}", self.error_log_entries)?;
        writeln!(
            f,
            "warning_temp_time         : {} min",
            self.warning_temp_minutes
        )?;
        write!(
            f,
            "critical_temp_time        : {} min",
            self.critical_temp_minutes
        )
    }
}

/// How much the tier may write per day without exceeding the drive's rated
/// endurance within the planned service life.
#[derive(Clone, Debug, PartialEq)]
pub struct EnduranceBudget {
    /// Estimated total endurance in bytes (TBW).
    pub total_bytes: f64,
    pub written_bytes: f64,
    pub remaining_bytes: f64,
    pub remaining_days: f64,
    /// Sustainable bytes per day across all writers on the drive.
    pub daily_bytes: f64,
    /// How the total was obtained.
    pub source: &'static str,
}

impl EnduranceBudget {
    /// Derive a budget from the health log.
    ///
    /// * `rated_tbw` — the datasheet TBW in bytes if known. Otherwise it is
    ///   extrapolated as `written / (percentage_used / 100)` once the drive
    ///   reports at least 1%; below that, `fallback_tbw` is used.
    /// * `service_years` — planned life (typically the warranty, 5 years).
    pub fn from_health(
        h: &HealthLog,
        rated_tbw: Option<f64>,
        fallback_tbw: f64,
        service_years: f64,
    ) -> EnduranceBudget {
        let written = h.bytes_written() as f64;
        let (total, source) = match rated_tbw {
            Some(t) => (t, "datasheet"),
            None if h.percentage_used >= 1 => (
                written * 100.0 / h.percentage_used as f64,
                "extrapolated from percentage_used",
            ),
            None => (fallback_tbw, "fallback"),
        };
        // Percentage used is the vendor's wear estimate and includes write
        // amplification; trust it when it is more pessimistic than host bytes.
        let used_by_pct = total * h.percentage_used as f64 / 100.0;
        let consumed = written.max(used_by_pct);
        let remaining = (total - consumed).max(0.0);
        let elapsed_days = h.power_on_hours as f64 / 24.0;
        let remaining_days = (service_years * 365.0 - elapsed_days).max(30.0);
        EnduranceBudget {
            total_bytes: total,
            written_bytes: written,
            remaining_bytes: remaining,
            remaining_days,
            daily_bytes: remaining / remaining_days,
            source,
        }
    }

    /// Convert a share of the daily budget to a governor rate (bytes/s).
    pub fn rate_for_share(&self, share: f64) -> u64 {
        (self.daily_bytes * share / 86_400.0).max(0.0) as u64
    }
}

/// TBW from a DWPD rating: `DWPD × capacity × 365 × warranty years`.
pub fn tbw_from_dwpd(dwpd: f64, capacity_bytes: f64, warranty_years: f64) -> f64 {
    dwpd * capacity_bytes * 365.0 * warranty_years
}

/// Identifies an NVMe device the OS readers can query.
#[derive(Clone, Debug)]
pub struct Device {
    /// `/dev/nvme0`, `\\.\PhysicalDrive0`, or an IOKit BSD name like `disk0`.
    pub path: String,
}

/// Find the NVMe device holding `path` (e.g. the backing file).
pub fn device_for_path(path: &Path) -> io::Result<Device> {
    #[cfg(target_os = "linux")]
    return linux::device_for_path(path);
    #[cfg(target_os = "macos")]
    return macos::device_for_path(path);
    #[cfg(windows)]
    return windows::device_for_path(path);
    #[allow(unreachable_code)]
    {
        let _ = path;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "SMART not supported on this OS",
        ))
    }
}

/// Read the raw 512-byte health log page from a device.
pub fn read_health_raw(dev: &Device) -> io::Result<[u8; LOG_PAGE_LEN]> {
    #[cfg(target_os = "linux")]
    return linux::read_health(dev);
    #[cfg(target_os = "macos")]
    return macos::read_health(dev);
    #[cfg(windows)]
    return windows::read_health(dev);
    #[allow(unreachable_code)]
    {
        let _ = dev;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "SMART not supported on this OS",
        ))
    }
}

pub fn read_health(dev: &Device) -> io::Result<HealthLog> {
    HealthLog::parse(&read_health_raw(dev)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> HealthLog {
        HealthLog {
            critical_warning: 0,
            composite_temp_k: 311,
            available_spare_pct: 100,
            available_spare_threshold_pct: 10,
            percentage_used: 4,
            data_units_read: 61_234_567,
            data_units_written: 48_123_456,
            host_read_commands: 912_345_678,
            host_write_commands: 701_234_567,
            controller_busy_minutes: 1_234,
            power_cycles: 1_024,
            power_on_hours: 8_760,
            unsafe_shutdowns: 37,
            temp_sensors_k: [311, 318, 0, 0, 0, 0, 0, 0],
            ..Default::default()
        }
    }

    #[test]
    fn roundtrip_and_offsets() {
        let h = sample();
        let b = h.to_bytes();
        // Spot-check offsets against the NVMe spec layout.
        assert_eq!(u16::from_le_bytes([b[1], b[2]]), 311);
        assert_eq!(b[5], 4);
        assert_eq!(
            u128::from_le_bytes(b[48..64].try_into().unwrap()),
            48_123_456
        );
        assert_eq!(u128::from_le_bytes(b[128..144].try_into().unwrap()), 8_760);
        assert_eq!(u16::from_le_bytes([b[202], b[203]]), 318);
        assert_eq!(HealthLog::parse(&b).unwrap(), h);
        assert_eq!(h.composite_temp_c(), 38);
        assert_eq!(h.bytes_written(), 48_123_456u128 * 512_000);
    }

    #[test]
    fn rejects_short_and_all_zero_pages() {
        assert!(HealthLog::parse(&[0u8; 100]).is_err());
        assert!(HealthLog::parse(&[0u8; LOG_PAGE_LEN]).is_err());
    }

    // The fixtures below are public smartctl captures quoted in
    // docs/research/ssd-requirements.md section 6.

    #[test]
    fn public_consumer_970_evo_plus() {
        let h = HealthLog {
            composite_temp_k: 35 + 273,
            available_spare_pct: 100,
            available_spare_threshold_pct: 10,
            percentage_used: 0,
            data_units_read: 2_025_034,
            data_units_written: 1_969_459,
            host_read_commands: 13_927_240,
            host_write_commands: 88_382_369,
            controller_busy_minutes: 892,
            power_cycles: 16,
            power_on_hours: 3149,
            unsafe_shutdowns: 12,
            error_log_entries: 22,
            ..Default::default()
        };
        let h = HealthLog::parse(&h.to_bytes()).unwrap();
        assert!((h.bytes_written() as f64 / 1e12 - 1.008).abs() < 0.001);
        assert!(!h.is_unsafe_for_tiering());
        // 150 TBW rating over 5 years: ~88 GB/day remain.
        let e = EnduranceBudget::from_health(&h, Some(150e12), 300e12, 5.0);
        let gb_day = e.daily_bytes / 1e9;
        assert!((80.0..95.0).contains(&gb_day), "{gb_day}");
    }

    #[test]
    fn public_datacenter_pm9a3() {
        let h = HealthLog {
            composite_temp_k: 22 + 273,
            available_spare_pct: 100,
            percentage_used: 3,
            data_units_read: 62_679_808,
            data_units_written: 639_454_618,
            host_write_commands: 16_276_566_236,
            power_on_hours: 4369,
            ..Default::default()
        };
        let rated = tbw_from_dwpd(1.0, 3.84e12, 5.0);
        assert_eq!(rated, 7008e12);
        let e = EnduranceBudget::from_health(&h, Some(rated), 300e12, 5.0);
        // Host bytes (327 TB) exceed 3% of 7008 TB (210 TB), so they count.
        assert!((e.written_bytes / 1e12 - 327.4).abs() < 0.1);
        let tb_day = e.daily_bytes / 1e12;
        assert!((4.0..4.2).contains(&tb_day), "{tb_day}");
    }

    #[test]
    fn public_high_waf_drive_trusts_percentage_used() {
        // 31% used after only 2.86 TB of small random host writes.
        let h = HealthLog {
            composite_temp_k: 32 + 273,
            available_spare_pct: 100,
            available_spare_threshold_pct: 10,
            percentage_used: 31,
            data_units_read: 1_299_288,
            data_units_written: 5_592_478,
            power_on_hours: 687,
            unsafe_shutdowns: 21,
            ..Default::default()
        };
        // Without a rating, life extrapolates to ~9.2 TB of host writes.
        let e = EnduranceBudget::from_health(&h, None, 300e12, 5.0);
        assert!(
            (e.total_bytes / 1e12 - 9.24).abs() < 0.05,
            "{}",
            e.total_bytes
        );
        // With an optimistic 600 TBW datasheet value, wear (31%) still
        // dominates host bytes: 186 TB consumed, not 2.86 TB.
        let e = EnduranceBudget::from_health(&h, Some(600e12), 300e12, 5.0);
        assert!(
            (e.remaining_bytes / 1e12 - 414.0).abs() < 0.5,
            "{}",
            e.remaining_bytes
        );
    }

    #[test]
    fn unsafe_bits() {
        let mut h = sample();
        assert!(!h.is_unsafe_for_tiering());
        h.critical_warning = critical::TEMPERATURE;
        assert!(!h.is_unsafe_for_tiering());
        h.critical_warning = critical::READ_ONLY;
        assert!(h.is_unsafe_for_tiering());
        assert!(h.warnings().iter().any(|w| w.contains("read-only")));
    }

    #[test]
    fn endurance_extrapolated() {
        // 24.6 TB written at 4% used => ~616 TBW total.
        let h = sample();
        let e = EnduranceBudget::from_health(&h, None, 300e12, 5.0);
        assert_eq!(e.source, "extrapolated from percentage_used");
        assert!(
            (e.total_bytes / 1e12 - 615.98).abs() < 0.1,
            "{}",
            e.total_bytes
        );
        assert!(e.remaining_bytes > 0.0);
        // 365 of 1825 days elapsed.
        assert!((e.remaining_days - 1460.0).abs() < 1.0);
        assert!(e.rate_for_share(0.5) > 0);
    }

    #[test]
    fn dwpd_conversion() {
        // 1 DWPD, 1 TB, 5 years = 1825 TBW.
        assert_eq!(tbw_from_dwpd(1.0, 1e12, 5.0), 1825e12);
    }
}
