use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex,
};
use std::thread::{self, JoinHandle};
use std::time;

use dbus::blocking::Connection;
use dbus::{arg, Message};
use lazy_static::lazy_static;
use log::*;
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

mod battery;
#[path = "../comms.rs"]
mod comms;
mod config;
mod device;
mod gpu;
mod kbd;
mod login1;
mod screensaver;

use crate::kbd::Effect;

lazy_static! {
    static ref EFFECT_MANAGER: Mutex<kbd::EffectManager> = Mutex::new(kbd::EffectManager::new());
    static ref DEV_MANAGER: Mutex<device::DeviceManager> = {
        match device::DeviceManager::read_laptops_file() {
            Ok(c) => Mutex::new(c),
            Err(_) => Mutex::new(device::DeviceManager::new()),
        }
    };
    /// Set to true while the system is suspended so GPU polling and HID
    /// writes are suppressed until the device is re-opened after resume.
    static ref SYSTEM_SLEEPING: AtomicBool = AtomicBool::new(false);
    static ref LOW_BAT_LIGHTS_FORCED_OFF: AtomicBool = AtomicBool::new(false);
    static ref LAST_BATTERY_PERCENT: Mutex<f64> = Mutex::new(100.0);
}

#[derive(Clone, Copy)]
struct TempFanController {
    prev_temp: f32,
    integral: f32,
    smoothed_temp: f32,
    smoothed_util: f32,
    thermal_energy: f32,
    last_update: std::time::Instant,
}

impl Default for TempFanController {
    fn default() -> Self {
        Self {
            prev_temp: 0.0,
            integral: 0.0,
            smoothed_temp: 0.0,
            smoothed_util: 0.0,
            thermal_energy: 0.0,
            last_update: std::time::Instant::now(),
        }
    }
}

// Main function for daemon
fn main() {
    setup_panic_hook();
    init_logging();
    let mut initial_low_battery: Option<(f64, bool)> = None;

    if let Ok(mut d) = DEV_MANAGER.lock() {
        d.discover_devices();
        if let Some(laptop) = d.get_device() {
            println!("supported device: {:?}", laptop.get_name());
        } else {
            println!("no supported device found");
            std::process::exit(1);
        }
    } else {
        println!("error loading supported devices");
        std::process::exit(1);
    }

    if let Ok(mut d) = DEV_MANAGER.lock() {
        let dbus_system = Connection::new_system().expect("failed to connect to D-Bus system bus");
        let proxy_ac = dbus_system.with_proxy(
            "org.freedesktop.UPower",
            "/org/freedesktop/UPower/devices/line_power_AC0",
            time::Duration::from_millis(5000),
        );
        use battery::OrgFreedesktopUPowerDevice;
        if let Ok(online) = proxy_ac.online() {
            info!("AC0 online: {:?}", online);
            // Restore all saved hardware settings (power, fan, brightness, logo)
            d.set_ac_state(online);
            let config = d.get_ac_config(online as usize);
            if let Some(config) = config {
                if let Some(laptop) = d.get_device() {
                    laptop.set_config(config);
                }
                // Apply saved RAPL limits for the current AC state.
                apply_rapl_for_profile(config.rapl_pl1_watts, config.rapl_pl2_watts);
            }
            d.restore_standard_effect();
            if let Ok(json) = config::Configuration::read_effects_file() {
                EFFECT_MANAGER.lock().unwrap().load_from_save(json);
            } else {
                println!("No effects save, creating a new one");
                // No effects found, start with a green static layer, just like synapse
                EFFECT_MANAGER
                    .lock()
                    .unwrap()
                    .push_effect(kbd::effects::Static::new(vec![0, 255, 0]), [true; 90]);
            }

            use battery::OrgFreedesktopUPowerDevice;
            let proxy_battery = dbus_system.with_proxy(
                "org.freedesktop.UPower",
                "/org/freedesktop/UPower/devices/battery_BAT0",
                time::Duration::from_millis(5000),
            );
            if let Ok(percentage) = proxy_battery.percentage() {
                store_last_battery_percent(percentage);
                initial_low_battery = Some((percentage, online));
            }
        } else {
            println!("error getting current power state");
            std::process::exit(1);
        }
    }

    if let Some((percentage, online)) = initial_low_battery {
        apply_low_battery_lighting(percentage, online);
    }

    start_keyboard_animator_task();
    start_gpu_load_monitor_task();
    start_temp_fan_control_task();
    start_screensaver_monitor_task();
    start_battery_monitor_task();
    let clean_thread = start_shutdown_task();

    if let Some(listener) = comms::create() {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => handle_data(stream),
                Err(_) => {} // Don't care about this
            }
        }
    } else {
        eprintln!("Could not create Unix socket!");
        std::process::exit(1);
    }
    clean_thread.join().unwrap();
}

/// Installs a custom panic hook to perform cleanup when the daemon crashes
fn setup_panic_hook() {
    let default_panic_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        error!("Something went wrong! Removing the socket path");
        if std::fs::metadata(comms::SOCKET_PATH).is_ok() {
            std::fs::remove_file(comms::SOCKET_PATH).unwrap();
        }
        default_panic_hook(info);
    }));
}

fn init_logging() {
    let mut builder = env_logger::Builder::from_default_env();
    builder.target(env_logger::Target::Stderr);
    builder.filter_level(log::LevelFilter::Info);
    builder.format_timestamp_millis();
    builder.parse_env("RAZER_LAPTOP_CONTROL_LOG");
    builder.init();
}

/// Handles keyboard animations
pub fn start_keyboard_animator_task() -> JoinHandle<()> {
    // Start the keyboard animator thread,
    thread::spawn(|| {
        loop {
            // Skip USB writes entirely while device is suspended — the HID fd
            // is closed during sleep and writes would fail or spin-wait.
            if !SYSTEM_SLEEPING.load(Ordering::Relaxed) {
                if let (Ok(mut dev), Ok(mut fx)) = (DEV_MANAGER.lock(), EFFECT_MANAGER.lock()) {
                    // If the current device has accumulated too many consecutive HID
                    // failures it was likely replaced by the kernel (e.g. USB
                    // re-enumeration mid-session). Drop it and re-open immediately.
                    let is_stale = dev.get_device().map_or(false, |l| l.is_stale());
                    if is_stale {
                        warn!("HID device stale (consecutive write failures), re-discovering...");
                        dev.device = None;
                        dev.discover_devices();
                    }
                    if let Some(laptop) = dev.get_device() {
                        fx.update(laptop);
                    }
                }
            }
            thread::sleep(std::time::Duration::from_millis(kbd::ANIMATION_SLEEP_MS));
        }
    })
}

fn gpu_monitor_on_ac() -> bool {
    DEV_MANAGER
        .lock()
        .ok()
        .and_then(|mut d| d.get_device().map(|laptop| laptop.get_ac_state() == 1))
        .unwrap_or(true)
}

/// Polls the GPU every 3 s (on AC) or 10 s (on battery) and keeps the status
/// cache fresh for GUI queries.
///
/// Idle backoff: after 5 consecutive polls where gpu_util==0 and power<15 W
/// (≈15 s of genuine idle), the poller stops querying for 10 polls (≈30 s) to
/// allow the NVIDIA runtime-PM system to autosuspend the dGPU.  This prevents
/// the continuous NVML file-descriptor open/close cycle from blocking suspend.
pub fn start_gpu_load_monitor_task() -> JoinHandle<()> {
    thread::spawn(|| {
        const IDLE_SUSPEND_AFTER: u32 = 5;  // 5 × 3 s = 15 s idle → start backoff
        const IDLE_BACKOFF_POLLS: u32 = 10; // 10 × 3 s = 30 s backoff window

        let mut idle_count: u32 = 0;
        let mut backoff_remaining: u32 = 0;

        // Do one query immediately so the cache is populated before the first
        // GUI poll arrives (which may happen within a second of startup).
        if gpu::should_query_nvidia(gpu_monitor_on_ac()) {
            if let Some(status) = gpu::query_nvidia_gpu() {
                gpu::store_gpu_cache(&status);
            }
        }

        loop {
            let on_ac = gpu_monitor_on_ac();
            thread::sleep(std::time::Duration::from_secs(if on_ac { 3 } else { 10 }));

            if SYSTEM_SLEEPING.load(Ordering::Relaxed) {
                gpu::clear_gpu_cache();
                idle_count = 0;
                backoff_remaining = 0;
                continue;
            }

            // During backoff window: skip querying so the GPU can runtime-suspend.
            // Store a zero-metric placeholder (stale=true) so the GUI chart goes
            // dark rather than keeping the last real sample on screen.
            if backoff_remaining > 0 {
                backoff_remaining -= 1;
                let idle_name = gpu::get_cached_gpu_status()
                    .map(|s| s.name)
                    .unwrap_or_else(|| "NVIDIA GPU".into());
                gpu::store_gpu_cache(&gpu::GpuStatus {
                    name: idle_name,
                    ..gpu::GpuStatus::default()
                });
                continue;
            }

            if !gpu::should_query_nvidia(on_ac) {
                // GPU is already runtime-suspended — reset idle counter.
                gpu::clear_gpu_cache();
                idle_count = 0;
                continue;
            }

            if let Some(status) = gpu::query_nvidia_gpu() {
                // Idle detection: gpu_util==0 and low power means display-only
                // overhead or genuine deep idle.  After IDLE_SUSPEND_AFTER
                // consecutive such samples, enter backoff to let the GPU sleep.
                if status.gpu_util == 0 && status.power_w < 15.0 {
                    idle_count += 1;
                    if idle_count >= IDLE_SUSPEND_AFTER {
                        info!("GPU idle for {} polls, entering {}-poll backoff to allow runtime-suspend",
                              IDLE_SUSPEND_AFTER, IDLE_BACKOFF_POLLS);
                        idle_count = 0;
                        backoff_remaining = IDLE_BACKOFF_POLLS;
                        // Immediately store zeros so the GUI chart clears on the next tick.
                        let idle_name = gpu::get_cached_gpu_status()
                            .map(|s| s.name)
                            .unwrap_or_else(|| "NVIDIA GPU".into());
                        gpu::store_gpu_cache(&gpu::GpuStatus {
                            name: idle_name,
                            ..gpu::GpuStatus::default()
                        });
                        continue;
                    }
                } else {
                    idle_count = 0;
                }
                gpu::store_gpu_cache(&status);
            } else {
                idle_count = 0;
            }
        }
    })
}

pub fn start_temp_fan_control_task() -> JoinHandle<()> {
    thread::spawn(|| {
        let mut controllers = [TempFanController::default(), TempFanController::default()];

        loop {
            thread::sleep(std::time::Duration::from_secs(1));

            if SYSTEM_SLEEPING.load(Ordering::Relaxed) {
                continue;
            }

            let mut manager = match DEV_MANAGER.lock() {
                Ok(manager) => manager,
                Err(error) => {
                    error!("DEV_MANAGER lock failed in temp fan controller: {}", error);
                    continue;
                }
            };

            let current_ac = manager
                .get_device()
                .map(|laptop| laptop.get_ac_state())
                .unwrap_or(1)
                .min(1);
            let target = manager.get_temp_target(current_ac);
            if target <= 0 {
                controllers[current_ac] = TempFanController::default();
                continue;
            }

            let (min_rpm, max_rpm) = manager.get_fan_range();
            let current_rpm = manager
                .get_fan_tachometer()
                .max(manager.get_fan_rpm(current_ac));

            let cpu_temp_c = read_cpu_package_temp_c();
            let gpu_status = gpu::get_cached_gpu_status();
            let gpu_temp_c = gpu_status.as_ref().map(|status| status.temp_c as f32);
            let gpu_util = gpu_status
                .as_ref()
                .map(|status| status.gpu_util as f32)
                .unwrap_or(0.0);

            let raw_temp = cpu_temp_c
                .into_iter()
                .chain(gpu_temp_c.into_iter())
                .fold(0.0_f32, f32::max);
            if raw_temp <= 0.0 {
                continue;
            }

            if let Some(next_rpm) = compute_temp_target_rpm(
                &mut controllers[current_ac],
                raw_temp,
                gpu_util,
                target,
                current_rpm.max(0) as f32,
                min_rpm as f32,
                max_rpm as f32,
            ) {
                let _ = manager.apply_runtime_fan_rpm(current_ac, next_rpm);
            }
        }
    })
}

fn start_screensaver_monitor_task() -> JoinHandle<()> {
    thread::spawn(move || {
        let dbus_session =
            Connection::new_session().expect("failed to connect to D-Bus session bus");
        // Uses org.freedesktop.ScreenSaver which is supported by both KDE Plasma and GNOME
        let proxy = dbus_session.with_proxy(
            "org.freedesktop.ScreenSaver",
            "/org/freedesktop/ScreenSaver",
            time::Duration::from_millis(5000),
        );
        let _id = proxy.match_signal(
            |h: screensaver::OrgFreedesktopScreenSaverActiveChanged,
             _: &Connection,
             _: &Message| {
                // Ignore screensaver events while the system is suspended — the HID
                // device is closed and restore_light() would fail silently.
                if SYSTEM_SLEEPING.load(Ordering::Relaxed) {
                    return true;
                }
                match DEV_MANAGER.lock() {
                    Ok(mut d) => {
                        if h.arg0 {
                            d.light_off();
                        } else {
                            d.restore_light();
                        }
                    }
                    Err(e) => error!("DEV_MANAGER lock failed in screensaver handler: {}", e),
                }
                true
            },
        );

        loop {
            dbus_session.process(time::Duration::from_millis(1000)).ok();
        }
    })
}

fn start_battery_monitor_task() -> JoinHandle<()> {
    thread::spawn(move || {
        let dbus_system =
            Connection::new_system().expect("should be able to connect to D-Bus system bus");
        info!("Connected to the system D-Bus");

        let proxy_ac = dbus_system.with_proxy(
            "org.freedesktop.UPower",
            "/org/freedesktop/UPower/devices/line_power_AC0",
            time::Duration::from_millis(5000),
        );

        let proxy_battery = dbus_system.with_proxy(
            "org.freedesktop.UPower",
            "/org/freedesktop/UPower/devices/battery_BAT0",
            time::Duration::from_millis(5000),
        );

        let proxy_login = dbus_system.with_proxy(
            "org.freedesktop.login1",
            "/org/freedesktop/login1",
            time::Duration::from_millis(5000),
        );

        let _id = proxy_ac.match_signal(
            |h: battery::OrgFreedesktopDBusPropertiesPropertiesChanged,
             _: &Connection,
             _: &Message| {
                let online: Option<&bool> = arg::prop_cast(&h.changed_properties, "Online");
                if let Some(online) = online {
                    info!("AC0 online: {:?}", online);
                    if let Ok(mut d) = DEV_MANAGER.lock() {
                        d.set_ac_state(*online);
                        // Apply the RAPL profile saved for the new AC state.
                        let ac = if *online { 1usize } else { 0usize };
                        if let Some(cfg) = d.get_ac_config(ac) {
                            apply_rapl_for_profile(cfg.rapl_pl1_watts, cfg.rapl_pl2_watts);
                        }
                    }
                    // Always clear GPU cache on AC state transition.
                    gpu::clear_gpu_cache();
                    apply_low_battery_lighting(read_last_battery_percent(), *online);
                }
                true
            },
        );

        let _id = proxy_battery.match_signal(
            |h: battery::OrgFreedesktopDBusPropertiesPropertiesChanged,
             _: &Connection,
             _: &Message| {
                let perc: Option<&f64> = arg::prop_cast(&h.changed_properties, "Percentage");
                if let Some(perc) = perc {
                    info!("Battery percentage: {:.1}", perc);
                    store_last_battery_percent(*perc);
                    let on_ac = DEV_MANAGER
                        .lock()
                        .ok()
                        .and_then(|mut d| d.get_device().map(|laptop| laptop.get_ac_state() == 1))
                        .unwrap_or(false);
                    apply_low_battery_lighting(*perc, on_ac);
                }
                true
            },
        );

        let _id = proxy_login.match_signal(|h: login1::OrgFreedesktopLogin1ManagerPrepareForSleep, _: &Connection, _: &Message| {
            info!("PrepareForSleep start={}", h.start);
            if h.start {
                // Going to sleep: blank keyboard then close HID device so the
                // USB subsystem can suspend the endpoint cleanly, avoiding
                // the multi-second stall that occurs when the kernel tears
                // down a device that still has open file descriptors.
                SYSTEM_SLEEPING.store(true, Ordering::SeqCst);
                if let Ok(mut d) = DEV_MANAGER.lock() {
                    d.light_off();
                    d.device = None; // drop HidDevice → close fd → USB can suspend
                }
            } else {
                // Waking up: offload all recovery to a background thread so
                // the D-Bus dispatch loop returns immediately. If recovery
                // runs inside this callback it blocks dbus_system.process()
                // for ~5-7 s; any queued PrepareForSleep(true) then fires
                // the moment we return, causing an immediate re-suspend.
                thread::spawn(|| {
                    // Wait for the NVIDIA display pipeline (PRIME/NVPCF) to
                    // reinitialise before any HID/EC writes. Early EC traffic
                    // can prevent the screen coming back after resume.
                    thread::sleep(std::time::Duration::from_millis(2000));
                    let mut discovered = false;
                    for attempt in 0..5_u32 {
                        let has_device = match DEV_MANAGER.lock() {
                            Ok(mut d) => { d.discover_devices(); d.device.is_some() }
                            Err(e) => { error!("DEV_MANAGER lock failed on resume: {}", e); break; }
                        };
                        if has_device {
                            info!("HID device ready after resume (attempt {})", attempt + 1);
                            discovered = true;
                            break;
                        }
                        warn!("HID device not ready on resume attempt {}, retrying in 300 ms", attempt + 1);
                        thread::sleep(std::time::Duration::from_millis(300));
                    }
                    if !discovered {
                        warn!("HID device unavailable after 5 attempts; backlight will remain off");
                    }
                    if let Ok(mut d) = DEV_MANAGER.lock() {
                        d.set_ac_state_get();
                        if discovered { d.restore_light(); }
                    }
                    SYSTEM_SLEEPING.store(false, Ordering::SeqCst);
                });
            }
            true
        });

        loop {
            if let Err(e) = dbus_system.process(time::Duration::from_millis(1000)) {
                error!("D-Bus system connection error: {}", e);
            }
        }
    })
}

/// Monitors signals and stops the daemon when receiving one
pub fn start_shutdown_task() -> JoinHandle<()> {
    thread::spawn(|| {
        let mut signals = Signals::new([SIGINT, SIGTERM]).unwrap();
        let _ = signals.forever().next();

        // If we reach this point, we have a signal and it is time to exit
        println!("Received signal, cleaning up");
        let json = EFFECT_MANAGER.lock().unwrap().save();
        if let Err(error) = config::Configuration::write_effects_save(json) {
            error!("Error writing config {}", error);
        }
        if std::fs::metadata(comms::SOCKET_PATH).is_ok() {
            std::fs::remove_file(comms::SOCKET_PATH).unwrap();
        }
        std::process::exit(0);
    })
}

fn handle_data(mut stream: UnixStream) {
    let mut buffer = [0u8; 4096];
    if stream.read(&mut buffer).is_err() {
        return;
    }

    if let Some(cmd) = comms::read_from_socket_req(&buffer) {
        if let Some(s) = process_client_request(cmd) {
            if let Ok(x) = bincode::serialize(&s) {
                let result = stream.write_all(&x);

                if let Err(error) = result {
                    println!("Client disconnected with error: {error}");
                }
            }
        }
    }
}

pub fn process_client_request(cmd: comms::DaemonCommand) -> Option<comms::DaemonResponse> {
    if let Ok(mut d) = DEV_MANAGER.lock() {
        return match cmd {
            comms::DaemonCommand::SetPowerMode { ac, pwr, cpu, gpu } => {
                let ok = d.set_power_mode(ac, pwr, cpu, gpu);
                // Verify the EC actually applied the mode — if HID write fails,
                // the readback will differ from what we sent.
                let confirmed = d.get_power_mode(ac);
                if confirmed == pwr {
                    info!(
                        "Power mode set OK (pwr={} cpu={} gpu={} ac={})",
                        pwr, cpu, gpu, ac
                    );
                } else {
                    warn!("Power mode mismatch: sent {} but EC reports {} (HID write may have failed)", pwr, confirmed);
                }
                // Invalidate the GPU cache so the next GetGpuStatus call fetches
                // fresh nvidia-smi data reflecting the new TGP for this profile.
                // nvidia-powerd (NVPCF2) may take ~1-2 s to update enforced.power.limit;
                // the stale cache value would otherwise persist for the full 3 s poll
                // window and confuse the user into thinking TGP did not change.
                gpu::clear_gpu_cache();
                Some(comms::DaemonResponse::SetPowerMode { result: ok })
            }
            comms::DaemonCommand::SetFanSpeed { ac, rpm } => {
                Some(comms::DaemonResponse::SetFanSpeed {
                    result: d.set_fan_rpm(ac, rpm),
                })
            }
            comms::DaemonCommand::SetFanTemperatureTarget { ac, temp_c } => {
                Some(comms::DaemonResponse::SetFanTemperatureTarget {
                    result: d.set_temp_target(ac, temp_c),
                })
            }
            comms::DaemonCommand::SetLogoLedState { ac, logo_state } => {
                Some(comms::DaemonResponse::SetLogoLedState {
                    result: d.set_logo_led_state(ac, logo_state),
                })
            }
            comms::DaemonCommand::SetBrightness { ac, val } => {
                Some(comms::DaemonResponse::SetBrightness {
                    result: d.set_brightness(ac, val),
                })
            }
            comms::DaemonCommand::SetIdle { ac, val } => Some(comms::DaemonResponse::SetIdle {
                result: d.change_idle(ac, val),
            }),
            comms::DaemonCommand::SetSync { sync } => Some(comms::DaemonResponse::SetSync {
                result: d.set_sync(sync),
            }),
            comms::DaemonCommand::GetBrightness { ac } => {
                Some(comms::DaemonResponse::GetBrightness {
                    result: d.get_brightness(ac),
                })
            }
            comms::DaemonCommand::GetLogoLedState { ac } => {
                Some(comms::DaemonResponse::GetLogoLedState {
                    logo_state: d.get_logo_led_state(ac),
                })
            }
            comms::DaemonCommand::GetKeyboardRGB { layer } => {
                let map = EFFECT_MANAGER.lock().unwrap().get_map(layer);
                Some(comms::DaemonResponse::GetKeyboardRGB {
                    layer,
                    rgbdata: map,
                })
            }
            comms::DaemonCommand::GetSync() => {
                Some(comms::DaemonResponse::GetSync { sync: d.get_sync() })
            }
            comms::DaemonCommand::GetFanSpeed { ac } => Some(comms::DaemonResponse::GetFanSpeed {
                rpm: d.get_fan_rpm(ac),
            }),
            comms::DaemonCommand::GetFanTemperatureTarget { ac } => {
                Some(comms::DaemonResponse::GetFanTemperatureTarget {
                    temp_c: d.get_temp_target(ac),
                })
            }
            comms::DaemonCommand::GetPwrLevel { ac } => Some(comms::DaemonResponse::GetPwrLevel {
                pwr: d.get_power_mode(ac),
            }),
            comms::DaemonCommand::GetCPUBoost { ac } => Some(comms::DaemonResponse::GetCPUBoost {
                cpu: d.get_cpu_boost(ac),
            }),
            comms::DaemonCommand::GetGPUBoost { ac } => Some(comms::DaemonResponse::GetGPUBoost {
                gpu: d.get_gpu_boost(ac),
            }),
            comms::DaemonCommand::SetEffect { name, params } => {
                let mut res = false;
                if let Ok(mut k) = EFFECT_MANAGER.lock() {
                    let effect = match name.as_str() {
                        "static" => Some(kbd::effects::Static::new(params)),
                        "static_gradient" => Some(kbd::effects::StaticGradient::new(params)),
                        "wave_gradient" => Some(kbd::effects::WaveGradient::new(params)),
                        "breathing_single" => Some(kbd::effects::BreathSingle::new(params)),
                        "breathing_dual" => Some(kbd::effects::BreathDual::new(params)),
                        "spectrum_cycle" => Some(kbd::effects::SpectrumCycle::new(params)),
                        "rainbow_wave" => Some(kbd::effects::RainbowWave::new(params)),
                        "starlight" => Some(kbd::effects::Starlight::new(params)),
                        "ripple" => Some(kbd::effects::Ripple::new(params)),
                        "wheel" => Some(kbd::effects::Wheel::new(params)),
                        _ => None,
                    };

                    if let Some(laptop) = d.get_device() {
                        if let Some(e) = effect {
                            k.pop_effect(laptop); // Remove old layer
                            k.push_effect(e, [true; 90]);
                            res = true; // only set true after push_effect succeeds
                        }
                    }
                }
                // Persist immediately so effects survive a crash / force-kill.
                if res {
                    if let Ok(mut k) = EFFECT_MANAGER.lock() {
                        let json = k.save();
                        if let Err(e) = config::Configuration::write_effects_save(json) {
                            error!("Failed to save effects: {}", e);
                        }
                    }
                }
                Some(comms::DaemonResponse::SetEffect { result: res })
            }

            comms::DaemonCommand::SetStandardEffect { name, params } => {
                // TODO save standart effect may be struct ?
                let mut res = false;
                if let Some(laptop) = d.get_device() {
                    if let Ok(mut k) = EFFECT_MANAGER.lock() {
                        k.pop_effect(laptop); // Remove old layer
                        let _res = match name.as_str() {
                            "off" => d.set_standard_effect(device::RazerLaptop::OFF, params),
                            "wave" => d.set_standard_effect(device::RazerLaptop::WAVE, params),
                            "reactive" => {
                                d.set_standard_effect(device::RazerLaptop::REACTIVE, params)
                            }
                            "breathing" => {
                                d.set_standard_effect(device::RazerLaptop::BREATHING, params)
                            }
                            "spectrum" => {
                                d.set_standard_effect(device::RazerLaptop::SPECTRUM, params)
                            }
                            "static" => d.set_standard_effect(device::RazerLaptop::STATIC, params),
                            "starlight" => {
                                d.set_standard_effect(device::RazerLaptop::STARLIGHT, params)
                            }
                            _ => false,
                        };
                        res = _res;
                    }
                } else {
                    res = false;
                }
                Some(comms::DaemonResponse::SetStandardEffect { result: res })
            }
            comms::DaemonCommand::SetBatteryHealthOptimizer { is_on, threshold } => {
                return Some(comms::DaemonResponse::SetBatteryHealthOptimizer {
                    result: d.set_bho_handler(is_on, threshold),
                });
            }
            comms::DaemonCommand::GetBatteryHealthOptimizer() => {
                return d.get_bho_handler().map(|result| {
                    comms::DaemonResponse::GetBatteryHealthOptimizer {
                        is_on: (result.0),
                        threshold: (result.1),
                    }
                });
            }
            comms::DaemonCommand::GetDeviceName => {
                let name = match &d.device {
                    Some(device) => device.get_name().to_string(),
                    None => "Unknown Device".to_string(),
                };
                return Some(comms::DaemonResponse::GetDeviceName { name });
            }

            comms::DaemonCommand::GetGpuStatus => {
                // Only read from the cache populated by the GPU load monitor.
                // Direct fallback queries have been removed: they defeated the
                // idle backoff and would prevent NVIDIA runtime-PM from suspending
                // the dGPU.  The monitor task populates or zeroes the cache each
                // poll; a cold cache (startup or sleeping) returns stale=true.
                let status = gpu::get_cached_gpu_status().unwrap_or_else(|| gpu::GpuStatus {
                    name: "NVIDIA GPU".into(),
                    ..gpu::GpuStatus::default()
                });
                return Some(comms::DaemonResponse::GetGpuStatus {
                    name: status.name,
                    temp_c: status.temp_c,
                    gpu_util: status.gpu_util,
                    mem_util: status.mem_util,
                    stale: status.power_w <= 0.0
                        && status.clock_gpu_mhz == 0
                        && status.clock_mem_mhz == 0,
                    power_w: status.power_w,
                    power_limit_w: status.power_limit_w,
                    power_max_limit_w: status.power_max_limit_w,
                    mem_used_mb: status.mem_used_mb,
                    mem_total_mb: status.mem_total_mb,
                    clock_gpu_mhz: status.clock_gpu_mhz,
                    clock_mem_mhz: status.clock_mem_mhz,
                });
            }

            comms::DaemonCommand::GetPowerLimits { ac } => {
                let pl1_max =
                    read_rapl_uw("/sys/class/powercap/intel-rapl:0/constraint_0_max_power_uw");
                // Return the saved config values for the requested AC profile if set,
                // otherwise fall back to the current sysfs (live) values.
                let (pl1_cfg, pl2_cfg) = d.get_rapl_limits(ac);
                let (pl1, pl2) = if pl1_cfg > 0 {
                    (pl1_cfg as u64 * 1_000_000, pl2_cfg as u64 * 1_000_000)
                } else {
                    (
                        read_rapl_uw(
                            "/sys/class/powercap/intel-rapl:0/constraint_0_power_limit_uw",
                        ),
                        read_rapl_uw(
                            "/sys/class/powercap/intel-rapl:0/constraint_1_power_limit_uw",
                        ),
                    )
                };
                return Some(comms::DaemonResponse::GetPowerLimits {
                    pl1_watts: (pl1 / 1_000_000) as u32,
                    pl2_watts: (pl2 / 1_000_000) as u32,
                    pl1_max_watts: (pl1_max / 1_000_000) as u32,
                });
            }

            comms::DaemonCommand::SetPowerLimits {
                ac,
                pl1_watts,
                pl2_watts,
            } => {
                // Persist to config for the given AC profile.
                let saved = d.set_rapl_limits(ac, pl1_watts, pl2_watts);
                // Apply immediately only if we're currently on the matching AC state.
                let current_ac = d.get_device().map(|l| l.get_ac_state()).unwrap_or(1);
                let applied = if current_ac == ac {
                    let ok1 = write_rapl_uw(
                        "/sys/class/powercap/intel-rapl:0/constraint_0_power_limit_uw",
                        pl1_watts as u64 * 1_000_000,
                    );
                    let ok2 = write_rapl_uw(
                        "/sys/class/powercap/intel-rapl:0/constraint_1_power_limit_uw",
                        pl2_watts as u64 * 1_000_000,
                    );
                    ok1 && ok2
                } else {
                    true // saved to config; will apply on next AC state switch
                };
                return Some(comms::DaemonResponse::SetPowerLimits {
                    result: saved && applied,
                });
            }

            comms::DaemonCommand::GetCurrentEffect => {
                let info = EFFECT_MANAGER
                    .lock()
                    .ok()
                    .and_then(|mut em| em.get_current_effect_info());
                let (name, args) = info.unwrap_or_else(|| (String::new(), Vec::new()));
                return Some(comms::DaemonResponse::GetCurrentEffect { name, args });
            }

            comms::DaemonCommand::GetFanTachometer => {
                Some(comms::DaemonResponse::GetFanTachometer {
                    rpm: d.get_fan_tachometer(),
                })
            }

            comms::DaemonCommand::SetLowBatteryLighting { threshold_pct } => {
                let result = d.set_low_battery_lighting_threshold(threshold_pct);
                let on_ac = d
                    .get_device()
                    .map(|laptop| laptop.get_ac_state() == 1)
                    .unwrap_or(false);
                drop(d);
                apply_low_battery_lighting(read_last_battery_percent(), on_ac);
                Some(comms::DaemonResponse::SetLowBatteryLighting { result })
            }

            comms::DaemonCommand::GetLowBatteryLighting => {
                Some(comms::DaemonResponse::GetLowBatteryLighting {
                    threshold_pct: d.get_low_battery_lighting_threshold(),
                })
            }
        };
    } else {
        return None;
    }
}

fn read_rapl_uw(path: &str) -> u64 {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

fn write_rapl_uw(path: &str, value: u64) -> bool {
    std::fs::write(path, value.to_string()).is_ok()
}

/// Apply RAPL PL1/PL2 limits from a saved profile.
/// Skips fields that are zero (meaning "not configured by user").
fn apply_rapl_for_profile(pl1_watts: u32, pl2_watts: u32) {
    if pl1_watts > 0 {
        let _ = write_rapl_uw(
            "/sys/class/powercap/intel-rapl:0/constraint_0_power_limit_uw",
            pl1_watts as u64 * 1_000_000,
        );
        info!("Applied RAPL PL1 = {} W", pl1_watts);
    }
    if pl2_watts > 0 {
        let _ = write_rapl_uw(
            "/sys/class/powercap/intel-rapl:0/constraint_1_power_limit_uw",
            pl2_watts as u64 * 1_000_000,
        );
        info!("Applied RAPL PL2 = {} W", pl2_watts);
    }
}

fn store_last_battery_percent(percentage: f64) {
    if let Ok(mut last) = LAST_BATTERY_PERCENT.lock() {
        *last = percentage.clamp(0.0, 100.0);
    }
}

fn read_last_battery_percent() -> f64 {
    LAST_BATTERY_PERCENT
        .lock()
        .map(|last| *last)
        .unwrap_or(100.0)
}

fn apply_low_battery_lighting(battery_pct: f64, on_ac: bool) {
    if SYSTEM_SLEEPING.load(Ordering::Relaxed) {
        return;
    }

    let threshold_pct = DEV_MANAGER
        .lock()
        .ok()
        .map(|mut manager| manager.get_low_battery_lighting_threshold())
        .unwrap_or(0.0);
    let should_dim = !on_ac && threshold_pct > 0.0 && battery_pct <= threshold_pct;
    let was_dimmed = LOW_BAT_LIGHTS_FORCED_OFF.swap(should_dim, Ordering::SeqCst);

    if should_dim && !was_dimmed {
        if let Ok(mut manager) = DEV_MANAGER.lock() {
            manager.light_off();
            info!(
                "Low-battery lighting engaged at {:.1}% (threshold {:.1}%)",
                battery_pct, threshold_pct,
            );
        }
    } else if !should_dim && was_dimmed {
        if let Ok(mut manager) = DEV_MANAGER.lock() {
            manager.restore_light();
            info!(
                "Low-battery lighting cleared at {:.1}% (threshold {:.1}%)",
                battery_pct, threshold_pct,
            );
        }
    }
}

fn read_cpu_package_temp_c() -> Option<f32> {
    let hwmons = std::fs::read_dir("/sys/class/hwmon").ok()?;
    let mut best_temp: Option<f32> = None;

    for hwmon in hwmons.flatten() {
        let path = hwmon.path();
        for index in 1..=10 {
            let label_path = path.join(format!("temp{}_label", index));
            let input_path = path.join(format!("temp{}_input", index));
            let label = std::fs::read_to_string(&label_path).ok();
            let is_cpu_label = label.as_ref().is_some_and(|label| {
                let label = label.trim();
                label.eq_ignore_ascii_case("Package id 0")
                    || label.eq_ignore_ascii_case("Tctl")
                    || label.eq_ignore_ascii_case("Tdie")
                    || label.to_ascii_lowercase().contains("package")
            });
            if !is_cpu_label || !input_path.exists() {
                continue;
            }
            let temp_c = std::fs::read_to_string(&input_path)
                .ok()
                .and_then(|value| value.trim().parse::<f32>().ok())
                .map(|millideg| millideg / 1000.0);
            if let Some(temp_c) = temp_c {
                best_temp = Some(best_temp.map_or(temp_c, |current| current.max(temp_c)));
            }
        }
    }

    if best_temp.is_some() {
        return best_temp;
    }

    let zones = std::fs::read_dir("/sys/class/thermal").ok()?;
    for zone in zones.flatten() {
        let path = zone.path();
        let type_path = path.join("type");
        let temp_path = path.join("temp");
        let zone_type = std::fs::read_to_string(type_path).ok()?;
        if !zone_type.trim().eq_ignore_ascii_case("x86_pkg_temp") || !temp_path.exists() {
            continue;
        }
        if let Ok(temp) = std::fs::read_to_string(temp_path) {
            if let Ok(temp) = temp.trim().parse::<f32>() {
                return Some(temp / 1000.0);
            }
        }
    }

    None
}

fn compute_temp_target_rpm(
    controller: &mut TempFanController,
    raw_temp: f32,
    raw_util: f32,
    target_temp: i32,
    current_rpm: f32,
    min_rpm: f32,
    max_rpm: f32,
) -> Option<i32> {
    let now = std::time::Instant::now();
    let mut dt = now.duration_since(controller.last_update).as_secs_f32();
    if dt < 0.25 {
        return None;
    }

    controller.last_update = now;
    dt = dt.clamp(0.25, 1.5);

    let raw_temp = raw_temp.clamp(0.0, 120.0);
    let raw_util = raw_util.clamp(0.0, 100.0);
    let min_rpm = min_rpm.min(max_rpm);
    let max_rpm = max_rpm.max(min_rpm);

    const TEMP_ALPHA: f32 = 0.35;
    const UTIL_ALPHA: f32 = 0.18;
    const KP: f32 = 170.0;
    const KI: f32 = 10.0;
    const KD: f32 = 40.0;
    const STEP_UP: f32 = 1000.0;
    const STEP_DOWN: f32 = 250.0;
    const ENERGY_INPUT_GAIN: f32 = 0.02;
    const ENERGY_DECAY: f32 = 0.92;
    const TEMP_PREDICT_GAIN: f32 = 1.6;
    const ENERGY_PREDICT_GAIN: f32 = 15.0;

    if controller.smoothed_temp == 0.0 {
        controller.smoothed_temp = raw_temp;
        controller.smoothed_util = raw_util;
        controller.prev_temp = raw_temp;
    }

    controller.smoothed_temp =
        TEMP_ALPHA * raw_temp + (1.0 - TEMP_ALPHA) * controller.smoothed_temp;
    controller.smoothed_util =
        UTIL_ALPHA * raw_util + (1.0 - UTIL_ALPHA) * controller.smoothed_util;

    let temp = controller.smoothed_temp;
    let util = controller.smoothed_util;
    controller.thermal_energy += util * ENERGY_INPUT_GAIN * dt;
    controller.thermal_energy *= ENERGY_DECAY;

    let velocity = (temp - controller.prev_temp) / dt;
    let predicted_temp =
        (temp + velocity * TEMP_PREDICT_GAIN + controller.thermal_energy * ENERGY_PREDICT_GAIN)
            .clamp(temp - 5.0, temp + 10.0);

    controller.prev_temp = temp;

    let error = predicted_temp - target_temp as f32;

    if raw_util < 5.0 && temp < target_temp as f32 - 8.0 {
        let idle_rpm = quantize_rpm(min_rpm, min_rpm, max_rpm) as i32;
        return if idle_rpm != current_rpm as i32 {
            Some(idle_rpm)
        } else {
            None
        };
    }

    if error.abs() < 0.8 && util < 35.0 {
        return None;
    }

    if error.abs() < 8.0 {
        controller.integral += error * dt;
        controller.integral = controller.integral.clamp(-160.0, 160.0);
    } else {
        controller.integral *= 0.9;
    }

    let pid = KP * error + KI * controller.integral + KD * velocity;
    let desired = (min_rpm + pid).clamp(min_rpm, max_rpm);
    let next = if desired > current_rpm {
        (current_rpm + STEP_UP * dt).min(desired)
    } else {
        (current_rpm - STEP_DOWN * dt).max(desired)
    };

    let next_rpm = quantize_rpm(next, min_rpm, max_rpm) as i32;
    if next_rpm != current_rpm as i32 {
        Some(next_rpm)
    } else {
        None
    }
}

fn quantize_rpm(rpm: f32, min_rpm: f32, max_rpm: f32) -> f32 {
    const FAN_STEPS: [f32; 9] = [
        1500.0, 1800.0, 2100.0, 2500.0, 3000.0, 3500.0, 4000.0, 4500.0, 5000.0,
    ];

    let mut steps: Vec<f32> = FAN_STEPS
        .iter()
        .copied()
        .filter(|step| *step >= min_rpm && *step <= max_rpm)
        .collect();
    if steps.is_empty() {
        steps.push(min_rpm);
        if (max_rpm - min_rpm).abs() > f32::EPSILON {
            steps.push(max_rpm);
        }
    }

    steps
        .iter()
        .min_by(|left, right| {
            (rpm.clamp(min_rpm, max_rpm) - **left)
                .abs()
                .partial_cmp(&(rpm.clamp(min_rpm, max_rpm) - **right).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .copied()
        .unwrap_or(rpm.clamp(min_rpm, max_rpm))
}
