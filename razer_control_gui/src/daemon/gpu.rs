use lazy_static::lazy_static;
use nvml_wrapper::enum_wrappers::device::{Clock, TemperatureSensor};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

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

struct NvmlEnergyBaseline {
    energy_mj: u64,
    at: Instant,
}

static NVML_ENERGY: OnceLock<Mutex<Option<NvmlEnergyBaseline>>> = OnceLock::new();
static NVML_INSTANCE: OnceLock<Mutex<Option<nvml_wrapper::Nvml>>> = OnceLock::new();

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
/// If the kernel has runtime-suspended the dGPU, avoid touching it in either
/// AC or battery mode so telemetry polling does not wake it up.
pub fn should_query_nvidia(_on_ac: bool) -> bool {
    match NVIDIA_RUNTIME_STATUS_PATH
        .as_ref()
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
    if let Some(status) = query_nvml() {
        return Some(status);
    }

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

fn query_nvml() -> Option<GpuStatus> {
    // Reuse a single NVML context for the daemon lifetime.
    // Re-initializing NVML every poll leaks kernel/event fds on some drivers,
    // eventually hitting EMFILE and breaking HID access.
    let state = NVML_INSTANCE.get_or_init(|| Mutex::new(None));
    let mut nvml_guard = state.lock().ok()?;
    if nvml_guard.is_none() {
        *nvml_guard = nvml_wrapper::Nvml::init().ok();
    }
    let nvml = nvml_guard.as_ref()?;
    let device = nvml.device_by_index(0).ok()?;

    let name = device.name().ok()?;
    let temp_c = device.temperature(TemperatureSensor::Gpu).ok()? as i32;
    let util = device.utilization_rates().ok()?;
    let power_mw = device.power_usage().ok()?;
    let limit_mw = device.power_management_limit().ok()?;
    let max_limit_mw = device
        .power_management_limit_constraints()
        .ok()
        .map(|limits| limits.max_limit)
        .unwrap_or(limit_mw);
    let mem = device.memory_info().ok()?;
    let clock_gpu_mhz = device.clock_info(Clock::Graphics).ok()?;
    let clock_mem_mhz = device.clock_info(Clock::Memory).ok()?;
    let energy_total_mj = device.total_energy_consumption().ok();

    let power_w = {
        let state = NVML_ENERGY.get_or_init(|| Mutex::new(None));
        let mut baseline = state.lock().ok()?;
        let averaged = match (energy_total_mj, baseline.as_ref()) {
            (Some(energy_mj), Some(previous)) => {
                let delta_mj = energy_mj.saturating_sub(previous.energy_mj);
                let delta_ms = previous.at.elapsed().as_millis().max(1) as f64;
                Some((delta_mj as f64 / delta_ms) as f32)
            }
            _ => None,
        };
        if let Some(energy_mj) = energy_total_mj {
            *baseline = Some(NvmlEnergyBaseline {
                energy_mj,
                at: Instant::now(),
            });
        }
        averaged.unwrap_or(power_mw as f32 / 1000.0)
    };

    Some(GpuStatus {
        name,
        temp_c,
        gpu_util: util.gpu as u8,
        mem_util: util.memory as u8,
        power_w,
        power_limit_w: limit_mw as f32 / 1000.0,
        power_max_limit_w: max_limit_mw as f32 / 1000.0,
        mem_used_mb: (mem.used / 1_048_576) as u32,
        mem_total_mb: (mem.total / 1_048_576) as u32,
        clock_gpu_mhz,
        clock_mem_mhz,
    })
}
