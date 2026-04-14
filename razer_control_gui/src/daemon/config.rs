use serde::{Deserialize, Serialize};
use std::io::prelude::*;
use std::{env, fs, fs::File, io};

const SETTINGS_FILE: &str = "/.local/share/razercontrol/daemon.json";
const EFFECTS_FILE: &str = "/.local/share/razercontrol/effects.json";

#[derive(Serialize, Deserialize, Copy, Clone)]
pub struct PowerConfig {
    pub power_mode: u8,
    pub cpu_boost: u8,
    pub gpu_boost: u8,
    pub fan_rpm: i32,
    pub brightness: u8,
    pub logo_state: u8,
    pub screensaver: bool, // turno of keyboard light if screen is blank
    pub idle: u32,
    /// RAPL PL1 (sustained) in watts. 0 = not configured — let firmware manage.
    #[serde(default)]
    pub rapl_pl1_watts: u32,
    /// RAPL PL2 (boost) in watts. 0 = not configured.
    #[serde(default)]
    pub rapl_pl2_watts: u32,
}

impl PowerConfig {
    pub fn new() -> PowerConfig {
        return PowerConfig {
            power_mode: 0,
            cpu_boost: 1,
            gpu_boost: 0,
            fan_rpm: 0,
            brightness: 128,
            logo_state: 0,
            screensaver: false,
            idle: 0,
            rapl_pl1_watts: 0,
            rapl_pl2_watts: 0,
        };
    }
}

#[derive(Serialize, Deserialize)]
pub struct Configuration {
    pub power: [PowerConfig; 2],
    pub sync: bool,    // sync light settings between ac and battery
    pub no_light: f64, // no light bellow this percentage of battery
    pub standard_effect: u8,
    pub standard_effect_params: Vec<u8>,
}

impl Configuration {
    pub fn new() -> Configuration {
        return Configuration {
            power: [PowerConfig::new(), PowerConfig::new()],
            sync: false,
            no_light: 0.0,
            standard_effect: 0, // off
            standard_effect_params: vec![],
        };
    }

    pub fn write_to_file(&self) -> io::Result<()> {
        let dir_path = get_home_directory() + "/.local/share/razercontrol";
        fs::create_dir_all(&dir_path)?;
        let j: String = serde_json::to_string_pretty(&self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        File::create(get_home_directory() + SETTINGS_FILE)?.write_all(j.as_bytes())?;
        Ok(())
    }

    pub fn read_from_config() -> io::Result<Configuration> {
        let str = fs::read_to_string(get_home_directory() + SETTINGS_FILE)?;
        let res: Configuration = serde_json::from_str(str.as_str())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(res)
    }

    /// Clears any persisted manual fan RPM back to 0 (auto) for all power profiles.
    /// Called on startup so a daemon crash with a high fan RPM set does not
    /// leave the fan stuck at speed after a restart.
    /// Returns true if any value was changed (i.e. a write is needed).
    pub fn reset_fan_profiles_to_auto(&mut self) -> bool {
        let mut changed = false;
        for slot in &mut self.power {
            if slot.fan_rpm != 0 {
                slot.fan_rpm = 0;
                changed = true;
            }
        }
        changed
    }

    pub fn write_effects_save(json: serde_json::Value) -> io::Result<()> {
        let dir_path = get_home_directory() + "/.local/share/razercontrol";
        fs::create_dir_all(&dir_path)?;
        let j: String = serde_json::to_string_pretty(&json)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        File::create(get_home_directory() + EFFECTS_FILE)?.write_all(j.as_bytes())?;
        Ok(())
    }

    pub fn read_effects_file() -> io::Result<serde_json::Value> {
        let str = fs::read_to_string(get_home_directory() + EFFECTS_FILE)?;
        let res: serde_json::Value = serde_json::from_str(str.as_str())?;
        Ok(res)
    }
}

fn get_home_directory() -> String {
    env::var("HOME").expect("The \"HOME\" environment variable must be set to a valid directory")
}
