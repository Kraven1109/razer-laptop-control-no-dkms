use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::{atomic::{AtomicU64, Ordering}, Mutex};
use std::thread::{self, JoinHandle};
use std::time;

use log::*;
use lazy_static::lazy_static;
use signal_hook::iterator::Signals;
use signal_hook::consts::{SIGINT, SIGTERM};
use dbus::blocking::Connection;
use dbus::{Message, arg};

#[path = "../comms.rs"]
mod comms;
mod config;
mod kbd;
mod device;
mod battery;
mod screensaver;
mod login1;
mod gpu;

use crate::kbd::Effect;

lazy_static! {
    static ref EFFECT_MANAGER: Mutex<kbd::EffectManager> = Mutex::new(kbd::EffectManager::new());
    static ref DEV_MANAGER: Mutex<device::DeviceManager> = {
        match device::DeviceManager::read_laptops_file() {
            Ok(c) => Mutex::new(c),
            Err(_) => Mutex::new(device::DeviceManager::new()),
        }
    };
    /// Dynamic keyboard animation interval (milliseconds).
    /// Raised under heavy GPU load to reduce EC USB traffic,
    /// mitigating EC interrupt contention with NVPCF Dynamic Boost
    /// that causes PRIME pipeline stalls (display flicker).
    static ref ANIM_SLEEP_MS: AtomicU64 = AtomicU64::new(kbd::ANIMATION_SLEEP_MS);
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
        let dbus_system = Connection::new_system()
            .expect("failed to connect to D-Bus system bus");
        let proxy_ac = dbus_system.with_proxy("org.freedesktop.UPower", "/org/freedesktop/UPower/devices/line_power_AC0", time::Duration::from_millis(5000));
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
            }
            d.restore_standard_effect();
            if let Ok(json) = config::Configuration::read_effects_file() {
                EFFECT_MANAGER.lock().unwrap().load_from_save(json);
            } else {
                println!("No effects save, creating a new one");
                // No effects found, start with a green static layer, just like synapse
                EFFECT_MANAGER.lock().unwrap().push_effect(
                    kbd::effects::Static::new(vec![0, 255, 0]), 
                    [true; 90]
                    );
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
            if let Some(laptop) = DEV_MANAGER.lock().unwrap().get_device() {
                EFFECT_MANAGER.lock().unwrap().update(laptop);
            }
            thread::sleep(std::time::Duration::from_millis(
                ANIM_SLEEP_MS.load(Ordering::Relaxed)
            ));
        }
    })
}

/// Monitors GPU utilization and dynamically adjusts keyboard animation rate.
/// Under heavy GPU load (>70%), animation is slowed to 3 FPS to reduce EC USB
/// HID traffic, lowering EC interrupt load during NVIDIA NVPCF Dynamic Boost
/// negotiations. This mitigates PRIME display pipeline stalls that appear as
/// display flickering on the built-in panel.
pub fn start_gpu_load_monitor_task() -> JoinHandle<()> {
    thread::spawn(|| {
        loop {
            thread::sleep(std::time::Duration::from_secs(5));
            if let Some(status) = gpu::query_nvidia_gpu() {
                let new_sleep = if status.gpu_util >= 70 {
                    333 // ~3 FPS — reduced EC traffic during heavy GPU load
                } else if status.gpu_util <= 20 {
                    kbd::ANIMATION_SLEEP_MS // restore normal 10 FPS when idle
                } else {
                    ANIM_SLEEP_MS.load(Ordering::Relaxed) // keep current rate
                };
                ANIM_SLEEP_MS.store(new_sleep, Ordering::Relaxed);
            }
        }
    })
}

fn start_screensaver_monitor_task() -> JoinHandle<()> {
    thread::spawn(move || {
        let dbus_session = Connection::new_session()
            .expect("failed to connect to D-Bus session bus");
        // Uses org.freedesktop.ScreenSaver which is supported by both KDE Plasma and GNOME
        let proxy = dbus_session.with_proxy("org.freedesktop.ScreenSaver", "/org/freedesktop/ScreenSaver", time::Duration::from_millis(5000));
        let _id = proxy.match_signal(|h: screensaver::OrgFreedesktopScreenSaverActiveChanged, _: &Connection, _: &Message| {
            println!("ActiveChanged {:?}", h.arg0);
            if let Ok(mut d) = DEV_MANAGER.lock() {
                if h.arg0 {
                    d.light_off();
                } else {
                    d.restore_light();
                }
            }
            true
        });

        loop {
            dbus_session.process(time::Duration::from_millis(1000)).ok();
        }
    })
}

fn start_battery_monitor_task() -> JoinHandle<()> {
    thread::spawn(move || {
        let dbus_system = Connection::new_system()
            .expect("should be able to connect to D-Bus system bus");
        info!("Connected to the system D-Bus");

        let proxy_ac = dbus_system.with_proxy(
            "org.freedesktop.UPower",
            "/org/freedesktop/UPower/devices/line_power_AC0",
            time::Duration::from_millis(5000)
        );

        let proxy_battery = dbus_system.with_proxy(
            "org.freedesktop.UPower",
            "/org/freedesktop/UPower/devices/battery_BAT0",
            time::Duration::from_millis(5000)
        );

        let proxy_login = dbus_system.with_proxy(
            "org.freedesktop.login1",
            "/org/freedesktop/login1",
            time::Duration::from_millis(5000)
        );

        let _id = proxy_ac.match_signal(|h: battery::OrgFreedesktopDBusPropertiesPropertiesChanged, _: &Connection, _: &Message| {
            let online: Option<&bool> = arg::prop_cast(&h.changed_properties, "Online");
            if let Some(online) = online {
                info!("AC0 online: {:?}", online);
                if let Ok(mut d) = DEV_MANAGER.lock() {
                    d.set_ac_state(*online);
                }
            }
            true
        });

        let _id = proxy_battery.match_signal(|h: battery::OrgFreedesktopDBusPropertiesPropertiesChanged, _: &Connection, _: &Message| {
            let perc: Option<&f64> = arg::prop_cast(&h.changed_properties, "Percentage");
            if let Some(perc) = perc {
                info!("Battery percentage: {:.1}", perc);
            }
            true
        });

        let _id = proxy_login.match_signal(|h: login1::OrgFreedesktopLogin1ManagerPrepareForSleep, _: &Connection, _: &Message| {
            info!("PrepareForSleep {:?}", h.start);
            if let Ok(mut d) = DEV_MANAGER.lock() {
                d.set_ac_state_get();
                if h.start {
                    d.light_off();
                } else {
                    d.restore_light();
                }
            }
            true
        });

        loop { dbus_system.process(time::Duration::from_millis(1000)).unwrap(); }
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
                // Pin GPU TGP to a stable value matching the power mode, preventing
                // Dynamic Boost oscillation that causes display flickering.
                // EC sets hardware TGP; nvidia-smi SW cap provides a stable software ceiling.
                let tgp: u32 = match pwr {
                    0 => 50,   // Silent
                    1 => 115,  // Balanced
                    2 => 150,  // Gaming
                    3 => 115,  // Creator
                    _ => 115,
                };
                pin_nvidia_tgp(tgp);
                Some(comms::DaemonResponse::SetPowerMode { result: ok })
            },
            comms::DaemonCommand::SetFanSpeed { ac, rpm } => {
                Some(comms::DaemonResponse::SetFanSpeed { result: d.set_fan_rpm(ac, rpm) })
            },
            comms::DaemonCommand::SetLogoLedState{ ac, logo_state } => {
                Some(comms::DaemonResponse::SetLogoLedState { result: d.set_logo_led_state(ac, logo_state) })
            },
            comms::DaemonCommand::SetBrightness { ac, val } => {
                Some(comms::DaemonResponse::SetBrightness {result: d.set_brightness(ac, val) })
            }
            comms::DaemonCommand::SetIdle { ac, val } => {
                Some(comms::DaemonResponse::SetIdle { result: d.change_idle(ac, val) })
            }
            comms::DaemonCommand::SetSync { sync } => {
                Some(comms::DaemonResponse::SetSync { result: d.set_sync(sync) })
            }
            comms::DaemonCommand::GetBrightness{ac} =>  {
                Some(comms::DaemonResponse::GetBrightness { result: d.get_brightness(ac)})
            },
            comms::DaemonCommand::GetLogoLedState{ac} => Some(comms::DaemonResponse::GetLogoLedState {logo_state: d.get_logo_led_state(ac) }),
            comms::DaemonCommand::GetKeyboardRGB { layer } => {
                let map = EFFECT_MANAGER.lock().unwrap().get_map(layer);
                Some(comms::DaemonResponse::GetKeyboardRGB {
                    layer,
                    rgbdata: map,
                })
            }
            comms::DaemonCommand::GetSync() => Some(comms::DaemonResponse::GetSync { sync: d.get_sync() }),
            comms::DaemonCommand::GetFanSpeed{ac} => Some(comms::DaemonResponse::GetFanSpeed { rpm: d.get_fan_rpm(ac)}),
            comms::DaemonCommand::GetPwrLevel{ac} => Some(comms::DaemonResponse::GetPwrLevel { pwr: d.get_power_mode(ac) }),
            comms::DaemonCommand::GetCPUBoost{ac} => Some(comms::DaemonResponse::GetCPUBoost { cpu: d.get_cpu_boost(ac) }),
            comms::DaemonCommand::GetGPUBoost{ac} => Some(comms::DaemonResponse::GetGPUBoost { gpu: d.get_gpu_boost(ac) }),
            comms::DaemonCommand::SetEffect{ name, params } => {
                let mut res = false;
                if let Ok(mut k) = EFFECT_MANAGER.lock() {
                    res = true;
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
                        _ => None
                    };

                    if let Some(laptop) = d.get_device() {
                        if let Some(e) = effect {
                            k.pop_effect(laptop); // Remove old layer
                            k.push_effect(
                                e,
                                [true; 90]
                                );
                        } else {
                            res = false
                        }
                    } else {
                        res = false;
                    }
                }
                Some(comms::DaemonResponse::SetEffect{result: res})
            }

            comms::DaemonCommand::SetStandardEffect{ name, params } => {
                // TODO save standart effect may be struct ?
                let mut res = false;
                if let Some(laptop) = d.get_device() {
                    if let Ok(mut k) = EFFECT_MANAGER.lock() {
                        k.pop_effect(laptop); // Remove old layer
                        let _res = match name.as_str() {
                            "off" => d.set_standard_effect(device::RazerLaptop::OFF, params),
                            "wave" => d.set_standard_effect(device::RazerLaptop::WAVE, params),
                            "reactive" => d.set_standard_effect(device::RazerLaptop::REACTIVE, params),
                            "breathing" => d.set_standard_effect(device::RazerLaptop::BREATHING, params),
                            "spectrum" => d.set_standard_effect(device::RazerLaptop::SPECTRUM, params),
                            "static" => d.set_standard_effect(device::RazerLaptop::STATIC, params),
                            "starlight" => d.set_standard_effect(device::RazerLaptop::STARLIGHT, params), 
                            _ => false,
                        };
                        res = _res;
                    }
                } else {
                    res = false;
                }
                Some(comms::DaemonResponse::SetStandardEffect{result: res})
            }
            comms::DaemonCommand::SetBatteryHealthOptimizer { is_on, threshold } => { 
                return Some(comms::DaemonResponse::SetBatteryHealthOptimizer { result: d.set_bho_handler(is_on, threshold)});
            }
            comms::DaemonCommand::GetBatteryHealthOptimizer() => {
                return d.get_bho_handler().map(|result| 
                    comms::DaemonResponse::GetBatteryHealthOptimizer {
                        is_on: (result.0), 
                        threshold: (result.1) 
                    }
                );
            }
            comms::DaemonCommand::GetDeviceName => {
                let name = match &d.device {
                    Some(device) => device.get_name(),
                    None => "Unknown Device".into()
                };
                return Some(comms::DaemonResponse::GetDeviceName { name });
            }

            comms::DaemonCommand::GetGpuStatus => {
                if let Some(status) = gpu::query_nvidia_gpu() {
                    return Some(comms::DaemonResponse::GetGpuStatus {
                        name: status.name,
                        temp_c: status.temp_c,
                        gpu_util: status.gpu_util,
                        mem_util: status.mem_util,
                        power_w: status.power_w,
                        power_limit_w: status.power_limit_w,
                        mem_used_mb: status.mem_used_mb,
                        mem_total_mb: status.mem_total_mb,
                        clock_gpu_mhz: status.clock_gpu_mhz,
                        clock_mem_mhz: status.clock_mem_mhz,
                    });
                }
                return None;
            }

            comms::DaemonCommand::GetPowerLimits => {
                let pl1 = read_rapl_uw("/sys/class/powercap/intel-rapl:0/constraint_0_power_limit_uw");
                let pl2 = read_rapl_uw("/sys/class/powercap/intel-rapl:0/constraint_1_power_limit_uw");
                let pl1_max = read_rapl_uw("/sys/class/powercap/intel-rapl:0/constraint_0_max_power_uw");
                return Some(comms::DaemonResponse::GetPowerLimits {
                    pl1_watts: (pl1 / 1_000_000) as u32,
                    pl2_watts: (pl2 / 1_000_000) as u32,
                    pl1_max_watts: (pl1_max / 1_000_000) as u32,
                });
            }

            comms::DaemonCommand::SetPowerLimits { pl1_watts, pl2_watts } => {
                let ok1 = write_rapl_uw(
                    "/sys/class/powercap/intel-rapl:0/constraint_0_power_limit_uw",
                    pl1_watts as u64 * 1_000_000,
                );
                let ok2 = write_rapl_uw(
                    "/sys/class/powercap/intel-rapl:0/constraint_1_power_limit_uw",
                    pl2_watts as u64 * 1_000_000,
                );
                return Some(comms::DaemonResponse::SetPowerLimits { result: ok1 && ok2 });
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

/// Pin the NVIDIA GPU software power cap to a stable wattage.
/// This prevents Dynamic Boost 2.0 from oscillating the TGP under load,
/// which on PRIME/Optimus configurations (like Blade 16 2023) causes the
/// NVIDIA GSP firmware↔EC power negotiation loop to emit rapid TGP changes
/// that stall the PRIME frame pipeline and appear as display flickering.
///
/// Uses persistence mode so the cap survives until next daemon restart.
/// The cap is re-applied on every power-mode switch.
fn pin_nvidia_tgp(watts: u32) {
    // Ensure persistence mode first (required for -pl to persist)
    let _ = std::process::Command::new("nvidia-smi")
        .args(["-pm", "1"])
        .output();
    let _ = std::process::Command::new("nvidia-smi")
        .args(["-pl", &watts.to_string()])
        .output();
    info!("Pinned GPU TGP to {}W", watts);
}
