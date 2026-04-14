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

// Main function for daemon
fn main() {
    setup_panic_hook();
    init_logging();

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
                apply_low_battery_lighting(percentage, online);
            }
        } else {
            println!("error getting current power state");
            std::process::exit(1);
        }
    }

    start_keyboard_animator_task();
    start_gpu_load_monitor_task();
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
/// cache fresh for GUI queries.  Anti-flicker guard logic has been removed;
/// see memo.md "Archived — PRIME Anti-Flicker Guard Design" for reference.
pub fn start_gpu_load_monitor_task() -> JoinHandle<()> {
    thread::spawn(|| {
        // Do one query immediately so the cache is populated before the first
        // GUI poll arrives (which may happen within a second of startup).
        if let Some(status) = gpu::query_nvidia_gpu() {
            gpu::store_gpu_cache(&status);
        }

        loop {
            let on_ac = gpu_monitor_on_ac();
            thread::sleep(std::time::Duration::from_secs(if on_ac { 3 } else { 10 }));

            if SYSTEM_SLEEPING.load(Ordering::Relaxed) {
                continue;
            }
            if !gpu::should_query_nvidia(on_ac) {
                gpu::clear_gpu_cache();
                continue;
            }
            if let Some(status) = gpu::query_nvidia_gpu() {
                gpu::store_gpu_cache(&status);
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
                // Use the cache populated by the GPU load monitor to avoid
                // spawning a second nvidia-smi per GUI poll cycle.
                // Fall back to a direct query only if the cache is cold.
                let on_ac = d
                    .get_device()
                    .map(|laptop| laptop.get_ac_state() == 1)
                    .unwrap_or(true);
                let status = if let Some(status) = gpu::get_cached_gpu_status() {
                    status
                } else if SYSTEM_SLEEPING.load(Ordering::Relaxed)
                    || !gpu::should_query_nvidia(on_ac)
                {
                    gpu::GpuStatus {
                        name: "NVIDIA GPU (runtime suspended)".into(),
                        ..gpu::GpuStatus::default()
                    }
                } else {
                    gpu::query_nvidia_gpu()
                        .map(|s| {
                            gpu::store_gpu_cache(&s);
                            s
                        })
                        .unwrap_or_else(|| gpu::GpuStatus {
                            name: "NVIDIA GPU (runtime suspended)".into(),
                            ..gpu::GpuStatus::default()
                        })
                };
                return Some(comms::DaemonResponse::GetGpuStatus {
                    name: status.name,
                    temp_c: status.temp_c,
                    gpu_util: status.gpu_util,
                    mem_util: status.mem_util,
                    stale: false,
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
