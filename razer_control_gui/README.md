# Razer Laptop Control (No DKMS)

Linux daemon and GUI for controlling Razer Blade laptops via HID (no kernel driver required).

## Features

- **Modern Libadwaita UI** — GTK4 + Libadwaita via Relm4, adaptive dark theme
- **Full background daemon** — Auto-restores all saved settings on startup (power, fan, brightness, logo, effects)
- **CLI, GUI, and system tray** for adjusting settings
- **10 software keyboard effects** — Static, Static Gradient, Wave Gradient, Breathing Single,
  Breathing Dual, Spectrum Cycle, Rainbow Wave, Starlight, Ripple, **Wheel**
- **7 hardware keyboard effects** — Off, Wave, Reactive, Breathing, Spectrum, Static, Starlight
- **Dynamic effect controls** — Speed, direction, density, duration sliders per effect
- **HSV color wheel picker** — Synapse-style color selection with drag interaction
- **Power management** — Balanced, Gaming, Creator, Silent, Custom (CPU/GPU boost)
- **CPU Power Limits (PDL1/PDL2)** — Read/write Intel RAPL PL1 (sustained) and PL2 (boost) via sysfs
- **Fan speed control** — Auto or manual RPM with live readout
- **Keyboard brightness** — 0-100% with live readout
- **Logo LED control** — Off, On, Breathing
- **Battery Health Optimizer** — Charge threshold control (50-80%)
- **NVIDIA GPU monitoring** — Real-time polling: temperature, utilization, VRAM, power, clocks
- **Performance timeline chart** — Live 60-second rolling chart of GPU temp, usage, and power draw
- **System tray quick actions** — Power modes, lighting effects, open settings
- **Settings persistence** — All settings saved to disk and restored on daemon restart
- **Cross-DE support** — KDE Plasma + Wayland, GNOME (with AppIndicator extension for tray)
- **Custom Razer-themed icon** — Dark blade/diamond with green gradient

## Supported Devices

See `data/devices/laptops.json` for the full list (37+ models).
Tested on: **Razer Blade 16 2023** (USB `1532:029F`).

## Installing

### Dependencies (Arch / CachyOS)

```sh
sudo pacman -S rust hidapi gtk4 libadwaita dbus pkgconf
```

### Dependencies (Debian / Ubuntu)

```sh
sudo apt install libdbus-1-dev libhidapi-dev libhidapi-hidraw0 pkg-config libudev-dev libgtk-4-dev libadwaita-1-dev
```

### Build & Install

```sh
./install.sh install
```

Then reboot, or:

```sh
sudo udevadm control --reload-rules && sudo udevadm trigger
systemctl --user daemon-reload
systemctl --user enable --now razercontrol
```

## Usage

### CLI — `razer-cli`

```sh
# Power & Fan
razer-cli read fan ac
razer-cli write fan ac 3500
razer-cli write power ac 1                 # Gaming mode
razer-cli write power ac 4 3 2             # Custom: CPU boost, GPU high

# Brightness & Logo
razer-cli write brightness ac 75
razer-cli write logo ac 1

# Battery Health Optimizer
razer-cli write bho on 75
razer-cli read bho

# NVIDIA GPU status
razer-cli gpu

# Hardware effects (run on keyboard controller)
razer-cli standard-effect off
razer-cli standard-effect wave 1
razer-cli standard-effect spectrum
razer-cli standard-effect static 255 0 128
razer-cli standard-effect reactive 2 0 255 0
razer-cli standard-effect breathing 0 0 255 0 0 0 0
razer-cli standard-effect starlight 0 1 255 255 255 0 0 0

# Software effects (animated by daemon)
razer-cli effect static 0 255 0
razer-cli effect static-gradient 255 0 0 0 0 255
razer-cli effect wave-gradient 0 128 255 255 0 128
razer-cli effect breathing-single 0 255 0 10
razer-cli effect breathing-dual 0 255 0 255 0 128 10
razer-cli effect spectrum-cycle 5
razer-cli effect rainbow-wave 5 0
razer-cli effect starlight 255 255 255 10
razer-cli effect ripple 0 255 128 5
razer-cli effect wheel 3 0                       # Wheel CW, speed 3
razer-cli effect wheel 5 1                       # Wheel CCW, speed 5

# CPU Power Limits (PDL1/PDL2)
razer-cli pdl                                    # Read current PL1/PL2
razer-cli set-pdl 120 160                         # Set PL1=120W, PL2=160W
```

### GUI — `razer-settings`

Launch from application menu or:
```sh
razer-settings
```

Pages: AC settings (+ CPU power limits), Battery settings, Keyboard (backlight effects + BHO), About (device info + live GPU stats + performance timeline).

The keyboard backlight section shows dynamic controls based on the selected effect:
per-effect speed, direction, density, and duration sliders, plus primary/secondary color pickers.

### System Tray

The tray icon appears automatically when `razer-settings` is running.
Provides quick access to power modes, fan speed, brightness, keyboard effects,
and standard effects.

- **KDE Plasma:** Works natively (StatusNotifierItem)
- **GNOME:** Install the AppIndicator/KStatusNotifierItem extension

## Architecture

```
razer-cli  ──┐
razer-settings ──┤──► Unix Socket ──► daemon ──► HID (hidapi) ──► Keyboard
system tray  ──┘                        │
                                        ├──► nvidia-smi ──► GPU stats
                                        ├──► D-Bus (UPower) ──► Battery/AC
                                        └──► D-Bus (ScreenSaver) ──► Idle detection
```

## Migration Notes

See [MIGRATION.md](MIGRATION.md) for detailed changes from the upstream fork.
