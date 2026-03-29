# Migration & Overhaul Documentation

## Overview

This document describes all changes made during the comprehensive overhaul of
`razer-laptop-control-no-dkms` to support the Razer Blade 16 2023 (USB `1532:029F`)
on CachyOS Linux (Arch-based), KDE Plasma + Wayland, with NVIDIA RTX 4090 Laptop GPU.

---

## 1. HID Protocol Fixes (Hardware API Correctness)

### Transaction ID Fix
- **File:** `src/daemon/device.rs`
- **Change:** `RazerPacket::new()` transaction ID changed from `0x1F` to `0xFF`
- **Reason:** OpenRazer kernel driver uses `transaction_id.id = 0xFF` for ALL Blade laptop models.
  The wrong transaction ID (`0x1F`) caused HID commands to be silently rejected by the firmware.

### Brightness Commands Fix
- **File:** `src/daemon/device.rs`
- **Change:** `set_brightness()` and `get_brightness()` now use blade-specific commands:
  - Set: command class `0x0E`, command ID `0x04`, data size `0x02`, `args[0]=0x01, args[1]=brightness`
  - Get: command class `0x0E`, command ID `0x84`, data size `0x02`, `args[0]=0x01`, response in `args[1]`
- **Reason:** Blade laptops do NOT use generic LED brightness commands (`0x03, 0x03/0x83`).
  They use `razer_chroma_misc_set_blade_brightness()` / `razer_chroma_misc_get_blade_brightness()`.
  The previous code used the wrong command class, causing brightness commands to fail or return
  incorrect values.

### Reference
All HID protocol details verified against OpenRazer kernel driver source:
`github.com/openrazer/openrazer/driver/razerkbd_driver.c`

---

## 2. Dead Code Removal

### Files Deleted
| File | Purpose | Reason for Removal |
|---|---|---|
| `src/gui.rs` | Legacy Glade-based GUI stub | Replaced by GTK3 razer-settings |
| `src/driver_sysfs.rs` | Legacy DKMS sysfs interface | This fork uses HID (hidapi) directly |
| `src/session_manager_presence.rs` | GNOME Session Manager stubs | Unused, replaced by FreeDesktop ScreenSaver |
| `src/daemon/dbus_mutter_displayconfig.rs` | GNOME Mutter display D-Bus | KDE-incompatible, GNOME-specific |
| `src/daemon/dbus_mutter_idlemonitor.rs` | GNOME Mutter idle monitor D-Bus | KDE-incompatible, GNOME-specific |

### Code Cleaned
- Removed dead `/* use crate::driver_sysfs; */` import from `src/daemon/kbd/board.rs`
- Removed unused `BACKLIGHT_LED` constant from `device.rs` (no longer needed after brightness fix)
- Deduplicated `SupportedDevice` struct: was defined in both `src/lib.rs` and `src/daemon/device.rs`;
  daemon now uses `service::SupportedDevice` from the library crate

---

## 3. New Software Effects

### Added to `src/daemon/kbd/effects.rs`

| Effect | Description | Parameters |
|---|---|---|
| `BreathDual` | Two-color alternating breathing cycle | `[R1, G1, B1, R2, G2, B2, duration_x100ms]` |
| `SpectrumCycle` | Full HSV hue cycling across all keys | `[speed 1-10]` |
| `RainbowWave` | Rainbow scrolling across keyboard columns | `[speed 1-10, direction 0=left/1=right]` |
| `Starlight` | Random key twinkling with fade-out | `[R, G, B, density 1-20]` |
| `Ripple` | Concentric ring waves from keyboard center | `[R, G, B, speed 1-10]` |

### Helper Added
- `hsv_to_rgb(h, s, v)` function for color space conversion (used by SpectrumCycle, RainbowWave)

### Integration Points Updated
- `src/daemon/kbd/mod.rs` — `EffectLayer::from_save()` now matches all 9 effect names
- `src/daemon/daemon.rs` — `process_client_request()` SetEffect handler now creates all 9 effects
- `src/cli/cli.rs` — New `Effect` subcommands: `breathing-dual`, `spectrum-cycle`, `rainbow-wave`,
  `starlight`, `ripple`
- `src/razer-settings/razer-settings.rs` — Effect dropdown now lists all 9 effects
- `src/razer-settings/tray.rs` — Tray menu now has "Keyboard Effect" and "Standard Effect" submenus

---

## 4. NVIDIA GPU Monitoring

### New Module
- **File:** `src/daemon/gpu.rs`
- **Method:** Calls `nvidia-smi` with CSV output format to query GPU stats
- **Data returned:** GPU name, temperature, GPU utilization, memory utilization, power draw,
  VRAM used/total, GPU clock, memory clock

### Integration
- `src/comms.rs` — New `GetGpuStatus` command and response variant
- `src/daemon/daemon.rs` — Handles `GetGpuStatus` by calling `gpu::query_nvidia_gpu()`
- `src/cli/cli.rs` — New `gpu` subcommand shows GPU stats in terminal
- `src/razer-settings/razer-settings.rs` — About page shows GPU info section

### Usage
```sh
razer-cli gpu
```

---

## 5. Tray Enhancements

### New Tray Menus
- **Keyboard Effect** submenu — all 9 software effects with preset parameters
- **Standard Effect** submenu — all 7 hardware effects (off, wave left/right, spectrum, static, reactive, breathing, starlight)

---

## 6. Cross-Desktop Environment Support

### Current Status
- **KDE Plasma + Wayland:** Fully supported (primary target)
  - Screensaver: `org.freedesktop.ScreenSaver` (KDE implements this)
  - System tray: `ksni` (StatusNotifierItem, native KDE protocol)
- **GNOME:** Supported with caveats
  - Screensaver: `org.freedesktop.ScreenSaver` (GNOME implements this)
  - System tray: Requires `AppIndicator` GNOME Shell extension for tray icon
  - GTK3 GUI works on both Wayland compositors

### Removed GNOME-specific code
Previous Mutter-specific D-Bus interfaces were removed in favor of the cross-desktop
`org.freedesktop.ScreenSaver` API which works on both KDE and GNOME.

---

## 7. Build Modernization

- **Cargo.toml:** `edition = "2018"` → `edition = "2021"`
- **hidapi:** Uses `linux-shared-hidraw` feature (set in prior session)
- **Build result:** Zero warnings, zero errors

---

## 8. Supported Hardware Effects (from OpenRazer)

For the Blade 16 2023 (029F), the following hardware-level effects are supported:

| Effect | Command | Parameters |
|---|---|---|
| Off (None) | `0x03, 0x0A` | none |
| Wave | `0x03, 0x0A` | direction (1=left, 2=right) |
| Reactive | `0x03, 0x0A` | speed, R, G, B |
| Breathing (single/dual/random) | `0x03, 0x0A` | varies by kind |
| Spectrum | `0x03, 0x0A` | none |
| Static | `0x03, 0x0A` | R, G, B |
| Starlight (single/dual/random) | `0x03, 0x0A` | varies by kind |
| Custom Frame | `0x03, 0x0A` (NOSTORE) | per-key matrix |

**Note:** "Wheel" hardware effect is NOT available on Blade laptops.
The **Wheel software effect** (rotating color sweep) is implemented in the daemon as a custom effect.

---

## 9. GTK3 → Relm4 (GTK4 + Libadwaita) Migration

### Framework Change
- **Old:** `gtk = "0.18.1"` + `glib = "0.19.7"` (GTK3, maintenance-mode)
- **New:** `relm4 = "0.9"` with features `["libadwaita", "gnome_47"]` → gtk4 0.9.7 + libadwaita 0.7.2
- **Reason:** GTK3 enters maintenance-only in 2026. Libadwaita provides modern adaptive UI with dark theme, proper widget styling, and HIG compliance.

### Files Rewritten
| File | Changes |
|---|---|
| `razer-settings.rs` | Complete rewrite: `SimpleComponent` pattern, `adw::ApplicationWindow`, `adw::ViewStack`/`ViewSwitcher`, `adw::PreferencesPage`/`PreferencesGroup`, `adw::ComboRow`/`SwitchRow`/`SpinRow`/`ActionRow` |
| `widgets.rs` | ColorWheel: Cairo DrawingArea with HSV disk + drag gesture (GTK4 GestureDrag API) |
| `tray.rs` | `glib::Sender` → `std::sync::mpsc::Sender` (GTK4 removed glib channel API) |
| `error_handling.rs` | Removed GTK3 `MessageDialog`; simplified to `eprintln` + `exit` |
| `style.css` | 250-line GTK3 dark theme → 6-line Libadwaita overrides (Adwaita handles styling) |

### Key Architecture Changes
- `view!` macro for window declaration + imperative `init()` for complex UI construction
- `adw::StyleManager::set_color_scheme(ForceDark)` instead of custom CSS dark theme
- Tray channel: `glib::MainContext::channel` → `mpsc::channel` + `glib::timeout_add_local` polling
- `#![deny(warnings)]` enforced on razer-settings binary

---

## 10. Wheel Lighting Effect

### New Software Effect
- **File:** `src/daemon/kbd/effects.rs` — `Wheel` struct
- **Parameters:** `[speed 1-10, direction 0=CW/1=CCW]`
- **Behavior:** Maps each key's angle from the **N key** (row 4, col 6 on the 6×15 matrix)
  to an HSV hue. Formula: `hue = angle + offset` (pure rotation, no radial spiral term
  that would mask spinning). `offset` advances by `speed × 2°` per 100ms tick.
  Radial brightness fade (`0.6 + 0.4 * dist/6.0`) adds depth without masking rotation.
- Registered in daemon.rs, mod.rs, cli.rs, and GUI effect dropdown (index 9)

---

## 11. Performance Timeline Chart

### About Page Enhancement
- New `adw::PreferencesGroup` "Performance Timeline" with Cairo `DrawingArea`
- 60-second rolling window (20 samples at 3-second intervals)
- Three overlaid lines: Temperature (orange, 0-100°C), GPU Usage (green, 0-100%), Power Draw (cyan, 0-200W)
- **Dual right Y-axis:** GPU% (green) and Watts (cyan, labelled 0W/50W/100W/150W/200W)
- **TGP reference line:** dashed cyan horizontal line for `power.default_limit` (hardware TGP)
- **TGP Limit** row added to GPU info group above chart
- `power_limit_w: f32` added to `GpuStatus`, `comms::DaemonResponse::GetGpuStatus`, CLI output

---

## 12. CPU Power Limits (PDL1/PDL2)

### Intel RAPL Integration
- **Sysfs path:** `/sys/class/powercap/intel-rapl:0/constraint_{0,1}_power_limit_uw`
- `constraint_0` = PL1 (long_term / sustained TDP)
- `constraint_1` = PL2 (short_term / boost TDP)

### New Commands
| Command | IPC | Description |
|---|---|---|
| `GetPowerLimits` | → `GetPowerLimits { pl1_watts, pl2_watts, pl1_max_watts }` | Read current RAPL limits |
| `SetPowerLimits { pl1, pl2 }` | → `SetPowerLimits { result }` | Write new limits (requires root daemon) |

### CLI
```sh
razer-cli pdl               # Read PL1/PL2/base TDP
razer-cli set-pdl 120 160   # Set PL1=120W, PL2=160W
```

### GUI
- AC power page: "CPU Power Limits (RAPL)" section with PL1/PL2 SpinRow + Apply button
- Only shown on AC page (power limit tuning is primarily an AC concern)

---

## 13. System Tray Lighting Effects

### New Submenu
- "Lighting Effect" submenu added to tray with 8 preset effects:
  Static (Green), Static (White), Spectrum Cycle, Rainbow Wave, Wheel, Breathing (Green), Ripple (Cyan), Starlight (White)

---

---

## 15. Smart Keyboard Animation Throttle

### GPU Load-Adaptive Animation Rate
- **Files:** `src/daemon/daemon.rs`, `src/daemon/kbd/mod.rs`
- `ANIM_SLEEP_MS: AtomicU64` replaces the previous const-based sleep in the animator loop
- `start_gpu_load_monitor_task()` polls GPU util every 5s via `gpu::query_nvidia_gpu()`:
  - util ≥ 70% → 333ms sleep (3 FPS)
  - util ≤ 20% → 100ms sleep (10 FPS, restored)
  - 20-70% → no change

### PRIME Display Flickering — Root Cause Investigation
**Symptom:** Built-in OLED flickers during heavy GPU load. External monitors unaffected.

**Analysis:**
- Display path: NVIDIA renders → PRIME DMA → Intel iGPU → eDP-1 (built-in)
- External path: NVIDIA → DP/HDMI directly (no flicker)
- Panel self-refresh: **NOT supported** (confirmed via `/sys/kernel/debug/dri/0000:00:02.0/eDP-1/i915_psr_status`)
- `nvidia-smi -pl`: **NOT supported** on this hardware (“Changing power management limit not supported in current scope”)
- NVPCF ACPI path: `\_SB_.NPCF` — Razer EC manages GPU TGP via ACPI WMI, not NVML
- Mechanism: NVIDIA GSP firmware ↔ Razer EC ↔ power rail = Dynamic Boost 2.0 oscillation
  under load → brief GPU clock transition → PRIME frame stall → display glitch
- History: was absent after a Mesa+NVIDIA driver update; regressed since
  → track `nvidia-utils` changelog for PRIME sync fix

**Daemon contribution:** 10 USB HID keyboard packets/s go to the same Razer EC that
handles NVPCF. Reducing this to 3/s under peak load lowers EC interrupt pressure.

---

## 16. GPU TGP Lock Attempt

- `pin_nvidia_tgp(watts: u32)` added to `daemon.rs`, called on `SetPowerMode`
- Calls `nvidia-smi -pm 1 && nvidia-smi -pl <watts>` to enforce NVML SW power cap
- **Result:** Silently skipped — `nvidia-smi -pl` is refused by driver on this hardware
- Function kept as no-op for forward compatibility

TGP mapping used (for future reference when NVML support lands):
| Mode | TGP target |
|---|---|
| Silent (0) | 50 W |
| Balanced (1) | 115 W |
| Gaming (2) | 150 W |
| Creator (3) | 115 W |

---

## 17. File Inventory (Current)

```
src/
├── lib.rs                          # Shared SupportedDevice struct, DEVICE_FILE const
├── comms.rs                        # IPC protocol (+PDL, +power_limit_w in GetGpuStatus)
├── cli/
│   └── cli.rs                      # razer-cli (clap 4, pdl/set-pdl/wheel, TGP in gpu output)
├── daemon/
│   ├── daemon.rs                   # Entry, IPC, RAPL, ANIM_SLEEP_MS AtomicU64, GPU monitor task
│   ├── device.rs                   # HID (RazerPacket, set_power_mode, set_brightness)
│   ├── config.rs                   # JSON config persistence
│   ├── gpu.rs                      # nvidia-smi query (+power_limit_w / power.default_limit)
│   ├── battery.rs                  # UPower D-Bus
│   ├── screensaver.rs              # FreeDesktop ScreenSaver D-Bus
│   ├── login1.rs                   # logind D-Bus (sleep/wake)
│   └── kbd/
│       ├── mod.rs                  # Effect trait, EffectManager (10 effects), ANIMATION_SLEEP_MS
│       ├── effects.rs              # 10 effects; Wheel centered on N key (row 4, col 6)
│       └── board.rs                # Keyboard matrix (6×15 = 90 keys)
└── razer-settings/
    ├── razer-settings.rs           # Relm4/GTK4/Libadwaita GUI (+TGP row, +W Y-axis, +TGP line)
    ├── tray.rs                     # KDE system tray (ksni, lighting effects submenu)
    ├── widgets.rs                  # ColorWheel (Cairo HSV picker)
    ├── error_handling.rs           # Panic/crash handling
    ├── style.css                   # Minimal Libadwaita CSS overrides
    └── util.rs                     # AC power check utility
```

---

## 18. Sleep/Resume & Shutdown Investigation

### Sudden Shutdown Root Cause

- **Boot -1 crash analysis:** last journal entries show GPU spiking 10W → 63W instantaneously.  
  Battery was at 51% (~50 Wh). No kernel panic, no NTFS3 errors — clean log until abrupt silence.  
- **Probable cause:** instantaneous power demand (GPU + CPU combined ≈ 110–150 W) exceeded  
  battery discharge protection threshold (hardware OCP/UVP) → hard power-off.  
- **NTFS3 corruption (/DATA2)** is a consequence: the NTFS3 driver doesn't get a clean unmount  
  when power is cut. Run `ntfsfix /dev/sdX` or boot Windows to trigger chkdsk.

### Sleep/Resume Latency Root Cause & Fix

**Root cause:** the daemon held an open `HidDevice` file descriptor across suspend/resume.  
The Linux USB subsystem must force-tear down active endpoints before PCI `D3cold`, causing  
a multi-second stall on suspend. On resume, the old fd is stale and `restore_light()` silently  
fails → keyboard backlight stays off.

**Fixes applied in this session:**

| File | Change |
|------|--------|
| `daemon/daemon.rs` | `SYSTEM_SLEEPING: AtomicBool` flag added |
| `daemon/daemon.rs` | `PrepareForSleep(true)`: set flag → `light_off()` → `d.device = None` (close HID fd) |
| `daemon/daemon.rs` | `PrepareForSleep(false)`: retry `discover_devices()` up to 5× with 300 ms backoff → `set_ac_state_get()` → `restore_light()` |
| `daemon/daemon.rs` | `start_keyboard_animator_task()`: skip USB writes while `SYSTEM_SLEEPING` is set |
| `daemon/daemon.rs` | `start_gpu_load_monitor_task()`: skip `nvidia-smi` calls while `SYSTEM_SLEEPING` is set |
| `daemon/daemon.rs` | Screensaver handler: ignore unlock event if `SYSTEM_SLEEPING` |
| `daemon/daemon.rs` | D-Bus process loop: `unwrap()` → graceful `error!()` logging |

### GPU Status Cache (nvidia-smi Reduction)

| Before | After |
|--------|-------|
| `nvidia-smi` spawned per GUI poll (every 3 s) + GPU monitor (every 5 s) | GPU monitor owns the single `nvidia-smi` call; `GetGpuStatus` reads `GPU_STATUS_CACHE` |
| ≈ 12–20 subprocess spawns/minute | ≈ 12 subprocess spawns/minute (one per 5 s) |

---

## 19. GUI Improvements (this session)

- **VRAM percentage fix:** changed from `mem_util` (nvidia-smi memory *bandwidth* %) to  
  `mem_used_mb * 100 / mem_total_mb` (actual VRAM *capacity* %). Previously showed ~0–20%  
  even when 15/16 GB was allocated.
- **Memory bandwidth chart series added:** dashed purple line (`MemBW`) on Performance  
  Timeline chart — shows `utilization.memory` from nvidia-smi (bandwidth saturation,  
  useful for AI/ML workloads that saturate HBM bandwidth).
- **Tray: Start Minimized:** `razer-settings --minimized` / `-m` starts with window hidden.  
  Useful for autostart entries — app lives in tray without cluttering the desktop.
- **Tray: Restart App:** new tray menu item re-launches the current binary and exits cleanly.

