use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::PathBuf;

const APP_DIR: &str = "razercontrol";
const GUI_CONFIG_FILE: &str = "gui.json";

fn data_dir() -> PathBuf {
    if let Ok(path) = std::env::var("XDG_DATA_HOME") {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed).join(APP_DIR);
        }
    }

    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".local/share").join(APP_DIR)
}

fn gui_config_path() -> PathBuf {
    data_dir().join(GUI_CONFIG_FILE)
}

fn default_page() -> String {
    "ac".into()
}

#[derive(Serialize, Deserialize, Clone)]
pub struct GuiConfig {
    #[serde(default = "default_page")]
    pub last_page: String,
    #[serde(default)]
    pub run_at_startup: bool,
    #[serde(default = "default_start_minimized")]
    pub start_minimized: bool,
}

fn default_start_minimized() -> bool {
    true
}

impl Default for GuiConfig {
    fn default() -> Self {
        Self {
            last_page: default_page(),
            run_at_startup: false,
            start_minimized: default_start_minimized(),
        }
    }
}

impl GuiConfig {
    pub fn load() -> Self {
        fs::read(gui_config_path())
            .ok()
            .and_then(|data| serde_json::from_slice(&data).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> io::Result<()> {
        let dir = data_dir();
        fs::create_dir_all(&dir)?;
        let json = serde_json::to_string_pretty(self)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        fs::write(gui_config_path(), json)
    }
}
