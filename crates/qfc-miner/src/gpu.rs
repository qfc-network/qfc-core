//! GPU detection and monitoring

use qfc_inference::runtime::{detect_hardware, HardwareInfo};
use tracing::info;

/// Detect and log hardware information
pub fn detect_and_log() -> HardwareInfo {
    let hw = detect_hardware();

    info!("=== Hardware Detection ===");
    info!("Backend:  {}", hw.backend);
    info!("Device:   {}", hw.device_name);
    info!("Memory:   {} MB", hw.memory_mb);
    info!("Cores:    {}", hw.compute_cores);
    info!("Tier:     {}", hw.tier);
    info!("==========================");

    hw
}
