//! Parsing and transport types for host CPU, memory, and load statistics.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Upper bound on reported cores; protects the phone from a malformed host.
pub const MAX_CORES: usize = 256;

/// A parsed `/proc/stat` snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CpuSnapshot {
    /// Aggregate times across all cores.
    pub total: CpuTimes,
    /// Times for each logical core, in kernel order.
    pub per_core: Vec<CpuTimes>,
}

/// Jiffy counters for one CPU row of `/proc/stat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CpuTimes {
    /// All non-idle and idle time, excluding `guest` and `guest_nice`.
    pub total: u64,
    /// Time spent idle or waiting for I/O.
    pub idle: u64,
}

/// CPU utilisation computed from two snapshots.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CpuUsage {
    /// Aggregate utilisation in percent, `0.0..=100.0`.
    pub overall: f32,
    /// Per-core utilisation in percent; empty if the core count changed.
    pub per_core: Vec<f32>,
}

impl CpuSnapshot {
    /// Parses the first `cpu` and `cpuN` rows of `/proc/stat`.
    ///
    /// # Errors
    ///
    /// Returns [`ProcError`] when the aggregate row is missing, a `cpu` row has
    /// fewer than eight counters, a counter is not a number, or the machine
    /// reports more than [`MAX_CORES`] logical cores.
    pub fn parse(proc_stat: &str) -> Result<Self, ProcError> {
        let mut aggregate = None;
        let mut per_core = Vec::new();

        for (index, line) in proc_stat.lines().enumerate() {
            if !line.starts_with("cpu") {
                continue;
            }
            let mut fields = line.split_whitespace();
            let label = fields
                .next()
                .ok_or(ProcError::MalformedCpuLine { line: index + 1 })?;
            let times = CpuTimes::parse_fields(fields, index + 1)?;

            if label == "cpu" {
                aggregate = Some(times);
            } else {
                if per_core.len() == MAX_CORES {
                    return Err(ProcError::TooManyCores { count: MAX_CORES });
                }
                per_core.push(times);
            }
        }

        let total = aggregate.ok_or(ProcError::MissingAggregateCpu)?;
        Ok(Self { total, per_core })
    }

    /// Computes utilisation between this later snapshot and an earlier one.
    ///
    /// A core count change yields an empty `per_core` vector: the two samples
    /// no longer describe the same machine.
    #[must_use]
    pub fn usage_since(&self, earlier: &Self) -> CpuUsage {
        let overall = percent_between(earlier.total, self.total);
        let per_core = if self.per_core.len() == earlier.per_core.len() {
            self.per_core
                .iter()
                .zip(&earlier.per_core)
                .map(|(now, before)| percent_between(*before, *now))
                .collect()
        } else {
            Vec::new()
        };
        CpuUsage { overall, per_core }
    }
}

impl CpuTimes {
    fn parse_fields<'a>(
        fields: impl Iterator<Item = &'a str>,
        line: usize,
    ) -> Result<Self, ProcError> {
        // user, nice, system, idle, iowait, irq, softirq, steal; guest rows are
        // already included in user/nice, so the kernel documentation says not
        // to add them again.
        let values = fields
            .take(8)
            .map(|value| {
                value
                    .parse::<u64>()
                    .map_err(|_| ProcError::MalformedCpuLine { line })
            })
            .collect::<Result<Vec<_>, _>>()?;

        if values.len() < 8 {
            return Err(ProcError::MalformedCpuLine { line });
        }

        Ok(Self {
            total: values.iter().sum(),
            idle: values[3] + values[4],
        })
    }
}

fn percent_between(earlier: CpuTimes, later: CpuTimes) -> f32 {
    let total = later.total.saturating_sub(earlier.total);
    if total == 0 {
        return 0.0;
    }
    let idle = later.idle.saturating_sub(earlier.idle).min(total);
    let busy = total - idle;
    (busy as f32 / total as f32) * 100.0
}

/// Memory counters from `/proc/meminfo`, in kibibytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MemoryInfo {
    /// Total usable RAM.
    pub total_kib: u64,
    /// RAM available for new workloads.
    pub available_kib: u64,
    /// Total swap space.
    pub swap_total_kib: u64,
    /// Unused swap space.
    pub swap_free_kib: u64,
}

impl MemoryInfo {
    /// Parses `/proc/meminfo`. Swap keys are optional and default to zero.
    ///
    /// # Errors
    ///
    /// Returns [`ProcError`] when `MemTotal` or `MemAvailable` is missing or a
    /// numeric field is malformed.
    pub fn parse(meminfo: &str) -> Result<Self, ProcError> {
        let mut total_kib = None;
        let mut available_kib = None;
        let mut swap_total_kib = 0;
        let mut swap_free_kib = 0;

        for line in meminfo.lines() {
            let Some((key, rest)) = line.split_once(':') else {
                continue;
            };
            let value = rest
                .split_whitespace()
                .next()
                .and_then(|raw| raw.parse::<u64>().ok());
            let Some(value) = value else {
                if matches!(key, "MemTotal" | "MemAvailable" | "SwapTotal" | "SwapFree") {
                    return Err(ProcError::MalformedMemoryLine {
                        key: key.to_owned(),
                    });
                }
                continue;
            };

            match key {
                "MemTotal" => total_kib = Some(value),
                "MemAvailable" => available_kib = Some(value),
                "SwapTotal" => swap_total_kib = value,
                "SwapFree" => swap_free_kib = value,
                _ => {}
            }
        }

        Ok(Self {
            total_kib: total_kib.ok_or(ProcError::MissingMemTotal)?,
            available_kib: available_kib.ok_or(ProcError::MissingMemAvailable)?,
            swap_total_kib,
            swap_free_kib,
        })
    }

    /// Used RAM, saturating at zero.
    #[must_use]
    pub fn used_kib(&self) -> u64 {
        self.total_kib.saturating_sub(self.available_kib)
    }

    /// Used RAM in percent, `0.0..=100.0`.
    #[must_use]
    pub fn used_percent(&self) -> f32 {
        if self.total_kib == 0 {
            return 0.0;
        }
        (self.used_kib() as f32 / self.total_kib as f32) * 100.0
    }

    /// Used swap, saturating at zero.
    #[must_use]
    pub fn swap_used_kib(&self) -> u64 {
        self.swap_total_kib.saturating_sub(self.swap_free_kib)
    }
}

/// One-minute, five-minute, and fifteen-minute load averages.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LoadAverage {
    /// One-minute average.
    pub one: f32,
    /// Five-minute average.
    pub five: f32,
    /// Fifteen-minute average.
    pub fifteen: f32,
}

impl LoadAverage {
    /// Parses the three numbers in `/proc/loadavg`.
    ///
    /// # Errors
    ///
    /// Returns [`ProcError::MalformedLoadAverage`] when a number is missing or
    /// malformed.
    pub fn parse(loadavg: &str) -> Result<Self, ProcError> {
        let mut fields = loadavg.split_whitespace();
        let parse = |field: Option<&str>| {
            field
                .and_then(|raw| raw.parse::<f32>().ok())
                .filter(|value| value.is_finite())
                .ok_or(ProcError::MalformedLoadAverage)
        };
        Ok(Self {
            one: parse(fields.next())?,
            five: parse(fields.next())?,
            fifteen: parse(fields.next())?,
        })
    }
}

/// One raw temperature counter, as `/sys/class/hwmon` reports it.
///
/// The chip name is the `name` file of the `hwmon` directory (`coretemp`,
/// `nvme`, `amdgpu`, …); the label is the chip's own name for the sensor
/// (`Package id 0`, `Composite`, `Tctl`, …).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TempReading {
    /// Driver that owns the sensor.
    pub chip: String,
    /// The sensor's name within that driver.
    pub label: String,
    /// Reading in millidegrees Celsius.
    pub milli_celsius: i32,
}

/// A temperature worth showing on a phone, in the phone's own words.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Temperature {
    /// Short name such as `CPU`, `SSD`, `显卡`.
    pub label: String,
    /// Reading in millidegrees Celsius.
    pub milli_celsius: i32,
}

impl Temperature {
    /// Reading in degrees Celsius, rounded to one decimal place.
    #[must_use]
    pub fn celsius(&self) -> f32 {
        self.milli_celsius as f32 / 1000.0
    }
}

/// How many sensors the phone has room for.
pub const MAX_TEMPERATURES: usize = 3;

/// Readings outside this range are junk: a sensor that reports 0 or 200 °C is
/// either unpopulated or broken, and showing it would look like a fault.
const MIN_PLAUSIBLE_MILLI: i32 = 1_000;
const MAX_PLAUSIBLE_MILLI: i32 = 150_000;

impl TempReading {
    /// Parses one `tempN_input` value, rejecting implausible readings.
    #[must_use]
    pub fn parse(chip: &str, label: &str, raw: &str) -> Option<Self> {
        let value = raw.trim().parse::<i32>().ok()?;
        if !(MIN_PLAUSIBLE_MILLI..=MAX_PLAUSIBLE_MILLI).contains(&value) {
            return None;
        }
        Some(Self {
            chip: chip.to_owned(),
            label: label.to_owned(),
            milli_celsius: value,
        })
    }
}

/// The part of a machine a sensor speaks for, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Part {
    Cpu,
    Storage,
    Graphics,
    Board,
}

fn part_of(chip: &str) -> Option<Part> {
    let chip = chip.to_ascii_lowercase();
    if chip.contains("coretemp")
        || chip.contains("k10temp")
        || chip.contains("k8temp")
        || chip.contains("zenpower")
        || chip.contains("cpu")
        || chip.contains("package")
        || chip.contains("pkg")
    {
        Some(Part::Cpu)
    } else if chip.contains("nvme") || chip.contains("drivetemp") {
        Some(Part::Storage)
    } else if chip.contains("amdgpu")
        || chip.contains("radeon")
        || chip.contains("nouveau")
        || chip.contains("gpu")
        || chip.contains("i915")
        || chip.contains("xe")
    {
        Some(Part::Graphics)
    } else if chip.contains("acpitz")
        || chip.contains("pch")
        || chip.contains("thermal")
        || chip.contains("it87")
        || chip.contains("nct")
        || chip.contains("soc")
        || chip.contains("board")
    {
        Some(Part::Board)
    } else {
        // Wi-Fi cards, batteries and the like: real sensors, but not a number
        // anyone wants labelled "主板".
        None
    }
}

impl Part {
    fn label(self) -> &'static str {
        match self {
            Self::Cpu => "CPU",
            Self::Storage => "SSD",
            Self::Graphics => "显卡",
            Self::Board => "主板",
        }
    }
}

/// Picks the sensors worth sending to a phone.
///
/// A modern desktop exposes dozens of readings — one per core, one per `NVMe`
/// namespace, plus a fistful of board sensors nobody asked about. The phone has
/// room for three numbers, so this keeps the hottest reading of each part, in
/// the order a user looks at them.
#[must_use]
pub fn pick_temperatures(readings: &[TempReading]) -> Vec<Temperature> {
    const PARTS: [Part; 4] = [Part::Cpu, Part::Storage, Part::Graphics, Part::Board];
    let mut best: [Option<&TempReading>; PARTS.len()] = [None; PARTS.len()];
    for reading in readings {
        let Some(part) = part_of(&reading.chip) else {
            continue;
        };
        let slot = &mut best[part as usize];
        if slot.is_none_or(|current| current.milli_celsius < reading.milli_celsius) {
            *slot = Some(reading);
        }
    }

    let mut picked: Vec<Temperature> = PARTS
        .into_iter()
        .zip(best)
        .filter_map(|(part, reading)| {
            let reading = reading?;
            Some(Temperature {
                label: part.label().to_owned(),
                milli_celsius: reading.milli_celsius,
            })
        })
        .collect();

    // The ACPI zone tracks the CPU package on most laptops, so showing it
    // beside a real CPU sensor is the same number twice under two names. It
    // only earns a slot when it is all the machine has.
    if picked.iter().any(|temp| temp.label == Part::Cpu.label()) {
        picked.retain(|temp| temp.label != Part::Board.label());
    }
    picked.truncate(MAX_TEMPERATURES);
    picked
}

/// Parses the seconds field of `/proc/uptime`.
///
/// # Errors
///
/// Returns [`ProcError::MalformedUptime`] when the first field is not a finite,
/// non-negative number.
pub fn parse_uptime(uptime: &str) -> Result<u64, ProcError> {
    uptime
        .split_whitespace()
        .next()
        .and_then(|raw| raw.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value >= 0.0)
        .map(|value| value as u64)
        .ok_or(ProcError::MalformedUptime)
}

/// A complete statistics sample pushed to the phone once per second.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemStats {
    /// Aggregate CPU utilisation in percent.
    pub cpu_percent: f32,
    /// Per-core CPU utilisation in percent.
    pub per_core_percent: Vec<f32>,
    /// Total RAM in KiB.
    pub mem_total_kib: u64,
    /// Available RAM in KiB.
    pub mem_available_kib: u64,
    /// Total swap in KiB.
    pub swap_total_kib: u64,
    /// Free swap in KiB.
    pub swap_free_kib: u64,
    /// One-minute load average.
    pub load_one: f32,
    /// Five-minute load average.
    pub load_five: f32,
    /// Fifteen-minute load average.
    pub load_fifteen: f32,
    /// Host uptime in seconds.
    pub uptime_secs: u64,
    /// Host wall-clock time in Unix milliseconds.
    pub timestamp_ms: u64,
    /// Temperature sensors, hottest of each part, in display order.
    ///
    /// Optional on the wire so that a daemon and a phone from different
    /// releases can still talk to each other.
    #[serde(default)]
    pub temps: Vec<Temperature>,
}

impl SystemStats {
    /// Used RAM in KiB.
    #[must_use]
    pub fn mem_used_kib(&self) -> u64 {
        self.mem_total_kib.saturating_sub(self.mem_available_kib)
    }

    /// Used RAM in percent, `0.0..=100.0`.
    #[must_use]
    pub fn mem_used_percent(&self) -> f32 {
        if self.mem_total_kib == 0 {
            return 0.0;
        }
        (self.mem_used_kib() as f32 / self.mem_total_kib as f32) * 100.0
    }

    /// Hottest reported sensor in millidegrees, when the host has any.
    #[must_use]
    pub fn hottest_milli(&self) -> Option<i32> {
        self.temps.iter().map(|temp| temp.milli_celsius).max()
    }
}

/// Failure modes shared by the `/proc` parsers.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ProcError {
    /// `/proc/stat` had no aggregate `cpu` row.
    #[error("missing aggregate cpu row")]
    MissingAggregateCpu,
    /// A `cpu` row could not be parsed.
    #[error("malformed cpu row on line {line}")]
    MalformedCpuLine {
        /// One-based line number.
        line: usize,
    },
    /// More cores were reported than [`MAX_CORES`].
    #[error("too many cpu cores: {count}")]
    TooManyCores {
        /// The configured ceiling.
        count: usize,
    },
    /// `/proc/meminfo` had no `MemTotal` row.
    #[error("missing MemTotal in meminfo")]
    MissingMemTotal,
    /// `/proc/meminfo` had no `MemAvailable` row.
    #[error("missing MemAvailable in meminfo")]
    MissingMemAvailable,
    /// A known `/proc/meminfo` value was not numeric.
    #[error("malformed meminfo value for {key}")]
    MalformedMemoryLine {
        /// The offending key.
        key: String,
    },
    /// `/proc/loadavg` had fewer than three valid numbers.
    #[error("malformed load average")]
    MalformedLoadAverage,
    /// `/proc/uptime` had no valid seconds field.
    #[error("malformed uptime")]
    MalformedUptime,
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROC_STAT: &str = "cpu  100 0 100 700 100 0 0 0 0 0\n\
cpu0 50 0 50 350 50 0 0 0 0 0\n\
cpu1 50 0 50 350 50 0 0 0 0 0\n\
intr 12345 0 0\n\
ctxt 999\n";

    #[test]
    fn parses_aggregate_and_per_core() {
        let snapshot = CpuSnapshot::parse(PROC_STAT).expect("fixture parses");
        assert_eq!(
            snapshot.total,
            CpuTimes {
                total: 1000,
                idle: 800
            }
        );
        assert_eq!(snapshot.per_core.len(), 2);
        assert_eq!(
            snapshot.per_core[0],
            CpuTimes {
                total: 500,
                idle: 400
            }
        );
    }

    #[test]
    fn usage_uses_idle_delta() {
        let earlier = CpuSnapshot::parse(PROC_STAT).expect("fixture parses");
        let later = CpuSnapshot::parse(
            "cpu  500 0 100 1300 100 0 0 0 0 0\n\
cpu0 250 0 50 650 50 0 0 0 0 0\n\
cpu1 250 0 50 650 50 0 0 0 0 0\n",
        )
        .expect("fixture parses");

        let usage = later.usage_since(&earlier);
        assert!((usage.overall - 40.0).abs() < 0.01, "{usage:?}");
        assert_eq!(usage.per_core.len(), 2);
        assert!((usage.per_core[0] - 40.0).abs() < 0.01, "{usage:?}");
    }

    #[test]
    fn identical_snapshots_report_zero() {
        let snapshot = CpuSnapshot::parse(PROC_STAT).expect("fixture parses");
        let usage = snapshot.usage_since(&snapshot);
        assert!(usage.overall.abs() < f32::EPSILON);
        assert_eq!(usage.per_core, vec![0.0, 0.0]);
    }

    #[test]
    fn core_count_change_drops_per_core() {
        let earlier = CpuSnapshot::parse(PROC_STAT).expect("fixture parses");
        let later = CpuSnapshot::parse("cpu  1 0 1 1 0 0 0 0 0 0\ncpu0 1 0 1 1 0 0 0 0 0 0\n")
            .expect("fixture parses");
        assert_eq!(later.usage_since(&earlier).per_core.len(), 0);
    }

    #[test]
    fn rejects_missing_aggregate() {
        assert_eq!(
            CpuSnapshot::parse("cpu0 1 2 3 4 5 6 7 8\n"),
            Err(ProcError::MissingAggregateCpu)
        );
    }

    #[test]
    fn rejects_short_cpu_row() {
        assert_eq!(
            CpuSnapshot::parse("cpu 1 2 3\n"),
            Err(ProcError::MalformedCpuLine { line: 1 })
        );
    }

    #[test]
    fn parses_memory() {
        let meminfo = "MemTotal:       16384000 kB\n\
MemFree:         1000000 kB\n\
MemAvailable:    8192000 kB\n\
SwapTotal:       2097152 kB\n\
SwapFree:        1048576 kB\n";
        let memory = MemoryInfo::parse(meminfo).expect("fixture parses");
        assert_eq!(memory.total_kib, 16_384_000);
        assert_eq!(memory.available_kib, 8_192_000);
        assert_eq!(memory.used_kib(), 8_192_000);
        assert!((memory.used_percent() - 50.0).abs() < 0.01);
        assert_eq!(memory.swap_used_kib(), 1_048_576);
    }

    #[test]
    fn swap_defaults_to_zero() {
        let memory =
            MemoryInfo::parse("MemTotal: 100 kB\nMemAvailable: 40 kB\n").expect("fixture parses");
        assert_eq!(memory.swap_total_kib, 0);
        assert_eq!(memory.swap_used_kib(), 0);
    }

    #[test]
    fn rejects_missing_memory_fields() {
        assert_eq!(
            MemoryInfo::parse("MemAvailable: 1 kB\n"),
            Err(ProcError::MissingMemTotal)
        );
        assert_eq!(
            MemoryInfo::parse("MemTotal: 1 kB\n"),
            Err(ProcError::MissingMemAvailable)
        );
    }

    #[test]
    fn parses_load_and_uptime() {
        let load = LoadAverage::parse("1.25 0.50 0.75 2/345 6789").expect("fixture parses");
        assert!((load.one - 1.25).abs() < f32::EPSILON);
        assert!((load.five - 0.5).abs() < f32::EPSILON);
        assert!((load.fifteen - 0.75).abs() < f32::EPSILON);
        assert_eq!(parse_uptime("12345.67 98765.43\n"), Ok(12_345));
    }

    #[test]
    fn rejects_bad_load_and_uptime() {
        assert_eq!(
            LoadAverage::parse("1.0 nope 3.0"),
            Err(ProcError::MalformedLoadAverage)
        );
        assert_eq!(parse_uptime("nope"), Err(ProcError::MalformedUptime));
    }

    #[test]
    fn stats_round_trip() {
        let stats = SystemStats {
            cpu_percent: 12.5,
            per_core_percent: vec![10.0, 15.0],
            mem_total_kib: 1000,
            mem_available_kib: 400,
            swap_total_kib: 200,
            swap_free_kib: 200,
            load_one: 0.5,
            load_five: 0.4,
            load_fifteen: 0.3,
            uptime_secs: 42,
            timestamp_ms: 1_700_000_000_000,
            temps: vec![Temperature {
                label: "CPU".to_owned(),
                milli_celsius: 61_500,
            }],
        };
        let json = serde_json::to_string(&stats).expect("serializes");
        let decoded: SystemStats = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(decoded, stats);
        assert_eq!(decoded.mem_used_kib(), 600);
        assert!((decoded.mem_used_percent() - 60.0).abs() < 0.01);
        assert_eq!(decoded.hottest_milli(), Some(61_500));
    }

    #[test]
    fn stats_without_temps_still_decode() {
        // A daemon from before temperatures existed must not break a newer
        // phone, and the other way round.
        let json = r#"{"cpu_percent":1.0,"per_core_percent":[],"mem_total_kib":1,
"mem_available_kib":1,"swap_total_kib":0,"swap_free_kib":0,"load_one":0.0,
"load_five":0.0,"load_fifteen":0.0,"uptime_secs":1,"timestamp_ms":1}"#;
        let decoded: SystemStats = serde_json::from_str(json).expect("decodes");
        assert_eq!(decoded.temps, Vec::new());
        assert_eq!(decoded.hottest_milli(), None);
    }

    #[test]
    fn temperature_parsing_rejects_junk() {
        assert_eq!(
            TempReading::parse("coretemp", "Package id 0", " 61500\n"),
            Some(TempReading {
                chip: "coretemp".to_owned(),
                label: "Package id 0".to_owned(),
                milli_celsius: 61_500,
            })
        );
        // Unpopulated sensors read zero; broken ones read absurd values.
        assert_eq!(TempReading::parse("coretemp", "Core 0", "0"), None);
        assert_eq!(TempReading::parse("coretemp", "Core 0", "200000"), None);
        assert_eq!(TempReading::parse("coretemp", "Core 0", "n/a"), None);
    }

    #[test]
    fn pick_keeps_hottest_of_each_part() {
        let readings = vec![
            TempReading::parse("coretemp", "Core 0", "45000").expect("valid"),
            TempReading::parse("coretemp", "Package id 0", "61000").expect("valid"),
            TempReading::parse("nvme", "Composite", "38800").expect("valid"),
            TempReading::parse("acpitz", "temp1", "62000").expect("valid"),
            TempReading::parse("iwlwifi_1", "temp1", "40000").expect("valid"),
        ];
        let picked = pick_temperatures(&readings);
        // CPU, SSD, and nothing else: the ACPI zone would only repeat the CPU
        // number, and a Wi-Fi sensor is not a board reading.
        assert_eq!(picked.len(), 2, "{picked:?}");
        assert_eq!(picked[0].label, "CPU");
        assert_eq!(picked[0].milli_celsius, 61_000);
        assert_eq!(picked[1].label, "SSD");
        assert_eq!(picked[1].milli_celsius, 38_800);
    }

    #[test]
    fn board_zone_fills_in_when_nothing_else_reports() {
        let readings = vec![TempReading::parse("acpitz", "temp1", "52000").expect("valid")];
        let picked = pick_temperatures(&readings);
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].label, "主板");
        assert_eq!(picked[0].milli_celsius, 52_000);
    }

    #[test]
    fn a_gpu_is_named_and_ordered_after_storage() {
        let readings = vec![
            TempReading::parse("amdgpu", "edge", "74000").expect("valid"),
            TempReading::parse("nvme", "Composite", "30000").expect("valid"),
        ];
        let picked = pick_temperatures(&readings);
        assert_eq!(picked.len(), 2);
        assert_eq!(picked[0].label, "SSD");
        assert_eq!(picked[1].label, "显卡");
    }

    #[test]
    fn pick_with_nothing_to_report_is_empty() {
        assert_eq!(pick_temperatures(&[]), Vec::<Temperature>::new());
    }

    #[test]
    fn celsius_keeps_one_decimal() {
        let temp = Temperature {
            label: "CPU".to_owned(),
            milli_celsius: 41_750,
        };
        assert!((temp.celsius() - 41.75).abs() < 0.001);
    }
}
