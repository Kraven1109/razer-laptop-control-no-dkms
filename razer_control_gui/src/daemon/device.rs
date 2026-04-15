use crate::battery;
use crate::config;
use dbus::blocking::Connection;
use hidapi::HidApi;
use log::*;
use serde::{Deserialize, Serialize};
use serde_big_array::BigArray;
use service::SupportedDevice;
use std::{fs, io, thread, time};

const RAZER_VENDOR_ID: u16 = 0x1532;
const REPORT_SIZE: usize = 91;

#[derive(Serialize, Deserialize, Debug)]
pub struct RazerPacket {
    report: u8,
    status: u8,
    id: u8,
    remaining_packets: u16,
    protocol_type: u8,
    data_size: u8,
    command_class: u8,
    command_id: u8,
    #[serde(with = "BigArray")]
    args: [u8; 80],
    crc: u8,
    reserved: u8,
}

impl RazerPacket {
    // Command status
    const RAZER_CMD_NEW: u8 = 0x00;
    // const RAZER_CMD_BUSY:u8 = 0x01;
    const RAZER_CMD_SUCCESSFUL: u8 = 0x02;
    // const RAZER_CMD_FAILURE:u8 = 0x03;
    // const RAZER_CMD_TIMEOUT:u8 =0x04;
    const RAZER_CMD_NOT_SUPPORTED: u8 = 0x05;

    fn new(command_class: u8, command_id: u8, data_size: u8) -> RazerPacket {
        return RazerPacket {
            report: 0x00,
            status: RazerPacket::RAZER_CMD_NEW,
            // OpenRazer uses 0xFF for all Blade laptop models (transaction_id)
            id: 0xFF,
            remaining_packets: 0x0000,
            protocol_type: 0x00,
            data_size,
            command_class,
            command_id,
            args: [0x00; 80],
            crc: 0x00,
            reserved: 0x00,
        };
    }

    /// Calculates the checksum and returns the serialized payload with the CRC embedded.
    /// Bug fix: re-serializes after computing self.crc so the wire bytes include the CRC.
    fn calc_crc_and_serialize(&mut self) -> Vec<u8> {
        let mut res: u8 = 0x00;
        let buf: Vec<u8> = bincode::serialize(self).unwrap();
        for i in 2..88 {
            res ^= buf[i];
        }
        self.crc = res;
        // Re-serialize to embed the newly calculated CRC byte into the final payload.
        bincode::serialize(self).unwrap()
    }
}

const DEVICE_FILE: &str = "/usr/share/razercontrol/laptops.json";
pub struct DeviceManager {
    pub device: Option<RazerLaptop>,
    supported_devices: Vec<SupportedDevice>,
    pub config: Option<config::Configuration>,
    /// Session-only fan overrides (RPM per AC state, not persisted to config).
    /// 0 = auto mode. Reset to session defaults on startup via reset_fan_profiles_to_auto().
    fan_overrides: [i32; 2],
    /// Tracks whether the screensaver/display-blank is active. Survives HID device re-opens
    /// so that stale-recovery in the animator thread can restore brightness correctly.
    pub screensaver_active: bool,
}

impl DeviceManager {
    pub fn new() -> DeviceManager {
        return DeviceManager {
            device: None,
            supported_devices: vec![],
            config: None,
            fan_overrides: [0, 0],
            screensaver_active: false,
        };
    }

    pub fn set_sync(&mut self, sync: bool) -> bool {
        let mut ac: usize = 0;
        if let Some(laptop) = self.get_device() {
            ac = laptop.ac_state as usize;
        }
        let other = (ac + 1) & 0x01;
        if let Some(config) = self.get_config() {
            config.sync = sync;
            config.power[other].brightness = config.power[ac].brightness;
            config.power[other].logo_state = config.power[ac].logo_state;
            config.power[other].screensaver = config.power[ac].screensaver;
            config.power[other].idle = config.power[ac].idle;
            if let Err(e) = config.write_to_file() {
                error!("Config write error: {:?}", e);
            }
        }

        return true;
    }

    pub fn get_sync(&mut self) -> bool {
        if let Some(config) = self.get_config() {
            return config.sync;
        }

        return false;
    }

    pub fn read_laptops_file() -> io::Result<DeviceManager> {
        let str: Vec<u8> = fs::read(DEVICE_FILE)?;
        let mut res: DeviceManager = DeviceManager::new();
        res.supported_devices = serde_json::from_slice(str.as_slice())?;
        info!("Supported devices loaded: {}", res.supported_devices.len());
        match config::Configuration::read_from_config() {
            Ok(mut c) => {
                if c.reset_fan_profiles_to_auto() {
                    let _ = c.write_to_file();
                }
                res.config = Some(c);
            }
            Err(_) => res.config = Some(config::Configuration::new()),
        }

        Ok(res)
    }

    pub fn get_ac_config(&mut self, ac: usize) -> Option<config::PowerConfig> {
        if let Some(c) = self.get_config() {
            return Some(c.power[ac]);
        }

        return None;
    }

    pub fn light_off(&mut self) {
        self.screensaver_active = true;
        if let Some(laptop) = self.get_device() {
            laptop.set_screensaver(true);
            laptop.set_brightness(0);
            laptop.set_logo_led_state(0);
        }
    }

    pub fn restore_light(&mut self) {
        self.screensaver_active = false;
        // If the HID device was lost (stale disconnect) while the screen was
        // blanked, attempt to re-open it now before writing brightness.
        if self.device.is_none() {
            self.discover_devices();
        }
        let mut brightness = 0;
        let mut logo_state = 0;
        let mut ac: usize = 0;
        if let Some(laptop) = self.get_device() {
            ac = laptop.get_ac_state();
        }
        if let Some(config) = self.get_ac_config(ac) {
            brightness = config.brightness;
            logo_state = config.logo_state;
        }
        if let Some(laptop) = self.get_device() {
            laptop.set_screensaver(false);
            laptop.set_brightness(brightness);
            laptop.set_logo_led_state(logo_state);
        }
    }

    pub fn restore_standard_effect(&mut self) {
        let mut effect = 0;
        let mut params: Vec<u8> = vec![];
        if let Some(config) = self.get_config() {
            effect = config.standard_effect;
            params = config.standard_effect_params.clone();
        }
        if let Some(laptop) = self.get_device() {
            laptop.set_standard_effect(effect, params);
        }
    }

    pub fn change_idle(&mut self, ac: usize, timeout: u32) -> bool {
        // let mut arm: bool = false;
        if let Some(config) = self.get_config() {
            if config.power[ac].idle != timeout {
                config.power[ac].idle = timeout;
                if config.sync {
                    let other = (ac + 1) & 0x01;
                    config.power[other].idle = timeout;
                }
                if let Err(e) = config.write_to_file() {
                    error!("Config write error: {:?}", e);
                }
                // arm = true;
            }
        }

        return true;
    }

    pub fn set_power_mode(&mut self, ac: usize, pwr: u8, cpu: u8, gpu: u8) -> bool {
        // Validate power mode (0=Balanced, 1=Gaming, 2=Creator, 3=Silent, 4=Custom)
        if pwr > 4 {
            return false;
        }
        // Validate boost values
        if cpu > 3 || gpu > 2 {
            return false;
        }

        let mut res: bool = false;
        if let Some(config) = self.get_config() {
            config.power[ac].power_mode = pwr;
            config.power[ac].cpu_boost = cpu;
            config.power[ac].gpu_boost = gpu;
            if let Err(e) = config.write_to_file() {
                error!("Config write error: {:?}", e);
            }
        }
        if let Some(laptop) = self.get_device() {
            let state = laptop.get_ac_state();
            if state != ac {
                res = true;
            } else {
                res = laptop.set_power_mode(pwr, cpu, gpu);
            }
        }

        return res;
    }

    pub fn set_standard_effect(&mut self, effect_id: u8, params: Vec<u8>) -> bool {
        if let Some(config) = self.get_config() {
            config.standard_effect = effect_id;
            config.standard_effect_params = params.clone();
            if let Err(e) = config.write_to_file() {
                error!("Config write error: {:?}", e);
            }
        }
        if let Some(laptop) = self.get_device() {
            laptop.set_standard_effect(effect_id, params);
        }

        return true;
    }

    pub fn set_fan_rpm(&mut self, ac: usize, rpm: i32) -> bool {
        let rpm = rpm.max(0);
        if let Some(config) = self.get_config() {
            if config.power[ac.min(1)].temp_target_c != 0 {
                config.power[ac.min(1)].temp_target_c = 0;
                if let Err(error) = config.write_to_file() {
                    error!("Config write error (fan temp target clear): {:?}", error);
                }
            }
        }
        // Store session-only override — do NOT persist to config so a crash
        // cannot leave the fan stuck at a manual RPM after restart.
        self.fan_overrides[ac.min(1)] = rpm;
        if let Some(laptop) = self.get_device() {
            let state = laptop.get_ac_state();
            if state != ac {
                return true; // saved for when AC state switches
            } else {
                return laptop.set_fan_rpm(rpm as u16);
            }
        }
        return true;
    }

    pub fn apply_runtime_fan_rpm(&mut self, ac: usize, rpm: i32) -> bool {
        let rpm = rpm.max(0);
        self.fan_overrides[ac.min(1)] = rpm;
        if let Some(laptop) = self.get_device() {
            let state = laptop.get_ac_state();
            if state != ac {
                return true;
            }
            return laptop.set_fan_rpm(rpm as u16);
        }
        true
    }

    pub fn set_logo_led_state(&mut self, ac: usize, logo_state: u8) -> bool {
        let mut res: bool = false;
        if let Some(config) = self.get_config() {
            config.power[ac].logo_state = logo_state;
            if config.sync {
                let other = (ac + 1) & 0x01;
                config.power[other].logo_state = logo_state;
            }
            if let Err(e) = config.write_to_file() {
                error!("Config write error: {:?}", e);
            }
        }

        if let Some(laptop) = self.get_device() {
            let state = laptop.get_ac_state();

            if state != ac {
                res = true;
            } else {
                res = laptop.set_logo_led_state(logo_state);
            }
        }

        return res;
    }

    pub fn get_logo_led_state(&mut self, ac: usize) -> u8 {
        if let Some(config) = self.get_ac_config(ac) {
            return config.logo_state;
        }

        return 0;
    }

    pub fn set_brightness(&mut self, ac: usize, brightness: u8) -> bool {
        let mut res: bool = false;
        let _val = brightness as u16 * 255 / 100;
        if let Some(config) = self.get_config() {
            config.power[ac].brightness = _val as u8;
            if config.sync {
                let other = (ac + 1) & 0x01;
                config.power[other].brightness = _val as u8;
            }
            if let Err(e) = config.write_to_file() {
                error!("Config write error: {:?}", e);
            }
        }

        if let Some(laptop) = self.get_device() {
            let state = laptop.get_ac_state();
            if state != ac {
                res = true;
            } else {
                res = laptop.set_brightness(_val as u8);
            }
        }

        return res;
    }

    pub fn get_brightness(&mut self, ac: usize) -> u8 {
        if let Some(laptop) = self.get_device() {
            if laptop.ac_state as usize == ac {
                let val = laptop.get_brightness() as u32;
                let mut perc = val * 100 * 100 / 255;
                perc += 50;
                perc /= 100;
                return perc as u8;
            }
        }

        if let Some(config) = self.get_ac_config(ac) {
            let val = config.brightness as u32;
            let mut perc = val * 100 * 100 / 255;
            perc += 50;
            perc /= 100;
            return perc as u8;
        }

        return 0;
    }

    pub fn get_fan_rpm(&mut self, ac: usize) -> i32 {
        // Return the user-configured manual target (0 = auto mode).
        self.fan_overrides[ac.min(1)]
    }

    /// Returns the live measured fan RPM from the EC tachometer.
    /// Falls back to 0 if no device is available.
    pub fn get_fan_tachometer(&mut self) -> i32 {
        if let Some(laptop) = self.get_device() {
            return laptop.get_fan_tachometer() as i32;
        }
        0
    }

    pub fn get_power_mode(&mut self, ac: usize) -> u8 {
        if let Some(laptop) = self.get_device() {
            if laptop.ac_state as usize == ac {
                return laptop.get_power_mode(0x01);
            }
        }

        if let Some(config) = self.get_ac_config(ac) {
            return config.power_mode;
        }

        return 0;
    }

    pub fn get_cpu_boost(&mut self, ac: usize) -> u8 {
        if let Some(laptop) = self.get_device() {
            if laptop.ac_state as usize == ac {
                return laptop.get_cpu_boost();
            }
        }

        if let Some(config) = self.get_ac_config(ac) {
            return config.cpu_boost;
        }

        return 0;
    }

    pub fn get_gpu_boost(&mut self, ac: usize) -> u8 {
        if let Some(laptop) = self.get_device() {
            if laptop.ac_state as usize == ac {
                return laptop.get_gpu_boost();
            }
        }

        if let Some(config) = self.get_ac_config(ac) {
            return config.gpu_boost;
        }

        return 0;
    }

    pub fn set_ac_state(&mut self, ac: bool) {
        let ac_idx = ac as usize;
        let override_rpm = self.fan_overrides[ac_idx.min(1)];
        if let Some(laptop) = self.get_device() {
            laptop.set_ac_state(ac);
        }
        let config: Option<config::PowerConfig> = self.get_ac_config(ac as usize);
        if let Some(config) = config {
            if let Some(laptop) = self.get_device() {
                laptop.set_config(config);
                // Re-apply session-only fan override (set_config restores the saved 0-RPM).
                if override_rpm > 0 {
                    laptop.set_fan_rpm(override_rpm as u16);
                }
            }
        }
    }

    pub fn set_ac_state_get(&mut self) {
        let dbus_system = Connection::new_system().expect("failed to connect to D-Bus system bus");
        let proxy_ac = dbus_system.with_proxy(
            "org.freedesktop.UPower",
            "/org/freedesktop/UPower/devices/line_power_AC0",
            time::Duration::from_millis(5000),
        );
        use battery::OrgFreedesktopUPowerDevice;
        if let Ok(online) = proxy_ac.online() {
            if let Some(laptop) = self.get_device() {
                laptop.set_ac_state(online);
            }
            let config: Option<config::PowerConfig> = self.get_ac_config(online as usize);
            if let Some(config) = config {
                if let Some(laptop) = self.get_device() {
                    laptop.set_config(config);
                }
            }
        }
    }

    pub fn get_device(&mut self) -> Option<&mut RazerLaptop> {
        return self.device.as_mut();
    }

    pub fn set_bho_handler(&mut self, is_on: bool, threshold: u8) -> bool {
        return self
            .get_device()
            .map_or(false, |laptop| laptop.set_bho(is_on, threshold));
    }

    pub fn get_bho_handler(&mut self) -> Option<(bool, u8)> {
        return self
            .get_device()
            .and_then(|laptop| laptop.get_bho().map(|result| byte_to_bho(result)));
    }

    pub fn set_low_battery_lighting_threshold(&mut self, threshold_pct: f64) -> bool {
        let threshold_pct = threshold_pct.clamp(0.0, 100.0);
        if let Some(config) = self.get_config() {
            config.no_light = threshold_pct;
            if let Err(error) = config.write_to_file() {
                error!("Config write error (low battery lighting): {:?}", error);
                return false;
            }
            return true;
        }
        false
    }

    pub fn set_temp_target(&mut self, ac: usize, temp_c: i32) -> bool {
        let ac = ac.min(1);
        let temp_c = if temp_c <= 0 { 0 } else { temp_c.clamp(60, 95) };

        if let Some(config) = self.get_config() {
            config.power[ac].temp_target_c = temp_c;
            if let Err(error) = config.write_to_file() {
                error!("Config write error (fan temp target): {:?}", error);
                return false;
            }
        }

        self.fan_overrides[ac] = 0;

        if let Some(laptop) = self.get_device() {
            if laptop.get_ac_state() == ac {
                return laptop.set_fan_rpm(0);
            }
        }

        true
    }

    pub fn get_temp_target(&mut self, ac: usize) -> i32 {
        self.get_ac_config(ac.min(1))
            .map(|config| config.temp_target_c.max(0))
            .unwrap_or(0)
    }

    pub fn get_fan_range(&mut self) -> (i32, i32) {
        if let Some(laptop) = self.get_device() {
            if laptop.fan.len() >= 2 {
                return (laptop.fan[0] as i32, laptop.fan[1] as i32);
            }
        }
        (3500, 5000)
    }

    pub fn get_low_battery_lighting_threshold(&mut self) -> f64 {
        self.get_config()
            .map(|config| config.no_light)
            .unwrap_or(0.0)
    }

    fn get_config(&mut self) -> Option<&mut config::Configuration> {
        return self.config.as_mut();
    }

    pub fn get_rapl_limits(&mut self, ac: usize) -> (u32, u32) {
        if let Some(cfg) = self.get_ac_config(ac) {
            return (cfg.rapl_pl1_watts, cfg.rapl_pl2_watts);
        }
        (0, 0)
    }

    pub fn set_rapl_limits(&mut self, ac: usize, pl1_watts: u32, pl2_watts: u32) -> bool {
        if let Some(config) = self.get_config() {
            config.power[ac].rapl_pl1_watts = pl1_watts;
            config.power[ac].rapl_pl2_watts = pl2_watts;
            if let Err(e) = config.write_to_file() {
                error!("Config write error (RAPL): {:?}", e);
                return false;
            }
        }
        true
    }

    pub fn find_supported_device(&mut self, vid: u16, pid: u16) -> Option<&SupportedDevice> {
        for device in &self.supported_devices {
            // Unwrap: we control the strings and know they are are valid
            let svid = u16::from_str_radix(&device.vid, 16).unwrap();
            let spid = u16::from_str_radix(&device.pid, 16).unwrap();

            if svid == vid && spid == pid {
                return Some(device);
            }
        }

        None
    }

    pub fn discover_devices(&mut self) {
        // Always create a fresh HidApi so post-resume USB re-enumeration is picked up.
        // The lazy_static cache was intentionally removed: it held stale device paths
        // from before sleep, causing discover_devices() to silently fail on every resume.
        let api = match HidApi::new() {
            Ok(a) => a,
            Err(e) => { error!("HidApi init error: {}", e); return; }
        };

        let mut devices: Vec<_> = api
            .device_list()
            .filter(|d| d.vendor_id() == RAZER_VENDOR_ID)
            .collect();

        // Log every candidate so resume failures are diagnosable via journalctl.
        for d in &devices {
            info!(
                "Razer HID candidate: PID=0x{:04X} IF={} usage_page=0x{:04X} usage=0x{:04X} path={}",
                d.product_id(),
                d.interface_number(),
                d.usage_page(),
                d.usage(),
                d.path().to_string_lossy()
            );
        }

        // Sort priority:
        //   0 — vendor-specific usage page (>=0xFF00): the Razer EC control interface.
        //       On Linux with hidraw + kernel ≥5.13, hidapi parses the HID report
        //       descriptor so usage_page is accurate. Windows also uses this filter.
        //   1 — interface_number == 1: known control interface on Blade 16 (PID 0x029F)
        //       and most other Razer laptops when usage_page isn't parsed (older kernels).
        //   3 — other mid-range interface numbers (fallback, descending).
        //   9 — interface_number == 0: this is always the boot-keyboard interface;
        //       it never has Feature report support on Razer Blade laptops.
        //
        // Previously we sorted by -interface_number, which put Interface 2 (mouse, page
        // 0x0001) before Interface 1 (EC control, page 0xFF00), so all feature-report
        // writes went to the mouse hidraw node and were silently discarded by the kernel.
        devices.sort_by_key(|d| {
            let up = d.usage_page();
            if up >= 0xFF00 {
                0i32
            } else if d.interface_number() == 1 {
                1i32
            } else if d.interface_number() == 0 {
                9i32
            } else {
                // Higher interface numbers first within this bucket.
                5i32 - (d.interface_number() as i32).min(4)
            }
        });

        for device in &devices {
            let result =
                self.find_supported_device(device.vendor_id(), device.product_id());
            if let Some(supported_device) = result {
                let name = supported_device.name.clone();
                let features = supported_device.features.clone();
                let fan = supported_device.fan.clone();
                match api.open_path(device.path()) {
                    Ok(dev) => {
                        info!(
                            "Opened HID device: {} (IF={} usage_page=0x{:04X})",
                            name,
                            device.interface_number(),
                            device.usage_page()
                        );
                        self.device = Some(RazerLaptop::new(name, features, fan, dev));
                        return;
                    }
                    Err(e) => {
                        warn!(
                            "Could not open IF={} (usage_page=0x{:04X}): {}",
                            device.interface_number(),
                            device.usage_page(),
                            e
                        );
                    }
                };
            }
        }
        error!("No supported Razer HID interface could be opened.\n\
                Make sure the hidraw device has the right permissions (udev rule installed).");
    }
}

pub struct RazerLaptop {
    name: String,
    features: Vec<String>,
    fan: Vec<u16>,
    device: hidapi::HidDevice,
    power: u8,    // need for fan
    fan_rpm: u8,  // need for power
    ac_state: u8, // index config array
    screensaver: bool,
    /// EC command used to read live fan RPM (probed once on first tachometer call).
    /// Some(0x88) = tachometer, Some(0x81) = set-point, Some(0) = unsupported.
    fan_read_cmd: Option<u8>,
    /// Tracks consecutive HID I/O failures. After 5 failures the device is
    /// considered stale (e.g. re-enumeration mid-session) and the daemon will
    /// drop it and call discover_devices() again.
    consecutive_failures: u32,
}
//
impl RazerLaptop {
    // LED STORAGE Options
    const NOSTORE: u8 = 0x00;
    const VARSTORE: u8 = 0x01;
    // LED definitions
    const LOGO_LED: u8 = 0x04;
    // effects
    pub const OFF: u8 = 0x00;
    pub const WAVE: u8 = 0x01;
    pub const REACTIVE: u8 = 0x02; // Afterglo
    pub const BREATHING: u8 = 0x03;
    pub const SPECTRUM: u8 = 0x04;
    pub const CUSTOMFRAME: u8 = 0x05;
    pub const STATIC: u8 = 0x06;
    pub const STARLIGHT: u8 = 0x19;

    pub fn new(
        name: String,
        features: Vec<String>,
        fan: Vec<u16>,
        device: hidapi::HidDevice,
    ) -> RazerLaptop {
        return RazerLaptop {
            name,
            features,
            fan,
            device,
            power: 0,
            fan_rpm: 0,
            ac_state: 0,
            screensaver: false,
            fan_read_cmd: None,
            consecutive_failures: 0,
        };
    }

    /// Returns true when accumulated HID failures suggest the device handle is stale
    /// (e.g. the hidraw node was replaced after USB re-enumeration mid-session).
    /// The daemon's animation thread calls this and re-opens the device if needed.
    pub fn is_stale(&self) -> bool {
        self.consecutive_failures >= 5
    }

    pub fn set_screensaver(&mut self, active: bool) {
        self.screensaver = active;
    }

    pub fn set_config(&mut self, config: config::PowerConfig) -> bool {
        let mut ret: bool = false;

        if !self.screensaver {
            ret |= self.set_brightness(config.brightness);
            ret |= self.set_logo_led_state(config.logo_state);
        } else {
            ret |= self.set_brightness(0);
            ret |= self.set_logo_led_state(0);
        }
        ret |= self.set_power_mode(config.power_mode, config.cpu_boost, config.gpu_boost);
        ret |= self.set_fan_rpm(config.fan_rpm as u16);

        return ret;
    }

    pub fn set_ac_state(&mut self, online: bool) -> usize {
        if online {
            self.ac_state = 1;
        } else {
            self.ac_state = 0;
        }

        return self.ac_state as usize;
    }

    pub fn get_ac_state(&mut self) -> usize {
        return self.ac_state as usize;
    }

    pub fn get_name(&self) -> &str {
        &self.name
    }

    pub fn have_feature(&self, fch: &str) -> bool {
        self.features.iter().any(|f| f == fch)
    }

    fn clamp_fan(&self, rpm: u16) -> u8 {
        if self.fan.len() < 2 {
            return (rpm / 100) as u8;
        }
        if rpm > self.fan[1] {
            return (self.fan[1] / 100) as u8;
        }
        if rpm < self.fan[0] {
            return (self.fan[0] / 100) as u8;
        }

        return (rpm / 100) as u8;
    }

    pub fn set_standard_effect(&mut self, effect_id: u8, params: Vec<u8>) -> bool {
        let mut report: RazerPacket = RazerPacket::new(0x03, 0x0a, 80);
        report.args[0] = effect_id; // effect id
                                    // take(79) prevents out-of-bounds write past args[80]
        for (idx, &p) in params.iter().take(79).enumerate() {
            report.args[idx + 1] = p;
        }
        if let Some(_) = self.send_report(report) {
            return true;
        }

        return false;
    }

    pub fn set_custom_frame_data(&mut self, row: u8, data: Vec<u8>) {
        // if data.len() == kbd::board::KEYS_PER_ROW {
        if data.len() == 45 {
            let mut report: RazerPacket = RazerPacket::new(0x03, 0x0b, 0x34);
            report.args[0] = 0xff;
            report.args[1] = row;
            report.args[2] = 0x00; // start col
            report.args[3] = 0x0f; // end col
            for idx in 0..data.len() {
                report.args[idx + 7] = data[idx];
            }
            self.send_report(report);
        }
    }

    pub fn set_custom_frame(&mut self) -> bool {
        let mut report: RazerPacket = RazerPacket::new(0x03, 0x0a, 0x02);
        report.args[0] = RazerLaptop::CUSTOMFRAME; // effect id
        report.args[1] = RazerLaptop::NOSTORE;
        if let Some(_) = self.send_report(report) {
            return true;
        }

        return false;
    }

    pub fn get_power_mode(&mut self, zone: u8) -> u8 {
        let mut report: RazerPacket = RazerPacket::new(0x0d, 0x82, 0x04);
        report.args[0] = 0x00;
        report.args[1] = zone;
        report.args[2] = 0x00;
        report.args[3] = 0x00;
        if let Some(response) = self.send_report(report) {
            return response.args[2];
        }
        return 0;
    }

    fn set_power(&mut self, zone: u8) -> bool {
        let mut report: RazerPacket = RazerPacket::new(0x0d, 0x02, 0x04);
        report.args[0] = 0x00;
        report.args[1] = zone;
        report.args[2] = self.power;
        match self.fan_rpm {
            0 => report.args[3] = 0x00,
            _ => report.args[3] = 0x01,
        }
        if let Some(_) = self.send_report(report) {
            return true;
        }

        return false;
    }

    pub fn get_cpu_boost(&mut self) -> u8 {
        let mut report: RazerPacket = RazerPacket::new(0x0d, 0x87, 0x03);
        report.args[0] = 0x00;
        report.args[1] = 0x01;
        report.args[2] = 0x00;
        if let Some(response) = self.send_report(report) {
            return response.args[2];
        }
        return 0;
    }

    fn set_cpu_boost(&mut self, mut boost: u8) -> bool {
        let mut report: RazerPacket = RazerPacket::new(0x0d, 0x07, 0x03);
        if boost == 3 && !self.have_feature("boost") {
            boost = 2;
        }
        report.args[0] = 0x00;
        report.args[1] = 0x01;
        report.args[2] = boost;
        if let Some(_) = self.send_report(report) {
            return true;
        }

        return false;
    }

    fn get_gpu_boost(&mut self) -> u8 {
        let mut report: RazerPacket = RazerPacket::new(0x0d, 0x87, 0x03);
        report.args[0] = 0x00;
        report.args[1] = 0x02;
        report.args[2] = 0x00;
        if let Some(response) = self.send_report(report) {
            return response.args[2];
        }
        return 0;
    }

    fn set_gpu_boost(&mut self, boost: u8) -> bool {
        let mut report: RazerPacket = RazerPacket::new(0x0d, 0x07, 0x03);
        report.args[0] = 0x00;
        report.args[1] = 0x02;
        report.args[2] = boost;
        if let Some(_) = self.send_report(report) {
            return true;
        }
        return false;
    }

    pub fn set_power_mode(&mut self, mode: u8, cpu_boost: u8, gpu_boost: u8) -> bool {
        // Validate inputs to prevent sending invalid values to firmware
        let mode = mode.min(4);
        let cpu_boost = if self.have_feature("boost") {
            cpu_boost.min(3)
        } else {
            cpu_boost.min(2)
        };
        let gpu_boost = gpu_boost.min(2);

        self.power = mode;

        if mode <= 3 {
            // Standard modes: set power for both CPU and GPU zones
            self.set_power(0x01);
            self.set_power(0x02);
        } else {
            // Custom mode (4): set power, then CPU/GPU boost independently
            self.fan_rpm = 0;
            self.set_power(0x01);
            self.set_cpu_boost(cpu_boost);
            self.set_power(0x02);
            self.set_gpu_boost(gpu_boost);
        }

        return true;
    }

    fn set_rpm(&mut self, zone: u8) -> bool {
        let mut report: RazerPacket = RazerPacket::new(0x0d, 0x01, 0x03);
        // Set fan RPM
        report.args[0] = 0x00;
        report.args[1] = zone;
        report.args[2] = self.fan_rpm;
        if let Some(_) = self.send_report(report) {
            return true;
        }

        return false;
    }

    pub fn set_fan_rpm(&mut self, value: u16) -> bool {
        match value == 0 {
            true => self.fan_rpm = 0,
            false => self.fan_rpm = self.clamp_fan(value),
        }

        if self.power == 4 {
            // Custom power mode: fan is managed by the firmware
            info!("Fan speed not adjustable in custom power mode");
            return true;
        }

        // Re-send current power mode with updated fan state for both zones
        self.set_power(0x01);
        if value != 0 {
            self.set_rpm(0x01);
        }
        self.set_power(0x02);
        if value != 0 {
            self.set_rpm(0x02);
        }

        return true;
    }

    pub fn get_fan_tachometer(&mut self) -> u16 {
        // Lazy probe: try 0x88 (tachometer) then 0x81 (set-point) once.
        if self.fan_read_cmd.is_none() {
            // Sentinel: Some(0) means "nothing works"
            self.fan_read_cmd = Some(0);
            for &cmd_id in &[0x88u8, 0x81u8] {
                let mut probe = RazerPacket::new(0x0d, cmd_id, 0x02);
                probe.args[0] = 0x00;
                probe.args[1] = 0x01;
                if self
                    .send_report(probe)
                    .map(|r| r.args[2] > 0)
                    .unwrap_or(false)
                {
                    let label = if cmd_id == 0x88 {
                        "tachometer"
                    } else {
                        "set-point"
                    };
                    info!(
                        "fan_rpm: EC 0x0D/0x{:02X} ({}) selected for {}",
                        cmd_id, label, self.name
                    );
                    self.fan_read_cmd = Some(cmd_id);
                    break;
                }
            }
            if self.fan_read_cmd == Some(0) {
                warn!(
                    "fan_rpm: no EC command supported on {} \u{2014} showing configured target",
                    self.name
                );
            }
        }

        match self.fan_read_cmd {
            Some(cmd_id) if cmd_id > 0 => {
                let mut report = RazerPacket::new(0x0d, cmd_id, 0x02);
                report.args[0] = 0x00;
                report.args[1] = 0x01;
                self.send_report(report)
                    .map(|r| r.args[2] as u16 * 100)
                    .filter(|&rpm| rpm > 0)
                    .unwrap_or(self.fan_rpm as u16 * 100)
            }
            _ => self.fan_rpm as u16 * 100,
        }
    }

    pub fn set_logo_led_state(&mut self, mode: u8) -> bool {
        if mode > 0 {
            let mut report: RazerPacket = RazerPacket::new(0x03, 0x02, 0x03);
            report.args[0] = RazerLaptop::VARSTORE;
            report.args[1] = RazerLaptop::LOGO_LED;
            if mode == 1 {
                report.args[2] = 0x00;
            } else if mode == 2 {
                report.args[2] = 0x02;
            }
            self.send_report(report);
        }

        let mut report: RazerPacket = RazerPacket::new(0x03, 0x00, 0x03);
        report.args[0] = RazerLaptop::VARSTORE;
        report.args[1] = RazerLaptop::LOGO_LED;
        report.args[2] = mode.clamp(0x00, 0x01);
        if let Some(_) = self.send_report(report) {
            return true;
        }

        return false;
    }

    pub fn set_brightness(&mut self, brightness: u8) -> bool {
        // Blade laptops use razer_chroma_misc_set_blade_brightness (0x0E, 0x04)
        let mut report: RazerPacket = RazerPacket::new(0x0E, 0x04, 0x02);
        report.args[0] = 0x01;
        report.args[1] = brightness;
        if let Some(_) = self.send_report(report) {
            return true;
        }

        return false;
    }

    pub fn get_brightness(&mut self) -> u8 {
        // Blade laptops use razer_chroma_misc_get_blade_brightness (0x0E, 0x84)
        let mut report: RazerPacket = RazerPacket::new(0x0E, 0x84, 0x02);
        report.args[0] = 0x01;
        if let Some(response) = self.send_report(report) {
            return response.args[1];
        }
        return 0;
    }

    pub fn get_bho(&mut self) -> Option<u8> {
        if !self.have_feature("bho") {
            return None;
        }

        let mut report: RazerPacket = RazerPacket::new(0x07, 0x92, 0x01);
        report.args[0] = 0x00;

        return self.send_report(report).map(|resp| resp.args[0]);
    }

    pub fn set_bho(&mut self, is_on: bool, threshold: u8) -> bool {
        if !self.have_feature("bho") {
            warn!("BHO not supported on this device");
            return false;
        }

        // Clamp threshold to safe range (50-80, must be multiple of 5)
        let threshold = threshold.clamp(50, 80);

        let mut report = RazerPacket::new(0x07, 0x12, 0x01);
        report.args[0] = bho_to_byte(is_on, threshold);

        return self.send_report(report).map_or(false, |r| {
            debug!("BHO response: {:?}", r);
            true
        });
    }

    /// Read the EC's feature-report response.
    fn read_response(&self, buf: &mut [u8; REPORT_SIZE]) -> Option<usize> {
        thread::sleep(time::Duration::from_millis(1));
        match self.device.get_feature_report(buf) {
            // Accept size >= REPORT_SIZE: on some Linux/hidraw configurations the
            // ioctl returns REPORT_SIZE+1 bytes (report ID prepended), which is
            // still a valid full response.
            Ok(size) if size >= REPORT_SIZE => Some(size),
            _ => None,
        }
    }

    fn send_report(&mut self, mut report: RazerPacket) -> Option<RazerPacket> {
        let mut temp_buf: [u8; REPORT_SIZE] = [0x00; REPORT_SIZE];

        // Serialize ONCE before the retry loop to avoid repeated allocations.
        let packet_payload = report.calc_crc_and_serialize();

        for attempt in 0..3 {
            match self.device.send_feature_report(&packet_payload) {
                Ok(_) => {
                    if self.read_response(&mut temp_buf).is_some() {
                        match bincode::deserialize::<RazerPacket>(&temp_buf) {
                            Ok(response) => {
                                // BHO status response has a different command_id
                                if response.command_id == 0x92 {
                                    self.consecutive_failures = 0;
                                    return Some(response);
                                }

                                if response.remaining_packets != report.remaining_packets
                                    || response.command_class != report.command_class
                                    || response.command_id != report.command_id
                                {
                                    warn!("HID response mismatch: expected class=0x{:02X} cmd=0x{:02X}, got class=0x{:02X} cmd=0x{:02X}",
                                                report.command_class, report.command_id,
                                                response.command_class, response.command_id);
                                } else if response.status == RazerPacket::RAZER_CMD_SUCCESSFUL {
                                    self.consecutive_failures = 0;
                                    return Some(response);
                                }
                                if response.status == RazerPacket::RAZER_CMD_NOT_SUPPORTED {
                                    debug!(
                                        "HID command not supported: class=0x{:02X} cmd=0x{:02X}",
                                        report.command_class, report.command_id
                                    );
                                    // "Not supported" is a valid EC reply — the interface is
                                    // working, just this particular command isn't available.
                                    self.consecutive_failures = 0;
                                    return None;
                                }
                            }
                            Err(e) => {
                                warn!("HID deserialize error (attempt {}): {}", attempt + 1, e);
                            }
                        }
                    } else {
                        warn!("HID read timeout (attempt {})", attempt + 1);
                    }
                }
                Err(e) => {
                    error!("HID write error (attempt {}): {}", attempt + 1, e);
                }
            };
            // Exponential backoff: 1ms, 2ms, 4ms
            thread::sleep(time::Duration::from_millis(1 << attempt));
        }

        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        error!(
            "HID command failed after 3 attempts (consecutive_failures={}): class=0x{:02X} cmd=0x{:02X}",
            self.consecutive_failures, report.command_class, report.command_id
        );
        return None;
    }
}

// top bit flags whether battery health optimization is on or off
// bottom bits are the actual threshold that it is set to
fn byte_to_bho(u: u8) -> (bool, u8) {
    return (u & (1 << 7) != 0, (u & 0b0111_1111));
}

fn bho_to_byte(is_on: bool, threshold: u8) -> u8 {
    if is_on {
        return threshold | 0b1000_0000;
    }
    return threshold;
}
