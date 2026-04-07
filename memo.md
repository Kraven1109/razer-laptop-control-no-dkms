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

### Current diagnosis (April 2026)
There were at least two overlapping causes, and they need to be kept separate.

### Root Cause 1 — Daemon-side `nvidia-smi` / NVML mutex stalls on PRIME (fixed in project)
`nvidia-smi` can grab the NVIDIA driver's global mutex long enough to block the PRIME DMA
completion fence. On this machine that produced KWin pageflip timeouts around 1.3-1.5 s and a
visible black-frame flicker.

**Project fixes now in place:**
- Two-track guard in `daemon.rs::start_gpu_load_monitor_task()`:
  - Gaming path: arm instantly from `/dev/dri/renderD128` fd growth, with zero `nvidia-smi`
  - CUDA / ComfyUI path: arm early at 30% util, then hold for 120 s with no extra NVML call
- `GetGpuStatus` no longer bypasses the guard with an on-demand `nvidia-smi`
- GUI/CLI/CSV now label held telemetry as cached, and the GUI chart skips stale held samples

### Root Cause 2 — Internal eDP HDR / WCG / EDR / 10-bit path in hybrid mode (investigation closed in this project)
Testing showed KWin pageflip timeouts still occurring while the flicker guard was already active,
meaning the remaining flicker is not coming from the daemon's `nvidia-smi` path.

Observed internal panel state during the residual issue:
- 3840x2400 @ 120 Hz
- HDR enabled
- Wide Color Gamut enabled
- Allow EDR = always
- 10 bits per color

Evidence gathered:
- External USB-C display does not show the same flicker under the same workload
- Disabling HDR on the built-in panel reduced flicker during the same ~108 s ComfyUI run
- **Updated (Session 5):** After wakeup from sleep, flickering still occurs even with HDR
  disabled, under heavy GPU workload. HDR state no longer makes a clear difference after resume.

**Decision:** Root Cause 2 (eDP pipeline / post-sleep flicker) is **out of scope for this project**.
Deep investigation will be done in a separate dedicated project.  This project focuses on
daemon-side power management and the keyboard control stack.

### Secondary contributor still worth keeping — Intel panel power features
Intel PSR2 / Panel Replay / related panel power-saving paths can still worsen eDP behavior.
`enable_psr=0` alone does NOT disable Panel Replay.

**Configured system-side mitigation (needs reboot):**
```
options i915 enable_psr=0 enable_fbc=0 enable_dc=0 enable_panel_replay=0
```

### Secondary contributor still worth keeping — NVIDIA D3cold wake latency
ComfyUI's bursty idle gaps can still make D3cold wake latency visible if runtime PM is too aggressive.

**Pinned-on workaround that was tested:**
```
/etc/udev/rules.d/99-nvidia-no-d3cold.rules:
ACTION=="add",    SUBSYSTEM=="pci", ATTR{vendor}=="0x10de", ATTR{class}=="0x030000", ATTR{power/control}="on"
ACTION=="bind",   SUBSYSTEM=="pci", ATTR{vendor}=="0x10de", ATTR{class}=="0x030000", ATTR{power/control}="on"
ACTION=="change", SUBSYSTEM=="pci", ATTR{vendor}=="0x10de", ATTR{class}=="0x030000", ATTR{power/control}="on"
```
This keeps the dGPU out of D3cold while the system is running. It is still a secondary mitigation,
not the best explanation for the remaining HDR-sensitive flicker.

---

## Code Architecture Notes

### GPU monitoring + cache (`src/daemon/gpu.rs`)
- `NVIDIA_RUNTIME_STATUS_PATH`: lazy-static sysfs path for `/sys/bus/pci/devices/<NVIDIA>/power/runtime_status`
- `should_query_nvidia(on_ac)`: returns `false` on battery when GPU is suspended
- `GPU_STATUS_CACHE`: mutex-guarded last-good `GpuStatus`; served to `GetGpuStatus` handler without spawning a new nvidia-smi per GUI poll
- `store_gpu_cache()` / `get_cached_gpu_status()` / `clear_gpu_cache()`: standard cache lifecycle
- `start_gpu_load_monitor_task()`: simple poller — polls nvidia-smi every 3 s (AC) / 10 s (battery), stores result in cache. Anti-flicker guard logic has been REMOVED (see Archived section below).

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
| Internal-panel HDR flicker | **Out of scope — moved to dedicated project** | Post-sleep flicker persists even with HDR disabled; investigating in a separate project |
| AC heavy-load flicker | **Reduced from daemon side** | Guard blocks extra `nvidia-smi`; residual eDP path under investigation externally |

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

### Session 4 — PRIME guard rewrite + HDR/eDP diagnosis
- Reworked the GPU guard into two tracks:
  - gaming detection from `renderD128` fd growth
  - CUDA / ComfyUI detection from early 30% util arm
- Guard hold and periodic check both extended to 120 s to cover long ComfyUI runs without reintroducing NVML stalls mid-job
- `GetGpuStatus` now respects the guard and never sneaks in an extra `nvidia-smi`
- Added stale telemetry propagation through daemon, GUI, CLI, and CSV log
- Verified from journal that residual KWin pageflip timeouts still occurred while the guard was already active
- New dominant hypothesis confirmed by testing: internal panel HDR / WCG / EDR / 10-bit path is the main remaining flicker source; disabling HDR massively reduces the issue, while external USB-C display path stays clean

### Session 5 — Post-sleep flicker diagnosis + GUI robustness + guard removal
- **Flicker finding (Session 5):** After wakeup from sleep, flickering still occurs with HDR disabled
  under heavy GPU workload. The eDP flicker issue is no longer connected to HDR state alone. Root
  Cause 2 is now classified as **out of scope** for this project — a dedicated project will be used
  for deep investigation.
- **All PRIME anti-flicker guard code removed from this project** (see "Archived" section below).
- **GUI robustness:**
  - `comms.rs`: added 1 500 ms read timeout + 1 000 ms write timeout on every IPC socket call —
    prevents the GTK main thread from blocking indefinitely when the daemon is slow (e.g. after
    sleep/resume).
  - `send_data()`: no longer crashes the GUI on `ENOENT` (daemon socket briefly absent after
    daemon restart); returns `None` gracefully instead.
  - GPU chart poll (System page): moved daemon IPC off the GTK main thread via
    `glib::MainContext::channel` + `std::thread::spawn`. An `Arc<AtomicBool>` in-flight guard
    prevents thread pile-up when the daemon is slow. The chart and labels now update via the
    channel callback on the main thread — the main loop is never blocked.
  - Shared chart data (`history`, `tgp_limit`, CSV file handle) converted from
    `Rc<RefCell<...>>` to `Arc<Mutex<...>>` to allow safe cross-thread access.
  - Keyboard brightness slider: debounced with a 200 ms `glib::timeout_add_local` timer — rapid
    scrolling no longer fires one blocking IPC call per step.
- **UI/UX:**
  - "About" tab renamed to "System" with `computer-symbolic` icon.
  - CPU Power Limits (RAPL PL1/PL2) moved from AC-only page to System tab — now accessible
    regardless of AC state, enabling battery users to cap turbo boost for longer runtime.
  - Apply button given `margin_top: 16` for clear visual separation from the spin rows.
  - `start_gpu_load_monitor_task()` simplified to a clean 3 s / 10 s cache-refresh poller.

---

## Archived — PRIME Anti-Flicker Guard Design (removed Session 5)

This guard was built to reduce EC interrupt contention between NVIDIA NVPCF Dynamic Boost
and HID traffic during heavy PRIME workloads (ComfyUI, games).  Later testing showed it did
not eliminate the residual KWin pageflip timeouts when the internal eDP HDR path was active.
The design is documented here for the dedicated flicker investigation project.

### Problem modelled
`nvidia-smi` acquires the NVIDIA driver's global mutex.  When held, the PRIME DMA completion
fence is blocked.  VS Code, Edge, and the GUI all render on `renderD128` (NVIDIA) and those
frames are PRIME-copied to Intel for KWin — so there is always a PRIME fence in flight.  At
≥ 60 % GPU util the mutex hold can exceed 1 368 ms, triggering i915's "Pageflip timed out!"
bug → 1-second blank screen.

### Two-track ARM logic
- **Track A — Gaming** (new `/dev/dri/renderD128` fd delta from `count_nvidia_render_fds()`):
  count increase > baseline + 2 → ARM guard instantly, ZERO `nvidia-smi`, no NVML stall.
- **Track B — CUDA / ComfyUI** (util threshold at low load):
  CUDA never opens `renderD128`. `nvidia-smi` is called at a LOW threshold (30 %) during
  model loading when the driver is lightly loaded and the mutex hold < 200 ms.

### Guard hold logic
- `GUARD_HOLD_MS = 120_000`: no `nvidia-smi` for first 2 min after ARM.
- `GUARD_POLL_MS = 120_000`: then periodic check every 2 min.
- `RELEASE_UTIL = 15 %`: release guard when util drops below this.
- `ARM_UTIL = 30 %`: arm from CUDA track at this threshold.
- While armed: `ANIM_SLEEP_MS = 600_000` (freeze animation) and
  `HIGH_POWER_FLICKER_GUARD = true`.

### Stale telemetry
When the guard was armed, `GPU_STATUS_CACHE_UPDATED_MS` tracked the last real NVML refresh.
Samples older than 6 s were marked `stale = true` in `DaemonResponse::GetGpuStatus`, causing:
- GUI chart: stale samples skipped (flat lines suppressed)
- CSV: `stale` column = 1
- Clock label: ` · cached` suffix
- CLI: ` (cached)` suffix on GPU name

### Key atomics (all removed)
`HIGH_POWER_FLICKER_GUARD`, `GUARD_ENTERED_MS`, `GUARD_ENTRY_TGP`,
`GUARD_LAST_CHECK_MS`, `RENDER128_BASELINE`, `ANIM_SLEEP_MS`
