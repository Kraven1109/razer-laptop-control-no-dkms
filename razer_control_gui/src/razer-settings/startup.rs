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

fn desktop_entry() -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=Razer Blade Control (tray)\nComment=Start Razer Blade Control minimized to the system tray\nExec={} --minimized\nIcon=razer-blade-control\nTerminal=false\nX-GNOME-Autostart-enabled=true\nHidden=false\n",
        current_exec(),
    )
}

pub fn is_enabled() -> bool {
    autostart_path().exists()
}

pub fn set_enabled(enable: bool) -> io::Result<()> {
    let path = autostart_path();
    if enable {
        fs::create_dir_all(autostart_dir())?;
        fs::write(path, desktop_entry())?;
    } else if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}
