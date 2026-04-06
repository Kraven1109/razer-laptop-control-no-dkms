use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::{atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering}, Mutex};
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
    /// When the GPU is operating very close to its enforced TGP, freeze keyboard
    /// animation updates entirely. This is a stronger mitigation for the built-in
    /// panel flicker seen on PRIME/Optimus laptops when EC traffic and NVIDIA's
    /// Dynamic Boost negotiation overlap under peak load.
    static ref HIGH_POWER_FLICKER_GUARD: AtomicBool = AtomicBool::new(false);
    /// Unix-epoch milliseconds when the flicker guard last transitioned to true.
    /// Used to enforce a minimum hold period so that brief idle gaps between
    /// ComfyUI/game inference bursts do not prematurely release the guard.
    static ref GUARD_ENTERED_MS: AtomicU64 = AtomicU64::new(0);
    /// TGP (enforced.power.limit, in whole watts) recorded at the moment the
    /// flicker guard was last enabled.  Used as a stable low-water reference
    /// for the stay condition: Dynamic Boost can raise TGP mid-guard, which
    /// would otherwise lift the stay threshold and cause spurious early releases.
    static ref GUARD_ENTRY_TGP: AtomicU32 = AtomicU32::new(0);
    /// Unix-epoch milliseconds of the last nvidia-smi call made while the guard
    /// was already armed (the periodic re-check calls).  Reset to 0 when the
    /// guard is newly armed so the GUARD_HOLD_MS window starts cleanly.
    static ref GUARD_LAST_CHECK_MS: AtomicU64 = AtomicU64::new(0);
    /// Baseline count of /dev/dri/renderD128 (NVIDIA render node) file
    /// descriptors open across all processes.  Established on startup when the
    /// GPU is idle.  A sudden increase means a new PRIME-offload app (game) has
    /// opened the NVIDIA render device → we can arm the flicker guard without
    /// calling nvidia-smi, avoiding the NVML mutex stall at game-start.
    static ref RENDER128_BASELINE: AtomicUsize = AtomicUsize::new(usize::MAX);
    /// Set to true while the system is suspended so GPU polling and HID
    /// writes are suppressed until the device is re-opened after resume.
    static ref SYSTEM_SLEEPING: AtomicBool = AtomicBool::new(false);
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
            // Skip USB writes entirely while device is suspended — the HID fd
            // is closed during sleep and writes would fail or spin-wait.
            if !SYSTEM_SLEEPING.load(Ordering::Relaxed) && !HIGH_POWER_FLICKER_GUARD.load(Ordering::Relaxed) {
                if let (Ok(mut dev), Ok(mut fx)) = (DEV_MANAGER.lock(), EFFECT_MANAGER.lock()) {
                    if let Some(laptop) = dev.get_device() {
                        fx.update(laptop);
                    }
                }
            }
            // Cap sleep at 200 ms so the animator notices when ANIM_SLEEP_MS
            // is reset to normal (100 ms) after the flicker guard releases.
            // Without this cap the thread sleeps for 600 000 ms (the guard
            // value) and the keyboard stays frozen for up to 10 minutes after
            // inference ends even though the guard has already been cleared.
            // 200 ms cap: still 0 HID writes during guard (checked above), but
            // the thread wakes and picks up the new sleep value within 200 ms.
            let sleep_ms = ANIM_SLEEP_MS.load(Ordering::Relaxed).min(200);
            thread::sleep(std::time::Duration::from_millis(sleep_ms));
        }
    })
}

fn gpu_monitor_on_ac() -> bool {
    DEV_MANAGER.lock()
        .ok()
        .and_then(|mut d| d.get_device().map(|laptop| laptop.get_ac_state() == 1))
        .unwrap_or(true)
}

/// Monitors GPU utilization and dynamically adjusts keyboard animation rate.
/// Under heavy GPU load (>70%), animation is slowed to 3 FPS to reduce EC USB
/// HID traffic, lowering EC interrupt load during NVIDIA NVPCF Dynamic Boost
/// negotiations. This mitigates PRIME display pipeline stalls that appear as
/// display flickering on the built-in panel. When GPU draw gets very close to
/// the enforced TGP, the keyboard animation is frozen completely until the GPU
/// drops back below a lower release threshold.
pub fn start_gpu_load_monitor_task() -> JoinHandle<()> {
    thread::spawn(|| {
        // Establish the renderD128 (NVIDIA DRM render node) fd baseline before
        // entering the poll loop.  We wait a few seconds for the desktop to
        // fully settle first (kwin, VS Code, Edge etc. all open renderD128).
        // Any subsequent *increase* over this baseline means a new PRIME-offload
        // app (game) just started — we can ARM the guard instantly, with zero
        // nvidia-smi calls, avoiding the NVML mutex stall at game-launch time.
        thread::sleep(std::time::Duration::from_secs(5));
        let baseline = gpu::count_nvidia_render_fds();
        RENDER128_BASELINE.store(baseline, Ordering::Relaxed);
        info!("PRIME renderD128 baseline: {} fds open", baseline);

        loop {
            let on_ac = gpu_monitor_on_ac();
            thread::sleep(std::time::Duration::from_secs(if on_ac { 3 } else { 10 }));

            if SYSTEM_SLEEPING.load(Ordering::Relaxed) {
                continue;
            }
            if !gpu::should_query_nvidia(on_ac) {
                HIGH_POWER_FLICKER_GUARD.store(false, Ordering::Relaxed);
                ANIM_SLEEP_MS.store(kbd::ANIMATION_SLEEP_MS, Ordering::Relaxed);
                gpu::clear_gpu_cache();
                continue;
            }

            let prev_guard = HIGH_POWER_FLICKER_GUARD.load(Ordering::Relaxed);
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;

            // ── Core anti-flicker logic ──────────────────────────────────────
            // nvidia-smi acquires the NVIDIA driver's global mutex.  While held,
            // the PRIME DMA completion fence is blocked.  VS Code, Edge, and our
            // own GUI all render on renderD128 (NVIDIA) and those frames are
            // PRIME-copied to Intel for KWin — so there is ALWAYS a PRIME fence
            // in flight.  At ≥ 60 % GPU util the mutex hold can exceed 1 368 ms,
            // triggering i915's "Pageflip timed out!" bug → 1-second blank screen.
            // (Confirmed in kwin_wayland journal; "not as severe in Silent profile"
            // → stall is proportional to driver busyness = GPU utilisation.)
            //
            // Two-track approach:
            //
            //  Track A — Gaming (new /dev/dri/renderD128 fd delta):
            //    Games open renderD128 at launch.  Count increase > baseline + 2
            //    → ARM guard instantly, ZERO nvidia-smi, no NVML mutex stall.
            //
            //  Track B — CUDA / ComfyUI (util threshold at low load):
            //    CUDA never opens renderD128.  nvidia-smi is called but with a
            //    LOW threshold (30 %) so it fires during model loading, when the
            //    driver is lightly loaded and the mutex is held for < 200 ms —
            //    well below the 1 000 ms i915 timeout, invisible to the user.
            //
            //  After ARM (either track): 120-s dead zone, then 2-minute periodic
            //  check via nvidia-smi.  Release when util < 15 %.
            const GUARD_HOLD_MS: u64 = 120_000; // no nvidia-smi for first 2 min
            const GUARD_POLL_MS: u64 = 120_000; // then check every 2 min
            const ARM_UTIL:     u8   = 30;       // arm at 30 % (fast NVML)
            const RELEASE_UTIL: u8   = 15;       // release when truly idle

            // ── Track A: gaming start via renderD128 delta ───────────────────
            let prime_fds  = gpu::count_nvidia_render_fds();
            let guard_base = RENDER128_BASELINE.load(Ordering::Relaxed);
            let new_prime_app = guard_base != usize::MAX
                && prime_fds > guard_base.saturating_add(2);

            if new_prime_app && !prev_guard {
                let tgp = gpu::get_cached_gpu_status()
                    .map(|s| s.power_limit_w as u32)
                    .unwrap_or(150);
                GUARD_ENTERED_MS.store(now_ms, Ordering::Relaxed);
                GUARD_ENTRY_TGP.store(tgp, Ordering::Relaxed);
                GUARD_LAST_CHECK_MS.store(0, Ordering::Relaxed);
                HIGH_POWER_FLICKER_GUARD.store(true, Ordering::Relaxed);
                ANIM_SLEEP_MS.store(600_000, Ordering::Relaxed);
                warn!(
                    "Flicker guard enabled (gaming): {} → {} renderD128 fds, no nvidia-smi",
                    guard_base, prime_fds,
                );
                continue;
            }

            // ── Guard active: dead zone / periodic-check skip ─────────────────
            if prev_guard {
                let guard_held_ms =
                    now_ms.saturating_sub(GUARD_ENTERED_MS.load(Ordering::Relaxed));
                let since_check = {
                    let lc = GUARD_LAST_CHECK_MS.load(Ordering::Relaxed);
                    if lc == 0 { u64::MAX } else { now_ms.saturating_sub(lc) }
                };
                if guard_held_ms < GUARD_HOLD_MS || since_check < GUARD_POLL_MS {
                    continue;
                }
                GUARD_LAST_CHECK_MS.store(now_ms, Ordering::Relaxed);
            }

            // ── nvidia-smi call (guard off OR periodic check while armed) ─────
            if let Some(status) = gpu::query_nvidia_gpu() {
                gpu::store_gpu_cache(&status);

                if prev_guard {
                    if status.gpu_util < RELEASE_UTIL {
                        info!(
                            "Flicker guard disabled: util={}%, {:.1}W",
                            status.gpu_util, status.power_w,
                        );
                        HIGH_POWER_FLICKER_GUARD.store(false, Ordering::Relaxed);
                        // Refresh baseline so future gaming sessions are detected.
                        RENDER128_BASELINE.store(prime_fds, Ordering::Relaxed);
                    }
                    // else util still elevated → keep guard
                } else if status.gpu_util >= ARM_UTIL {
                    warn!(
                        "Flicker guard enabled (CUDA/{}% util, {:.1}W)",
                        status.gpu_util, status.power_w,
                    );
                    GUARD_ENTERED_MS.store(now_ms, Ordering::Relaxed);
                    GUARD_ENTRY_TGP.store(status.power_limit_w as u32, Ordering::Relaxed);
                    GUARD_LAST_CHECK_MS.store(0, Ordering::Relaxed);
                    HIGH_POWER_FLICKER_GUARD.store(true, Ordering::Relaxed);
                }

                let current_guard = HIGH_POWER_FLICKER_GUARD.load(Ordering::Relaxed);
                let new_sleep = if current_guard {
                    600_000
                } else if status.gpu_util >= 70 {
                    333
                } else if status.gpu_util <= 20 {
                    kbd::ANIMATION_SLEEP_MS
                } else {
                    ANIM_SLEEP_MS.load(Ordering::Relaxed)
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
            // Ignore screensaver events while the system is suspended — the HID
            // device is closed and restore_light() would fail silently.
            if SYSTEM_SLEEPING.load(Ordering::Relaxed) {
                return true;
            }
            match DEV_MANAGER.lock() {
                Ok(mut d) => {
                    if h.arg0 { d.light_off(); } else { d.restore_light(); }
                }
                Err(e) => error!("DEV_MANAGER lock failed in screensaver handler: {}", e),
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
                // Always clear the GPU cache on an AC state transition:
                // on battery → the cached AC TGP (e.g. 150 W Gaming) would be
                //   misleading once the GPU runtime-suspends;
                // on AC connect → the cached "0 W suspended" placeholder would
                //   persist for up to 3 s before the GPU monitor next queries.
                gpu::clear_gpu_cache();
                HIGH_POWER_FLICKER_GUARD.store(false, Ordering::Relaxed);
                ANIM_SLEEP_MS.store(kbd::ANIMATION_SLEEP_MS, Ordering::Relaxed);
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
                    info!("Power mode set OK (pwr={} cpu={} gpu={} ac={})", pwr, cpu, gpu, ac);
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
                // Use the cache populated by the GPU load monitor to avoid
                // spawning a second nvidia-smi per GUI poll cycle.
                // Fall back to a direct query only if the cache is cold
                // (e.g., first request after daemon start).
                let on_ac = d.get_device()
                    .map(|laptop| laptop.get_ac_state() == 1)
                    .unwrap_or(true);
                let status = gpu::get_cached_gpu_status().or_else(|| {
                    if SYSTEM_SLEEPING.load(Ordering::Relaxed) || !gpu::should_query_nvidia(on_ac) {
                        None
                    }
                    else {
                        let s = gpu::query_nvidia_gpu();
                        if let Some(ref s) = s { gpu::store_gpu_cache(s); }
                        s
                    }
                }).unwrap_or_else(|| gpu::GpuStatus {
                    name: "NVIDIA GPU (runtime suspended)".into(),
                    ..gpu::GpuStatus::default()
                });
                return Some(comms::DaemonResponse::GetGpuStatus {
                    name: status.name,
                    temp_c: status.temp_c,
                    gpu_util: status.gpu_util,
                    mem_util: status.mem_util,
                    power_w: status.power_w,
                    power_limit_w: status.power_limit_w,
                    power_max_limit_w: status.power_max_limit_w,
                    mem_used_mb: status.mem_used_mb,
                    mem_total_mb: status.mem_total_mb,
                    clock_gpu_mhz: status.clock_gpu_mhz,
                    clock_mem_mhz: status.clock_mem_mhz,
                });
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

            comms::DaemonCommand::GetCurrentEffect => {
                let info = EFFECT_MANAGER.lock().ok()
                    .and_then(|mut em| em.get_current_effect_info());
                let (name, args) = info.unwrap_or_else(|| (String::new(), Vec::new()));
                return Some(comms::DaemonResponse::GetCurrentEffect { name, args });
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
