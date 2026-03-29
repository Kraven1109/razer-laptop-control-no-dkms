use std::process::Command;
use std::sync::Mutex;
use serde::{Deserialize, Serialize};
use lazy_static::lazy_static;

lazy_static! {
    /// Most recent GPU snapshot taken by the GPU load monitor.
    /// The GUI's GetGpuStatus handler reads from this cache so that only one
    /// nvidia-smi subprocess is spawned per 5-second monitor cycle rather than
    /// once per 3-second GUI poll.
    static ref GPU_STATUS_CACHE: Mutex<Option<GpuStatus>> = Mutex::new(None);
}

/// Store a freshly queried status into the cache.
pub fn store_gpu_cache(status: &GpuStatus) {
    if let Ok(mut cache) = GPU_STATUS_CACHE.lock() {
        *cache = Some(status.clone());
    }
}

/// Return the most recently cached GPU status without spawning nvidia-smi.
pub fn get_cached_gpu_status() -> Option<GpuStatus> {
    GPU_STATUS_CACHE.lock().ok().and_then(|g| g.clone())
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct GpuStatus {
    pub name: String,
    pub temp_c: i32,
    pub gpu_util: u8,
    pub mem_util: u8,
    pub power_w: f32,
    pub power_limit_w: f32,
    pub mem_used_mb: u32,
    pub mem_total_mb: u32,
    pub clock_gpu_mhz: u32,
    pub clock_mem_mhz: u32,
}

pub fn query_nvidia_gpu() -> Option<GpuStatus> {
    let output = Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,temperature.gpu,utilization.gpu,utilization.memory,power.draw,power.default_limit,memory.used,memory.total,clocks.gr,clocks.mem",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.trim();
    let parts: Vec<&str> = line.splitn(10, ", ").collect();
    if parts.len() < 10 {
        return None;
    }

    Some(GpuStatus {
        name: parts[0].to_string(),
        temp_c: parts[1].trim().parse().unwrap_or(0),
        gpu_util: parts[2].trim().parse().unwrap_or(0),
        mem_util: parts[3].trim().parse().unwrap_or(0),
        power_w: parts[4].trim().parse().unwrap_or(0.0),
        power_limit_w: parts[5].trim().parse().unwrap_or(0.0),
        mem_used_mb: parts[6].trim().parse().unwrap_or(0),
        mem_total_mb: parts[7].trim().parse().unwrap_or(0),
        clock_gpu_mhz: parts[8].trim().parse().unwrap_or(0),
        clock_mem_mhz: parts[9].trim().parse().unwrap_or(0),
    })
}
