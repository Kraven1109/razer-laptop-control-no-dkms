#![deny(warnings)]

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::io::ErrorKind;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use adw::prelude::*;
use gtk::glib;
use lazy_static::lazy_static;
use relm4::prelude::*;

#[path = "../comms.rs"]
mod comms;
mod error_handling;
mod gui_config;
mod startup;
mod tray;
mod util;
mod widgets;

use error_handling::*;
use service::SupportedDevice;
use util::*;
use widgets::ColorWheel;

lazy_static! {
    /// Set when autostart requests a tray-only launch; keeps the main
    /// window hidden until opened from the tray icon.
    static ref START_MINIMIZED: AtomicBool = AtomicBool::new(false);
}

// ── Daemon communication helpers ──────────────────────────────────────────

fn send_data(opt: comms::DaemonCommand) -> Option<comms::DaemonResponse> {
    match comms::try_bind() {
        Ok(socket) => comms::send_to_daemon(opt, socket),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            // Daemon socket absent (briefly after restart/resume) — return None gracefully
            // rather than crashing; poll timers will retry on the next tick.
            eprintln!("Daemon socket not found: {error}");
            return None;
        }
        Err(error) => {
            eprintln!("Error opening socket: {error}");
            None
        }
    }
}

fn get_device_name() -> Option<String> {
    match send_data(comms::DaemonCommand::GetDeviceName)? {
        comms::DaemonResponse::GetDeviceName { name } => Some(name),
        r => {
            eprintln!("Unexpected: {r:?}");
            None
        }
    }
}

fn scrub_deprecated_gtk_dark_pref_setting() {
    let home = match std::env::var("HOME") {
        Ok(value) => value,
        Err(_) => return,
    };
    let settings_path = std::path::Path::new(&home).join(".config/gtk-4.0/settings.ini");

    let content = match std::fs::read_to_string(&settings_path) {
        Ok(value) => value,
        Err(_) => return,
    };

    let mut removed = false;
    let filtered_lines: Vec<&str> = content
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            let deprecated = trimmed.starts_with("gtk-application-prefer-dark-theme=");
            if deprecated {
                removed = true;
            }
            !deprecated
        })
        .collect();

    if !removed {
        return;
    }

    let mut updated = filtered_lines.join("\n");
    if content.ends_with('\n') {
        updated.push('\n');
    }

    match std::fs::write(&settings_path, updated) {
        Ok(_) => eprintln!(
            "Removed deprecated GTK setting from {} to avoid libadwaita warning",
            settings_path.display()
        ),
        Err(error) => eprintln!(
            "Failed to update {}: {}",
            settings_path.display(),
            error
        ),
    }
}

fn find_device_config_with_retry(devices: &[SupportedDevice]) -> Option<SupportedDevice> {
    const RETRIES: u32 = 40;
    const RETRY_DELAY_MS: u64 = 250;

    for attempt in 0..RETRIES {
        if let Some(device_name) = get_device_name() {
            if let Some(device) = devices.iter().find(|d| d.name == device_name) {
                if attempt > 0 {
                    eprintln!(
                        "Connected to daemon after {} startup retries",
                        attempt
                    );
                }
                return Some(device.clone());
            }
        }
        if attempt + 1 < RETRIES {
            std::thread::sleep(std::time::Duration::from_millis(RETRY_DELAY_MS));
        }
    }

    None
}

fn get_bho() -> Option<(bool, u8)> {
    match send_data(comms::DaemonCommand::GetBatteryHealthOptimizer())? {
        comms::DaemonResponse::GetBatteryHealthOptimizer { is_on, threshold } => {
            Some((is_on, threshold))
        }
        r => {
            eprintln!("Unexpected: {r:?}");
            None
        }
    }
}

fn set_bho(is_on: bool, threshold: u8) -> Option<bool> {
    match send_data(comms::DaemonCommand::SetBatteryHealthOptimizer { is_on, threshold })? {
        comms::DaemonResponse::SetBatteryHealthOptimizer { result } => Some(result),
        r => {
            eprintln!("Unexpected: {r:?}");
            None
        }
    }
}

fn get_brightness(ac: bool) -> Option<u8> {
    let ac = if ac { 1 } else { 0 };
    match send_data(comms::DaemonCommand::GetBrightness { ac })? {
        comms::DaemonResponse::GetBrightness { result } => Some(result),
        r => {
            eprintln!("Unexpected: {r:?}");
            None
        }
    }
}

fn set_brightness(ac: bool, val: u8) -> Option<bool> {
    let ac = if ac { 1 } else { 0 };
    match send_data(comms::DaemonCommand::SetBrightness { ac, val })? {
        comms::DaemonResponse::SetBrightness { result } => Some(result),
        r => {
            eprintln!("Unexpected: {r:?}");
            None
        }
    }
}

fn get_logo(ac: bool) -> Option<u8> {
    let ac = if ac { 1 } else { 0 };
    match send_data(comms::DaemonCommand::GetLogoLedState { ac })? {
        comms::DaemonResponse::GetLogoLedState { logo_state } => Some(logo_state),
        r => {
            eprintln!("Unexpected: {r:?}");
            None
        }
    }
}

fn set_logo(ac: bool, logo_state: u8) -> Option<bool> {
    let ac = if ac { 1 } else { 0 };
    match send_data(comms::DaemonCommand::SetLogoLedState { ac, logo_state })? {
        comms::DaemonResponse::SetLogoLedState { result } => Some(result),
        r => {
            eprintln!("Unexpected: {r:?}");
            None
        }
    }
}

fn set_effect(name: &str, values: Vec<u8>) -> Option<bool> {
    match send_data(comms::DaemonCommand::SetEffect {
        name: name.into(),
        params: values,
    })? {
        comms::DaemonResponse::SetEffect { result } => Some(result),
        r => {
            eprintln!("Unexpected: {r:?}");
            None
        }
    }
}

fn get_power(ac: bool) -> Option<(u8, u8, u8)> {
    let ac_val = if ac { 1 } else { 0 };
    let pwr = match send_data(comms::DaemonCommand::GetPwrLevel { ac: ac_val })? {
        comms::DaemonResponse::GetPwrLevel { pwr } => pwr,
        r => {
            eprintln!("Unexpected: {r:?}");
            return None;
        }
    };
    let cpu = match send_data(comms::DaemonCommand::GetCPUBoost { ac: ac_val })? {
        comms::DaemonResponse::GetCPUBoost { cpu } => cpu,
        r => {
            eprintln!("Unexpected: {r:?}");
            return None;
        }
    };
    let gpu = match send_data(comms::DaemonCommand::GetGPUBoost { ac: ac_val })? {
        comms::DaemonResponse::GetGPUBoost { gpu } => gpu,
        r => {
            eprintln!("Unexpected: {r:?}");
            return None;
        }
    };
    Some((pwr, cpu, gpu))
}

fn set_power(ac: bool, power: (u8, u8, u8)) -> Option<bool> {
    let ac = if ac { 1 } else { 0 };
    match send_data(comms::DaemonCommand::SetPowerMode {
        ac,
        pwr: power.0,
        cpu: power.1,
        gpu: power.2,
    })? {
        comms::DaemonResponse::SetPowerMode { result } => Some(result),
        r => {
            eprintln!("Unexpected: {r:?}");
            None
        }
    }
}

fn get_fan_speed(ac: bool) -> Option<i32> {
    let ac = if ac { 1 } else { 0 };
    match send_data(comms::DaemonCommand::GetFanSpeed { ac })? {
        comms::DaemonResponse::GetFanSpeed { rpm } => Some(rpm),
        r => {
            eprintln!("Unexpected: {r:?}");
            None
        }
    }
}

fn get_fan_tachometer() -> Option<i32> {
    match send_data(comms::DaemonCommand::GetFanTachometer)? {
        comms::DaemonResponse::GetFanTachometer { rpm } => Some(rpm),
        r => {
            eprintln!("Unexpected: {r:?}");
            None
        }
    }
}

fn set_fan_speed(ac: bool, value: i32) -> Option<bool> {
    let ac = if ac { 1 } else { 0 };
    match send_data(comms::DaemonCommand::SetFanSpeed { ac, rpm: value })? {
        comms::DaemonResponse::SetFanSpeed { result } => Some(result),
        r => {
            eprintln!("Unexpected: {r:?}");
            None
        }
    }
}

fn get_fan_temperature_target(ac: bool) -> Option<i32> {
    let ac = if ac { 1 } else { 0 };
    match send_data(comms::DaemonCommand::GetFanTemperatureTarget { ac })? {
        comms::DaemonResponse::GetFanTemperatureTarget { temp_c } => Some(temp_c),
        r => {
            eprintln!("Unexpected: {r:?}");
            None
        }
    }
}

fn set_fan_temperature_target(ac: bool, temp_c: i32) -> Option<bool> {
    let ac = if ac { 1 } else { 0 };
    match send_data(comms::DaemonCommand::SetFanTemperatureTarget { ac, temp_c })? {
        comms::DaemonResponse::SetFanTemperatureTarget { result } => Some(result),
        r => {
            eprintln!("Unexpected: {r:?}");
            None
        }
    }
}

fn get_power_limits(ac: bool) -> Option<(u32, u32, u32)> {
    let ac = if ac { 1 } else { 0 };
    match send_data(comms::DaemonCommand::GetPowerLimits { ac })? {
        comms::DaemonResponse::GetPowerLimits {
            pl1_watts,
            pl2_watts,
            pl1_max_watts,
        } => Some((pl1_watts, pl2_watts, pl1_max_watts)),
        r => {
            eprintln!("Unexpected: {r:?}");
            None
        }
    }
}

fn set_power_limits(ac: bool, pl1: u32, pl2: u32) -> Option<bool> {
    let ac = if ac { 1 } else { 0 };
    match send_data(comms::DaemonCommand::SetPowerLimits {
        ac,
        pl1_watts: pl1,
        pl2_watts: pl2,
    })? {
        comms::DaemonResponse::SetPowerLimits { result } => Some(result),
        r => {
            eprintln!("Unexpected: {r:?}");
            None
        }
    }
}

/// Returns (effect_name, args) for the currently active keyboard effect,
/// or None if no effect is running (keyboard is off / daemon has no effect).
fn get_current_effect() -> Option<(String, Vec<u8>)> {
    match send_data(comms::DaemonCommand::GetCurrentEffect)? {
        comms::DaemonResponse::GetCurrentEffect { name, args } if !name.is_empty() => {
            Some((name, args))
        }
        _ => None,
    }
}

fn get_low_battery_lighting_threshold() -> Option<f64> {
    match send_data(comms::DaemonCommand::GetLowBatteryLighting)? {
        comms::DaemonResponse::GetLowBatteryLighting { threshold_pct } => Some(threshold_pct),
        r => {
            eprintln!("Unexpected: {r:?}");
            None
        }
    }
}

fn set_low_battery_lighting_threshold(threshold_pct: f64) -> Option<bool> {
    match send_data(comms::DaemonCommand::SetLowBatteryLighting { threshold_pct })? {
        comms::DaemonResponse::SetLowBatteryLighting { result } => Some(result),
        r => {
            eprintln!("Unexpected: {r:?}");
            None
        }
    }
}

fn get_sync() -> Option<bool> {
    match send_data(comms::DaemonCommand::GetSync())? {
        comms::DaemonResponse::GetSync { sync } => Some(sync),
        r => {
            eprintln!("Unexpected: {r:?}");
            None
        }
    }
}

fn set_sync(sync: bool) -> Option<bool> {
    match send_data(comms::DaemonCommand::SetSync { sync })? {
        comms::DaemonResponse::SetSync { result } => Some(result),
        r => {
            eprintln!("Unexpected: {r:?}");
            None
        }
    }
}

/// Returns the minimum safe fan RPM given the current GPU temperature.
/// Mirrors Windows safe_min_rpm() — raises the floor when the GPU is hot
/// to prevent the dGPU from overheating while the fan is on manual control.
fn safe_min_fan_rpm(gpu_temp_c: i32, min_fan: f64, max_fan: f64) -> f64 {
    if gpu_temp_c >= 95 {
        max_fan
    } else if gpu_temp_c >= 80 {
        min_fan + (max_fan - min_fan) * 0.35
    } else {
        min_fan
    }
}

/// Returns the live GPU temperature from the daemon, or 0 if unavailable.
fn get_gpu_temp() -> i32 {
    match send_data(comms::DaemonCommand::GetGpuStatus) {
        Some(comms::DaemonResponse::GetGpuStatus { temp_c, stale, .. }) if !stale => temp_c,
        _ => 0,
    }
}

// ── Main Application Model ───────────────────────────────────────────────

struct App {
    _device: SupportedDevice,
}

#[derive(Debug)]
enum AppMsg {}

#[relm4::component]
impl SimpleComponent for App {
    type Init = SupportedDevice;
    type Input = AppMsg;
    type Output = ();

    view! {
        adw::ApplicationWindow {
            set_title: Some("Razer Blade Control"),
            set_default_width: 780,
            set_default_height: 650,
            set_icon_name: Some("razer-blade-control"),
        }
    }

    fn init(
        device: Self::Init,
        root: Self::Root,
        _sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let model = App {
            _device: device.clone(),
        };
        let widgets = view_output!();

        // Window title shows the actual device model name.
        root.set_title(Some(&format!("{} — Control", device.name)));

        // Build the view stack
        let view_stack = adw::ViewStack::new();        view_stack.add_titled_with_icon(
            &build_power_page(true, &device),
            Some("ac"),
            "AC",
            "thunderbolt-symbolic",
        );
        view_stack.add_titled_with_icon(
            &build_power_page(false, &device),
            Some("battery"),
            "Battery",
            "battery-symbolic",
        );
        view_stack.add_titled_with_icon(
            &build_keyboard_page(),
            Some("keyboard"),
            "Keyboard",
            "input-keyboard-symbolic",
        );
        view_stack.add_titled_with_icon(
            &build_system_page(&device),
            Some("system"),
            "System",
            "computer-symbolic",
        );

        let view_switcher = adw::ViewSwitcher::new();
        view_switcher.set_stack(Some(&view_stack));
        view_switcher.set_policy(adw::ViewSwitcherPolicy::Wide);

        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(&view_switcher));

        let toolbar_view = adw::ToolbarView::new();
        toolbar_view.add_top_bar(&header);
        toolbar_view.set_content(Some(&view_stack));

        root.set_content(Some(&toolbar_view));

        // Show battery tab if on battery
        let saved_page = gui_config::GuiConfig::load().last_page;
        if ["ac", "battery", "keyboard", "system"].contains(&saved_page.as_str()) {
            view_stack.set_visible_child_name(&saved_page);
        } else if let Some(false) = check_if_running_on_ac_power() {
            view_stack.set_visible_child_name("battery");
        }

        view_stack.connect_visible_child_name_notify(|stack| {
            if let Some(page) = stack.visible_child_name() {
                let mut config = gui_config::GuiConfig::load();
                config.last_page = page.to_string();
                let _ = config.save();
            }
        });

        // Minimize to tray
        root.connect_close_request(|win| {
            win.set_visible(false);
            glib::Propagation::Stop
        });

        // Keep the window hidden until the tray explicitly reopens it.
        if START_MINIMIZED.load(Ordering::Relaxed) {
            root.set_visible(false);
        }

        ComponentParts { model, widgets }
    }
}

// ── Page builders ─────────────────────────────────────────────────────────

fn build_power_page(ac: bool, device: &SupportedDevice) -> gtk::ScrolledWindow {
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .build();

    let page = adw::PreferencesPage::new();

    // Logo section
    if device.has_logo() {
        let group = adw::PreferencesGroup::builder().title("Logo").build();
        let logo = get_logo(ac).unwrap_or(0).min(2);

        let logo_row = adw::ComboRow::builder()
            .title("Logo Mode")
            .subtitle("Control the Razer logo LED")
            .build();
        let model = gtk::StringList::new(&["Off", "On", "Breathing"]);
        logo_row.set_model(Some(&model));
        logo_row.set_selected(logo as u32);

        logo_row.connect_selected_notify(move |row| {
            let val = row.selected() as u8;
            set_logo(ac, val);
            let readback = get_logo(ac).unwrap_or(0).min(2);
            row.set_selected(readback as u32);
        });

        group.add(&logo_row);
        page.add(&group);
    }

    // Power section
    if let Some(power) = get_power(ac) {
        let group = adw::PreferencesGroup::builder().title("Power").build();

        let power_row = adw::ComboRow::builder()
            .title("Power Profile")
            .subtitle("System performance mode")
            .build();
        power_row.set_model(Some(&gtk::StringList::new(&[
            "Balanced", "Gaming", "Creator", "Silent", "Custom",
        ])));
        power_row.set_selected(power.0 as u32);

        let cpu_row = adw::ComboRow::builder()
            .title("CPU Boost")
            .subtitle("Processor performance level")
            .build();
        let mut cpu_opts = vec!["Low", "Medium", "High"];
        if device.can_boost() {
            cpu_opts.push("Boost");
        }
        cpu_row.set_model(Some(&gtk::StringList::new(&cpu_opts)));
        cpu_row.set_selected(power.1 as u32);
        cpu_row.set_visible(power.0 == 4);

        let gpu_row = adw::ComboRow::builder()
            .title("GPU Boost")
            .subtitle("Graphics performance level")
            .build();
        gpu_row.set_model(Some(&gtk::StringList::new(&["Low", "Medium", "High"])));
        gpu_row.set_selected(power.2 as u32);
        gpu_row.set_visible(power.0 == 4);

        let cpu_clone = cpu_row.clone();
        let gpu_clone = gpu_row.clone();
        power_row.connect_selected_notify(move |row| {
            let profile = row.selected() as u8;
            let cpu = cpu_clone.selected() as u8;
            let gpu = gpu_clone.selected() as u8;
            set_power(ac, (profile, cpu, gpu));
            if let Some(p) = get_power(ac) {
                row.set_selected(p.0 as u32);
                cpu_clone.set_selected(p.1 as u32);
                gpu_clone.set_selected(p.2 as u32);
                cpu_clone.set_visible(p.0 == 4);
                gpu_clone.set_visible(p.0 == 4);
            }
        });

        let power_row_c = power_row.clone();
        let gpu_row_c = gpu_row.clone();
        cpu_row.connect_selected_notify(move |row| {
            let profile = power_row_c.selected() as u8;
            let cpu = row.selected() as u8;
            let gpu = gpu_row_c.selected() as u8;
            set_power(ac, (profile, cpu, gpu));
            if let Some(p) = get_power(ac) {
                power_row_c.set_selected(p.0 as u32);
                row.set_selected(p.1 as u32);
                gpu_row_c.set_selected(p.2 as u32);
            }
        });

        let power_row_c2 = power_row.clone();
        let cpu_row_c2 = cpu_row.clone();
        gpu_row.connect_selected_notify(move |row| {
            let profile = power_row_c2.selected() as u8;
            let cpu = cpu_row_c2.selected() as u8;
            let gpu = row.selected() as u8;
            set_power(ac, (profile, cpu, gpu));
            if let Some(p) = get_power(ac) {
                power_row_c2.set_selected(p.0 as u32);
                cpu_row_c2.set_selected(p.1 as u32);
                row.set_selected(p.2 as u32);
            }
        });

        group.add(&power_row);
        group.add(&cpu_row);
        group.add(&gpu_row);

        // Sync toggle: only on AC tab — mirrors profiles to Battery when enabled.
        if ac {
            let sync_enabled = get_sync().unwrap_or(false);
            let sync_row = adw::SwitchRow::builder()
                .title("Sync AC and Battery Profiles")
                .subtitle("Mirror power profile and brightness settings to both power states")
                .active(sync_enabled)
                .build();
            sync_row.connect_active_notify(move |row| {
                set_sync(row.is_active());
                if let Some(s) = get_sync() {
                    row.set_active(s);
                }
            });
            group.add(&sync_row);
        }

        page.add(&group);
    }

    // Fan Speed section
    {
        let group = adw::PreferencesGroup::builder().title("Fan Speed").build();
        let fan_speed = get_fan_speed(ac).unwrap_or(0);
        let fan_temp_target = get_fan_temperature_target(ac).unwrap_or(0);
        let min_fan = *device.fan.get(0).unwrap_or(&3500) as f64;
        let max_fan = *device.fan.get(1).unwrap_or(&5000) as f64;
        let fan_mode = if fan_temp_target > 0 {
            2
        } else if fan_speed == 0 {
            0
        } else {
            1
        };

        let mode_row = adw::ComboRow::builder()
            .title("Mode")
            .subtitle("Choose firmware auto, fixed RPM, or daemon temperature control")
            .build();
        mode_row.set_model(Some(&gtk::StringList::new(&[
            "Auto",
            "Manual RPM",
            "Temperature Target",
        ])));
        mode_row.set_selected(fan_mode);

        // ── Manual RPM row (slider) ────────────────────────────────────────
        let rpm_row = adw::ActionRow::builder()
            .title("Speed (RPM)")
            .subtitle("Manual fan speed — floor raised when GPU is hot")
            .build();
        let rpm_scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, min_fan, max_fan, 100.0);
        rpm_scale.set_value(if fan_speed == 0 { min_fan } else { fan_speed as f64 });
        rpm_scale.set_hexpand(true);
        rpm_scale.set_valign(gtk::Align::Center);
        rpm_scale.set_size_request(180, -1);
        rpm_scale.set_sensitive(fan_mode == 1);
        rpm_scale.add_mark(min_fan, gtk::PositionType::Bottom, None);
        rpm_scale.add_mark((min_fan + max_fan) / 2.0, gtk::PositionType::Bottom, None);
        rpm_scale.add_mark(max_fan, gtk::PositionType::Bottom, None);

        let rpm_val_label = gtk::Label::builder()
            .label(&if fan_speed == 0 { format!("{min_fan:.0} RPM") } else { format!("{fan_speed} RPM") })
            .width_chars(9)
            .xalign(1.0)
            .css_classes(["monospace"])
            .build();
        rpm_row.add_suffix(&rpm_scale);
        rpm_row.add_suffix(&rpm_val_label);

        // Pending RPM: signal handler only writes, IPC timer reads — no reentrancy.
        let pending_rpm: Rc<RefCell<Option<i32>>> = Rc::new(RefCell::new(None));

        rpm_scale.connect_value_changed(glib::clone!(
            #[weak]
            rpm_val_label,
            #[strong]
            pending_rpm,
            move |s| {
                let v = s.value().clamp(min_fan, max_fan) as i32;
                rpm_val_label.set_text(&format!("{v} RPM"));
                *pending_rpm.borrow_mut() = Some(v);
            }
        ));

        // IPC sender (300 ms) — applies safety floor before sending.
        glib::timeout_add_local(
            std::time::Duration::from_millis(300),
            glib::clone!(
                #[weak]
                rpm_scale,
                #[strong]
                pending_rpm,
                #[strong]
                mode_row,
                #[upgrade_or]
                glib::ControlFlow::Break,
                move || {
                    if mode_row.selected() == 1 {
                        if let Some(v) = pending_rpm.borrow_mut().take() {
                            let gpu_temp = get_gpu_temp();
                            let safe_min = safe_min_fan_rpm(gpu_temp, min_fan, max_fan);
                            let clamped = (v as f64).max(safe_min) as i32;
                            let _ = set_fan_speed(ac, clamped);
                            if let Some(rb) = get_fan_speed(ac) {
                                if rb > 0 && rb != clamped {
                                    rpm_scale.set_value(rb as f64);
                                }
                            }
                        }
                    } else {
                        // Clear pending if mode changed away from Manual
                        pending_rpm.borrow_mut().take();
                    }
                    glib::ControlFlow::Continue
                }
            ),
        );

        // ── Temperature target row (SpinRow) ──────────────────────────────
        let temp_row = adw::SpinRow::with_range(60.0, 95.0, 1.0);
        temp_row.set_title("Target Temperature");
        temp_row.set_subtitle("Daemon keeps CPU/GPU thermals near this target");
        temp_row.set_value(if fan_temp_target > 0 { fan_temp_target as f64 } else { 78.0 });
        temp_row.set_sensitive(fan_mode == 2);

        // Mode change handler wires up sensitivity and applies immediate IPC.
        let rpm_scale_c = rpm_scale.clone();
        let temp_clone = temp_row.clone();
        mode_row.connect_selected_notify(move |row| {
            match row.selected() {
                0 => {
                    let _ = set_fan_temperature_target(ac, 0);
                    let _ = set_fan_speed(ac, 0);
                }
                1 => {
                    let _ = set_fan_temperature_target(ac, 0);
                    let rpm = rpm_scale_c.value().clamp(min_fan, max_fan) as i32;
                    let _ = set_fan_speed(ac, rpm);
                }
                2 => {
                    let _ = set_fan_temperature_target(ac, temp_clone.value() as i32);
                }
                _ => {}
            }
            let readback_temp = get_fan_temperature_target(ac).unwrap_or(0);
            let readback_rpm = get_fan_speed(ac).unwrap_or(0);
            let mode = if readback_temp > 0 { 2 } else if readback_rpm == 0 { 0 } else { 1 };
            row.set_selected(mode);
            rpm_scale_c.set_sensitive(mode == 1);
            temp_clone.set_sensitive(mode == 2);
            if readback_rpm > 0 { rpm_scale_c.set_value(readback_rpm as f64); }
            if readback_temp > 0 { temp_clone.set_value(readback_temp as f64); }
        });

        let mode_clone = mode_row.clone();
        temp_row.connect_value_notify(move |row| {
            if mode_clone.selected() != 2 {
                return;
            }
            let temp_c = row.value().clamp(60.0, 95.0) as i32;
            let _ = set_fan_temperature_target(ac, temp_c);
            let readback = get_fan_temperature_target(ac).unwrap_or(0);
            if readback > 0 {
                row.set_value(readback as f64);
            }
        });

        let live_row = adw::ActionRow::builder()
            .title("Live Fan RPM")
            .subtitle("Measured EC tachometer reading")
            .build();
        let live_label = gtk::Label::builder()
            .label("-- RPM")
            .css_classes(["monospace"])
            .build();
        live_row.add_suffix(&live_label);

        glib::timeout_add_seconds_local(
            2,
            glib::clone!(
                #[weak]
                live_label,
                #[upgrade_or]
                glib::ControlFlow::Break,
                move || {
                    if let Some(rpm) = get_fan_tachometer() {
                        let text = if rpm > 0 {
                            format!("{rpm} RPM")
                        } else {
                            "Unavailable".into()
                        };
                        live_label.set_text(&text);
                    }
                    glib::ControlFlow::Continue
                }
            ),
        );

        group.add(&mode_row);
        group.add(&rpm_row);
        group.add(&temp_row);
        group.add(&live_row);
        page.add(&group);
    }

    // Brightness section
    {
        let group = adw::PreferencesGroup::builder()
            .title("Keyboard Brightness")
            .build();
        let brightness = get_brightness(ac).unwrap_or(50);

        // Slider row — more intuitive drag interaction than SpinRow for 0-100 range.
        let bright_row = adw::ActionRow::builder()
            .title("Brightness")
            .subtitle("Keyboard backlight intensity")
            .build();
        let bright_scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 100.0, 1.0);
        bright_scale.set_value(brightness as f64);
        bright_scale.set_hexpand(true);
        bright_scale.set_valign(gtk::Align::Center);
        bright_scale.set_size_request(180, -1);
        bright_scale.add_mark(0.0,   gtk::PositionType::Bottom, None);
        bright_scale.add_mark(50.0,  gtk::PositionType::Bottom, None);
        bright_scale.add_mark(100.0, gtk::PositionType::Bottom, None);

        let bright_val_label = gtk::Label::builder()
            .label(&format!("{}%", brightness))
            .width_chars(5)
            .xalign(1.0)
            .css_classes(["monospace"])
            .build();
        bright_row.add_suffix(&bright_scale);
        bright_row.add_suffix(&bright_val_label);

        // Pending value: signal handler only writes here, IPC timer reads it.
        // This fully decouples UI events from blocking IPC calls.
        let pending: Rc<RefCell<Option<u8>>> = Rc::new(RefCell::new(None));
        let suppress_brightness_write = Rc::new(Cell::new(false));

        bright_scale.connect_value_changed(glib::clone!(
            #[weak]
            bright_val_label,
            #[strong]
            pending,
            #[strong]
            suppress_brightness_write,
            move |s| {
                let v = s.value().clamp(0.0, 100.0) as u8;
                bright_val_label.set_text(&format!("{}%", v));
                if !suppress_brightness_write.get() {
                    *pending.borrow_mut() = Some(v);
                }
            }
        ));

        // IPC sender: coalesces all drag events into one call per 300 ms tick.
        let suppress_brightness_write_send = suppress_brightness_write.clone();
        glib::timeout_add_local(
            std::time::Duration::from_millis(300),
            glib::clone!(
                #[weak]
                bright_scale,
                #[strong]
                pending,
                #[strong]
                suppress_brightness_write_send,
                #[upgrade_or]
                glib::ControlFlow::Break,
                move || {
                    if let Some(v) = pending.borrow_mut().take() {
                        set_brightness(ac, v);
                        if let Some(rb) = get_brightness(ac) {
                            if rb != v {
                                suppress_brightness_write_send.set(true);
                                bright_scale.set_value(rb as f64);
                                suppress_brightness_write_send.set(false);
                            }
                        }
                    }
                    glib::ControlFlow::Continue
                }
            ),
        );

        // External sync poll (every 2 s) — reflects daemon changes not made from this page.
        glib::timeout_add_seconds_local(
            2,
            glib::clone!(
                #[weak]
                bright_scale,
                #[strong]
                pending,
                #[strong]
                suppress_brightness_write,
                #[upgrade_or]
                glib::ControlFlow::Break,
                move || {
                    // Skip if user is actively editing (a pending value is queued).
                    if check_if_running_on_ac_power() == Some(ac) && pending.borrow().is_none() {
                        if let Some(readback) = get_brightness(ac) {
                            let current = bright_scale.value().round() as u8;
                            if current != readback {
                                suppress_brightness_write.set(true);
                                bright_scale.set_value(readback as f64);
                                suppress_brightness_write.set(false);
                            }
                        }
                    }
                    glib::ControlFlow::Continue
                }
            ),
        );

        group.add(&bright_row);
        page.add(&group);
    }

    // CPU Power Limits (RAPL) — per power profile, persisted across reboots
    if let Some((pl1, pl2, pl1_max)) = get_power_limits(ac) {
        let tdp_base = if pl1_max > 0 { pl1_max } else { 55 };
        let max_pl = (tdp_base * 4).max(pl1.max(pl2) + 20);

        let rapl_group = adw::PreferencesGroup::builder()
            .title("CPU Power Limits (RAPL)")
            .description("Intel PL1 (sustained) and PL2 (boost). Lowering PL1 on battery limits turbo boost and extends runtime. Requires root daemon.")
            .build();

        let pl1_row = adw::SpinRow::with_range(tdp_base as f64, max_pl as f64, 5.0);
        pl1_row.set_title("PL1 — Sustained (W)");
        pl1_row.set_subtitle(&format!("Long-term power limit (base TDP: {tdp_base} W)"));
        pl1_row.set_value(pl1 as f64);

        let pl2_row = adw::SpinRow::with_range(tdp_base as f64, max_pl as f64, 5.0);
        pl2_row.set_title("PL2 — Boost (W)");
        pl2_row.set_subtitle("Short-term burst power limit");
        pl2_row.set_value(pl2 as f64);

        let apply_btn = gtk::Button::builder()
            .label("Apply Power Limits")
            .halign(gtk::Align::Center)
            .css_classes(["suggested-action", "pill"])
            .margin_top(16)
            .build();
        apply_btn.set_size_request(200, -1);

        let pl1_ref = pl1_row.clone();
        let pl2_ref = pl2_row.clone();
        apply_btn.connect_clicked(move |_| {
            let p1 = pl1_ref.value() as u32;
            let p2 = pl2_ref.value() as u32;
            set_power_limits(ac, p1, p2);
            if let Some((r1, r2, _)) = get_power_limits(ac) {
                pl1_ref.set_value(r1 as f64);
                pl2_ref.set_value(r2 as f64);
            }
        });

        rapl_group.add(&pl1_row);
        rapl_group.add(&pl2_row);
        rapl_group.add(&apply_btn);
        page.add(&rapl_group);
    }

    scroll.set_child(Some(&page));
    scroll
}

fn build_keyboard_page() -> gtk::ScrolledWindow {
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .build();

    let page = adw::PreferencesPage::new();

    // Effect selection
    let effect_group = adw::PreferencesGroup::builder()
        .title("Keyboard Backlight")
        .description("Choose a lighting effect and configure its parameters")
        .build();

    let effect_names = [
        "Static",
        "Static Gradient",
        "Wave Gradient",
        "Breathing",
        "Breathing Dual",
        "Spectrum Cycle",
        "Rainbow Wave",
        "Starlight",
        "Ripple",
        "Wheel",
    ];

    let effect_row = adw::ComboRow::builder()
        .title("Effect")
        .subtitle("Keyboard lighting effect")
        .build();
    effect_row.set_model(Some(&gtk::StringList::new(&effect_names)));

    let desc_label = gtk::Label::builder()
        .label("Set a single color across all keys")
        .halign(gtk::Align::Start)
        .css_classes(["effect-description"])
        .build();

    // ── Restore state from daemon's current effect ─────────────────────
    // Query once at page-build time so the UI reflects the running effect
    // rather than always defaulting to "Static / green".
    struct EffectInit {
        idx: u32,
        r1: u8,
        g1: u8,
        b1: u8,
        r2: u8,
        g2: u8,
        b2: u8,
        speed: u8,
        dir: u32,
        density: u8,
        duration: u8,
    }
    let ei: EffectInit = get_current_effect()
        .and_then(|(name, args)| {
            let idx = match name.as_str() {
                "Static" => 0u32,
                "Static Gradient" => 1,
                "Wave Gradient" => 2,
                "Breathing Single" => 3,
                "Breathing Dual" => 4,
                "Spectrum Cycle" => 5,
                "Rainbow Wave" => 6,
                "Starlight" => 7,
                "Ripple" => 8,
                "Wheel" => 9,
                _ => return None,
            };
            let a = |i: usize| args.get(i).copied().unwrap_or(0u8);
            let (r1, g1, b1) = (a(0), a(1), a(2));
            let (r2, g2, b2) = if matches!(idx, 1 | 2 | 4) {
                (a(3), a(4), a(5))
            } else {
                (0, 128, 255)
            };
            let speed = match idx {
                5 => a(0).max(1), // Spectrum Cycle: [speed]
                6 => a(0).max(1), // Rainbow Wave:   [speed, dir]
                8 => a(3).max(1), // Ripple:         [R,G,B, speed]
                9 => a(0).max(1), // Wheel:          [speed, dir]
                _ => 5,
            };
            let dir = if matches!(idx, 6 | 9) { a(1) as u32 } else { 0 };
            let density = if idx == 7 { a(3).max(1) } else { 10 };
            let duration = match idx {
                3 => a(3).max(1), // Breathing Single: [R,G,B, duration]
                4 => a(6).max(1), // Breathing Dual:   [R1,G1,B1,R2,G2,B2, duration]
                _ => 10,
            };
            Some(EffectInit {
                idx,
                r1,
                g1,
                b1,
                r2,
                g2,
                b2,
                speed,
                dir,
                density,
                duration,
            })
        })
        .unwrap_or(EffectInit {
            idx: 0,
            r1: 0,
            g1: 255,
            b1: 0,
            r2: 0,
            g2: 128,
            b2: 255,
            speed: 5,
            dir: 0,
            density: 10,
            duration: 10,
        });

    effect_row.set_selected(ei.idx);

    // Color wheels
    let wheel1 = ColorWheel::new(160);
    let wheel2 = ColorWheel::new(160);
    wheel1.set_rgb(ei.r1, ei.g1, ei.b1);
    wheel2.set_rgb(ei.r2, ei.g2, ei.b2);

    let wheel1_frame = gtk::Frame::builder()
        .label("Primary Color")
        .halign(gtk::Align::Center)
        .css_classes(["color-wheel-frame"])
        .build();
    wheel1_frame.set_child(Some(&wheel1.widget));

    let wheel2_frame = gtk::Frame::builder()
        .label("Secondary Color")
        .halign(gtk::Align::Center)
        .css_classes(["color-wheel-frame"])
        .build();
    wheel2_frame.set_child(Some(&wheel2.widget));

    let wheels_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(24)
        .halign(gtk::Align::Center)
        .margin_top(8)
        .margin_bottom(8)
        .build();
    wheels_box.append(&wheel1_frame);
    wheels_box.append(&wheel2_frame);

    // Parameter controls
    let speed_row = adw::SpinRow::with_range(1.0, 10.0, 1.0);
    speed_row.set_title("Speed");
    speed_row.set_subtitle("Animation speed");
    speed_row.set_value(ei.speed as f64);

    let direction_row = adw::ComboRow::builder()
        .title("Direction")
        .subtitle("Wave direction")
        .build();
    direction_row.set_model(Some(&gtk::StringList::new(&[
        "Left → Right",
        "Right → Left",
    ])));
    direction_row.set_selected(ei.dir);

    let density_row = adw::SpinRow::with_range(1.0, 20.0, 1.0);
    density_row.set_title("Density");
    density_row.set_subtitle("Star density");
    density_row.set_value(ei.density as f64);

    let duration_row = adw::SpinRow::with_range(1.0, 20.0, 1.0);
    duration_row.set_title("Duration");
    duration_row.set_subtitle("Breath cycle length");
    duration_row.set_value(ei.duration as f64);

    // Apply button
    let apply_btn = gtk::Button::builder()
        .label("Apply Effect")
        .halign(gtk::Align::Center)
        .css_classes(["suggested-action", "pill"])
        .build();
    apply_btn.set_size_request(200, -1);

    // Dynamic visibility
    let descriptions = [
        "Set a single color across all keys",
        "Smooth gradient between two colors",
        "Animated wave blending two colors",
        "Pulsing glow with a single color",
        "Alternating pulse between two colors",
        "Cycle through the full color spectrum",
        "Rainbow wave flowing across the keyboard",
        "Random twinkling keys with shimmer",
        "Color ripple from the center outward",
        "Rotating color wheel around the keyboard center",
    ];

    let update_visibility = {
        let wheel1_frame = wheel1_frame.clone();
        let wheel2_frame = wheel2_frame.clone();
        let speed_row = speed_row.clone();
        let direction_row = direction_row.clone();
        let density_row = density_row.clone();
        let duration_row = duration_row.clone();
        let desc_label = desc_label.clone();
        move |idx: u32| {
            wheel1_frame.set_visible(matches!(idx, 0 | 1 | 2 | 3 | 4 | 7 | 8));
            wheel2_frame.set_visible(matches!(idx, 1 | 2 | 4));
            speed_row.set_visible(matches!(idx, 5 | 6 | 8 | 9));
            direction_row.set_visible(matches!(idx, 6 | 9));
            density_row.set_visible(idx == 7);
            duration_row.set_visible(matches!(idx, 3 | 4));
            desc_label.set_text(descriptions.get(idx as usize).unwrap_or(&""));
        }
    };

    update_visibility(ei.idx);

    effect_row.connect_selected_notify({
        let update = update_visibility.clone();
        move |row| {
            update(row.selected());
        }
    });

    // Apply logic
    let wheel1_ref = wheel1;
    let wheel2_ref = wheel2;
    let effect_row_ref = effect_row.clone();
    let speed_ref = speed_row.clone();
    let dir_ref = direction_row.clone();
    let density_ref = density_row.clone();
    let duration_ref = duration_row.clone();

    apply_btn.connect_clicked(move |_| {
        let (r1, g1, b1) = wheel1_ref.get_rgb();
        let (r2, g2, b2) = wheel2_ref.get_rgb();
        let speed = speed_ref.value() as u8;
        let dir = dir_ref.selected() as u8;
        let density = density_ref.value() as u8;
        let duration = duration_ref.value() as u8;

        match effect_row_ref.selected() {
            0 => {
                set_effect("static", vec![r1, g1, b1]);
            }
            1 => {
                set_effect("static_gradient", vec![r1, g1, b1, r2, g2, b2]);
            }
            2 => {
                set_effect("wave_gradient", vec![r1, g1, b1, r2, g2, b2]);
            }
            3 => {
                set_effect("breathing_single", vec![r1, g1, b1, duration]);
            }
            4 => {
                set_effect("breathing_dual", vec![r1, g1, b1, r2, g2, b2, duration]);
            }
            5 => {
                set_effect("spectrum_cycle", vec![speed]);
            }
            6 => {
                set_effect("rainbow_wave", vec![speed, dir]);
            }
            7 => {
                set_effect("starlight", vec![r1, g1, b1, density]);
            }
            8 => {
                set_effect("ripple", vec![r1, g1, b1, speed]);
            }
            9 => {
                set_effect("wheel", vec![speed, dir]);
            }
            _ => {}
        }
    });

    effect_group.add(&effect_row);
    effect_group.add(&desc_label);
    effect_group.add(&wheels_box);
    effect_group.add(&speed_row);
    effect_group.add(&direction_row);
    effect_group.add(&density_row);
    effect_group.add(&duration_row);
    effect_group.add(&apply_btn);
    page.add(&effect_group);

    // BHO section
    if let Some(bho) = get_bho() {
        let group = adw::PreferencesGroup::builder()
            .title("Battery Health Optimizer")
            .description("Limit charge level to preserve battery longevity")
            .build();

        let bho_switch = adw::SwitchRow::builder()
            .title("Enable BHO")
            .subtitle("Limit maximum charge level")
            .active(bho.0)
            .build();

        let threshold_row = adw::SpinRow::with_range(50.0, 80.0, 1.0);
        threshold_row.set_title("Threshold");
        threshold_row.set_subtitle("Maximum charge percentage");
        threshold_row.set_value(bho.1 as f64);
        threshold_row.set_sensitive(bho.0);

        let thresh_c = threshold_row.clone();
        bho_switch.connect_active_notify(move |row| {
            let is_on = row.is_active();
            let threshold = thresh_c.value().clamp(50.0, 80.0) as u8;
            set_bho(is_on, threshold);
            if let Some((on, th)) = get_bho() {
                row.set_active(on);
                thresh_c.set_value(th as f64);
                thresh_c.set_sensitive(on);
            }
        });

        let switch_c = bho_switch.clone();
        threshold_row.connect_value_notify(move |row| {
            let is_on = switch_c.is_active();
            let threshold = row.value().clamp(50.0, 80.0) as u8;
            set_bho(is_on, threshold);
            if let Some((on, th)) = get_bho() {
                switch_c.set_active(on);
                row.set_value(th as f64);
                row.set_sensitive(on);
            }
        });

        group.add(&bho_switch);
        group.add(&threshold_row);
        page.add(&group);
    }

    {
        let threshold = get_low_battery_lighting_threshold().unwrap_or(0.0);
        let group = adw::PreferencesGroup::builder()
            .title("Low Battery Lighting")
            .description("Automatically turn off keyboard lighting below a battery threshold")
            .build();

        let enabled = threshold > 0.0;
        let toggle = adw::SwitchRow::builder()
            .title("Enable Low-Battery Light Saver")
            .subtitle("Preserve battery by blanking the keyboard and logo on low charge")
            .active(enabled)
            .build();

        let threshold_row = adw::SpinRow::with_range(5.0, 100.0, 1.0);
        threshold_row.set_title("Threshold");
        threshold_row.set_subtitle("Battery percentage that triggers lighting shutdown");
        threshold_row.set_value(if enabled { threshold } else { 20.0 });
        threshold_row.set_sensitive(enabled);

        let threshold_row_toggle = threshold_row.clone();
        toggle.connect_active_notify(move |row| {
            let next_threshold = if row.is_active() {
                threshold_row_toggle.value().clamp(5.0, 100.0)
            } else {
                0.0
            };
            set_low_battery_lighting_threshold(next_threshold);
            if let Some(readback) = get_low_battery_lighting_threshold() {
                row.set_active(readback > 0.0);
                threshold_row_toggle.set_sensitive(readback > 0.0);
                if readback > 0.0 {
                    threshold_row_toggle.set_value(readback);
                }
            }
        });

        let toggle_clone = toggle.clone();
        threshold_row.connect_value_notify(move |row| {
            if !toggle_clone.is_active() {
                return;
            }
            let next_threshold = row.value().clamp(5.0, 100.0);
            set_low_battery_lighting_threshold(next_threshold);
            if let Some(readback) = get_low_battery_lighting_threshold() {
                toggle_clone.set_active(readback > 0.0);
                if readback > 0.0 {
                    row.set_value(readback);
                }
            }
        });

        group.add(&toggle);
        group.add(&threshold_row);
        page.add(&group);
    }

    scroll.set_child(Some(&page));
    scroll
}

// ── System-page helper functions ────────────────────────────────────────────

/// Reads /proc/stat aggregate line.  Returns (total_ticks, idle_ticks).
fn cpu_stat_ticks() -> Option<(u64, u64)> {
    let s = std::fs::read_to_string("/proc/stat").ok()?;
    let line = s.lines().next()?;
    let p: Vec<u64> = line.split_whitespace().skip(1)
        .filter_map(|v| v.parse().ok()).collect();
    if p.len() < 4 { return None; }
    let idle   = p[3] + p.get(4).copied().unwrap_or(0); // idle + iowait
    let total  = p.iter().sum::<u64>();
    Some((total, idle))
}

/// Returns (used_mb, total_mb) from /proc/meminfo.
fn read_ram_mb() -> (u64, u64) {
    let s = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let mut total_kb = 0u64;
    let mut avail_kb = 0u64;
    for line in s.lines() {
        let mut parts = line.split_whitespace();
        match parts.next() {
            Some("MemTotal:")     => total_kb = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0),
            Some("MemAvailable:") => avail_kb = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0),
            _ => {}
        }
    }
    (total_kb.saturating_sub(avail_kb) / 1024, total_kb / 1024)
}

/// Returns CPU package temperature in °C from hwmon (coretemp / k10temp / zenpower).
fn read_cpu_temp_c() -> Option<i32> {
    let entries = std::fs::read_dir("/sys/class/hwmon").ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(name) = std::fs::read_to_string(path.join("name")) else { continue };
        let name = name.trim();
        if name == "coretemp" || name == "k10temp" || name == "zenpower" {
            if let Ok(v) = std::fs::read_to_string(path.join("temp1_input")) {
                if let Ok(m) = v.trim().parse::<i32>() { return Some(m / 1000); }
            }
        }
    }
    None
}

/// Returns NVMe SSD temperature in °C from hwmon.
fn read_nvme_temp_c() -> Option<i32> {
    let entries = std::fs::read_dir("/sys/class/hwmon").ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(name) = std::fs::read_to_string(path.join("name")) else { continue };
        if name.trim().starts_with("nvme") {
            if let Ok(v) = std::fs::read_to_string(path.join("temp1_input")) {
                if let Ok(m) = v.trim().parse::<i32>() { return Some(m / 1000); }
            }
        }
    }
    None
}

/// Creates a metric display tile.
/// Returns (container_box, value_label, subtitle_label) for later update.
fn make_stat_tile(title: &str, value: &str, subtitle: &str, val_css: &'static str)
    -> (gtk::Box, gtk::Label, gtk::Label)
{
    let tile = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .css_classes(["metric-tile"])
        .spacing(0).hexpand(true).build();
    let lbl_t = gtk::Label::builder().label(title)
        .css_classes(["metric-tile-title"]).halign(gtk::Align::Start).build();
    let lbl_v = gtk::Label::builder().label(value)
        .css_classes(["metric-tile-value", val_css]).halign(gtk::Align::Start).build();
    let lbl_s = gtk::Label::builder().label(subtitle)
        .css_classes(["metric-tile-sub"]).halign(gtk::Align::Start).wrap(true).build();
    tile.append(&lbl_t);
    tile.append(&lbl_v);
    tile.append(&lbl_s);
    (tile, lbl_v, lbl_s)
}

/// Returns the CSS colour class for a GPU/CPU temperature value.
fn temp_css(temp_c: i32, stale: bool) -> &'static str {
    if stale || temp_c == 0 { "metric-white" }
    else if temp_c >= 90     { "metric-red"    }
    else if temp_c >= 75     { "metric-orange" }
    else                     { "metric-green"  }
}

fn build_system_page(device: &SupportedDevice) -> gtk::ScrolledWindow {
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .build();

    let page = adw::PreferencesPage::new();

    // ── Initial data query ────────────────────────────────────────────
    // GetGpuStatus only reads the daemon cache — never wakes a suspended dGPU.
    let (gpu_name, gpu_util_init, gpu_temp_init, gpu_stale_init,
         power_w_init, power_limit_w_init, _power_max_limit_w_init,
         mem_used_init, mem_total_init, clock_gpu_init, clock_mem_init) =
        match send_data(comms::DaemonCommand::GetGpuStatus) {
            Some(comms::DaemonResponse::GetGpuStatus {
                name, temp_c, gpu_util, stale, power_w, power_limit_w, power_max_limit_w,
                mem_used_mb, mem_total_mb, clock_gpu_mhz, clock_mem_mhz, ..
            }) => (name, gpu_util, temp_c, stale,
                   power_w, power_limit_w, power_max_limit_w,
                   mem_used_mb, mem_total_mb, clock_gpu_mhz, clock_mem_mhz),
            _ => ("NVIDIA GPU".into(), 0u8, 0i32, true,
                  0.0f32, 0.0f32, 0.0f32, 0u32, 0u32, 0u32, 0u32),
        };
    let mem_pct_init = if mem_total_init > 0 { mem_used_init * 100 / mem_total_init } else { 0 };

    // CPU / RAM / temperature — fast sysfs reads, no daemon, no dGPU risk.
    let cpu_prev: Rc<RefCell<Option<(u64, u64)>>> = Rc::new(RefCell::new(cpu_stat_ticks()));
    let (ram_used_init, ram_total_init) = read_ram_mb();
    let cpu_temp_init                   = read_cpu_temp_c();
    let nvme_temp_init                  = read_nvme_temp_c();
    let fan_rpm_init                    = get_fan_tachometer();

    // ── Status + metric tiles ──────────────────────────────────────────
    let monitors_group = adw::PreferencesGroup::builder()
        .title("System Monitor")
        .description("Real-time GPU, CPU and memory performance")
        .build();

    // GPU state badge — updated every 3 s by the poll timer.
    let gpu_badge = gtk::Label::builder()
        .label(if gpu_stale_init { "GPU: suspended" } else { "GPU: live" })
        .halign(gtk::Align::Start)
        .build();
    gpu_badge.set_css_classes(if gpu_stale_init {
        &["status-badge", "badge-stale"]
    } else {
        &["status-badge", "badge-live"]
    });
    monitors_group.add(&gpu_badge);

    // ── Side-by-side GPU/System panels ───────────────────────────────
    let panels_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12).margin_top(4).margin_bottom(4)
        .build();

    // ── GPU panel ─────────────────────────────────────────────────────
    let gpu_panel = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .css_classes(["card"]).spacing(0).hexpand(true).build();
    {
        let hdr = gtk::Box::builder().orientation(gtk::Orientation::Vertical)
            .margin_start(14).margin_top(10).margin_bottom(6).spacing(2).build();
        hdr.append(&gtk::Label::builder().label("GPU")
            .css_classes(["panel-header"]).halign(gtk::Align::Start).build());
        hdr.append(&gtk::Label::builder().label(&gpu_name)
            .css_classes(["panel-subheader"]).halign(gtk::Align::Start).build());
        gpu_panel.append(&hdr);
    }
    let gpu_grid = gtk::Grid::builder()
        .row_spacing(8).column_spacing(8)
        .margin_start(12).margin_end(12).margin_bottom(12).build();
    gpu_grid.set_column_homogeneous(true);

    let gpu_util_str  = if gpu_stale_init { "--".into() } else { format!("{}%",    gpu_util_init) };
    let gpu_temp_str  = if gpu_stale_init { "--".into() } else { format!("{}°C",   gpu_temp_init) };
    let gpu_vram_str  = if gpu_stale_init { "--".into() } else { format!("{} MB",  mem_used_init) };
    let gpu_tgp_str   = if gpu_stale_init { "--".into() } else { format!("{:.0} W", power_w_init) };
    let gpu_clock_str = if gpu_stale_init { "--".into() } else { format!("{} MHz", clock_gpu_init) };

    let (gpu_util_tile,  gpu_util_lbl,  _)            = make_stat_tile("GPU",  &gpu_util_str,  "Utilization", "metric-green");
    let (gpu_temp_tile,  gpu_temp_lbl,  _)            = make_stat_tile("Temp", &gpu_temp_str,  "Sensor",      temp_css(gpu_temp_init, gpu_stale_init));
    let (gpu_vram_tile,  gpu_vram_lbl,  gpu_vram_sub) = make_stat_tile("VRAM", &gpu_vram_str,  "Utilization", "metric-purple");
    let (gpu_tgp_tile,   gpu_tgp_lbl,   gpu_tgp_sub) = make_stat_tile("TGP",  &gpu_tgp_str,   "Power draw",  "metric-cyan");
    let (gpu_clock_tile, gpu_clock_lbl, gpu_clock_sub)= make_stat_tile("Core", &gpu_clock_str, "Clock",       "metric-white");

    if !gpu_stale_init {
        gpu_vram_sub.set_text(&format!("/ {} MB · {}%",   mem_total_init, mem_pct_init));
        gpu_tgp_sub.set_text( &format!("/ {:.0} W limit", power_limit_w_init));
        gpu_clock_sub.set_text(&format!("{} MHz mem",     clock_mem_init));
    }
    gpu_grid.attach(&gpu_util_tile,  0, 0, 1, 1);
    gpu_grid.attach(&gpu_temp_tile,  1, 0, 1, 1);
    gpu_grid.attach(&gpu_vram_tile,  0, 1, 1, 1);
    gpu_grid.attach(&gpu_tgp_tile,   1, 1, 1, 1);
    gpu_grid.attach(&gpu_clock_tile, 0, 2, 2, 1); // spans both columns
    gpu_panel.append(&gpu_grid);

    // ── System panel ──────────────────────────────────────────────────
    let sys_panel = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .css_classes(["card"]).spacing(0).hexpand(true).build();
    {
        let hdr = gtk::Box::builder().orientation(gtk::Orientation::Vertical)
            .margin_start(14).margin_top(10).margin_bottom(6).spacing(2).build();
        hdr.append(&gtk::Label::builder().label("System")
            .css_classes(["panel-header"]).halign(gtk::Align::Start).build());
        hdr.append(&gtk::Label::builder().label("Performance counters")
            .css_classes(["panel-subheader"]).halign(gtk::Align::Start).build());
        sys_panel.append(&hdr);
    }
    let sys_grid = gtk::Grid::builder()
        .row_spacing(8).column_spacing(8)
        .margin_start(12).margin_end(12).margin_bottom(12).build();
    sys_grid.set_column_homogeneous(true);

    let ram_gb       = ram_used_init as f64 / 1024.0;
    let ram_total_gb = ram_total_init as f64 / 1024.0;
    let ram_pct      = if ram_total_init > 0 { ram_used_init * 100 / ram_total_init } else { 0 };

    let (cpu_util_tile,  cpu_util_lbl,  _)           = make_stat_tile("CPU",      "--",  "Utilization",  "metric-green");
    let (cpu_ram_tile,   cpu_ram_lbl,   cpu_ram_sub) = make_stat_tile(
        "RAM", &format!("{:.1} GB", ram_gb),
        &format!("/ {:.0} GB \u{00b7} {}% used", ram_total_gb, ram_pct), "metric-green");
    let (cpu_temp_tile,  cpu_temp_lbl,  _)           = make_stat_tile(
        "CPU Temp", &cpu_temp_init.map_or("--".into(), |t| format!("{}°C", t)),
        "Package sensor", "metric-orange");
    let (nvme_temp_tile, nvme_temp_lbl, _)           = make_stat_tile(
        "SSD Temp", &nvme_temp_init.map_or("--".into(), |t| format!("{}°C", t)),
        "NVMe sensor", "metric-cyan");
    let (fan_rpm_tile, fan_rpm_lbl, fan_rpm_sub)     = make_stat_tile(
        "Fan", &fan_rpm_init.map_or("--".into(), |rpm| format!("{} RPM", rpm.max(0))),
        "EC tachometer", "metric-cyan");
    if fan_rpm_init.unwrap_or(0) <= 0 {
        fan_rpm_sub.set_text("Unavailable");
    }

    sys_grid.attach(&cpu_util_tile,  0, 0, 1, 1);
    sys_grid.attach(&cpu_ram_tile,   1, 0, 1, 1);
    sys_grid.attach(&cpu_temp_tile,  0, 1, 1, 1);
    sys_grid.attach(&nvme_temp_tile, 1, 1, 1, 1);
    sys_grid.attach(&fan_rpm_tile,   0, 2, 2, 1);
    sys_panel.append(&sys_grid);

    panels_box.append(&gpu_panel);
    panels_box.append(&sys_panel);
    monitors_group.add(&panels_box);
    page.add(&monitors_group);

    // ── Timeline chart ────────────────────────────────────────────────
    const CHART_HISTORY: usize = 20; // 20 samples × 3s = 60s window

        #[derive(Clone, Copy, Default)]
        struct Sample {
            temp_c: f64,
            gpu_pct: f64,
            vram_pct: f64, // VRAM capacity used % (mem_used / mem_total × 100)
            power_w: f64,
        }

        let history: Arc<Mutex<VecDeque<Sample>>> =
            Arc::new(Mutex::new(VecDeque::with_capacity(CHART_HISTORY + 1)));
        let tgp_limit: Arc<Mutex<f64>> = Arc::new(Mutex::new(power_limit_w_init as f64));
        {
            let mut h = history.lock().unwrap();
            if !gpu_stale_init {
                h.push_back(Sample {
                    temp_c: gpu_temp_init as f64,
                    gpu_pct: gpu_util_init as f64,
                    vram_pct: if mem_total_init > 0 {
                        mem_used_init as f64 * 100.0 / mem_total_init as f64
                    } else {
                        0.0
                    },
                    power_w: power_w_init as f64,
                });
            }
        }

        let chart_group = adw::PreferencesGroup::builder()
            .title("Performance Timeline")
            .description("Temperature · GPU% · VRAM% · Power — last 60 s")
            .build();

        let chart = gtk::DrawingArea::new();
        chart.set_size_request(-1, 260);
        chart.set_margin_start(12);
        chart.set_margin_end(12);
        chart.set_margin_bottom(8);

        // Wrap the chart draw function in an Rc<dyn Fn> so it can be shared
        // between the embedded chart and any pop-out floating monitor windows.
        let draw_fn: Rc<dyn Fn(&gtk::DrawingArea, &gtk::cairo::Context, i32, i32)> = Rc::new({
            let hist_draw = history.clone();
            let tgp_draw = tgp_limit.clone();
            move |_da, cr: &gtk::cairo::Context, w: i32, h: i32| {
                let w = w as f64;
                let h = h as f64;
                let hist = hist_draw.lock().unwrap_or_else(|e| e.into_inner());
                let n = hist.len();
                if n < 2 {
                    return;
                }

                let pad_l = 42.0;
                let pad_r = 76.0;
                let pad_t = 12.0;
                let pad_b = 28.0;
                let cw = w - pad_l - pad_r;
                let ch = h - pad_t - pad_b;

                // Adaptive font sizes — scale with chart height for readability
                let fs_axis = (ch * 0.052).clamp(8.5, 11.0);
                let fs_legend = (ch * 0.056).clamp(9.5, 12.0);

                // Background
                cr.set_source_rgba(0.12, 0.12, 0.14, 1.0);
                let _ = cr.paint();

                // Grid lines (5 horizontal)
                cr.set_source_rgba(0.25, 0.25, 0.28, 1.0);
                cr.set_line_width(0.5);
                for i in 0..=4 {
                    let y = pad_t + ch * (i as f64 / 4.0);
                    cr.move_to(pad_l, y);
                    cr.line_to(pad_l + cw, y);
                    let _ = cr.stroke();
                }

                // Y-axis labels: left = °C (orange), right col1 = GPU% (green), col2 = Watts (cyan)
                cr.set_font_size(fs_axis);
                for i in 0..=4 {
                    let val = 100 - i * 25; // 100, 75, 50, 25, 0
                    let watts = val * 2; // 200, 150, 100, 50, 0
                    let y = pad_t + ch * (i as f64 / 4.0) + fs_axis * 0.38;
                    // Temp axis (left, orange)
                    cr.set_source_rgba(1.0, 0.6, 0.2, 0.85);
                    cr.move_to(2.0, y);
                    let _ = cr.show_text(&format!("{val}°"));
                    // GPU% axis (right inner, green)
                    cr.set_source_rgba(0.27, 1.0, 0.63, 0.85);
                    cr.move_to(w - pad_r + 2.0, y);
                    let _ = cr.show_text(&format!("{val}%"));
                    // Watts axis (right outer, cyan)
                    cr.set_source_rgba(0.3, 0.8, 1.0, 0.85);
                    cr.move_to(w - pad_r + 34.0, y);
                    let _ = cr.show_text(&format!("{watts}W"));
                }

                let x_step = cw / (CHART_HISTORY.max(2) - 1) as f64;

                // Draw line helper
                let draw_line =
                    |data: &dyn Fn(usize) -> f64, r: f64, g: f64, b: f64, max_val: f64| {
                        cr.set_source_rgba(r, g, b, 0.9);
                        cr.set_line_width(2.0);
                        let start_idx = if n > CHART_HISTORY {
                            n - CHART_HISTORY
                        } else {
                            0
                        };
                        let points: Vec<(f64, f64)> = (start_idx..n)
                            .enumerate()
                            .map(|(i, idx)| {
                                let x = pad_l + i as f64 * x_step;
                                let val = data(idx).clamp(0.0, max_val);
                                let y = pad_t + ch * (1.0 - val / max_val);
                                (x, y)
                            })
                            .collect();
                        if let Some(&(x0, y0)) = points.first() {
                            cr.move_to(x0, y0);
                            for &(x, y) in &points[1..] {
                                cr.line_to(x, y);
                            }
                            let _ = cr.stroke();
                        }
                    };

                let hist_ref: &VecDeque<Sample> = &hist;
                // Temp (orange)
                draw_line(&|i| hist_ref[i].temp_c, 1.0, 0.6, 0.2, 100.0);
                // GPU compute % (green)
                draw_line(&|i| hist_ref[i].gpu_pct, 0.27, 1.0, 0.63, 100.0);
                // VRAM capacity % (purple, solid)
                draw_line(&|i| hist_ref[i].vram_pct, 0.8, 0.4, 1.0, 100.0);
                // Power (cyan, scaled 0-200W → 0-100)
                draw_line(
                    &|i| hist_ref[i].power_w * (100.0 / 200.0),
                    0.3,
                    0.8,
                    1.0,
                    100.0,
                );

                // TGP limit — dashed horizontal reference line
                let tgp = *tgp_draw.lock().unwrap_or_else(|e| e.into_inner());
                if tgp > 0.0 {
                    let tgp_y = pad_t + ch * (1.0 - (tgp * (100.0 / 200.0)) / 100.0);
                    cr.set_source_rgba(0.3, 0.8, 1.0, 0.35);
                    cr.set_line_width(1.5);
                    cr.set_dash(&[5.0, 4.0], 0.0);
                    cr.move_to(pad_l, tgp_y);
                    cr.line_to(pad_l + cw, tgp_y);
                    let _ = cr.stroke();
                    cr.set_dash(&[], 0.0);
                    cr.set_font_size(fs_axis * 0.85);
                    cr.set_source_rgba(0.3, 0.8, 1.0, 0.7);
                    cr.move_to(pad_l + 2.0, tgp_y - 2.0);
                    let _ = cr.show_text(&format!("TGP {tgp:.0}W"));
                }

                // Legend — each label centered in its equal quarter of the chart
                cr.set_font_size(fs_legend);
                let legend = [
                    ("Temp", 1.0_f64, 0.6, 0.2),
                    ("GPU%", 0.27, 1.0, 0.63),
                    ("VRAM%", 0.8, 0.4, 1.0),
                    ("Power", 0.3, 0.8, 1.0),
                ];
                let n_items = legend.len() as f64;
                let legend_step = cw / n_items;
                let box_sz = (fs_legend * 0.8).round();
                for (idx, (label, r, g, b)) in legend.iter().enumerate() {
                    let lx = pad_l + idx as f64 * legend_step + (legend_step - 55.0).max(0.0) * 0.5;
                    cr.set_source_rgb(*r, *g, *b);
                    cr.rectangle(lx, h - 22.0, box_sz, box_sz);
                    let _ = cr.fill();
                    cr.move_to(lx + box_sz + 3.0, h - 8.0);
                    let _ = cr.show_text(label);
                }
            }
        });

        let draw_fn_c = draw_fn.clone();
        chart.set_draw_func(move |da, cr, w, h| draw_fn_c(da, cr, w, h));

        // CSV log file handle — None when logging is off, Some(file) when on.
        // Arc<Mutex<>> so it can be shared with the background poll thread.
        let csv_log: Arc<Mutex<Option<std::fs::File>>> = Arc::new(Mutex::new(None));

        // Bottom button row: [Log CSV]  ·  ·  ·  [Pop Out]
        let btn_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .margin_start(4)
            .margin_end(4)
            .margin_bottom(4)
            .build();

        let log_btn = gtk::ToggleButton::builder()
            .label("Log CSV")
            .icon_name("document-save-symbolic")
            .css_classes(["flat"])
            .tooltip_text("Log GPU metrics to ~/.local/share/razer-control/gpu_monitor.csv")
            .build();
        let csv_log_toggle = csv_log.clone();
        log_btn.connect_toggled(move |btn| {
            let mut guard = csv_log_toggle.lock().unwrap_or_else(|e| e.into_inner());
            if btn.is_active() {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
                let log_dir = std::path::Path::new(&home).join(".local/share/razer-control");
                let _ = std::fs::create_dir_all(&log_dir);
                let log_path = log_dir.join("gpu_monitor.csv");
                let file_exists = log_path.exists();
                if let Ok(f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&log_path)
                {
                    if !file_exists {
                        use std::io::Write;
                        let mut fw = &f;
                        let _ = writeln!(
                            fw,
                            "timestamp,power_limit_w,power_w,gpu_util,vram_pct,temp_c,stale"
                        );
                    }
                    *guard = Some(f);
                    btn.set_tooltip_text(Some(&format!("Logging → {}", log_path.display())));
                } else {
                    btn.set_active(false);
                }
            } else {
                *guard = None;
                btn.set_tooltip_text(Some(
                    "Log GPU metrics to ~/.local/share/razer-control/gpu_monitor.csv",
                ));
            }
        });
        btn_row.append(&log_btn);

        // Spacer between left and right buttons
        let spacer = gtk::Box::builder().hexpand(true).build();
        btn_row.append(&spacer);

        // "Pop Out" button — opens a floating GPU monitor window.
        // The floating chart shares the same history/tgp_limit Rc data as the
        // embedded chart, so both update in lock-step from the 3-second poll timer.
        let popout_btn = gtk::Button::builder()
            .label("Pop Out")
            .icon_name("window-new-symbolic")
            .css_classes(["flat"])
            .build();
        // Use a WeakRef to the main chart so the popout closure can hide/show it
        // without preventing chart from being properly destroyed with the main window.
        let chart_wr = chart.downgrade();
        let draw_fn_popup = draw_fn.clone();
        popout_btn.connect_clicked(glib::clone!(
            #[weak]
            popout_btn,
            #[upgrade_or]
            (),
            move |_| {
                // Hide main chart — no duplicate rendering while popped out.
                let Some(chart_main) = chart_wr.upgrade() else {
                    return;
                };
                chart_main.set_visible(false);
                popout_btn.set_sensitive(false);

                let float_win = gtk::Window::builder()
                    .title("GPU Monitor — Razer Blade")
                    .default_width(700)
                    .default_height(380)
                    .resizable(true)
                    .build();

                let float_chart = gtk::DrawingArea::new();
                float_chart.set_size_request(-1, 340);
                float_chart.set_margin_start(12);
                float_chart.set_margin_end(12);
                float_chart.set_margin_top(4);
                float_chart.set_margin_bottom(4);
                float_chart.set_vexpand(true);

                let draw_fn_c = draw_fn_popup.clone();
                float_chart.set_draw_func(move |da, cr, w, h| draw_fn_c(da, cr, w, h));

                // Refresh floating chart every 3 s — main poll timer already updates
                // the shared history Rc, so this only needs queue_draw().
                glib::timeout_add_seconds_local(
                    3,
                    glib::clone!(
                        #[weak]
                        float_chart,
                        #[upgrade_or]
                        glib::ControlFlow::Break,
                        move || {
                            float_chart.queue_draw();
                            glib::ControlFlow::Continue
                        }
                    ),
                );

                // Restore main chart when the floating window is closed.
                let chart_wr_c = chart_wr.clone();
                float_win.connect_destroy(glib::clone!(
                    #[weak]
                    popout_btn,
                    #[upgrade_or]
                    (),
                    move |_| {
                        if let Some(c) = chart_wr_c.upgrade() {
                            c.set_visible(true);
                        }
                        popout_btn.set_sensitive(true);
                    }
                ));

                // ── KWin "keep above" rule ────────────────────────────────────
                // Write a persistent kwinrulesrc rule BEFORE presenting the window.
                // KWin's "Apply Initially" policy (aboverule=2) sets keepAbove when
                // the window is first mapped — no timing race with scripting delays.
                // kwriteconfig6 handles the INI format correctly.
                // On the very first call this blocks ~150 ms (8 subprocesses);
                // subsequent calls only call qdbus6 reconfigure (~20 ms).
                {
                    let count: usize = std::process::Command::new("kreadconfig6")
                        .args([
                            "--file",
                            "kwinrulesrc",
                            "--group",
                            "General",
                            "--key",
                            "count",
                            "--default",
                            "0",
                        ])
                        .output()
                        .ok()
                        .and_then(|o| String::from_utf8(o.stdout).ok())
                        .and_then(|s| s.trim().parse().ok())
                        .unwrap_or(0);
                    let already = (1..=count).any(|i| {
                        std::process::Command::new("kreadconfig6")
                            .args([
                                "--file",
                                "kwinrulesrc",
                                "--group",
                                &i.to_string(),
                                "--key",
                                "Description",
                                "--default",
                                "",
                            ])
                            .output()
                            .ok()
                            .and_then(|o| String::from_utf8(o.stdout).ok())
                            .map(|s| s.contains("razer-gpu-monitor"))
                            .unwrap_or(false)
                    });
                    if !already {
                        let n = (count + 1).to_string();
                        for (k, v) in [
                            ("Description", "razer-gpu-monitor"),
                            ("above", "true"),
                            ("aboverule", "2"),
                            ("title", "GPU Monitor"),
                            ("titlematch", "2"),
                            ("wmclassmatch", "0"),
                        ] {
                            let _ = std::process::Command::new("kwriteconfig6")
                                .args(["--file", "kwinrulesrc", "--group", &n, "--key", k, v])
                                .status();
                        }
                        let _ = std::process::Command::new("kwriteconfig6")
                            .args([
                                "--file",
                                "kwinrulesrc",
                                "--group",
                                "General",
                                "--key",
                                "count",
                                &n,
                            ])
                            .status();
                    }
                    // Reload rules synchronously — the window maps AFTER this returns.
                    let _ = std::process::Command::new("qdbus6")
                        .args(["org.kde.KWin", "/KWin", "org.kde.KWin.reconfigure"])
                        .status();
                }
                float_win.set_child(Some(&float_chart));
                float_win.present();

                // KWin scripting backup — catches cases where "Apply Initially"
                // rule doesn't fire (e.g. compositor restarts or rule policy mismatch).
                // Uses a larger delay (2 s) and unique script name to avoid cached IDs.
                glib::timeout_add_local(std::time::Duration::from_millis(2000), move || {
                    let script = r#"var wins = workspace.windows || [];
for (var i = 0; i < wins.length; i++) {
    if (wins[i] && wins[i].caption &&
            wins[i].caption.indexOf("GPU Monitor") !== -1) {
        wins[i].keepAbove = true;
    }
}"#;
                    let ts = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis();
                    let script_name = format!("razer-pin-{ts}");
                    let tmp = std::env::temp_dir().join("razer-kwin-pin.js");
                    if std::fs::write(&tmp, script).is_ok() {
                        let _ = std::process::Command::new("qdbus6")
                            .args([
                                "org.kde.KWin",
                                "/Scripting",
                                "org.kde.kwin.Scripting.loadScript",
                                tmp.to_str().unwrap_or(""),
                                &script_name,
                            ])
                            .status();
                        let _ = std::process::Command::new("qdbus6")
                            .args(["org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting.start"])
                            .status();
                        let _ = std::fs::remove_file(&tmp);
                    }
                    glib::ControlFlow::Break
                });
            }
        ));
        btn_row.append(&popout_btn);

        chart_group.add(&chart);
        chart_group.add(&btn_row);
        page.add(&chart_group);

        // ── Background GPU poll: IPC runs off the GTK main thread ──────────────────────────
        // Struct for passing GPU data through the channel (must be Send).
        #[derive(Clone, Copy)]
        struct GpuSample {
            temp_c: i32,
            gpu_util: u8,
            stale: bool,
            power_w: f32,
            power_limit_w: f32,
            #[allow(dead_code)]
            power_max_limit_w: f32,
            mem_used_mb: u32,
            mem_total_mb: u32,
            clock_gpu_mhz: u32,
            clock_mem_mhz: u32,
        }

        // mpsc channel: background thread → main thread.
        // Receiver is !Send so it stays on the main thread (used in _local timer below).
        let (gpu_tx, gpu_rx) = std::sync::mpsc::channel::<Option<GpuSample>>();
        let poll_in_flight = Arc::new(AtomicBool::new(false));

        // How many consecutive samples have had gpu_util==0 and power<15 W.
        // When this reaches IDLE_THRESHOLD both the chart AND the tile widgets
        // flip to "idle" state simultaneously — guaranteed zero desync.
        // IDLE_THRESHOLD=1 means: on the very first 3s poll at 0% util both
        // chart and tiles flip, so the flat-zero-line phase never appears.
        const IDLE_THRESHOLD: u32 = 1;
        let idle_count: Rc<RefCell<u32>> = Rc::new(RefCell::new(0));

        // Timer: non-blocking try_recv on each tick + launch background IPC thread.
        // Main thread never blocks: try_recv() is instant, thread::spawn() is instant.
        {
            let hist_t = history.clone();
            let tgp_t = tgp_limit.clone();
            let csv_t = csv_log.clone();
            let chart_t = chart.clone();
            let in_flight = poll_in_flight;
            let idle_cnt = idle_count.clone();
            // Cloned strong refs — GLib releases the timer source (and these refs)
            // when the main window closes.
            let gpu_badge_t  = gpu_badge.clone();
            let gpu_util_l   = gpu_util_lbl.clone();
            let gpu_temp_l   = gpu_temp_lbl.clone();
            let gpu_vram_l   = gpu_vram_lbl.clone();
            let gpu_vram_s   = gpu_vram_sub.clone();
            let gpu_tgp_l    = gpu_tgp_lbl.clone();
            let gpu_tgp_s    = gpu_tgp_sub.clone();
            let gpu_clock_l  = gpu_clock_lbl.clone();
            let gpu_clock_s  = gpu_clock_sub.clone();
            let cpu_util_l   = cpu_util_lbl.clone();
            let cpu_ram_l    = cpu_ram_lbl.clone();
            let cpu_ram_s    = cpu_ram_sub.clone();
            let cpu_temp_l   = cpu_temp_lbl.clone();
            let nvme_temp_l  = nvme_temp_lbl.clone();
            let fan_rpm_l    = fan_rpm_lbl.clone();
            let cpu_prev_t   = cpu_prev.clone();
            glib::timeout_add_seconds_local(3, move || {
                // 1. Consume any fresh data produced by the previous background thread.
                if let Ok(Some(s)) = gpu_rx.try_recv() {
                    // Determine display state: stale = GPU runtime-suspended;
                    // idle = GPU active but at 0% util for IDLE_THRESHOLD polls.
                    // Both states show '--' in tiles and clear the chart so that
                    // tiles and chart always flip at the exact same glib tick.
                    let stale = s.stale;
                    if !stale && s.gpu_util == 0 && s.power_w < 15.0 {
                        let cnt = *idle_cnt.borrow() + 1;
                        *idle_cnt.borrow_mut() = cnt;
                    } else {
                        *idle_cnt.borrow_mut() = 0;
                    }
                    let idle = stale || *idle_cnt.borrow() >= IDLE_THRESHOLD;
                    if idle {
                        let badge_text = if stale { "GPU: suspended" } else { "GPU: idle" };
                        gpu_badge_t.set_text(badge_text);
                        gpu_badge_t.set_css_classes(&["status-badge", "badge-stale"]);
                        gpu_util_l.set_text("--");
                        gpu_temp_l.set_text("--");
                        gpu_vram_l.set_text("--");
                        gpu_vram_s.set_text("/ -- MB");
                        gpu_tgp_l.set_text("--");
                        gpu_tgp_s.set_text("");
                        gpu_clock_l.set_text("--");
                        gpu_clock_s.set_text("");
                    } else {
                        gpu_badge_t.set_text("GPU: live");
                        gpu_badge_t.set_css_classes(&["status-badge", "badge-live"]);
                        let mem_pct = if s.mem_total_mb > 0 { s.mem_used_mb * 100 / s.mem_total_mb } else { 0 };
                        gpu_util_l.set_text(&format!("{}%",    s.gpu_util));
                        gpu_temp_l.set_text(&format!("{}°C",   s.temp_c));
                        gpu_temp_l.set_css_classes(&["metric-tile-value", temp_css(s.temp_c, false)]);
                        gpu_vram_l.set_text(&format!("{} MB",  s.mem_used_mb));
                        gpu_vram_s.set_text(&format!("/ {} MB \u{00b7} {}%", s.mem_total_mb, mem_pct));
                        gpu_tgp_l.set_text( &format!("{:.0} W", s.power_w));
                        gpu_tgp_s.set_text( &format!("/ {:.0} W limit", s.power_limit_w));
                        gpu_clock_l.set_text(&format!("{} MHz", s.clock_gpu_mhz));
                        gpu_clock_s.set_text(&format!("{} MHz mem", s.clock_mem_mhz));
                    }

                    // System tiles: CPU/RAM/temp — no dGPU access, safe always.
                    {
                        let (used_mb, total_mb) = read_ram_mb();
                        let gb   = used_mb  as f64 / 1024.0;
                        let tot  = total_mb as f64 / 1024.0;
                        let pct  = if total_mb > 0 { used_mb * 100 / total_mb } else { 0 };
                        cpu_ram_l.set_text(&format!("{:.1} GB", gb));
                        cpu_ram_s.set_text(&format!("/ {:.0} GB \u{00b7} {}% used", tot, pct));
                    }
                    if let Some(t) = read_cpu_temp_c() {
                        cpu_temp_l.set_text(&format!("{}°C", t));
                        cpu_temp_l.set_css_classes(&["metric-tile-value", temp_css(t, false)]);
                    }
                    if let Some(t) = read_nvme_temp_c() {
                        nvme_temp_l.set_text(&format!("{}°C", t));
                    }
                    if let Some(rpm) = get_fan_tachometer() {
                        if rpm > 0 {
                            fan_rpm_l.set_text(&format!("{} RPM", rpm));
                        } else {
                            fan_rpm_l.set_text("--");
                        }
                    }
                    // CPU utilization: delta between two successive /proc/stat samples.
                    {
                        let cur = cpu_stat_ticks();
                        let prev = cpu_prev_t.borrow().clone();
                        if let (Some((ct, ci)), Some((pt, pi))) = (cur, prev) {
                            let dt = ct.saturating_sub(pt);
                            let di = ci.saturating_sub(pi);
                            let util = if dt > 0 { (100 * (dt - di) / dt) as u8 } else { 0 };
                            cpu_util_l.set_text(&format!("{}%", util));
                        }
                        *cpu_prev_t.borrow_mut() = cpu_stat_ticks();
                    }

                    *tgp_t.lock().unwrap_or_else(|e| e.into_inner()) = s.power_limit_w as f64;
                    let sample = Sample {
                        temp_c: s.temp_c as f64,
                        gpu_pct: s.gpu_util as f64,
                        vram_pct: if s.mem_total_mb > 0 {
                            s.mem_used_mb as f64 * 100.0 / s.mem_total_mb as f64
                        } else {
                            0.0
                        },
                        power_w: s.power_w as f64,
                    };
                    // Chart history: push samples only while not idle (same condition as tiles).
                    // Both chart and tiles flip at the exact same glib tick — guaranteed sync.
                    if !idle {
                        let mut h = hist_t.lock().unwrap_or_else(|e| e.into_inner());
                        h.push_back(sample);
                        while h.len() > CHART_HISTORY {
                            h.pop_front();
                        }
                    } else {
                        hist_t.lock().unwrap_or_else(|e| e.into_inner()).clear();
                    }
                    if let Some(ref mut f) = *csv_t.lock().unwrap_or_else(|e| e.into_inner()) {
                        use std::io::Write;
                        let ts = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs();
                        let _ = writeln!(
                            f,
                            "{ts},{:.1},{:.1},{},{:.1},{},{}",
                            s.power_limit_w,
                            s.power_w,
                            s.gpu_util,
                            sample.vram_pct,
                            s.temp_c,
                            if s.stale { 1u8 } else { 0u8 }
                        );
                    }
                    chart_t.queue_draw();
                }
                // 2. Launch next background IPC thread (skipped if one is already running).
                if !in_flight.swap(true, Ordering::AcqRel) {
                    let tx2 = gpu_tx.clone();
                    let in_flight2 = in_flight.clone();
                    std::thread::spawn(move || {
                        let resp = comms::try_bind().ok().and_then(|s| {
                            comms::send_to_daemon(comms::DaemonCommand::GetGpuStatus, s)
                        });
                        in_flight2.store(false, Ordering::Release);
                        let msg = match resp {
                            Some(comms::DaemonResponse::GetGpuStatus {
                                temp_c,
                                gpu_util,
                                stale,
                                power_w,
                                power_limit_w,
                                power_max_limit_w,
                                mem_used_mb,
                                mem_total_mb,
                                clock_gpu_mhz,
                                clock_mem_mhz,
                                ..
                            }) => Some(GpuSample {
                                temp_c,
                                gpu_util,
                                stale,
                                power_w,
                                power_limit_w,
                                power_max_limit_w,
                                mem_used_mb,
                                mem_total_mb,
                                clock_gpu_mhz,
                                clock_mem_mhz,
                            }),
                            _ => None,
                        };
                        let _ = tx2.send(msg);
                    });
                }
                glib::ControlFlow::Continue
            });
        }

    // 3. System Information
    {
        let sysinfo_expander = adw::ExpanderRow::builder()
            .title("System Information")
            .expanded(true)
            .build();

        let add_info = |title: &str, value: &str| -> adw::ActionRow {
            let row = adw::ActionRow::builder().title(title).build();
            row.add_suffix(
                &gtk::Label::builder()
                    .label(value)
                    .css_classes(["monospace"])
                    .build(),
            );
            row
        };

        // ── OS & hardware ─────────────────────────────────────────────
        let dmi_product = std::fs::read_to_string("/sys/devices/virtual/dmi/id/product_name")
            .unwrap_or_default()
            .trim()
            .to_string();
        let dmi_vendor = std::fs::read_to_string("/sys/devices/virtual/dmi/id/sys_vendor")
            .unwrap_or_default()
            .trim()
            .to_string();
        let dmi_bios = std::fs::read_to_string("/sys/devices/virtual/dmi/id/bios_version")
            .unwrap_or_default()
            .trim()
            .to_string();

        // OS name from /etc/os-release PRETTY_NAME
        let os_name = std::fs::read_to_string("/etc/os-release")
            .unwrap_or_default()
            .lines()
            .find(|l| l.starts_with("PRETTY_NAME="))
            .map(|l| {
                l.trim_start_matches("PRETTY_NAME=")
                    .trim_matches('"')
                    .to_string()
            })
            .unwrap_or_else(|| "Linux".into());

        // Kernel version — first 3 fields of /proc/version_signature, else first field of /proc/version
        let kernel = std::fs::read_to_string("/proc/version").unwrap_or_default();
        let kernel_version = kernel
            .split_whitespace()
            .nth(2)
            .unwrap_or("unknown")
            .to_string();

        // CPU model from /proc/cpuinfo
        let cpu_model = std::fs::read_to_string("/proc/cpuinfo")
            .unwrap_or_default()
            .lines()
            .find(|l| l.starts_with("model name"))
            .and_then(|l| l.split(':').nth(1))
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "unknown".into());

        // Core count
        let cpu_cores: usize = std::fs::read_to_string("/proc/cpuinfo")
            .unwrap_or_default()
            .lines()
            .filter(|l| l.starts_with("processor"))
            .count();

        // RAM from /proc/meminfo
        let ram_kb: u64 = std::fs::read_to_string("/proc/meminfo")
            .unwrap_or_default()
            .lines()
            .find(|l| l.starts_with("MemTotal:"))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let ram_gb = ram_kb / (1024 * 1024);
        let ram_str = if ram_gb > 0 {
            format!("{ram_gb} GiB")
        } else {
            format!("{} MiB", ram_kb / 1024)
        };

        if !dmi_product.is_empty() {
            let host = if dmi_vendor.is_empty() {
                dmi_product
            } else {
                format!("{dmi_vendor} {dmi_product}")
            };
            sysinfo_expander.add_row(&add_info("Host", &host));
        }
        sysinfo_expander.add_row(&add_info("OS", &os_name));
        sysinfo_expander.add_row(&add_info("Kernel", &kernel_version));
        sysinfo_expander.add_row(&add_info(
            "Processor",
            &format!("{cpu_model} × {cpu_cores}"),
        ));
        sysinfo_expander.add_row(&add_info("Memory", &ram_str));
        if !dmi_bios.is_empty() {
            sysinfo_expander.add_row(&add_info("BIOS", &dmi_bios));
        }

        // ── Razer device ──────────────────────────────────────────────
        sysinfo_expander.add_row(&add_info("Device", &device.name));
        sysinfo_expander.add_row(&add_info(
            "USB ID",
            &format!("{}:{}", device.vid, device.pid),
        ));
        sysinfo_expander.add_row(&add_info("Features", &device.features.join(", ")));
        sysinfo_expander.add_row(&add_info(
            "Fan Range",
            &format!(
                "{} – {} RPM",
                device.fan.first().unwrap_or(&0),
                device.fan.get(1).unwrap_or(&0)
            ),
        ));

        let sysinfo_group = adw::PreferencesGroup::new();
        sysinfo_group.add(&sysinfo_expander);
        page.add(&sysinfo_group);
    }

    {
        let saved_gui = gui_config::GuiConfig::load();
        let autostart_enabled = startup::is_enabled();
        let integration_group = adw::PreferencesGroup::builder()
            .title("Desktop Integration")
            .description("Manage how the GTK control panel integrates with your desktop session")
            .build();

        let startup_row = adw::SwitchRow::builder()
            .title("Start Tray App on Login")
            .subtitle("Launch razer-settings automatically at session start")
            .active(autostart_enabled)
            .build();

        let minimized_row = adw::SwitchRow::builder()
            .title("Start to System Tray Only")
            .subtitle("Keep the window hidden on login; tray icon and app process still run")
            .active(saved_gui.start_minimized)
            .build();

        minimized_row.connect_active_notify(move |row| {
            let mut config = gui_config::GuiConfig::load();
            config.start_minimized = row.is_active();
            let _ = config.save();

            if startup::is_enabled() {
                if let Err(error) = startup::set_enabled(true, config.start_minimized) {
                    eprintln!("Failed to refresh autostart entry: {error}");
                }
            }
        });

        let minimized_row_clone = minimized_row.clone();
        startup_row.connect_active_notify(move |row| {
            let requested = row.is_active();
            if let Err(error) = startup::set_enabled(requested, minimized_row_clone.is_active()) {
                eprintln!("Failed to update autostart entry: {error}");
            }
            let actual = startup::is_enabled();
            row.set_active(actual);

            let mut config = gui_config::GuiConfig::load();
            config.run_at_startup = actual;
            let _ = config.save();
        });

        let state_row = adw::ActionRow::builder()
            .title("Autostart State")
            .subtitle("Source of truth: XDG autostart desktop entry")
            .build();
        let state_label = gtk::Label::builder()
            .label(if autostart_enabled || saved_gui.run_at_startup {
                "Enabled"
            } else {
                "Disabled"
            })
            .css_classes(["monospace"])
            .build();
        state_row.add_suffix(&state_label);

        startup_row.connect_active_notify(move |_| {
            state_label.set_text(if startup::is_enabled() {
                "Enabled"
            } else {
                "Disabled"
            });
        });

        integration_group.add(&startup_row);
        integration_group.add(&minimized_row);
        integration_group.add(&state_row);
        page.add(&integration_group);
    }

    scroll.set_child(Some(&page));
    scroll
}

// ── Entry point ───────────────────────────────────────────────────────────

struct InstanceGuard {
    _listener: UnixListener,
    socket_path: PathBuf,
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

fn acquire_instance_guard() -> Option<InstanceGuard> {
    let socket_path = std::env::temp_dir().join("razer-settings-singleton.sock");
    if socket_path.exists() {
        // If another process answers on this socket, an instance is already running.
        if UnixStream::connect(&socket_path).is_ok() {
            return None;
        }
        // Stale socket file from a previous crash.
        let _ = std::fs::remove_file(&socket_path);
    }

    let listener = UnixListener::bind(&socket_path).ok()?;
    Some(InstanceGuard {
        _listener: listener,
        socket_path,
    })
}

fn main() {
    setup_panic_hook();

    // If this deprecated key is present in the user's GTK4 settings, libadwaita
    // prints a warning on every launch. Remove it once at startup.
    scrub_deprecated_gtk_dark_pref_setting();

    let _instance_guard = match acquire_instance_guard() {
        Some(guard) => guard,
        None => {
            eprintln!("razer-settings is already running");
            return;
        }
    };

    // GTK rejects unknown CLI options, so minimized autostart is passed via env.
    let env_minimized = std::env::var("RAZER_SETTINGS_START_MINIMIZED")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    // Fallback for desktop environments that launch from autostart without
    // propagating custom Exec environment variables.
    let autostart_minimized = std::env::var_os("DESKTOP_AUTOSTART_ID").is_some()
        && gui_config::GuiConfig::load().start_minimized;
    if env_minimized || autostart_minimized {
        START_MINIMIZED.store(true, Ordering::Relaxed);
    }

    let device_file =
        std::fs::read_to_string(service::DEVICE_FILE).or_crash("Failed to read the device file");
    let devices: Vec<SupportedDevice> =
        serde_json::from_str(&device_file).or_crash("Failed to parse the device file");
    let device = find_device_config_with_retry(&devices).or_crash("Failed to find device config");

    let app = RelmApp::new("io.github.razer-linux.razer-blade-control");

    // Load minimal CSS overrides
    relm4::set_global_css(include_str!("style.css"));

    // Let libadwaita follow the desktop color preference.
    let style_manager = adw::StyleManager::default();
    style_manager.set_color_scheme(adw::ColorScheme::Default);

    // Spawn tray icon
    {
        let (tray_sender, tray_receiver) = std::sync::mpsc::channel::<tray::TrayAction>();

        // Poll the tray receiver periodically to present the window or restart
        glib::timeout_add_local(std::time::Duration::from_millis(200), move || {
            while let Ok(action) = tray_receiver.try_recv() {
                match action {
                    tray::TrayAction::ShowWindow => {
                        if let Some(app) = gtk::gio::Application::default() {
                            app.activate();
                        }
                    }
                    tray::TrayAction::Restart => {
                        let exe =
                            std::env::current_exe().unwrap_or_else(|_| "razer-settings".into());
                        let _ = std::process::Command::new(exe).spawn();
                        std::process::exit(0);
                    }
                    tray::TrayAction::Quit => std::process::exit(0),
                    tray::TrayAction::SetPowerMode { ac, profile } => {
                        let current = get_power(ac).unwrap_or((profile, 0, 0));
                        let _ = set_power(ac, (profile, current.1, current.2));
                    }
                    tray::TrayAction::SetBrightness { ac, percent } => {
                        let _ = set_brightness(ac, percent);
                    }
                    tray::TrayAction::SetEffect { name, params } => {
                        let _ = set_effect(&name, params);
                    }
                }
            }
            glib::ControlFlow::Continue
        });

        std::thread::spawn(move || {
            use ksni::blocking::TrayMethods;
            let t = tray::RazerTray {
                action_sender: tray_sender,
            };
            match t.spawn() {
                Ok(handle) => std::mem::forget(handle),
                Err(e) => eprintln!("Warning: tray icon failed: {e}"),
            }
        });
    }

    app.run::<App>(device);
}
