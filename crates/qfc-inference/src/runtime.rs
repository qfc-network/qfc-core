//! Runtime detection (CUDA, Metal, OpenCL, CPU)

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
    /// AMD ROCm GPU (via ONNX Runtime)
    Rocm,
    /// CPU-only fallback
    Cpu,
    /// OpenCL GPU (AMD/Intel without ROCm, via ONNX Runtime)
    OpenCl,
}

impl fmt::Display for BackendType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BackendType::Cuda => write!(f, "CUDA"),
            BackendType::Metal => write!(f, "Metal"),
            BackendType::Rocm => write!(f, "ROCm"),
            BackendType::OpenCl => write!(f, "OpenCL"),
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
    /// Standardized benchmark score (0-10000), computed via compute_benchmark_score
    pub score: u32,
}

/// Compute a standardized benchmark score (0-10000) and GPU tier from a BenchmarkResult
pub fn compute_benchmark_score(bench: &BenchmarkResult) -> (u32, u8) {
    let score = ((bench.flops / 1e12) * 1000.0).min(10000.0) as u32;
    let tier = match score {
        0..=2999 => 1,
        3000..=6999 => 2,
        _ => 3,
    };
    (score, tier)
}

/// Layer 1 validation: GPU model → expected score range.
/// Returns true if the claimed score is plausible for the GPU model.
/// Unknown models always pass.
pub fn validate_gpu_claim(gpu_model: &str, claimed_score: u32) -> bool {
    let model_lower = gpu_model.to_lowercase();
    let range = if model_lower.contains("4060") {
        Some((2000, 3500))
    } else if model_lower.contains("3090") {
        Some((4000, 5500))
    } else if model_lower.contains("4090") {
        Some((6000, 7500))
    } else if model_lower.contains("a100") {
        Some((7500, 9000))
    } else if model_lower.contains("h100") {
        Some((9000, 10000))
    } else {
        None // unknown model — always pass
    };

    match range {
        Some((min, max)) => claimed_score >= min && claimed_score <= max,
        None => true,
    }
}

/// Detect the best available backend for the current system
pub fn detect_backend() -> BackendType {
    if is_cuda_available() {
        BackendType::Cuda
    } else if is_metal_available() {
        BackendType::Metal
    } else if is_rocm_available() {
        BackendType::Rocm
    } else if is_opencl_available() {
        BackendType::OpenCl
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
        BackendType::Rocm => detect_rocm_hardware(),
        BackendType::OpenCl => detect_opencl_hardware(),
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
        BackendType::Rocm | BackendType::OpenCl => {
            if memory_mb >= 24_000 {
                GpuTier::Hot
            } else if memory_mb >= 8_000 {
                GpuTier::Warm
            } else {
                GpuTier::Cold
            }
        }
        BackendType::Cpu => GpuTier::Cold,
    }
}

/// Check if CUDA is available
fn is_cuda_available() -> bool {
    #[cfg(feature = "cuda")]
    {
        // Check for nvidia-smi using platform-aware path resolution
        let nvidia_smi = find_nvidia_smi();
        std::process::Command::new(&nvidia_smi)
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

/// Check if AMD ROCm is available
fn is_rocm_available() -> bool {
    #[cfg(feature = "rocm")]
    {
        // Check for rocm-smi or ROCm runtime
        std::process::Command::new("rocm-smi")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
            || std::path::Path::new("/opt/rocm").exists()
    }
    #[cfg(not(feature = "rocm"))]
    false
}

/// Check if OpenCL is available (Linux AMD/Intel GPUs without ROCm)
fn is_opencl_available() -> bool {
    #[cfg(feature = "opencl")]
    {
        // Only consider OpenCL on Linux where ROCm is not available
        if cfg!(target_os = "linux") {
            // Check for clinfo to detect OpenCL-capable devices
            if let Ok(output) = std::process::Command::new("clinfo").arg("--list").output() {
                if output.status.success() {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    // Look for at least one GPU device (not just CPU OpenCL)
                    return stdout.to_lowercase().contains("gpu")
                        || stdout.contains("Platform #")
                        || stdout.lines().count() > 1;
                }
            }
            // Fallback: check if any OpenCL ICD files exist
            std::path::Path::new("/etc/OpenCL/vendors").exists()
        } else {
            false
        }
    }
    #[cfg(not(feature = "opencl"))]
    false
}

/// Detect OpenCL GPU hardware info
fn detect_opencl_hardware() -> (u64, u32, String) {
    // Try clinfo for detailed device info
    if let Ok(output) = std::process::Command::new("clinfo").output() {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let mut device_name = String::new();
            let mut global_mem_bytes: u64 = 0;
            let mut compute_units: u32 = 0;

            for line in stdout.lines() {
                let trimmed = line.trim();
                if device_name.is_empty() && trimmed.starts_with("Device Name") {
                    device_name = trimmed
                        .split_once(|c: char| c == ':' || c == '\t')
                        .map(|(_, v)| v.trim().to_string())
                        .unwrap_or_default();
                }
                if global_mem_bytes == 0 && trimmed.starts_with("Global memory size") {
                    global_mem_bytes = trimmed
                        .split_whitespace()
                        .filter_map(|w| w.parse::<u64>().ok())
                        .next()
                        .unwrap_or(0);
                }
                if compute_units == 0 && trimmed.starts_with("Max compute units") {
                    compute_units = trimmed
                        .split_whitespace()
                        .filter_map(|w| w.parse::<u32>().ok())
                        .next()
                        .unwrap_or(0);
                }
            }

            if !device_name.is_empty() {
                let mem_mb = global_mem_bytes / (1024 * 1024);
                return (mem_mb, compute_units, format!("{} (OpenCL)", device_name));
            }
        }
    }

    // Fallback: check lspci for AMD/Intel GPU
    let name = std::process::Command::new("lspci")
        .output()
        .ok()
        .and_then(|o| {
            String::from_utf8(o.stdout).ok().and_then(|s| {
                s.lines()
                    .find(|l| {
                        let lower = l.to_lowercase();
                        lower.contains("vga")
                            && (lower.contains("amd")
                                || lower.contains("radeon")
                                || lower.contains("intel"))
                    })
                    .map(|l| l.to_string())
            })
        })
        .unwrap_or_else(|| "GPU (OpenCL)".to_string());

    // Use system memory as approximation
    let mem = get_system_memory_mb();
    (mem / 4, 0, format!("{} (OpenCL)", name))
}

/// Detect AMD ROCm GPU hardware info
fn detect_rocm_hardware() -> (u64, u32, String) {
    // Try rocm-smi for GPU info
    let output = std::process::Command::new("rocm-smi")
        .args(["--showmeminfo", "vram", "--csv"])
        .output();

    if let Ok(out) = output {
        if out.status.success() {
            let stdout = String::from_utf8_lossy(&out.stdout);
            // Parse VRAM from rocm-smi output
            let vram_mb = stdout
                .lines()
                .skip(1) // skip header
                .filter_map(|line| {
                    line.split(',')
                        .nth(1) // total VRAM column
                        .and_then(|v| v.trim().parse::<u64>().ok())
                })
                .next()
                .unwrap_or(0)
                / (1024 * 1024); // bytes to MB

            // Get GPU name
            let name = std::process::Command::new("rocm-smi")
                .args(["--showproductname", "--csv"])
                .output()
                .ok()
                .and_then(|o| {
                    String::from_utf8(o.stdout)
                        .ok()
                        .and_then(|s| s.lines().nth(1).map(|l| l.trim().to_string()))
                })
                .unwrap_or_else(|| "AMD GPU (ROCm)".to_string());

            return (vram_mb, 0, name);
        }
    }

    // Fallback: check lspci for AMD GPU
    let name = std::process::Command::new("lspci")
        .output()
        .ok()
        .and_then(|o| {
            String::from_utf8(o.stdout).ok().and_then(|s| {
                s.lines()
                    .find(|l| {
                        let lower = l.to_lowercase();
                        lower.contains("vga") && (lower.contains("amd") || lower.contains("radeon"))
                    })
                    .map(|l| l.to_string())
            })
        })
        .unwrap_or_else(|| "AMD GPU".to_string());

    // Use system memory as approximation
    let mem = get_system_memory_mb();
    (mem / 4, 0, name) // rough estimate: 1/4 system memory
}

/// Detect CUDA GPU hardware info via nvidia-smi
fn detect_cuda_hardware() -> (u64, u32, String) {
    // nvidia-smi lives in PATH on Linux; on Windows it is typically in
    // "C:\Windows\System32\nvidia-smi.exe" (driver install) or the CUDA bin dir.
    let nvidia_smi = find_nvidia_smi();

    let output = std::process::Command::new(&nvidia_smi)
        .args([
            "--query-gpu=memory.total,name",
            "--format=csv,noheader,nounits",
        ])
        .output();

    match output {
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let line = stdout.lines().next().unwrap_or("");
            let parts: Vec<&str> = line.split(", ").collect();
            let memory_mb = parts
                .first()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(0);
            let name = parts
                .get(1)
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|| "NVIDIA GPU".to_string());
            (memory_mb, 0, name)
        }
        _ => {
            // Fallback: report system memory / 4 as rough GPU estimate
            let mem = get_system_memory_mb();
            (mem / 4, 0, "NVIDIA GPU (nvidia-smi unavailable)".to_string())
        }
    }
}

/// Locate nvidia-smi, checking the standard Windows driver path if needed
pub fn find_nvidia_smi() -> String {
    #[cfg(target_os = "windows")]
    {
        // nvidia-smi is installed by the NVIDIA driver into System32
        let sys32 = std::env::var("SystemRoot")
            .unwrap_or_else(|_| r"C:\Windows".to_string());
        let sys32_path =
            std::path::Path::new(&sys32).join("System32").join("nvidia-smi.exe");
        if sys32_path.exists() {
            return sys32_path.to_string_lossy().to_string();
        }
        // Also check CUDA_PATH\bin
        if let Ok(cuda) = std::env::var("CUDA_PATH") {
            let cuda_bin = std::path::Path::new(&cuda).join("bin").join("nvidia-smi.exe");
            if cuda_bin.exists() {
                return cuda_bin.to_string_lossy().to_string();
            }
        }
    }
    "nvidia-smi".to_string()
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
    #[cfg(target_os = "windows")]
    {
        get_windows_memory_mb()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        0
    }
}

/// Get total physical memory on Windows via `wmic` or `systeminfo`
#[cfg(target_os = "windows")]
fn get_windows_memory_mb() -> u64 {
    // Method 1: wmic (fast, structured output)
    if let Ok(output) = std::process::Command::new("wmic")
        .args(["ComputerSystem", "get", "TotalPhysicalMemory", "/value"])
        .output()
    {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                if let Some(val) = line.strip_prefix("TotalPhysicalMemory=") {
                    if let Ok(bytes) = val.trim().parse::<u64>() {
                        return bytes / (1024 * 1024);
                    }
                }
            }
        }
    }

    // Method 2: PowerShell (fallback)
    if let Ok(output) = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command",
               "(Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory"])
        .output()
    {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Ok(bytes) = stdout.trim().parse::<u64>() {
                return bytes / (1024 * 1024);
            }
        }
    }

    0
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
        assert_eq!(format!("{}", BackendType::Rocm), "ROCm");
        assert_eq!(format!("{}", BackendType::OpenCl), "OpenCL");
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
            BackendType::Cpu | BackendType::Metal | BackendType::Cuda | BackendType::OpenCl
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
