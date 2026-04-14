use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};

/// Razer laptop control socket path
pub const SOCKET_PATH: &str = "/tmp/razercontrol-socket";

#[derive(Serialize, Deserialize, Debug)]
/// Represents data sent TO the daemon
pub enum DaemonCommand {
    SetFanSpeed { ac: usize, rpm: i32 },      // Fan speed
    GetFanSpeed { ac: usize },                 // Get (Fan speed)
    SetPowerMode { ac: usize, pwr: u8, cpu: u8, gpu: u8}, // Power mode
    GetPwrLevel { ac: usize },                 // Get (Power mode)
    GetCPUBoost { ac: usize },                 // Get (CPU boost)
    GetGPUBoost { ac: usize },                 // Get (GPU boost)
    SetLogoLedState{ ac:usize, logo_state: u8 },
    GetLogoLedState { ac: usize },
    GetKeyboardRGB { layer: i32 }, // Layer ID
    SetEffect { name: String, params: Vec<u8> }, // Set keyboard colour
    SetStandardEffect { name: String, params: Vec<u8> }, // Set keyboard colour
    SetBrightness { ac:usize, val: u8 },
    SetIdle {ac: usize, val: u32 },
    GetBrightness { ac: usize },
    SetSync { sync: bool },
    GetSync (),
    SetBatteryHealthOptimizer { is_on: bool, threshold: u8 },
    GetBatteryHealthOptimizer (),
    GetDeviceName,
    GetGpuStatus,
    GetPowerLimits { ac: usize },
    SetPowerLimits { ac: usize, pl1_watts: u32, pl2_watts: u32 },
    GetCurrentEffect,
    /// Live fan RPM from the EC tachometer (model-agnostic).
    GetFanTachometer,
}

#[derive(Serialize, Deserialize, Debug)]
/// Represents data sent back from Daemon after it receives
/// a command.
pub enum DaemonResponse {
    SetFanSpeed { result: bool },                    // Response
    GetFanSpeed { rpm: i32 },                        // Get (Fan speed)
    SetPowerMode { result: bool },                   // Response
    GetPwrLevel { pwr: u8 },                         // Get (Power mode)
    GetCPUBoost { cpu: u8 },                         // Get (CPU boost)
    GetGPUBoost { gpu: u8 },                         // Get (GPU boost)
    SetLogoLedState {result: bool },
    GetLogoLedState { logo_state: u8 },
    GetKeyboardRGB { layer: i32, rgbdata: Vec<u8> }, // Response (RGB) of 90 keys
    SetEffect { result: bool },                       // Set keyboard colour
    SetStandardEffect { result: bool },                       // Set keyboard colour
    SetBrightness { result: bool },
    SetIdle { result: bool },
    GetBrightness { result: u8 },
    SetSync { result: bool },
    GetSync { sync: bool },
    SetBatteryHealthOptimizer { result: bool },
    GetBatteryHealthOptimizer { is_on: bool, threshold: u8 },
    GetDeviceName { name: String },
    GetGpuStatus {
        name: String,
        temp_c: i32,
        gpu_util: u8,
        mem_util: u8,
        stale: bool,
        power_w: f32,
        power_limit_w: f32,
        power_max_limit_w: f32,
        mem_used_mb: u32,
        mem_total_mb: u32,
        clock_gpu_mhz: u32,
        clock_mem_mhz: u32,
    },
    GetPowerLimits {
        pl1_watts: u32,
        pl2_watts: u32,
        pl1_max_watts: u32,
    },
    SetPowerLimits { result: bool },
    /// Name is the internal effect name (e.g. "Static", "Rainbow Wave"); args are
    /// the raw parameter bytes as passed to the effect constructor.
    /// Returns None-equivalent (name="", args=[]) if no effect is loaded.
    GetCurrentEffect { name: String, args: Vec<u8> },
    /// Live fan RPM from the EC tachometer.
    GetFanTachometer { rpm: i32 },
}

/// Returns `true` when the daemon socket is connectable (used by CLI for liveness check).
#[allow(dead_code)]
pub fn is_daemon_running() -> bool {
    UnixStream::connect(SOCKET_PATH).is_ok()
}

#[allow(dead_code)]
pub fn bind() -> Option<UnixStream> {
    if let Ok(socket) = UnixStream::connect(SOCKET_PATH) {
        return Some(socket);
    } else {
        return None;
    }
}

#[allow(dead_code)]
/// We use this from the app, but it should replace bind
pub fn try_bind() -> std::io::Result<UnixStream> {
    UnixStream::connect(SOCKET_PATH)
}

#[allow(dead_code)]
pub fn create() -> Option<UnixListener> {
    if let Ok(_) = std::fs::metadata(SOCKET_PATH) {
        eprintln!("UNIX Socket already exists. Is another daemon running?");
        return None;
    }
    if let Ok(listener) = UnixListener::bind(SOCKET_PATH) {
        let mut perms = std::fs::metadata(SOCKET_PATH).unwrap().permissions();
        perms.set_readonly(false);
        if std::fs::set_permissions(SOCKET_PATH, perms).is_err() {
            eprintln!("Could not set socket permissions");
            return None;
        }
        return Some(listener);
    }
    return None;
}

#[allow(dead_code)]
pub fn send_to_daemon(command: DaemonCommand, mut sock: UnixStream) -> Option<DaemonResponse> {
    // Bound the blocking time when the daemon is slow (e.g. right after sleep/resume).
    // Without timeouts the GTK main thread can block indefinitely on a non-responsive daemon.
    let _ = sock.set_read_timeout(Some(std::time::Duration::from_millis(1500)));
    let _ = sock.set_write_timeout(Some(std::time::Duration::from_millis(1000)));

    let encoded = bincode::serialize(&command).ok()?;
    sock.write_all(&encoded).ok()?;

    let mut buf = [0u8; 4096];
    match sock.read(&mut buf) {
        Ok(n) if n > 0 => read_from_socket_resp(&buf[..n]),
        Ok(_) => {
            eprintln!("No response from daemon");
            None
        }
        Err(e) => {
            eprintln!("Read failed: {}", e);
            None
        }
    }
}

/// Deserializes incoming bytes into a `DaemonResponse`. Returns None on failure.
pub fn read_from_socket_resp(bytes: &[u8]) -> Option<DaemonResponse> {
    match bincode::deserialize::<DaemonResponse>(bytes) {
        Ok(res) => Some(res),
        Err(e) => {
            eprintln!("RES deserialize error: {}", e);
            None
        }
    }
}

/// Deserializes incoming bytes into a `DaemonCommand`. Returns None on failure.
#[allow(dead_code)]
pub fn read_from_socket_req(bytes: &[u8]) -> Option<DaemonCommand> {
    match bincode::deserialize::<DaemonCommand>(bytes) {
        Ok(res) => Some(res),
        Err(e) => {
            eprintln!("REQ deserialize error: {}", e);
            None
        }
    }
}
