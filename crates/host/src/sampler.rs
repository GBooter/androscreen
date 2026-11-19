//! Host statistics sampling without blocking the streaming loop.
//!
//! CPU usage is the delta between two `/proc/stat` snapshots, so samples are
//! cheap once per second instead of sleeping between readings.

use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use androscreen_core::stats::{
    parse_uptime, pick_temperatures, CpuSnapshot, LoadAverage, MemoryInfo, ProcError, SystemStats,
    TempReading, Temperature,
};
use thiserror::Error;

const PROC_STAT: &str = "/proc/stat";
const PROC_MEMINFO: &str = "/proc/meminfo";
const PROC_LOADAVG: &str = "/proc/loadavg";
const PROC_UPTIME: &str = "/proc/uptime";
const HWMON: &str = "/sys/class/hwmon";
const THERMAL: &str = "/sys/class/thermal";

/// Produces [`SystemStats`] samples relative to the previous one.
#[derive(Debug)]
pub(crate) struct StatsSampler {
    previous: CpuSnapshot,
}

impl StatsSampler {
    /// Takes the baseline CPU snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`SamplerError`] when `/proc/stat` cannot be read or parsed.
    pub(crate) fn new() -> Result<Self, SamplerError> {
        let previous = CpuSnapshot::parse(&read(PROC_STAT)?)?;
        Ok(Self { previous })
    }

    /// Samples CPU, memory, load, uptime, and temperatures.
    ///
    /// CPU percentages cover the time since the previous call.
    ///
    /// # Errors
    ///
    /// Returns [`SamplerError`] when a `/proc` file cannot be read or parsed.
    /// Temperatures are best effort and never fail the sample: a machine with
    /// no readable sensor is still worth reporting.
    pub(crate) fn sample(&mut self) -> Result<SystemStats, SamplerError> {
        let current = CpuSnapshot::parse(&read(PROC_STAT)?)?;
        let usage = current.usage_since(&self.previous);
        self.previous = current;

        let memory = MemoryInfo::parse(&read(PROC_MEMINFO)?)?;
        let load = LoadAverage::parse(&read(PROC_LOADAVG)?)?;
        let uptime_secs = parse_uptime(&read(PROC_UPTIME)?)?;
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| {
                u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
            });

        Ok(SystemStats {
            cpu_percent: usage.overall,
            per_core_percent: usage.per_core,
            mem_total_kib: memory.total_kib,
            mem_available_kib: memory.available_kib,
            swap_total_kib: memory.swap_total_kib,
            swap_free_kib: memory.swap_free_kib,
            load_one: load.one,
            load_five: load.five,
            load_fifteen: load.fifteen,
            uptime_secs,
            timestamp_ms,
            temps: temperatures(),
        })
    }
}

/// Reads the sensors worth showing, or nothing when the machine has none.
///
/// `hwmon` is tried first because its readings are the ones the hardware
/// actually exposes per part; `/sys/class/thermal` only knows ACPI zones, and
/// is the fallback on machines whose drivers do not register an `hwmon` node.
fn temperatures() -> Vec<Temperature> {
    let mut readings = hwmon_readings();
    if readings.is_empty() {
        readings = thermal_readings();
    }
    pick_temperatures(&readings)
}

/// Collects every `tempN_input` under `/sys/class/hwmon`.
fn hwmon_readings() -> Vec<TempReading> {
    let Ok(chips) = std::fs::read_dir(HWMON) else {
        return Vec::new();
    };
    let mut readings = Vec::new();
    for chip in chips.flatten() {
        let dir = chip.path();
        let name = std::fs::read_to_string(dir.join("name")).unwrap_or_default();
        let chip_name = name.trim();
        if chip_name.is_empty() {
            continue;
        }
        let Ok(files) = std::fs::read_dir(&dir) else {
            continue;
        };
        for file in files.flatten() {
            let file_name = file.file_name();
            let Some(file_name) = file_name.to_str() else {
                continue;
            };
            let Some(prefix) = file_name.strip_suffix("_input") else {
                continue;
            };
            if !prefix.starts_with("temp") {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(file.path()) else {
                continue;
            };
            // `tempN_label` is optional; the sensor index is a fine fallback.
            let label = std::fs::read_to_string(dir.join(format!("{prefix}_label")))
                .map_or_else(|_| prefix.to_owned(), |text| text.trim().to_owned());
            if let Some(reading) = TempReading::parse(chip_name, &label, &raw) {
                readings.push(reading);
            }
        }
    }
    readings
}

/// Collects `thermal_zone*` readings, whose type doubles as the chip name.
fn thermal_readings() -> Vec<TempReading> {
    let Ok(zones) = std::fs::read_dir(THERMAL) else {
        return Vec::new();
    };
    let mut readings = Vec::new();
    for zone in zones.flatten() {
        let dir = zone.path();
        if !dir
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("thermal_zone"))
        {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(dir.join("temp")) else {
            continue;
        };
        let kind = std::fs::read_to_string(dir.join("type")).unwrap_or_default();
        let kind = kind.trim();
        if let Some(reading) = TempReading::parse(kind, kind, &raw) {
            readings.push(reading);
        }
    }
    readings
}

fn read(path: &'static str) -> Result<String, SamplerError> {
    std::fs::read_to_string(path).map_err(|source| SamplerError::Io { path, source })
}

/// Failure while reading host statistics.
#[derive(Debug, Error)]
pub(crate) enum SamplerError {
    /// A `/proc` file could not be read.
    #[error("reading {path} failed: {source}")]
    Io {
        /// File that failed.
        path: &'static str,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
    /// A `/proc` file had an unexpected shape.
    #[error("host statistics: {0}")]
    Proc(#[from] ProcError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_plausible_values() {
        let mut sampler = StatsSampler::new().expect("proc stat is readable");
        std::thread::sleep(std::time::Duration::from_millis(20));
        let stats = sampler.sample().expect("stats sample");
        assert!((0.0..=100.0).contains(&stats.cpu_percent));
        assert!(stats.per_core_percent.len() <= androscreen_core::stats::MAX_CORES);
        assert!(stats.mem_total_kib > 0);
        assert!(stats.mem_available_kib <= stats.mem_total_kib);
        assert!(stats.timestamp_ms > 0);
        for core in &stats.per_core_percent {
            assert!((0.0..=100.0).contains(core));
        }
    }
}
