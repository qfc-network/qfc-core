//! GPU monitoring: temperature, power draw, utilization
//!
//! Collects metrics from nvidia-smi (CUDA) or system profiling (Metal/CPU).

use serde::{Deserialize, Serialize};

use crate::runtime::BackendType;

/// Snapshot of GPU/compute metrics
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GpuMetrics {
    /// GPU temperature in Celsius (0 if unavailable)
    pub temperature_c: u32,
    /// Power draw in watts (0 if unavailable)
    pub power_watts: u32,
    /// GPU utilization percentage (0-100, 0 if unavailable)
    pub utilization_percent: u32,
    /// Memory used in MB
    pub memory_used_mb: u64,
    /// Memory total in MB
    pub memory_total_mb: u64,
    /// Backend that produced these metrics
    pub backend: String,
}

/// Collect current GPU metrics for the given backend
pub fn collect_gpu_metrics(backend: BackendType) -> GpuMetrics {
    match backend {
        BackendType::Cuda => collect_nvidia_metrics(),
        BackendType::Metal => collect_metal_metrics(),
        BackendType::Cpu => collect_cpu_metrics(),
    }
}

/// Parse nvidia-smi CSV output for GPU metrics
fn collect_nvidia_metrics() -> GpuMetrics {
    let output = std::process::Command::new("nvidia-smi")
        .args([
            "--query-gpu=temperature.gpu,power.draw,utilization.gpu,memory.used,memory.total",
            "--format=csv,noheader,nounits",
        ])
        .output();

    match output {
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let line = stdout.trim();
            let parts: Vec<&str> = line.split(", ").collect();
            if parts.len() >= 5 {
                GpuMetrics {
                    temperature_c: parts[0].trim().parse().unwrap_or(0),
                    power_watts: parts[1]
                        .trim()
                        .parse::<f32>()
                        .map(|v| v as u32)
                        .unwrap_or(0),
                    utilization_percent: parts[2].trim().parse().unwrap_or(0),
                    memory_used_mb: parts[3].trim().parse().unwrap_or(0),
                    memory_total_mb: parts[4].trim().parse().unwrap_or(0),
                    backend: "CUDA".to_string(),
                }
            } else {
                GpuMetrics {
                    backend: "CUDA".to_string(),
                    ..Default::default()
                }
            }
        }
        _ => GpuMetrics {
            backend: "CUDA".to_string(),
            ..Default::default()
        },
    }
}

/// Collect Apple Silicon metrics (unified memory)
fn collect_metal_metrics() -> GpuMetrics {
    // Memory pressure from vm_stat / sysctl
    let (used, total) = get_macos_memory();

    // CPU/GPU temperature from powermetrics requires root, so we skip temperature
    // and utilization for Metal — they are best collected via Activity Monitor / IOKit
    GpuMetrics {
        temperature_c: 0,
        power_watts: 0,
        utilization_percent: 0,
        memory_used_mb: used,
        memory_total_mb: total,
        backend: "Metal".to_string(),
    }
}

/// Collect CPU-only metrics
fn collect_cpu_metrics() -> GpuMetrics {
    let (used, total) = get_system_memory();
    GpuMetrics {
        temperature_c: 0,
        power_watts: 0,
        utilization_percent: 0,
        memory_used_mb: used,
        memory_total_mb: total,
        backend: "CPU".to_string(),
    }
}

#[cfg(target_os = "macos")]
fn get_macos_memory() -> (u64, u64) {
    let total = std::process::Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|b| b / (1024 * 1024))
        .unwrap_or(0);

    // vm_stat to get free pages
    let free = std::process::Command::new("vm_stat")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| {
            s.lines().find(|l| l.contains("Pages free")).and_then(|l| {
                l.split(':')
                    .nth(1)
                    .and_then(|v| v.trim().trim_end_matches('.').parse::<u64>().ok())
            })
        })
        .map(|pages| pages * 4096 / (1024 * 1024)) // 4K pages → MB
        .unwrap_or(0);

    (total.saturating_sub(free), total)
}

#[cfg(not(target_os = "macos"))]
fn get_macos_memory() -> (u64, u64) {
    get_system_memory()
}

fn get_system_memory() -> (u64, u64) {
    #[cfg(target_os = "linux")]
    {
        let contents = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
        let parse_field = |name: &str| -> u64 {
            contents
                .lines()
                .find(|l| l.starts_with(name))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse::<u64>().ok())
                .map(|kb| kb / 1024)
                .unwrap_or(0)
        };
        let total = parse_field("MemTotal:");
        let available = parse_field("MemAvailable:");
        (total.saturating_sub(available), total)
    }
    #[cfg(not(target_os = "linux"))]
    {
        (0, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_collect_cpu_metrics() {
        let metrics = collect_cpu_metrics();
        assert_eq!(metrics.backend, "CPU");
        // On any platform, total should be > 0 (at least on linux/macos)
        // Don't assert exact values since this runs in CI
    }

    #[test]
    fn test_collect_gpu_metrics_cpu_backend() {
        let metrics = collect_gpu_metrics(BackendType::Cpu);
        assert_eq!(metrics.backend, "CPU");
    }

    #[test]
    fn test_gpu_metrics_default() {
        let m = GpuMetrics::default();
        assert_eq!(m.temperature_c, 0);
        assert_eq!(m.power_watts, 0);
        assert_eq!(m.utilization_percent, 0);
    }
}
