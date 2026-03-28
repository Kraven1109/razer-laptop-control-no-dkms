#![deny(warnings)]

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::ErrorKind;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use relm4::prelude::*;

#[path = "../comms.rs"]
mod comms;
mod error_handling;
mod widgets;
mod util;
mod tray;

use service::SupportedDevice;
use error_handling::*;
use widgets::ColorWheel;
use util::*;

// ── Daemon communication helpers ──────────────────────────────────────────

fn send_data(opt: comms::DaemonCommand) -> Option<comms::DaemonResponse> {
    match comms::try_bind() {
        Ok(socket) => comms::send_to_daemon(opt, socket),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            crash_with_msg("Can't connect to the daemon");
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
        r => { eprintln!("Unexpected: {r:?}"); None }
    }
}

fn get_bho() -> Option<(bool, u8)> {
    match send_data(comms::DaemonCommand::GetBatteryHealthOptimizer())? {
        comms::DaemonResponse::GetBatteryHealthOptimizer { is_on, threshold } => Some((is_on, threshold)),
        r => { eprintln!("Unexpected: {r:?}"); None }
    }
}

fn set_bho(is_on: bool, threshold: u8) -> Option<bool> {
    match send_data(comms::DaemonCommand::SetBatteryHealthOptimizer { is_on, threshold })? {
        comms::DaemonResponse::SetBatteryHealthOptimizer { result } => Some(result),
        r => { eprintln!("Unexpected: {r:?}"); None }
    }
}

fn get_brightness(ac: bool) -> Option<u8> {
    let ac = if ac { 1 } else { 0 };
    match send_data(comms::DaemonCommand::GetBrightness { ac })? {
        comms::DaemonResponse::GetBrightness { result } => Some(result),
        r => { eprintln!("Unexpected: {r:?}"); None }
    }
}

fn set_brightness(ac: bool, val: u8) -> Option<bool> {
    let ac = if ac { 1 } else { 0 };
    match send_data(comms::DaemonCommand::SetBrightness { ac, val })? {
        comms::DaemonResponse::SetBrightness { result } => Some(result),
        r => { eprintln!("Unexpected: {r:?}"); None }
    }
}

fn get_logo(ac: bool) -> Option<u8> {
    let ac = if ac { 1 } else { 0 };
    match send_data(comms::DaemonCommand::GetLogoLedState { ac })? {
        comms::DaemonResponse::GetLogoLedState { logo_state } => Some(logo_state),
        r => { eprintln!("Unexpected: {r:?}"); None }
    }
}

fn set_logo(ac: bool, logo_state: u8) -> Option<bool> {
    let ac = if ac { 1 } else { 0 };
    match send_data(comms::DaemonCommand::SetLogoLedState { ac, logo_state })? {
        comms::DaemonResponse::SetLogoLedState { result } => Some(result),
        r => { eprintln!("Unexpected: {r:?}"); None }
    }
}

fn set_effect(name: &str, values: Vec<u8>) -> Option<bool> {
    match send_data(comms::DaemonCommand::SetEffect { name: name.into(), params: values })? {
        comms::DaemonResponse::SetEffect { result } => Some(result),
        r => { eprintln!("Unexpected: {r:?}"); None }
    }
}

fn get_power(ac: bool) -> Option<(u8, u8, u8)> {
    let ac_val = if ac { 1 } else { 0 };
    let pwr = match send_data(comms::DaemonCommand::GetPwrLevel { ac: ac_val })? {
        comms::DaemonResponse::GetPwrLevel { pwr } => pwr,
        r => { eprintln!("Unexpected: {r:?}"); return None }
    };
    let cpu = match send_data(comms::DaemonCommand::GetCPUBoost { ac: ac_val })? {
        comms::DaemonResponse::GetCPUBoost { cpu } => cpu,
        r => { eprintln!("Unexpected: {r:?}"); return None }
    };
    let gpu = match send_data(comms::DaemonCommand::GetGPUBoost { ac: ac_val })? {
        comms::DaemonResponse::GetGPUBoost { gpu } => gpu,
        r => { eprintln!("Unexpected: {r:?}"); return None }
    };
    Some((pwr, cpu, gpu))
}

fn set_power(ac: bool, power: (u8, u8, u8)) -> Option<bool> {
    let ac = if ac { 1 } else { 0 };
    match send_data(comms::DaemonCommand::SetPowerMode { ac, pwr: power.0, cpu: power.1, gpu: power.2 })? {
        comms::DaemonResponse::SetPowerMode { result } => Some(result),
        r => { eprintln!("Unexpected: {r:?}"); None }
    }
}

fn get_fan_speed(ac: bool) -> Option<i32> {
    let ac = if ac { 1 } else { 0 };
    match send_data(comms::DaemonCommand::GetFanSpeed { ac })? {
        comms::DaemonResponse::GetFanSpeed { rpm } => Some(rpm),
        r => { eprintln!("Unexpected: {r:?}"); None }
    }
}

fn set_fan_speed(ac: bool, value: i32) -> Option<bool> {
    let ac = if ac { 1 } else { 0 };
    match send_data(comms::DaemonCommand::SetFanSpeed { ac, rpm: value })? {
        comms::DaemonResponse::SetFanSpeed { result } => Some(result),
        r => { eprintln!("Unexpected: {r:?}"); None }
    }
}

fn get_power_limits() -> Option<(u32, u32, u32)> {
    match send_data(comms::DaemonCommand::GetPowerLimits)? {
        comms::DaemonResponse::GetPowerLimits { pl1_watts, pl2_watts, pl1_max_watts } => {
            Some((pl1_watts, pl2_watts, pl1_max_watts))
        }
        r => { eprintln!("Unexpected: {r:?}"); None }
    }
}

fn set_power_limits(pl1: u32, pl2: u32) -> Option<bool> {
    match send_data(comms::DaemonCommand::SetPowerLimits { pl1_watts: pl1, pl2_watts: pl2 })? {
        comms::DaemonResponse::SetPowerLimits { result } => Some(result),
        r => { eprintln!("Unexpected: {r:?}"); None }
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
        let model = App { _device: device.clone() };
        let widgets = view_output!();

        // Build the view stack
        let view_stack = adw::ViewStack::new();
        view_stack.add_titled_with_icon(
            &build_power_page(true, &device), Some("ac"), "AC", "thunderbolt-symbolic",
        );
        view_stack.add_titled_with_icon(
            &build_power_page(false, &device), Some("battery"), "Battery", "battery-symbolic",
        );
        view_stack.add_titled_with_icon(
            &build_keyboard_page(), Some("keyboard"), "Keyboard", "input-keyboard-symbolic",
        );
        view_stack.add_titled_with_icon(
            &build_about_page(&device), Some("about"), "About", "help-about-symbolic",
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
        if let Some(false) = check_if_running_on_ac_power() {
            view_stack.set_visible_child_name("battery");
        }

        // Minimize to tray
        root.connect_close_request(|win| {
            win.set_visible(false);
            glib::Propagation::Stop
        });

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
        power_row.set_model(Some(&gtk::StringList::new(&["Balanced", "Gaming", "Creator", "Silent", "Custom"])));
        power_row.set_selected(power.0 as u32);

        let cpu_row = adw::ComboRow::builder()
            .title("CPU Boost")
            .subtitle("Processor performance level")
            .build();
        let mut cpu_opts = vec!["Low", "Medium", "High"];
        if device.can_boost() { cpu_opts.push("Boost"); }
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
        page.add(&group);
    }

    // Fan Speed section
    {
        let group = adw::PreferencesGroup::builder().title("Fan Speed").build();
        let fan_speed = get_fan_speed(ac).unwrap_or(0);
        let min_fan = *device.fan.get(0).unwrap_or(&3500) as f64;
        let max_fan = *device.fan.get(1).unwrap_or(&5000) as f64;

        let auto_row = adw::SwitchRow::builder()
            .title("Auto")
            .subtitle("Let the firmware manage fan speed")
            .active(fan_speed == 0)
            .build();

        let spin_row = adw::SpinRow::with_range(min_fan, max_fan, 100.0);
        spin_row.set_title("Speed (RPM)");
        spin_row.set_subtitle("Manual fan speed");
        spin_row.set_value(if fan_speed == 0 { min_fan } else { fan_speed as f64 });
        spin_row.set_sensitive(fan_speed != 0);

        let spin_clone = spin_row.clone();
        auto_row.connect_active_notify(move |row| {
            let rpm = if row.is_active() { 0 } else { min_fan as i32 };
            set_fan_speed(ac, rpm);
            let readback = get_fan_speed(ac).unwrap_or(0);
            let is_auto = readback == 0;
            row.set_active(is_auto);
            spin_clone.set_sensitive(!is_auto);
            if !is_auto { spin_clone.set_value(readback as f64); }
        });

        let auto_clone = auto_row.clone();
        spin_row.connect_value_notify(move |row| {
            let val = row.value().clamp(min_fan, max_fan) as i32;
            set_fan_speed(ac, val);
            let readback = get_fan_speed(ac).unwrap_or(0);
            let is_auto = readback == 0;
            auto_clone.set_active(is_auto);
            row.set_sensitive(!is_auto);
            if !is_auto { row.set_value(readback as f64); }
        });

        group.add(&auto_row);
        group.add(&spin_row);
        page.add(&group);
    }

    // Brightness section
    {
        let group = adw::PreferencesGroup::builder().title("Keyboard Brightness").build();
        let brightness = get_brightness(ac).unwrap_or(50);

        let spin_row = adw::SpinRow::with_range(0.0, 100.0, 1.0);
        spin_row.set_title("Brightness");
        spin_row.set_subtitle("Keyboard backlight intensity");
        spin_row.set_value(brightness as f64);

        spin_row.connect_value_notify(move |row| {
            let val = row.value().clamp(0.0, 100.0) as u8;
            set_brightness(ac, val);
            let readback = get_brightness(ac).unwrap_or(val);
            row.set_value(readback as f64);
        });

        group.add(&spin_row);
        page.add(&group);
    }

    // CPU Power Limits (PDL1/PDL2) — AC page only
    if ac {
        if let Some((pl1, pl2, pl1_max)) = get_power_limits() {
            let tdp_base = if pl1_max > 0 { pl1_max } else { 55 };
            let max_pl = (tdp_base * 4).max(pl1.max(pl2) + 20); // generous upper bound

            let group = adw::PreferencesGroup::builder()
                .title("CPU Power Limits (RAPL)")
                .description("Intel PL1 (sustained) and PL2 (boost) — requires root daemon")
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
                .build();
            apply_btn.set_size_request(200, -1);

            let pl1_ref = pl1_row.clone();
            let pl2_ref = pl2_row.clone();
            apply_btn.connect_clicked(move |_| {
                let p1 = pl1_ref.value() as u32;
                let p2 = pl2_ref.value() as u32;
                set_power_limits(p1, p2);
                if let Some((r1, r2, _)) = get_power_limits() {
                    pl1_ref.set_value(r1 as f64);
                    pl2_ref.set_value(r2 as f64);
                }
            });

            group.add(&pl1_row);
            group.add(&pl2_row);
            group.add(&apply_btn);
            page.add(&group);
        }
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
        "Static", "Static Gradient", "Wave Gradient", "Breathing",
        "Breathing Dual", "Spectrum Cycle", "Rainbow Wave", "Starlight", "Ripple",
        "Wheel",
    ];

    let effect_row = adw::ComboRow::builder()
        .title("Effect")
        .subtitle("Keyboard lighting effect")
        .build();
    effect_row.set_model(Some(&gtk::StringList::new(&effect_names)));
    effect_row.set_selected(0);

    let desc_label = gtk::Label::builder()
        .label("Set a single color across all keys")
        .halign(gtk::Align::Start)
        .css_classes(["effect-description"])
        .build();

    // Color wheels
    let wheel1 = ColorWheel::new(160);
    let wheel2 = ColorWheel::new(160);
    wheel2.set_rgb(0, 128, 255);

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
    speed_row.set_value(5.0);

    let direction_row = adw::ComboRow::builder()
        .title("Direction")
        .subtitle("Wave direction")
        .build();
    direction_row.set_model(Some(&gtk::StringList::new(&["Left → Right", "Right → Left"])));
    direction_row.set_selected(0);

    let density_row = adw::SpinRow::with_range(1.0, 20.0, 1.0);
    density_row.set_title("Density");
    density_row.set_subtitle("Star density");
    density_row.set_value(10.0);

    let duration_row = adw::SpinRow::with_range(1.0, 20.0, 1.0);
    duration_row.set_title("Duration");
    duration_row.set_subtitle("Breath cycle length");
    duration_row.set_value(10.0);

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

    update_visibility(0);

    effect_row.connect_selected_notify({
        let update = update_visibility.clone();
        move |row| { update(row.selected()); }
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
            0 => { set_effect("static", vec![r1, g1, b1]); }
            1 => { set_effect("static_gradient", vec![r1, g1, b1, r2, g2, b2]); }
            2 => { set_effect("wave_gradient", vec![r1, g1, b1, r2, g2, b2]); }
            3 => { set_effect("breathing_single", vec![r1, g1, b1, duration]); }
            4 => { set_effect("breathing_dual", vec![r1, g1, b1, r2, g2, b2, duration]); }
            5 => { set_effect("spectrum_cycle", vec![speed]); }
            6 => { set_effect("rainbow_wave", vec![speed, dir]); }
            7 => { set_effect("starlight", vec![r1, g1, b1, density]); }
            8 => { set_effect("ripple", vec![r1, g1, b1, speed]); }
            9 => { set_effect("wheel", vec![speed, dir]); }
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

    scroll.set_child(Some(&page));
    scroll
}

fn build_about_page(device: &SupportedDevice) -> gtk::ScrolledWindow {
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .build();

    let page = adw::PreferencesPage::new();

    // System Information
    {
        let group = adw::PreferencesGroup::builder().title("System Information").build();

        let add_info = |title: &str, value: &str| {
            let row = adw::ActionRow::builder()
                .title(title)
                .build();
            row.add_suffix(&gtk::Label::builder().label(value).css_classes(["monospace"]).build());
            group.add(&row);
        };

        add_info("Device", &device.name);

        let dmi_product = std::fs::read_to_string("/sys/devices/virtual/dmi/id/product_name")
            .unwrap_or_default().trim().to_string();
        let dmi_vendor = std::fs::read_to_string("/sys/devices/virtual/dmi/id/sys_vendor")
            .unwrap_or_default().trim().to_string();
        let dmi_bios = std::fs::read_to_string("/sys/devices/virtual/dmi/id/bios_version")
            .unwrap_or_default().trim().to_string();

        if !dmi_product.is_empty() {
            let host = if dmi_vendor.is_empty() { dmi_product } else { format!("{dmi_vendor} {dmi_product}") };
            add_info("Host", &host);
        }
        add_info("USB ID", &format!("{}:{}", device.vid, device.pid));
        add_info("Features", &device.features.join(", "));
        if !dmi_bios.is_empty() { add_info("BIOS", &dmi_bios); }
        add_info("Fan Range", &format!("{} – {} RPM",
            device.fan.first().unwrap_or(&0), device.fan.get(1).unwrap_or(&0)));

        page.add(&group);
    }

    // GPU section
    if let Some(comms::DaemonResponse::GetGpuStatus {
        name, temp_c, gpu_util, mem_util, power_w, power_limit_w,
        mem_used_mb, mem_total_mb, clock_gpu_mhz, clock_mem_mhz,
    }) = send_data(comms::DaemonCommand::GetGpuStatus) {
        let group = adw::PreferencesGroup::builder().title("NVIDIA GPU").build();

        let gpu_row = adw::ActionRow::builder().title("GPU").build();
        gpu_row.add_suffix(&gtk::Label::builder().label(&name).css_classes(["monospace"]).build());
        group.add(&gpu_row);

        let temp_label = gtk::Label::builder().label(&format!("{temp_c}°C")).css_classes(["monospace"]).build();
        let usage_label = gtk::Label::builder().label(&format!("{gpu_util}%")).css_classes(["monospace"]).build();
        let vram_label = gtk::Label::builder().label(&format!("{mem_used_mb}/{mem_total_mb} MiB ({mem_util}%)")).css_classes(["monospace"]).build();
        let power_label = gtk::Label::builder().label(&format!("{power_w:.1} W")).css_classes(["monospace"]).build();
        let tgp_label = gtk::Label::builder().label(&format!("{power_limit_w:.0} W (default)")).css_classes(["monospace"]).build();
        let clock_label = gtk::Label::builder().label(&format!("GPU {clock_gpu_mhz} / Mem {clock_mem_mhz} MHz")).css_classes(["monospace"]).build();

        let make_row = |title: &str, suffix: &gtk::Label| {
            let row = adw::ActionRow::builder().title(title).build();
            row.add_suffix(suffix);
            group.add(&row);
        };

        make_row("Temperature", &temp_label);
        make_row("GPU Usage", &usage_label);
        make_row("VRAM", &vram_label);
        make_row("Power Draw", &power_label);
        make_row("TGP Limit", &tgp_label);
        make_row("Clocks", &clock_label);

        page.add(&group);

        // ── Timeline chart ────────────────────────────────────────────
        const CHART_HISTORY: usize = 20; // 20 samples × 3s = 60s window

        #[derive(Clone, Copy, Default)]
        struct Sample {
            temp_c: f64,
            gpu_pct: f64,
            power_w: f64,
        }

        let history: Rc<RefCell<VecDeque<Sample>>> = Rc::new(RefCell::new(VecDeque::with_capacity(CHART_HISTORY + 1)));
        let tgp_limit: Rc<RefCell<f64>> = Rc::new(RefCell::new(power_limit_w as f64));
        {
            let mut h = history.borrow_mut();
            h.push_back(Sample { temp_c: temp_c as f64, gpu_pct: gpu_util as f64, power_w: power_w as f64 });
        }

        let chart_group = adw::PreferencesGroup::builder()
            .title("Performance Timeline")
            .description("Temperature / GPU usage / Power — last 60 s")
            .build();

        let chart = gtk::DrawingArea::new();
        chart.set_size_request(-1, 180);
        chart.set_margin_start(12);
        chart.set_margin_end(12);
        chart.set_margin_bottom(8);

        let hist_draw = history.clone();
        let tgp_draw = tgp_limit.clone();
        chart.set_draw_func(move |_da, cr, w, h| {
            let w = w as f64;
            let h = h as f64;
            let hist = hist_draw.borrow();
            let n = hist.len();
            if n < 2 { return; }

            let pad_l = 40.0;
            let pad_r = 72.0; // wider: % + W dual axis
            let pad_t = 10.0;
            let pad_b = 20.0;
            let cw = w - pad_l - pad_r;
            let ch = h - pad_t - pad_b;

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
            cr.set_font_size(9.0);
            for i in 0..=4 {
                let val = 100 - i * 25;  // 100, 75, 50, 25, 0
                let watts = val * 2;     // 200, 150, 100, 50, 0
                let y = pad_t + ch * (i as f64 / 4.0) + 3.0;
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
                cr.move_to(w - pad_r + 32.0, y);
                let _ = cr.show_text(&format!("{watts}W"));
            }

            let x_step = cw / (CHART_HISTORY.max(2) - 1) as f64;

            // Draw line helper
            let draw_line = |data: &dyn Fn(usize) -> f64, r: f64, g: f64, b: f64, max_val: f64| {
                cr.set_source_rgba(r, g, b, 0.9);
                cr.set_line_width(2.0);
                let start_idx = if n > CHART_HISTORY { n - CHART_HISTORY } else { 0 };
                let points: Vec<(f64, f64)> = (start_idx..n).enumerate().map(|(i, idx)| {
                    let x = pad_l + i as f64 * x_step;
                    let val = data(idx).clamp(0.0, max_val);
                    let y = pad_t + ch * (1.0 - val / max_val);
                    (x, y)
                }).collect();
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
            // GPU % (green)
            draw_line(&|i| hist_ref[i].gpu_pct, 0.27, 1.0, 0.63, 100.0);
            // Power (cyan, scaled 0-200W → 0-100)
            draw_line(&|i| hist_ref[i].power_w * (100.0 / 200.0), 0.3, 0.8, 1.0, 100.0);

            // TGP limit — dashed horizontal reference line
            let tgp = *tgp_draw.borrow();
            if tgp > 0.0 {
                let tgp_y = pad_t + ch * (1.0 - (tgp * (100.0 / 200.0)) / 100.0);
                cr.set_source_rgba(0.3, 0.8, 1.0, 0.35);
                cr.set_line_width(1.5);
                cr.set_dash(&[5.0, 4.0], 0.0);
                cr.move_to(pad_l, tgp_y);
                cr.line_to(pad_l + cw, tgp_y);
                let _ = cr.stroke();
                cr.set_dash(&[], 0.0);
                cr.set_font_size(8.0);
                cr.set_source_rgba(0.3, 0.8, 1.0, 0.7);
                cr.move_to(pad_l + 2.0, tgp_y - 2.0);
                let _ = cr.show_text(&format!("TGP {tgp:.0}W"));
            }

            // Legend
            cr.set_font_size(10.0);
            let legend = [("Temp", 1.0_f64, 0.6, 0.2), ("GPU%", 0.27, 1.0, 0.63), ("W draw", 0.3, 0.8, 1.0)];
            let mut lx = pad_l + 4.0;
            for (label, r, g, b) in &legend {
                cr.set_source_rgb(*r, *g, *b);
                cr.rectangle(lx, h - 12.0, 8.0, 8.0);
                let _ = cr.fill();
                cr.move_to(lx + 11.0, h - 4.5);
                let _ = cr.show_text(label);
                lx += 58.0;
            }
        });

        chart_group.add(&chart);
        page.add(&chart_group);

        // Poll every 3 seconds — update labels + chart
        let hist_poll = history;
        let tgp_poll = tgp_limit;
        let chart_ref = chart;
        glib::timeout_add_seconds_local(3, glib::clone!(
            #[weak] temp_label,
            #[weak] usage_label,
            #[weak] vram_label,
            #[weak] power_label,
            #[weak] tgp_label,
            #[weak] clock_label,
            #[weak] chart_ref,
            #[upgrade_or] glib::ControlFlow::Break,
            move || {
                if let Some(comms::DaemonResponse::GetGpuStatus {
                    temp_c, gpu_util, mem_util, power_w, power_limit_w,
                    mem_used_mb, mem_total_mb, clock_gpu_mhz, clock_mem_mhz, ..
                }) = send_data(comms::DaemonCommand::GetGpuStatus) {
                    temp_label.set_text(&format!("{temp_c}°C"));
                    usage_label.set_text(&format!("{gpu_util}%"));
                    vram_label.set_text(&format!("{mem_used_mb}/{mem_total_mb} MiB ({mem_util}%)"));
                    power_label.set_text(&format!("{power_w:.1} W"));
                    tgp_label.set_text(&format!("{power_limit_w:.0} W (default)"));
                    clock_label.set_text(&format!("GPU {clock_gpu_mhz} / Mem {clock_mem_mhz} MHz"));

                    *tgp_poll.borrow_mut() = power_limit_w as f64;
                    let mut h = hist_poll.borrow_mut();
                    h.push_back(Sample { temp_c: temp_c as f64, gpu_pct: gpu_util as f64, power_w: power_w as f64 });
                    while h.len() > CHART_HISTORY { h.pop_front(); }
                    drop(h);
                    chart_ref.queue_draw();
                }
                glib::ControlFlow::Continue
            }
        ));
    }

    scroll.set_child(Some(&page));
    scroll
}

// ── Entry point ───────────────────────────────────────────────────────────

fn main() {
    setup_panic_hook();

    let device_file = std::fs::read_to_string(service::DEVICE_FILE)
        .or_crash("Failed to read the device file");
    let devices: Vec<SupportedDevice> = serde_json::from_str(&device_file)
        .or_crash("Failed to parse the device file");
    let device_name = get_device_name()
        .or_crash("Failed to get device name");
    let device = devices.into_iter().find(|d| d.name == device_name)
        .or_crash("Failed to find device config");

    let app = RelmApp::new("io.github.razer-linux.razer-blade-control");

    // Load minimal CSS overrides
    relm4::set_global_css(include_str!("style.css"));

    // Force dark color scheme
    let style_manager = adw::StyleManager::default();
    style_manager.set_color_scheme(adw::ColorScheme::ForceDark);

    // Spawn tray icon
    {
        let (tray_sender, tray_receiver) = std::sync::mpsc::channel::<()>();

        // Poll the tray receiver periodically to present the window
        glib::timeout_add_local(std::time::Duration::from_millis(200), move || {
            if tray_receiver.try_recv().is_ok() {
                if let Some(app) = gtk::gio::Application::default() {
                    app.activate();
                }
            }
            glib::ControlFlow::Continue
        });

        std::thread::spawn(move || {
            use ksni::blocking::TrayMethods;
            let t = tray::RazerTray { show_window_sender: tray_sender };
            match t.spawn() {
                Ok(handle) => std::mem::forget(handle),
                Err(e) => eprintln!("Warning: tray icon failed: {e}"),
            }
        });
    }

    app.run::<App>(device);
}
