//! Runtime detection (CUDA, Metal, CPU)

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Backend type for inference execution
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    Hash,
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
)]
pub enum BackendType {
    /// NVIDIA CUDA GPU
    Cuda,
    /// Apple Metal GPU (Apple Silicon)
    Metal,
    /// CPU-only fallback
    Cpu,
}

impl fmt::Display for BackendType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BackendType::Cuda => write!(f, "CUDA"),
            BackendType::Metal => write!(f, "Metal"),
            BackendType::Cpu => write!(f, "CPU"),
        }
    }
}

/// GPU tier classification based on hardware capability
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GpuTier {
    /// High-end GPU, 24GB+ VRAM (A100, H100, RTX 4090)
    /// Tasks: LLM 70B, fine-tuning
    Hot,
    /// Mid GPU, 8-16GB (RTX 3080, M2 Pro/Max)
    /// Tasks: LLM 7-13B, embeddings
    Warm,
    /// Low GPU or CPU-only (GTX 1660, M1, CPU)
    /// Tasks: Small models, classification
    Cold,
}

impl fmt::Display for GpuTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GpuTier::Hot => write!(f, "Hot"),
            GpuTier::Warm => write!(f, "Warm"),
            GpuTier::Cold => write!(f, "Cold"),
        }
    }
}

/// Hardware information detected at runtime
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HardwareInfo {
    /// Detected backend type
    pub backend: BackendType,
    /// Available GPU/unified memory in MB
    pub memory_mb: u64,
    /// Number of compute cores (CUDA cores, GPU cores, or CPU cores)
    pub compute_cores: u32,
    /// Device name (e.g. "NVIDIA A100", "Apple M3 Max", "AMD Ryzen 9")
    pub device_name: String,
    /// GPU tier classification
    pub tier: GpuTier,
}

/// Benchmark result from an inference engine
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BenchmarkResult {
    /// Estimated FLOPS (floating point operations per second)
    pub flops: f64,
    /// Tokens per second (for LLM benchmarks)
    pub tokens_per_second: f64,
    /// Memory bandwidth in GB/s
    pub memory_bandwidth_gbps: f64,
    /// Backend type
    pub backend: BackendType,
    /// Time taken for benchmark in ms
    pub benchmark_time_ms: u64,
}

/// Detect the best available backend for the current system
pub fn detect_backend() -> BackendType {
    if is_cuda_available() {
        BackendType::Cuda
    } else if is_metal_available() {
        BackendType::Metal
    } else {
        BackendType::Cpu
    }
}

/// Detect hardware information for the current system
pub fn detect_hardware() -> HardwareInfo {
    let backend = detect_backend();
    let (memory_mb, compute_cores, device_name) = match backend {
        BackendType::Cuda => detect_cuda_hardware(),
        BackendType::Metal => detect_metal_hardware(),
        BackendType::Cpu => detect_cpu_hardware(),
    };
    let tier = classify_tier(backend, memory_mb);

    HardwareInfo {
        backend,
        memory_mb,
        compute_cores,
        device_name,
        tier,
    }
}

/// Classify GPU tier based on backend and available memory
pub fn classify_tier(backend: BackendType, memory_mb: u64) -> GpuTier {
    match backend {
        BackendType::Cuda => {
            if memory_mb >= 24_000 {
                GpuTier::Hot
            } else if memory_mb >= 8_000 {
                GpuTier::Warm
            } else {
                GpuTier::Cold
            }
        }
        BackendType::Metal => {
            // Apple Silicon unified memory
            if memory_mb >= 32_000 {
                GpuTier::Hot // M2 Max/Ultra, M3 Max/Ultra with 32GB+
            } else if memory_mb >= 16_000 {
                GpuTier::Warm // M1 Pro/Max, M2 Pro, M3 Pro with 16GB+
            } else {
                GpuTier::Cold // M1/M2/M3 base with 8GB
            }
        }
        BackendType::Cpu => GpuTier::Cold,
    }
}

/// Check if CUDA is available
fn is_cuda_available() -> bool {
    #[cfg(feature = "cuda")]
    {
        // Check for nvidia-smi or CUDA runtime
        std::process::Command::new("nvidia-smi")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
    #[cfg(not(feature = "cuda"))]
    false
}

/// Check if Metal is available (macOS only)
fn is_metal_available() -> bool {
    #[cfg(feature = "metal")]
    {
        cfg!(target_os = "macos")
    }
    #[cfg(not(feature = "metal"))]
    false
}

/// Detect CUDA GPU hardware info
fn detect_cuda_hardware() -> (u64, u32, String) {
    // Placeholder — will use candle or nvidia-smi for real detection
    (0, 0, "CUDA GPU (detection pending)".to_string())
}

/// Detect Metal/Apple Silicon hardware info
fn detect_metal_hardware() -> (u64, u32, String) {
    // On macOS, use sysctl to get memory info
    #[cfg(target_os = "macos")]
    {
        let total_mem = get_macos_memory_mb();
        // Apple Silicon shares memory between CPU and GPU
        let gpu_mem = total_mem; // unified memory
        (gpu_mem, 0, "Apple Silicon (Metal)".to_string())
    }
    #[cfg(not(target_os = "macos"))]
    {
        (0, 0, "Metal unavailable (not macOS)".to_string())
    }
}

/// Detect CPU hardware info
fn detect_cpu_hardware() -> (u64, u32, String) {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1);
    let ram_mb = get_system_memory_mb();
    (ram_mb, cores, format!("CPU ({} cores)", cores))
}

/// Get system memory in MB
fn get_system_memory_mb() -> u64 {
    #[cfg(target_os = "macos")]
    {
        get_macos_memory_mb()
    }
    #[cfg(target_os = "linux")]
    {
        // Read from /proc/meminfo
        std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("MemTotal:"))
                    .and_then(|l| {
                        l.split_whitespace()
                            .nth(1)
                            .and_then(|v| v.parse::<u64>().ok())
                    })
            })
            .map(|kb| kb / 1024) // KB to MB
            .unwrap_or(0)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        0
    }
}

#[cfg(target_os = "macos")]
fn get_macos_memory_mb() -> u64 {
    std::process::Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()
        .and_then(|o| {
            String::from_utf8(o.stdout)
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
        })
        .map(|bytes| bytes / (1024 * 1024)) // bytes to MB
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backend_display() {
        assert_eq!(format!("{}", BackendType::Cuda), "CUDA");
        assert_eq!(format!("{}", BackendType::Metal), "Metal");
        assert_eq!(format!("{}", BackendType::Cpu), "CPU");
    }

    #[test]
    fn test_tier_classification() {
        // CUDA tiers
        assert_eq!(classify_tier(BackendType::Cuda, 80_000), GpuTier::Hot);
        assert_eq!(classify_tier(BackendType::Cuda, 24_000), GpuTier::Hot);
        assert_eq!(classify_tier(BackendType::Cuda, 12_000), GpuTier::Warm);
        assert_eq!(classify_tier(BackendType::Cuda, 4_000), GpuTier::Cold);

        // Metal tiers
        assert_eq!(classify_tier(BackendType::Metal, 64_000), GpuTier::Hot);
        assert_eq!(classify_tier(BackendType::Metal, 32_000), GpuTier::Hot);
        assert_eq!(classify_tier(BackendType::Metal, 16_000), GpuTier::Warm);
        assert_eq!(classify_tier(BackendType::Metal, 8_000), GpuTier::Cold);

        // CPU is always Cold
        assert_eq!(classify_tier(BackendType::Cpu, 128_000), GpuTier::Cold);
    }

    #[test]
    fn test_detect_backend() {
        let backend = detect_backend();
        // On CI/dev machines without GPU, should default to CPU
        // (unless cuda or metal features are enabled AND hardware is present)
        assert!(matches!(
            backend,
            BackendType::Cpu | BackendType::Metal | BackendType::Cuda
        ));
    }

    #[test]
    fn test_detect_hardware() {
        let hw = detect_hardware();
        assert!(!hw.device_name.is_empty());
    }

    #[test]
    fn test_gpu_tier_display() {
        assert_eq!(format!("{}", GpuTier::Hot), "Hot");
        assert_eq!(format!("{}", GpuTier::Warm), "Warm");
        assert_eq!(format!("{}", GpuTier::Cold), "Cold");
    }
}
