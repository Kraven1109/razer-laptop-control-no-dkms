use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering},
    Condvar, Mutex,
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

/// Principle 2 (revise.md): explicit system power state machine.
/// Background threads must only do hardware I/O when state == AWAKE.
/// Using u8 constants so AtomicU8 can be stored in a lazy_static.
mod sys_state {
    /// Normal operation — all subsystems active.
    pub const AWAKE: u8 = 0;
    /// PrepareForSleep(true) received; tearing down file descriptors.
    pub const SUSPENDING: u8 = 1;
    /// Fully suspended; HID device is closed and USB endpoint is powered off.
    pub const ASLEEP: u8 = 2;
    /// Waking up; waiting for hardware barriers before writing to EC/HID.
    pub const RESUMING: u8 = 3;
}

const RAPL_PL1_UW: &str = "/sys/class/powercap/intel-rapl:0/constraint_0_power_limit_uw";
const RAPL_PL2_UW: &str = "/sys/class/powercap/intel-rapl:0/constraint_1_power_limit_uw";
const RAPL_PL1_MAX_UW: &str = "/sys/class/powercap/intel-rapl:0/constraint_0_max_power_uw";

lazy_static! {
    static ref EFFECT_MANAGER: Mutex<kbd::EffectManager> = Mutex::new(kbd::EffectManager::new());
    static ref DEV_MANAGER: Mutex<device::DeviceManager> = {
        match device::DeviceManager::read_laptops_file() {
            Ok(c) => Mutex::new(c),
            Err(_) => Mutex::new(device::DeviceManager::new()),
        }
    };
    /// Power state machine (see sys_state). Replaces the old AtomicBool for
    /// race-free, multi-phase lifecycle management (principle 2, revise.md).
    static ref SYSTEM_STATE: AtomicU8 = AtomicU8::new(sys_state::AWAKE);
    /// Timestamp (ms since epoch) of the last PrepareForSleep(false) event.
    /// Used for ACPI debouncing (principle 6, revise.md).
    static ref LAST_WAKE_MS: AtomicU64 = AtomicU64::new(0);
    /// Set by the udev monitor thread when a Razer HID device is added.
    /// Resume handler waits on this instead of blind sleep() polling (principle 1).
    /// Uses Condvar so the resume thread truly sleeps (0 CPU) and wakes the
    /// exact microsecond udev signals arrival (revise.md principle 2).
    static ref HID_APPEARED: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());
    static ref LOW_BAT_LIGHTS_FORCED_OFF: AtomicBool = AtomicBool::new(false);
    static ref LAST_BATTERY_PERCENT: Mutex<f64> = Mutex::new(100.0);
    /// Set true on PrepareForSleep(false) and cleared on the first
    /// ActiveChanged signal after wake.  While true the screensaver poll
    /// must NOT blank the KB — the user is at the lock screen and needs
    /// visible keys to type their password.
    static ref RESUME_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
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
    let mut initial_low_battery: Option<(f64, bool, f64)> = None;

    if let Ok(mut d) = DEV_MANAGER.lock() {
        d.discover_devices();
        if let Some(laptop) = d.get_device() {
            println!("supported device: {:?}", laptop.get_name());
        } else {
            println!("no supported device found");
            std::process::exit(1);
        }

        use battery::OrgFreedesktopUPowerDevice;
        let dbus_system = Connection::new_system().expect("failed to connect to D-Bus system bus");
        let proxy_ac = dbus_system.with_proxy(
            "org.freedesktop.UPower",
            "/org/freedesktop/UPower/devices/line_power_AC0",
            time::Duration::from_millis(5000),
        );
        if let Ok(online) = proxy_ac.online() {
            info!("AC0 online: {:?}", online);
            d.set_ac_state(online);
            if let Some(config) = d.get_ac_config(online as usize) {
                if let Some(laptop) = d.get_device() {
                    laptop.set_config(config);
                }
                apply_rapl_for_profile(config.rapl_pl1_watts, config.rapl_pl2_watts);
            }
            d.restore_standard_effect();
            if let Ok(json) = config::Configuration::read_effects_file() {
                EFFECT_MANAGER.lock().unwrap().load_from_save(json);
            } else {
                println!("No effects save, creating a new one");
                EFFECT_MANAGER
                    .lock()
                    .unwrap()
                    .push_effect(kbd::effects::Static::new(vec![0, 255, 0]), [true; 90]);
            }

            let proxy_battery = dbus_system.with_proxy(
                "org.freedesktop.UPower",
                "/org/freedesktop/UPower/devices/battery_BAT0",
                time::Duration::from_millis(5000),
            );
            if let Ok(percentage) = proxy_battery.percentage() {
                store_last_battery_percent(percentage);
                initial_low_battery = Some((
                    percentage,
                    online,
                    d.get_low_battery_lighting_threshold(),
                ));
            }
        } else {
            println!("error getting current power state");
            std::process::exit(1);
        }
    } else {
        println!("error loading supported devices");
        std::process::exit(1);
    }

    if let Some((percentage, online, threshold_pct)) = initial_low_battery {
        apply_low_battery_lighting(percentage, online, threshold_pct);
    }

    start_keyboard_animator_task();
    start_gpu_load_monitor_task();
    start_temp_fan_control_task();
    start_udev_hid_monitor_task(); // Principle 1: event-driven device discovery
    start_screensaver_monitor_task();
    start_battery_monitor_task();
    let clean_thread = start_shutdown_task();

    if let Some(listener) = comms::create() {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => handle_data(stream),
                Err(error) => {
                    error!("Unix socket accept failed: {}", error);
                    thread::sleep(std::time::Duration::from_millis(200));
                }
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
            if let Err(error) = std::fs::remove_file(comms::SOCKET_PATH) {
                eprintln!("Failed to remove socket during panic cleanup: {error}");
            }
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
            // Principle 2: only operate when fully awake (not suspending/resuming).
            if SYSTEM_STATE.load(Ordering::Relaxed) == sys_state::AWAKE {
                if let (Ok(mut dev), Ok(mut fx)) = (DEV_MANAGER.lock(), EFFECT_MANAGER.lock()) {
                    // If the current device has accumulated too many consecutive HID
                    // failures it was likely replaced by the kernel (e.g. USB
                    // re-enumeration mid-session). Drop it and re-open immediately.
                    let is_stale = dev.get_device().map_or(false, |l| l.is_stale());
                    if is_stale {
                        warn!("HID device stale (consecutive write failures), re-discovering...");
                        dev.device = None;
                        dev.discover_devices();
                        // Sync the runtime screensaver flag onto the new RazerLaptop instance
                        // (new instances default to screensaver=false, which is wrong when the
                        // display is still blanked).  Then, if the screen is active, restore the
                        // configured brightness so the KB wakes up even if the screensaver
                        // ActiveChanged(false) signal already fired before the device came back.
                        let ss = dev.screensaver_active;
                        if let Some(laptop) = dev.get_device() {
                            laptop.set_screensaver(ss);
                        }
                        if dev.device.is_some() && !ss {
                            dev.restore_light();
                            info!("Restored KB brightness after stale HID re-open");
                        }
                        // Force the effect manager to re-send every row to the new device
                        // (otherwise the diff logic may skip rows the new device hasn't seen).
                        fx.clear_row_cache();
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

        let cached_gpu_name = || {
            gpu::get_cached_gpu_status()
                .map(|status| status.name)
                .unwrap_or_else(|| "NVIDIA GPU".into())
        };

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

            // Principle 2: skip GPU polling in any non-AWAKE state.
            if SYSTEM_STATE.load(Ordering::Relaxed) != sys_state::AWAKE {
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
                gpu::store_gpu_cache(&gpu::GpuStatus {
                    name: cached_gpu_name(),
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
                        gpu::store_gpu_cache(&gpu::GpuStatus {
                            name: cached_gpu_name(),
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

            // Principle 2 + 5: check state; reset PID controllers on RESUMING
            // to prevent thermal-integral wind-up causing fan spikes after wake.
            let sys = SYSTEM_STATE.load(Ordering::Relaxed);
            if sys != sys_state::AWAKE {
                if sys == sys_state::RESUMING {
                    controllers[0] = TempFanController::default();
                    controllers[1] = TempFanController::default();
                }
                continue;
            }

            // Phase 1: read all config from DEV_MANAGER, then release the lock.
            // read_cpu_package_temp_c() does multiple sysfs reads; holding the
            // mutex across those would stall concurrent IPC handlers.
            let fan_config = match DEV_MANAGER.lock() {
                Ok(mut manager) => {
                    let current_ac = manager
                        .get_device()
                        .map(|laptop| laptop.get_ac_state())
                        .unwrap_or(1)
                        .min(1);
                    let target = manager.get_temp_target(current_ac);
                    if target <= 0 {
                        controllers[current_ac] = TempFanController::default();
                        None
                    } else {
                        let (min_rpm, max_rpm) = manager.get_fan_range();
                        let current_rpm = manager
                            .get_fan_tachometer()
                            .max(manager.get_fan_rpm(current_ac));
                        Some((current_ac, target, min_rpm, max_rpm, current_rpm))
                    }
                }
                Err(error) => {
                    error!("DEV_MANAGER lock failed in temp fan controller: {}", error);
                    continue;
                }
            };
            let (current_ac, target, min_rpm, max_rpm, current_rpm) = match fan_config {
                Some(cfg) => cfg,
                None => continue,
            };

            // Phase 2: sysfs reads — no lock held.
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

            // Phase 3: compute PID output and write RPM command under a brief re-lock.
            if let Some(next_rpm) = compute_temp_target_rpm(
                &mut controllers[current_ac],
                raw_temp,
                gpu_util,
                target,
                current_rpm.max(0) as f32,
                min_rpm as f32,
                max_rpm as f32,
            ) {
                if let Ok(mut manager) = DEV_MANAGER.lock() {
                    let _ = manager.apply_runtime_fan_rpm(current_ac, next_rpm);
                }
            }
        }
    })
}

/// Principle 1 (revise.md): listens for kernel uevents and signals the resume
/// handler the exact moment a Razer HID device becomes available, eliminating
/// the race between blind sleep() and actual USB re-enumeration.
fn start_udev_hid_monitor_task() -> JoinHandle<()> {
    thread::spawn(|| {
        let socket = match udev::MonitorBuilder::new()
            .and_then(|builder| builder.match_subsystem("hidraw"))
            .and_then(|builder| builder.listen())
        {
            Ok(socket) => socket,
            Err(error) => {
                error!(
                    "udev hidraw monitor failed to start: {} — device hot-plug detection disabled",
                    error
                );
                return;
            }
        };
        info!("udev hidraw monitor started");

        let mut iter = socket.iter();
        loop {
            let Some(event) = iter.next() else {
                thread::sleep(std::time::Duration::from_millis(250));
                continue;
            };
            if event.event_type() != udev::EventType::Add {
                continue;
            }
            let dev = event.device();
            let is_razer = if let Ok(Some(usb)) = dev.parent_with_subsystem("usb") {
                usb.attribute_value("idVendor")
                    .and_then(|vendor| vendor.to_str())
                    .map(|vendor: &str| vendor.trim().eq_ignore_ascii_case("1532"))
                    .unwrap_or(false)
            } else {
                false
            };
            if is_razer {
                info!("udev: Razer HID device appeared at {}", event.syspath().display());
                let (lock, cvar) = &*HID_APPEARED;
                *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
                cvar.notify_all();
            }
        }
    })
}

fn set_keyboard_backlight(blank: bool, reason: Option<&str>) {
    if let Some(reason) = reason {
        info!("{}", reason);
    }
    if let Ok(mut manager) = DEV_MANAGER.lock() {
        // Keep screensaver_active in sync so the poll loop's `our_active`
        // check remains accurate — without this the "compositor unlocked"
        // branch in start_screensaver_monitor_task would never fire.
        manager.screensaver_active = blank;
        if blank {
            manager.light_off();
        } else {
            manager.restore_light();
        }
    }
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
                if SYSTEM_STATE.load(Ordering::Relaxed) != sys_state::AWAKE {
                    return true;
                }
                // Any screensaver signal means we're past the resume lock screen.
                RESUME_IN_PROGRESS.store(false, Ordering::Relaxed);
                if h.arg0 {
                    set_keyboard_backlight(true, Some("Screensaver active — blanking KB"));
                } else {
                    set_keyboard_backlight(false, Some("Screensaver inactive (signal) — restoring KB"));
                    // Retry 600 ms later — HID endpoint may still be waking from
                    // USB autosuspend when the signal arrives.
                    thread::spawn(|| {
                        thread::sleep(std::time::Duration::from_millis(600));
                        if let Ok(mut d) = DEV_MANAGER.lock() {
                            if !d.screensaver_active
                                && SYSTEM_STATE.load(Ordering::Relaxed) == sys_state::AWAKE
                            {
                                d.restore_light();
                            }
                        }
                    });
                }
                true
            },
        );

        // Polling fallback: KDE Plasma Wayland does not always fire ActiveChanged(false)
        // when the user wakes the display at the lock screen.  Every 2 s we query
        // GetActive() directly and reconcile with our flag.
        //
        // KDE Plasma Wayland specifics (confirmed empirically):
        //   • GetSessionIdleTime()  → NotSupported
        //   • logind IdleHint       → always false (KDE never calls SetIdleHint)
        //   • USB power/control for the Razer HID device = "on" (never autosuspended)
        //
        // Because USB is always active, set_brightness() always succeeds regardless of
        // DPMS state.  The correct behaviour for a lock screen (NOT a suspend — music
        // keeps playing) is: restore KB brightness immediately so the user can see
        // the keys while typing their password.  ActiveChanged(false) handles final
        // full restore on unlock.
        let mut poll_tick: u32 = 0;
        loop {
            dbus_session.process(time::Duration::from_millis(1000)).ok();
            poll_tick = poll_tick.wrapping_add(1);
            if poll_tick % 2 == 0 && SYSTEM_STATE.load(Ordering::Relaxed) == sys_state::AWAKE {
                use screensaver::OrgFreedesktopScreenSaver;
                let active = proxy.get_active().unwrap_or(false);
                let our_active = DEV_MANAGER
                    .lock()
                    .ok()
                    .map(|d| d.screensaver_active)
                    .unwrap_or(false);
                let resuming = RESUME_IN_PROGRESS.load(Ordering::Relaxed);

                // While RESUME_IN_PROGRESS is set the user is at the post-wake
                // lock screen; never blank the KB here (they need to type a password).
                // Only blank once the first ActiveChanged signal clears the flag.
                if active && !our_active && !resuming {
                    // Compositor says active but we haven't blanked: missed signal.
                    set_keyboard_backlight(
                        true,
                        Some("Screensaver poll: compositor active — blanking KB"),
                    );
                } else if our_active && !active {
                    // Compositor unlocked but we're still blank: missed signal.
                    set_keyboard_backlight(
                        false,
                        Some("Screensaver poll: compositor unlocked — restoring KB"),
                    );
                }
            }
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
                    let mut low_battery_threshold = 0.0;
                    if let Ok(mut d) = DEV_MANAGER.lock() {
                        d.set_ac_state(*online);
                        // Apply the RAPL profile saved for the new AC state.
                        let ac = if *online { 1usize } else { 0usize };
                        if let Some(cfg) = d.get_ac_config(ac) {
                            apply_rapl_for_profile(cfg.rapl_pl1_watts, cfg.rapl_pl2_watts);
                        }
                        low_battery_threshold = d.get_low_battery_lighting_threshold();
                    }
                    // Always clear GPU cache on AC state transition.
                    gpu::clear_gpu_cache();
                    apply_low_battery_lighting(
                        read_last_battery_percent(),
                        *online,
                        low_battery_threshold,
                    );
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
                    let (on_ac, low_battery_threshold) = DEV_MANAGER
                        .lock()
                        .ok()
                        .map(|mut d| {
                            (
                                d.get_device().map(|laptop| laptop.get_ac_state() == 1).unwrap_or(false),
                                d.get_low_battery_lighting_threshold(),
                            )
                        })
                        .unwrap_or((false, 0.0));
                    apply_low_battery_lighting(*perc, on_ac, low_battery_threshold);
                }
                true
            },
        );

        let _id = proxy_login.match_signal(|h: login1::OrgFreedesktopLogin1ManagerPrepareForSleep, _: &Connection, _: &Message| {
            info!("PrepareForSleep start={}", h.start);
            if h.start {
                // Principle 7: synchronous pre-suspend resource teardown.
                // SUSPENDING → blank KB → drop HID fd → ASLEEP.
                // Background threads see non-AWAKE and stop all HID writes
                // before we yield control back to the D-Bus dispatch loop.
                SYSTEM_STATE.store(sys_state::SUSPENDING, Ordering::SeqCst);
                // Principle 1: reset udev arrival flag so the resume path knows
                // to wait for the device to re-enumerate.
                *HID_APPEARED.0.lock().unwrap_or_else(|e| e.into_inner()) = false;
                if let Ok(mut d) = DEV_MANAGER.lock() {
                    d.light_off();
                    d.device = None; // drop HidDevice → close fd → USB can suspend
                }
                SYSTEM_STATE.store(sys_state::ASLEEP, Ordering::SeqCst);
            } else {
                // Principle 6: ACPI debounce — lid switch jitter can send
                // PrepareForSleep(true→false→true) within milliseconds.
                // Discard wakes within 1500 ms of the previous wake event.
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                let last_wake = LAST_WAKE_MS.load(Ordering::Relaxed);
                let elapsed = now_ms.saturating_sub(last_wake);
                if last_wake > 0 && elapsed < 1500 {
                    warn!("PrepareForSleep(false) debounced ({}ms since last wake) — ignoring", elapsed);
                    return true;
                }
                LAST_WAKE_MS.store(now_ms, Ordering::Relaxed);
                // Signal poll loop: don't blank KB while at the post-resume lock screen.
                RESUME_IN_PROGRESS.store(true, Ordering::Relaxed);
                // Enter RESUMING: background threads yield until we set AWAKE.
                SYSTEM_STATE.store(sys_state::RESUMING, Ordering::SeqCst);
                // Offload recovery to a background thread so the D-Bus dispatch
                // loop returns immediately (blocking here causes ~5-7 s stall
                // which triggers an immediate re-suspend on some firmware).
                thread::spawn(|| {
                    let resume_start = std::time::Instant::now();
                    // Principle 3: deterministic GPU/display pipeline barrier.
                    // Poll for the NVIDIA driver file instead of a blind sleep().
                    // Early HID/EC writes before the PRIME pipeline reinitialises
                    // can prevent the screen from coming back after resume.
                    // Only run the barrier if the NVIDIA kernel module is actually
                    // loaded (skips the full 10-s timeout on iGPU-only boots).
                    if std::path::Path::new("/sys/module/nvidia").exists() {
                        let barrier_deadline = resume_start + std::time::Duration::from_secs(10);
                        loop {
                            if std::path::Path::new("/proc/driver/nvidia/version").exists() {
                                info!("NVIDIA driver ready at {}ms after resume",
                                      resume_start.elapsed().as_millis());
                                break;
                            }
                            if std::time::Instant::now() >= barrier_deadline {
                                warn!("NVIDIA driver barrier timed out at {}ms, proceeding",
                                      resume_start.elapsed().as_millis());
                                break;
                            }
                            thread::sleep(std::time::Duration::from_millis(50));
                        }
                    } else {
                        debug!("NVIDIA module not present — skipping driver barrier");
                    }
                    // Principle 1: event-driven device readiness via udev.
                    // Condvar wait: zero CPU spin until udev signals HID arrival or
                    // 5-s timeout expires (revise.md principle 2).
                    {
                        let (lock, cvar) = &*HID_APPEARED;
                        let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
                        let (_guard, timed_out) = cvar
                            .wait_timeout_while(guard, std::time::Duration::from_secs(5), |appeared| !*appeared)
                            .unwrap_or_else(|e| e.into_inner());
                        if timed_out.timed_out() {
                            info!("udev HID signal not received within 5s, falling back to polling");
                        }
                    }
                    let mut discovered = false;
                    for attempt in 0..5_u32 {
                        let has_device = match DEV_MANAGER.lock() {
                            Ok(mut d) => { d.discover_devices(); d.device.is_some() }
                            Err(e) => { error!("DEV_MANAGER lock failed on resume: {}", e); break; }
                        };
                        if has_device {
                            info!("HID device ready after resume (attempt {}, {}ms total)",
                                  attempt + 1, resume_start.elapsed().as_millis());
                            discovered = true;
                            break;
                        }
                        warn!("HID not ready on attempt {}, retrying in 300ms", attempt + 1);
                        thread::sleep(std::time::Duration::from_millis(300));
                    }
                    if !discovered {
                        warn!("HID device unavailable after 5 attempts; backlight will remain off");
                    }
                    // Point 1 (revise.md): query D-Bus BEFORE acquiring DEV_MANAGER lock.
                    // If we held the mutex during the 5-s recv_timeout the daemon would
                    // appear frozen to any concurrent GUI/CLI request that tries to lock
                    // DEV_MANAGER (e.g. handle_data, apply_low_battery_lighting).
                    let ss_active = if discovered {
                        Connection::new_session()
                            .ok()
                            .and_then(|conn| {
                                use screensaver::OrgFreedesktopScreenSaver;
                                let proxy = conn.with_proxy(
                                    "org.freedesktop.ScreenSaver",
                                    "/org/freedesktop/ScreenSaver",
                                    time::Duration::from_millis(2000),
                                );
                                proxy.get_active().ok()
                            })
                            .unwrap_or_else(|| {
                                warn!(
                                    "Failed to query screensaver state on resume; defaulting screensaver=false"
                                );
                                false
                            })
                    } else {
                        false
                    };
                    // NOW lock briefly to apply AC state + KB hardware changes.
                    if let Ok(mut d) = DEV_MANAGER.lock() {
                        // Principle 4: treat wake as total state invalidation.
                        // Actively query AC state and screensaver — don't trust
                        // missed signals from before/during suspend.
                        d.set_ac_state_get();
                        if discovered {
                            // Always restore KB on wake — user needs visible keys
                            // at the lock screen to type their password.
                            // The screensaver ActiveChanged(true) signal will blank
                            // it again if a real idle screensaver kicks in.
                            d.screensaver_active = false;
                            d.restore_light();
                            info!("Resume: KB restored (was screensaver_active={})", ss_active);
                        }
                    }
                    // Principle 5: fan PID reset is handled in start_temp_fan_control_task
                    // which detects the RESUMING state and zeroes its integrators.
                    // Transition to AWAKE — background threads resume normal operation.
                    SYSTEM_STATE.store(sys_state::AWAKE, Ordering::SeqCst);
                    info!("System AWAKE at {}ms after resume", resume_start.elapsed().as_millis());
                    // Deferred brightness re-apply: Razer Blade firmware can accept the
                    // HID brightness command (ACK returned) but silently discard it if
                    // the LED controller hasn't fully reinitialised post-resume. A single
                    // 2s retry ensures the command takes effect once the firmware is ready,
                    // without racing against any screensaver/lock-screen state change.
                    if discovered {
                        thread::spawn(|| {
                            thread::sleep(std::time::Duration::from_millis(2000));
                            if SYSTEM_STATE.load(Ordering::Relaxed) != sys_state::AWAKE {
                                return; // another sleep happened, abort
                            }
                            if let Ok(mut d) = DEV_MANAGER.lock() {
                                if !d.screensaver_active {
                                    info!("Resume brightness retry: re-applying KB backlight");
                                    d.restore_light();
                                }
                            }
                        });
                    }
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
        let mut signals = match Signals::new([SIGINT, SIGTERM]) {
            Ok(signals) => signals,
            Err(error) => {
                error!("Failed to register shutdown signals: {}", error);
                return;
            }
        };
        let _ = signals.forever().next();

        // If we reach this point, we have a signal and it is time to exit
        println!("Received signal, cleaning up");
        if let Ok(mut effect_manager) = EFFECT_MANAGER.lock() {
            let json = effect_manager.save();
            if let Err(error) = config::Configuration::write_effects_save(json) {
                error!("Error writing config {}", error);
            }
        } else {
            error!("EFFECT_MANAGER lock failed during shutdown cleanup");
        }
        if std::fs::metadata(comms::SOCKET_PATH).is_ok() {
            if let Err(error) = std::fs::remove_file(comms::SOCKET_PATH) {
                error!("Failed to remove socket during shutdown cleanup: {}", error);
            }
        }
        std::process::exit(0);
    })
}

fn handle_data(mut stream: UnixStream) {
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(200)));

    let mut buffer = Vec::with_capacity(4096);
    let mut chunk = [0u8; 1024];

    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read_len) => {
                if buffer.len() + read_len > 16 * 1024 {
                    warn!("Dropping oversized daemon request (>{} bytes)", 16 * 1024);
                    return;
                }
                buffer.extend_from_slice(&chunk[..read_len]);
                if bincode::deserialize::<comms::DaemonCommand>(&buffer).is_ok() {
                    break;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break;
            }
            Err(error) => {
                debug!("Failed to read daemon request: {}", error);
                return;
            }
        }
    }

    if let Ok(cmd) = bincode::deserialize::<comms::DaemonCommand>(&buffer) {
        if let Some(s) = process_client_request(cmd) {
            if let Ok(x) = bincode::serialize(&s) {
                let result = stream.write_all(&x);

                if let Err(error) = result {
                    println!("Client disconnected with error: {error}");
                }
            }
        }
    } else if !buffer.is_empty() {
        warn!(
            "Failed to deserialize daemon request after reading {} bytes",
            buffer.len()
        );
    }
}

pub fn process_client_request(cmd: comms::DaemonCommand) -> Option<comms::DaemonResponse> {
    match cmd {
        comms::DaemonCommand::SetPowerMode { ac, pwr, cpu, gpu } => {
            let mut d = DEV_MANAGER.lock().ok()?;
            let ok = d.set_power_mode(ac, pwr, cpu, gpu);
            let confirmed = d.get_power_mode(ac);
            if confirmed == pwr {
                info!(
                    "Power mode set OK (pwr={} cpu={} gpu={} ac={})",
                    pwr, cpu, gpu, ac
                );
            } else {
                warn!("Power mode mismatch: sent {} but EC reports {} (HID write may have failed)", pwr, confirmed);
            }
            gpu::clear_gpu_cache();
            Some(comms::DaemonResponse::SetPowerMode { result: ok })
        }
        comms::DaemonCommand::SetFanSpeed { ac, rpm } => {
            let mut d = DEV_MANAGER.lock().ok()?;
            Some(comms::DaemonResponse::SetFanSpeed {
                result: d.set_fan_rpm(ac, rpm),
            })
        }
        comms::DaemonCommand::SetFanTemperatureTarget { ac, temp_c } => {
            let mut d = DEV_MANAGER.lock().ok()?;
            Some(comms::DaemonResponse::SetFanTemperatureTarget {
                result: d.set_temp_target(ac, temp_c),
            })
        }
        comms::DaemonCommand::SetLogoLedState { ac, logo_state } => {
            let mut d = DEV_MANAGER.lock().ok()?;
            Some(comms::DaemonResponse::SetLogoLedState {
                result: d.set_logo_led_state(ac, logo_state),
            })
        }
        comms::DaemonCommand::SetBrightness { ac, val } => {
            let mut d = DEV_MANAGER.lock().ok()?;
            Some(comms::DaemonResponse::SetBrightness {
                result: d.set_brightness(ac, val),
            })
        }
        comms::DaemonCommand::SetIdle { ac, val } => {
            let mut d = DEV_MANAGER.lock().ok()?;
            Some(comms::DaemonResponse::SetIdle {
                result: d.change_idle(ac, val),
            })
        }
        comms::DaemonCommand::SetSync { sync } => {
            let mut d = DEV_MANAGER.lock().ok()?;
            Some(comms::DaemonResponse::SetSync {
                result: d.set_sync(sync),
            })
        }
        comms::DaemonCommand::GetBrightness { ac } => {
            let mut d = DEV_MANAGER.lock().ok()?;
            Some(comms::DaemonResponse::GetBrightness {
                result: d.get_brightness(ac),
            })
        }
        comms::DaemonCommand::GetLogoLedState { ac } => {
            let mut d = DEV_MANAGER.lock().ok()?;
            Some(comms::DaemonResponse::GetLogoLedState {
                logo_state: d.get_logo_led_state(ac),
            })
        }
        comms::DaemonCommand::GetKeyboardRGB { layer } => EFFECT_MANAGER.lock().ok().map(|mut manager| {
            comms::DaemonResponse::GetKeyboardRGB {
                layer,
                rgbdata: manager.get_map(layer),
            }
        }),
        comms::DaemonCommand::GetSync() => {
            let mut d = DEV_MANAGER.lock().ok()?;
            Some(comms::DaemonResponse::GetSync { sync: d.get_sync() })
        }
        comms::DaemonCommand::GetFanSpeed { ac } => {
            let mut d = DEV_MANAGER.lock().ok()?;
            Some(comms::DaemonResponse::GetFanSpeed {
                rpm: d.get_fan_rpm(ac),
            })
        }
        comms::DaemonCommand::GetFanTemperatureTarget { ac } => {
            let mut d = DEV_MANAGER.lock().ok()?;
            Some(comms::DaemonResponse::GetFanTemperatureTarget {
                temp_c: d.get_temp_target(ac),
            })
        }
        comms::DaemonCommand::GetPwrLevel { ac } => {
            let mut d = DEV_MANAGER.lock().ok()?;
            Some(comms::DaemonResponse::GetPwrLevel {
                pwr: d.get_power_mode(ac),
            })
        }
        comms::DaemonCommand::GetCPUBoost { ac } => {
            let mut d = DEV_MANAGER.lock().ok()?;
            Some(comms::DaemonResponse::GetCPUBoost {
                cpu: d.get_cpu_boost(ac),
            })
        }
        comms::DaemonCommand::GetGPUBoost { ac } => {
            let mut d = DEV_MANAGER.lock().ok()?;
            Some(comms::DaemonResponse::GetGPUBoost {
                gpu: d.get_gpu_boost(ac),
            })
        }
        comms::DaemonCommand::SetEffect { name, params } => {
            let mut d = DEV_MANAGER.lock().ok()?;
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
                    if let Some(effect) = effect {
                        k.pop_effect(laptop);
                        k.push_effect(effect, [true; 90]);
                        let json = k.save();
                        if let Err(error) = config::Configuration::write_effects_save(json) {
                            error!("Failed to save effects: {}", error);
                        }
                        res = true;
                    }
                }
            }
            Some(comms::DaemonResponse::SetEffect { result: res })
        }
        comms::DaemonCommand::SetStandardEffect { name, params } => {
            let mut d = DEV_MANAGER.lock().ok()?;
            let mut res = false;
            if let Some(laptop) = d.get_device() {
                if let Ok(mut k) = EFFECT_MANAGER.lock() {
                    k.pop_effect(laptop);
                    res = match name.as_str() {
                        "off" => d.set_standard_effect(device::RazerLaptop::OFF, params),
                        "wave" => d.set_standard_effect(device::RazerLaptop::WAVE, params),
                        "reactive" => d.set_standard_effect(device::RazerLaptop::REACTIVE, params),
                        "breathing" => d.set_standard_effect(device::RazerLaptop::BREATHING, params),
                        "spectrum" => d.set_standard_effect(device::RazerLaptop::SPECTRUM, params),
                        "static" => d.set_standard_effect(device::RazerLaptop::STATIC, params),
                        "starlight" => d.set_standard_effect(device::RazerLaptop::STARLIGHT, params),
                        _ => false,
                    };
                }
            }
            Some(comms::DaemonResponse::SetStandardEffect { result: res })
        }
        comms::DaemonCommand::SetBatteryHealthOptimizer { is_on, threshold } => {
            let mut d = DEV_MANAGER.lock().ok()?;
            Some(comms::DaemonResponse::SetBatteryHealthOptimizer {
                result: d.set_bho_handler(is_on, threshold),
            })
        }
        comms::DaemonCommand::GetBatteryHealthOptimizer() => {
            let mut d = DEV_MANAGER.lock().ok()?;
            d.get_bho_handler().map(|result| comms::DaemonResponse::GetBatteryHealthOptimizer {
                is_on: result.0,
                threshold: result.1,
            })
        }
        comms::DaemonCommand::GetDeviceName => {
            let d = DEV_MANAGER.lock().ok()?;
            let name = match &d.device {
                Some(device) => device.get_name().to_string(),
                None => "Unknown Device".to_string(),
            };
            Some(comms::DaemonResponse::GetDeviceName { name })
        }
        comms::DaemonCommand::GetGpuStatus => {
            let status = gpu::get_cached_gpu_status().unwrap_or_else(|| gpu::GpuStatus {
                name: "NVIDIA GPU".into(),
                ..gpu::GpuStatus::default()
            });
            Some(comms::DaemonResponse::GetGpuStatus {
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
            })
        }
        comms::DaemonCommand::GetPowerLimits { ac } => {
            // Read sysfs before locking — powercap reads are safe without the mutex.
            let pl1_max = read_rapl_uw(RAPL_PL1_MAX_UW);
            let pl1_live = read_rapl_uw(RAPL_PL1_UW);
            let pl2_live = read_rapl_uw(RAPL_PL2_UW);
            let mut d = DEV_MANAGER.lock().ok()?;
            let (pl1_cfg, pl2_cfg) = d.get_rapl_limits(ac);
            let (pl1, pl2) = if pl1_cfg > 0 {
                (pl1_cfg as u64 * 1_000_000, pl2_cfg as u64 * 1_000_000)
            } else {
                (pl1_live, pl2_live)
            };
            Some(comms::DaemonResponse::GetPowerLimits {
                pl1_watts: (pl1 / 1_000_000) as u32,
                pl2_watts: (pl2 / 1_000_000) as u32,
                pl1_max_watts: (pl1_max / 1_000_000) as u32,
            })
        }
        comms::DaemonCommand::SetPowerLimits {
            ac,
            pl1_watts,
            pl2_watts,
        } => {
            // Persist to config and check whether to apply immediately.
            // Release the lock before the sysfs writes so concurrent IPC
            // handlers are not blocked during the filesystem round-trip.
            let (saved, should_apply) = {
                let mut d = DEV_MANAGER.lock().ok()?;
                let saved = d.set_rapl_limits(ac, pl1_watts, pl2_watts);
                let current_ac = d.get_device().map(|l| l.get_ac_state()).unwrap_or(1);
                (saved, current_ac == ac)
            };
            let applied = if should_apply {
                let ok1 = write_rapl_uw(RAPL_PL1_UW, pl1_watts as u64 * 1_000_000);
                let ok2 = write_rapl_uw(RAPL_PL2_UW, pl2_watts as u64 * 1_000_000);
                ok1 && ok2
            } else {
                true // saved to config; will apply on next AC state switch
            };
            Some(comms::DaemonResponse::SetPowerLimits {
                result: saved && applied,
            })
        }
        comms::DaemonCommand::GetCurrentEffect => {
            let info = EFFECT_MANAGER
                .lock()
                .ok()
                .and_then(|mut em| em.get_current_effect_info());
            let (name, args) = info.unwrap_or_else(|| (String::new(), Vec::new()));
            Some(comms::DaemonResponse::GetCurrentEffect { name, args })
        }
        comms::DaemonCommand::GetFanTachometer => {
            let mut d = DEV_MANAGER.lock().ok()?;
            Some(comms::DaemonResponse::GetFanTachometer {
                rpm: d.get_fan_tachometer(),
            })
        }
        comms::DaemonCommand::SetLowBatteryLighting { threshold_pct } => {
            let mut d = DEV_MANAGER.lock().ok()?;
            let result = d.set_low_battery_lighting_threshold(threshold_pct);
            let on_ac = d
                .get_device()
                .map(|laptop| laptop.get_ac_state() == 1)
                .unwrap_or(false);
            let low_battery_threshold = d.get_low_battery_lighting_threshold();
            drop(d);
            apply_low_battery_lighting(
                read_last_battery_percent(),
                on_ac,
                low_battery_threshold,
            );
            Some(comms::DaemonResponse::SetLowBatteryLighting { result })
        }
        comms::DaemonCommand::GetLowBatteryLighting => {
            let mut d = DEV_MANAGER.lock().ok()?;
            Some(comms::DaemonResponse::GetLowBatteryLighting {
                threshold_pct: d.get_low_battery_lighting_threshold(),
            })
        }
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
        let _ = write_rapl_uw(RAPL_PL1_UW, pl1_watts as u64 * 1_000_000);
        info!("Applied RAPL PL1 = {} W", pl1_watts);
    }
    if pl2_watts > 0 {
        let _ = write_rapl_uw(RAPL_PL2_UW, pl2_watts as u64 * 1_000_000);
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

fn apply_low_battery_lighting(battery_pct: f64, on_ac: bool, threshold_pct: f64) {
    // Principle 2: skip while system is not fully awake.
    if SYSTEM_STATE.load(Ordering::Relaxed) != sys_state::AWAKE {
        return;
    }

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
        let Some(zone_type) = std::fs::read_to_string(type_path).ok() else {
            continue;
        };
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

    let clamped_rpm = rpm.clamp(min_rpm, max_rpm);

    FAN_STEPS
        .iter()
        .copied()
        .filter(|step| *step >= min_rpm && *step <= max_rpm)
        .min_by(|left, right| {
            (clamped_rpm - *left)
                .abs()
                .partial_cmp(&(clamped_rpm - *right).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or(clamped_rpm)
}
