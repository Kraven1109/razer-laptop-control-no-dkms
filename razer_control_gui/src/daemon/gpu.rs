use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use serde::{Deserialize, Serialize};
use lazy_static::lazy_static;

lazy_static! {
    /// Most recent GPU snapshot taken by the GPU load monitor.
    /// The GUI's GetGpuStatus handler reads from this cache so that only one
    /// nvidia-smi subprocess is spawned per poll cycle rather than once per
    /// GUI tick.
    static ref GPU_STATUS_CACHE: Mutex<Option<GpuStatus>> = Mutex::new(None);
    /// sysfs runtime_status path for the NVIDIA dGPU, used to avoid waking the
    /// device on battery just to discover that it is already suspended.
    static ref NVIDIA_RUNTIME_STATUS_PATH: Option<PathBuf> = find_nvidia_runtime_status_path();
}

fn find_nvidia_runtime_status_path() -> Option<PathBuf> {
    let devices = fs::read_dir("/sys/bus/pci/devices").ok()?;
    for entry in devices.flatten() {
        let path = entry.path();
        let vendor = fs::read_to_string(path.join("vendor")).ok()?;
        if vendor.trim() != "0x10de" {
            continue;
        }
        let class = fs::read_to_string(path.join("class")).ok()?;
        let class = class.trim();
        if class == "0x030000" || class == "0x030200" {
            let runtime_status = path.join("power/runtime_status");
            if runtime_status.exists() {
                return Some(runtime_status);
            }
        }
    }
    None
}

/// Store a freshly queried status into the cache.
pub fn store_gpu_cache(status: &GpuStatus) {
    if let Ok(mut cache) = GPU_STATUS_CACHE.lock() {
        *cache = Some(status.clone());
    }
}

pub fn clear_gpu_cache() {
    if let Ok(mut cache) = GPU_STATUS_CACHE.lock() {
        *cache = None;
    }
}

/// Return the most recently cached GPU status without spawning nvidia-smi.
pub fn get_cached_gpu_status() -> Option<GpuStatus> {
    GPU_STATUS_CACHE.lock().ok().and_then(|g| g.clone())
}

/// Returns true when it is reasonable to query nvidia-smi.
/// On battery, avoid touching the dGPU if the kernel already runtime-suspended it.
pub fn should_query_nvidia(on_ac: bool) -> bool {
    if on_ac {
        return true;
    }

    match NVIDIA_RUNTIME_STATUS_PATH.as_ref()
        .and_then(|path| fs::read_to_string(path).ok())
        .map(|status| status.trim().to_string())
    {
        Some(status) => status != "suspended",
        None => true,
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct GpuStatus {
    pub name: String,
    pub temp_c: i32,
    pub gpu_util: u8,
    pub mem_util: u8,
    pub power_w: f32,
    pub power_limit_w: f32,
    pub power_max_limit_w: f32,
    pub mem_used_mb: u32,
    pub mem_total_mb: u32,
    pub clock_gpu_mhz: u32,
    pub clock_mem_mhz: u32,
}

pub fn query_nvidia_gpu() -> Option<GpuStatus> {
    let output = Command::new("nvidia-smi")
        .args([
            // enforced.power.limit reflects the current firmware-set TGP (e.g. 135W in
            // Balanced, 150W in Gaming). power.max_limit is the hardware ceiling exposed
            // by the vBIOS/driver (175W on this GPU), which may only be reachable through
            // Dynamic Boost rather than a user-selectable EC power profile.
            "--query-gpu=name,temperature.gpu,utilization.gpu,utilization.memory,power.draw,enforced.power.limit,power.max_limit,memory.used,memory.total,clocks.gr,clocks.mem",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.trim();
    let parts: Vec<&str> = line.splitn(11, ", ").collect();
    if parts.len() < 11 {
        return None;
    }

    Some(GpuStatus {
        name: parts[0].to_string(),
        temp_c: parts[1].trim().parse().unwrap_or(0),
        gpu_util: parts[2].trim().parse().unwrap_or(0),
        mem_util: parts[3].trim().parse().unwrap_or(0),
        power_w: parts[4].trim().parse().unwrap_or(0.0),
        power_limit_w: parts[5].trim().parse().unwrap_or(0.0),
        power_max_limit_w: parts[6].trim().parse().unwrap_or(0.0),
        mem_used_mb: parts[7].trim().parse().unwrap_or(0),
        mem_total_mb: parts[8].trim().parse().unwrap_or(0),
        clock_gpu_mhz: parts[9].trim().parse().unwrap_or(0),
        clock_mem_mhz: parts[10].trim().parse().unwrap_or(0),
    })
}
