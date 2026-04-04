# Razer Blade 16 2023 — Research & Fix Log

Unified reference for all hardware research, code changes, system fixes, and diagnostic commands.
Replaces: `MIGRATION.md`, `POTENTIAL_FIXES.md`, `RESEARCH_COMMANDS.md`

---

## Hardware & System Facts

| Item | Value |
|---|---|
| Model | Razer Blade 16 2023 — RZ09-0483, USB `1532:029F` |
| CPU | Intel Core i9-13950HX (Raptor Lake-P, 24-core) |
| GPU (dGPU) | NVIDIA GeForce RTX 4090 Laptop `10de:2757 rev a1` |
| GPU (iGPU) | Intel Raptor Lake-S UHD `8086:a788 rev 04` |
| RAM | LPDDR5 (spd5118 at 0x50, 0x52) |
| OS | CachyOS (Arch-based), KDE Plasma + Wayland |
| Kernels | `linux-cachyos 6.19.10` / `linux-cachyos-lts 6.18.20` |
| Boot | Limine with mkinitcpio |
| NVIDIA driver | Proprietary open-kernel-module |

**TGP per profile (measured with `nvidia-smi --query-gpu=enforced.power.limit`):**
| Profile | TGP |
|---|---|
| Silent (3) | 115 W |
| Balanced (0) | 135 W |
| Gaming (1) | 150 W |
| Dynamic Boost ceiling | 175 W (max_limit) |

---

## Critical System Configuration

### `/etc/modprobe.d/i915-flicker.conf` *(requires reboot/initramfs)*
```
options i915 enable_psr=0 enable_fbc=0 enable_dc=0
```
Disables Intel Panel Self Refresh and Frame Buffer Compression — both are root causes of
eDP blink artefacts when the PRIME pipeline is active.

### `/etc/modprobe.d/nvidia-power.conf` *(requires reboot/initramfs)*
```
options nvidia NVreg_EnableGpuFirmware=0 NVreg_DynamicPowerManagement=0x03
```
- `EnableGpuFirmware=0`: disables GSP firmware upload (reduces PRIME timing jitter).
- `DynamicPowerManagement=0x03`: fine-grained runtime PM — GPU can suspend to D3cold at idle
  **even with the display on**, not just when the display is off (0x02).

### `/etc/udev/rules.d/80-nvidia-pm.rules`
Sets `power/control=auto` on all NVIDIA PCI devices (class 0x030000 / 0x030200) so the
kernel runtime-PM subsystem is allowed to suspend the dGPU.  Already active in current session.

### `nvidia-powerd.service` — **MUST remain enabled**
`nvidia-powerd` implements NVPCF2 (NVIDIA Platform Controller Framework for Power v2).
**Without it, `enforced.power.limit` is stuck at the driver default (115 W) regardless of
the Razer power profile.** The EC cannot push TGP changes to the NVIDIA driver without this
userspace daemon processing NVPCF ACPI notifications.

Re-enable with: `sudo systemctl enable --now nvidia-powerd`

---

## HID Protocol Facts (`src/daemon/device.rs`)

### Transaction ID
All Blade laptops use `transaction_id = 0xFF` — not `0x1F`.
Wrong ID causes HID commands to be silently rejected by the EC firmware.
Reference: `openrazer/driver/razerkbd_driver.c`.

### Brightness Commands
Blade laptops use blade-specific brightness commands, **not** generic LED commands:
- Set: class `0x0E`, cmd `0x04`, size `0x02`, `args[0]=0x01`, `args[1]=brightness`
- Get: class `0x0E`, cmd `0x84`, size `0x02`, `args[0]=0x01`, response at `args[1]`

### Power Mode HID Packet
```
class=0x0d, cmd=0x02, size=0x04
args[0]=0x00, args[1]=zone(0x01=CPU 0x02=GPU), args[2]=mode, args[3]=fan_flag
```
Modes: 0=Balanced, 1=Gaming, 2=Creator, 3=Silent, 4=Custom.
After the EC receives this command, it triggers ACPI NVPCF2 notifications → `nvidia-powerd`
receives them → updates `enforced.power.limit`.

---

## Display Flickering Root Causes & Fixes

### Root Cause 1 — Intel PSR + Panel Replay (PSR successor on Raptor Lake)
Intel PSR2 and its successor **Panel Replay** (eDP 1.5, separate `enable_panel_replay` flag)
selectively refresh regions of the eDP panel. `enable_psr=0` alone does NOT disable Panel Replay.
**Fix (permanent, needs reboot):**
```
options i915 enable_psr=0 enable_fbc=0 enable_dc=0 enable_panel_replay=0
```
Written to `/etc/modprobe.d/i915-flicker.conf` in Session 3.

### Root Cause 2 — EC interrupt contention with NVPCF Dynamic Boost
NVIDIA Dynamic Boost (NVPCF2) sends ~1 Hz power budget adjustment packets via the EC.
Our keyboard animation also sends USB HID packets to the same EC. When both happen at the
same time under peak GPU load, the EC IRQ handler can stall the PRIME display pipeline.

**Mitigation in daemon (`daemon.rs` `start_gpu_load_monitor_task`):**
- At GPU util ≥ 70%: keyboard animation slowed from 100 ms → 333 ms (3 FPS)
- Near TGP (≥ 94% of `enforced.power.limit` at ≥ 60% GPU util): animation frozen entirely
  (`HIGH_POWER_FLICKER_GUARD = true`, animator loop sleeps 1000 ms)
- Hysteresis: guard releases only when power drops below 88% of TGP at util < 40%
- Thresholds are **percentage-based** (not absolute W) so they work in every profile.

### Root Cause 3 — Daemon calling `nvidia-smi` every 3 s on battery
nvidia-smi spawns a subprocess that wakes the dGPU from D3cold.  On battery this prevents
the GPU from staying suspended, which activates the PRIME display pipeline continuously.
**Fix:** `gpu::should_query_nvidia(on_ac)` — skips nvidia-smi when `runtime_status == "suspended"`.

### Root Cause 4 — NVIDIA D3cold wakeup latency during ComfyUI burst pattern
`NVreg_DynamicPowerManagement=0x03` (fine-grained PM) allows GPU to enter D3cold during
the ~9 s idle gaps **between** ComfyUI inference steps. Wakeup from D3cold takes 200–500 ms,
stalling the PRIME frame pipeline → visible black frame / flicker at the start of each step.

**Fix (permanent, immediate):**
```
/etc/udev/rules.d/99-nvidia-no-d3cold.rules:
ACTION=="add",    SUBSYSTEM=="pci", ATTR{vendor}=="0x10de", ATTR{class}=="0x030000", ATTR{power/control}="on"
ACTION=="bind",   SUBSYSTEM=="pci", ATTR{vendor}=="0x10de", ATTR{class}=="0x030000", ATTR{power/control}="on"
ACTION=="change", SUBSYSTEM=="pci", ATTR{vendor}=="0x10de", ATTR{class}=="0x030000", ATTR{power/control}="on"
```
Rule number 99 fires AFTER `71-nvidia.rules` and `80-nvidia-pm.rules` (both set "auto" on add/bind).
Also applied immediately: `echo on > /sys/bus/pci/devices/0000:01:00.0/power/control`
GPU idle power: ~15 W instead of ~0 W while system is running. S3 sleep is unaffected.

---

## Code Architecture Notes

### GPU monitoring + cache (`src/daemon/gpu.rs`)
- `NVIDIA_RUNTIME_STATUS_PATH`: lazy-static sysfs path for `/sys/bus/pci/devices/<NVIDIA>/power/runtime_status`
- `should_query_nvidia(on_ac)`: returns `false` on battery when GPU is suspended
- `GPU_STATUS_CACHE`: mutex-guarded last-good `GpuStatus`; served to `GetGpuStatus` handler without spawning a new nvidia-smi per GUI poll
- `clear_gpu_cache()`: called on profile switch, AC state change, and on_ac → battery

### Profile switch → TGP update latency
On profile switch, the daemon:
1. Sends HID to EC → EC triggers NVPCF ACPI notification → nvidia-powerd updates TGP (~1–2 s)
2. `clear_gpu_cache()` is called immediately, so the next `GetGpuStatus` request calls nvidia-smi directly rather than serving the stale cache value.

The GPU monitor task also polls every 3 s (on AC) and repopulates the cache; the About page polls the daemon every 3 s. So worst-case TGP display lag after a profile switch is ~5 s (1 s EC processing + up to 3 s monitor poll + up to 3 s GUI poll). After the cache clear it's typically 1–2 s.

### `GetCurrentEffect` protocol command
Added to expose the running keyboard effect to the GUI on startup:
- `comms::DaemonCommand::GetCurrentEffect`→ `DaemonResponse::GetCurrentEffect { name: String, args: Vec<u8> }`
- Served from `EFFECT_MANAGER.get_current_effect_info()` (returns `EffectSave.name` + `EffectSave.args` of the topmost layer)
- GUI `build_keyboard_page()` calls this once at page construction and restores: effect index, color wheels, speed/direction/density/duration controls.

### Sleep / Resume handling (`daemon.rs` `start_battery_monitor_task`)
On `PrepareForSleep(start=true)`:
- `SYSTEM_SLEEPING = true` (suppresses HID writes + GPU queries)
- `d.light_off()` (blank keyboard so EC doesn't hold HID fd)
- `d.device = None` (drops `HidDevice`, closes fd → USB can suspend cleanly)

On `PrepareForSleep(start=false)` (wake):
- Offloaded to background thread (prevents blocking dbus dispatch loop)
- 2 s initial wait for PRIME/NVPCF pipeline to reinitialise before HID writes
- Up to 5 attempts (300 ms apart) to re-discover HID device
- `SYSTEM_SLEEPING = false` once device is ready

### AC state change (`start_battery_monitor_task`)
On any AC Online change (plug or unplug):
- `d.set_ac_state(online)` → applies saved `PowerConfig` for the new AC state (power mode, fan, brightness, logo)
- `gpu::clear_gpu_cache()` — always, not just on battery: avoids serving stale TGP from the other AC state
- `HIGH_POWER_FLICKER_GUARD = false`, `ANIM_SLEEP_MS = ANIMATION_SLEEP_MS` — resets animation guards

---

## Keyboard Effects (`src/daemon/kbd/`)

### Effect save names (used in `effects.json` and `GetCurrentEffect`)
| GUI label | Daemon `save()` name | Args format |
|---|---|---|
| Static | `Static` | `[R, G, B]` |
| Static Gradient | `Static Gradient` | `[R1,G1,B1, R2,G2,B2]` |
| Wave Gradient | `Wave Gradient` | `[R1,G1,B1, R2,G2,B2]` |
| Breathing | `Breathing Single` | `[R, G, B, duration]` |
| Breathing Dual | `Breathing Dual` | `[R1,G1,B1, R2,G2,B2, duration]` |
| Spectrum Cycle | `Spectrum Cycle` | `[speed]` |
| Rainbow Wave | `Rainbow Wave` | `[speed, direction]` |
| Starlight | `Starlight` | `[R, G, B, density]` |
| Ripple | `Ripple` | `[R, G, B, speed]` |
| Wheel | `Wheel` | `[speed, direction]` |

Effects are saved to `~/.local/share/razercontrol/effects.json` on daemon shutdown.

---

## Diagnostic Commands

### Check TGP per profile
```bash
razer-cli write power ac 0 1 0 && sleep 3 && nvidia-smi --query-gpu=enforced.power.limit --format=csv,noheader,nounits  # Balanced → 135 W
razer-cli write power ac 1 2 2 && sleep 3 && nvidia-smi --query-gpu=enforced.power.limit --format=csv,noheader,nounits  # Gaming → 150 W
razer-cli write power ac 3 0 0 && sleep 3 && nvidia-smi --query-gpu=enforced.power.limit --format=csv,noheader,nounits  # Silent → 115 W
```

### Check GPU runtime suspend state
```bash
cat /sys/bus/pci/devices/0000:01:00.0/power/runtime_status   # "suspended" or "active"
cat /sys/bus/pci/devices/0000:01:00.0/power/control          # should be "auto"
```

### Check PSR status (post-reboot with i915-flicker.conf)
```bash
sudo cat /sys/kernel/debug/dri/0/i915_edp_psr_status
```

### Verify daemon is applying resume correctly
```bash
journalctl -p info --since "5 min ago" --no-pager | grep -E "PrepareForSleep|HID device ready|discover|AC0"
```

### Live GPU power/TGP watch
```bash
watch -n1 "nvidia-smi --query-gpu=power.draw,enforced.power.limit,clocks.gr --format=csv,noheader,nounits"
```

### Daemon logs (current session)
```bash
journalctl -p info --since "10 min ago" --no-pager | grep -iE 'power|mode|tgp|razer|flicker|guard'
```

### Check all power limits
```bash
nvidia-smi -q | grep -A5 "Power Limit"
razer-cli pdl        # CPU RAPL PL1/PL2
```

---

## Known Remaining Issues / Pending

| Item | Status | Notes |
|---|---|---|
| `i915 enable_psr=0` | **Needs reboot** | In initramfs via `/etc/modprobe.d/i915-flicker.conf` |
| `NVreg_DynamicPowerManagement=0x03` | **Needs reboot** | In `/etc/modprobe.d/nvidia-power.conf` |
| Raptor Lake duplicate-eDP VBT bug | **If PSR fix insufficient** | Install `intel-gpu-tools` (AUR), decode VBT, patch LFP2 entry, add `i915.vbt_firmware=i915/modified.vbt` |
| AC heavy-load flicker | **Needs post-reboot test** | Flicker guard active; if still occurs, escalate to VBT fix |

---

## Build & Install Reference

```bash
cd ~/Coding/RZer_laptop_control/razer-laptop-control-no-dkms/razer_control_gui
pkill -f '/usr/bin/razer-settings|razer-settings' || true
cargo build --release 2>&1 | tail -5
./install.sh install
/usr/bin/razer-settings >/tmp/razer-settings.log 2>&1 & disown && echo "launched"
```

---

## Session Change Log

### Session 1 (prior) — Major HID/UI overhaul
- Transaction ID `0x1F` → `0xFF`; brightness fixed to blade-specific commands
- Dead files removed; GTK3 → Relm4/GTK4 + Libadwaita migration
- New software effects: BreathDual, SpectrumCycle, RainbowWave, Starlight, Ripple, Wheel
- GPU monitoring (`gpu.rs`, `GetGpuStatus`); About page with perf timeline chart
- Tray menus for effect and standard effect selection; BHO support

### Session 2 — Flicker diagnosis & battery GPU suspend
- Root cause identified: daemon calling nvidia-smi every 3 s on battery kept GPU active
- Added `should_query_nvidia()`, GPU status cache, battery-aware poll intervals
- Added `HIGH_POWER_FLICKER_GUARD` (keyboard animation freeze near TGP) to reduce EC interrupt contention
- System: `nvidia-powerd` disabled, `NVreg_DynamicPowerManagement` → 0x03 in initramfs, udev rules for NVIDIA runtime PM, `i915-flicker.conf` written, initramfs rebuilt
- Fixed GUI regression (GetGpuStatus None on battery) + deadlock in `GetGpuStatus` handler
- Added keyboard brightness live-sync timer (2 s poll)

### Session 3 (today) — TGP per-profile fix + keyboard effect restore
- **Root cause of TGP issue**: `nvidia-powerd` disabled → NVPCF2 broken → TGP stuck at 115 W
  - **Fix**: `sudo systemctl enable --now nvidia-powerd` → TGP now: Balanced=135W Gaming=150W Silent=115W
- `daemon.rs` `SetPowerMode` handler: `gpu::clear_gpu_cache()` added after profile set, so stale TGP doesn't persist in GUI
- `daemon.rs` AC state monitor: cache + guards cleared on **both** plug AND unplug (previously only on unplug)
- `daemon.rs` flicker guard: replaced hardcoded `>= 140 W` threshold with `power_w >= tgp * 0.94` (enter, at 60% util) and `power_w >= tgp * 0.88` (stay, at 40% util) — now fires correctly for Silent/Balanced/Gaming
- `kbd/mod.rs`: `EffectManager::get_current_effect_info()` added
- `comms.rs`: `GetCurrentEffect` command + `GetCurrentEffect { name, args }` response added
- `daemon.rs`: `GetCurrentEffect` handler implemented
- `razer-settings.rs`: `get_current_effect()` function; `build_keyboard_page()` restores effect index, both color wheels, speed/direction/density/duration on startup
