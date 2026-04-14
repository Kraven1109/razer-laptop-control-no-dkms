use std::fs;
use std::io;
use std::path::PathBuf;

const AUTOSTART_FILE: &str = "razer-settings.desktop";

fn autostart_dir() -> PathBuf {
    if let Ok(path) = std::env::var("XDG_CONFIG_HOME") {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed).join("autostart");
        }
    }

    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".config").join("autostart")
}

fn autostart_path() -> PathBuf {
    autostart_dir().join(AUTOSTART_FILE)
}

fn current_exec() -> String {
    std::env::current_exe()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "razer-settings".into())
}

fn desktop_quote(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn desktop_entry(start_minimized: bool) -> String {
    let exec = if start_minimized {
        format!(
            "/usr/bin/env RAZER_SETTINGS_START_MINIMIZED=1 {}",
            desktop_quote(&current_exec())
        )
    } else {
        desktop_quote(&current_exec())
    };

    format!(
        "[Desktop Entry]\nType=Application\nName=Razer Blade Control (tray)\nComment=Start Razer Blade Control from your desktop session\nExec={exec}\nIcon=razer-blade-control\nTerminal=false\nStartupNotify=false\nX-GNOME-Autostart-enabled=true\nHidden=false\n",
    )
}

pub fn is_enabled() -> bool {
    autostart_path().exists()
}

pub fn set_enabled(enable: bool, start_minimized: bool) -> io::Result<()> {
    let path = autostart_path();
    if enable {
        fs::create_dir_all(autostart_dir())?;
        fs::write(path, desktop_entry(start_minimized))?;
    } else if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}
