# Research & Diagnostic Commands

All terminal commands used for investigating daemon behaviour, GPU power,
display flickering, sleep/resume latency, and sudden shutdown issues.

---

## 0 — Session Investigation Log (Chronological)

Commands actually run during the investigation sessions, in order, with findings.

```bash
# [1] Check running daemon health + what it polls every 3 s
journalctl --user -u razercontrol --no-pager --output=short-precise -n 200 \
  | grep -v "GetGpuStatus"
# → Daemon healthy; battery % logged every ~30-90 s; nvidia-smi called every 3 s from GUI

# [2] List all boot records to find sudden shutdowns
journalctl --no-pager --list-boots
# → Boot -1 last entry: GPU at 100%→91%→84% util; NO clean shutdown sequence

# [3] Inspect the very last log entries of the crashing boot
journalctl -b -1 --no-pager | tail -60
# → Battery at 51%, GPU suddenly spiked 10W→63W in 3 s, then system died
# → NTFS3 showed NO errors — crash was a power-delivery event (battery OCP)

# [4] Look for NTFS3 / OOM / kernel panic evidence in crashing boot
journalctl -b -1 --no-pager \
  | grep -iE "ntfs|oom|out of memory|kernel panic|mce|hardware error|watchdog" | head -30
# → No NTFS errors. Confirmed: abrupt power-delivery failure under heavy GPU load

# [5] Check login1.rs and daemon sleep handling
grep -n "PrepareForSleep\|inhibit\|sleep" \
  src/daemon/daemon.rs src/daemon/login1.rs
# → PrepareForSleep existed but HID not closed on sleep + not re-discovered on wake

# [6] Count open HID fds after sleep cycle (post-fix validation)
ls -la /proc/$(pgrep -x razercontrol)/fd | grep hidraw
```

---

## 00 — Display Flickering: Commands That Helped

These commands were part of the investigation into PRIME display pipeline flicker
on the built-in panel. Some may trigger or reduce the flickering behavior.

```bash
# Check Panel Self Refresh state (PSR causes blink during PRIME compositing)
cat /sys/kernel/debug/dri/0/i915_edp_psr_status 2>/dev/null \
  || sudo cat /sys/kernel/debug/dri/*/i915_edp_psr_status 2>/dev/null

# TEMPORARILY disable PSR to test if it removes flicker (non-persistent)
sudo sh -c "echo 0 > /sys/kernel/debug/dri/0/i915_edp_psr_status"

# Check NVIDIA dynamic boost mode (NVPCF) — this negotiates TGP every ~1 s
sudo cat /sys/kernel/debug/dri/*/nv_clients_list 2>/dev/null | head -20

# Identify if nvidia-smi is being spawned too frequently (keeps GPU powered)
watch -n1 "ps aux | grep nvidia-smi | grep -v grep"

# Confirm keyboard HID USB traffic during heavy GPU load (EC IRQ contention)
sudo udevadm monitor --kernel --subsystem-match=usb 2>&1 | grep -i "hid\|razer"

# Watch GPU clocks + power — look for oscillation during Dynamic Boost
watch -n0.5 "nvidia-smi --query-gpu=power.draw,clocks.gr,clocks.mem \
  --format=csv,noheader,nounits"

# Check TGP cap (may differ from default limit on some Razer models)
nvidia-smi --query-gpu=power.limit,power.default_limit \
  --format=csv,noheader,nounits

# Attempt hardware TGP pin (root; likely blocked on Optimus laptops)
sudo nvidia-smi -pl 115

# Check if VRR/FreeSync is active on built-in panel (can cause PRIME sync issues)
cat /sys/class/drm/card*/card*eDP*/vrr_capable 2>/dev/null

# Intel PSR2 selective update debug (kernel 6.x+)
sudo cat /sys/kernel/debug/dri/0/i915_edp_psr_status 2>/dev/null | grep -i "psr2\|selective\|enabled"

# Add kernel boot parameter to permanently disable PSR2 (edit GRUB cmdline)
# intel_iommu=on i915.enable_psr=0
# Then: sudo grub-mkconfig -o /boot/grub/grub.cfg && sudo mkinitcpio -P
```

**What fixed / mitigated the flicker (in this codebase):**
- Reduced keyboard animation from 10 FPS to 3 FPS (`333 ms`) during heavy GPU load (GPU util ≥ 70%) — reduces EC USB interrupt rate during NVPCF Dynamic Boost negotiation
- GPU load monitor now also populates a cache so `nvidia-smi` is spawned only once per 5 s instead of once per 3 s (GUI) + once per 5 s (monitor)

---

## 1 — Daemon Logs

```bash
# Live daemon log (last 200 lines, filter noisy GetGpuStatus entries)
journalctl --user -u razercontrol --no-pager --output=short-precise -n 200 \
  | grep -v "GetGpuStatus"

# Follow daemon log in real-time
journalctl --user -u razercontrol -f

# Full daemon log since last boot
journalctl --user -u razercontrol -b 0 --no-pager

# Log since a specific boot (use -1 for previous boot etc.)
journalctl --user -u razercontrol -b -1 --no-pager
```

---

## 2 — Boot History & Shutdown Analysis

```bash
# List all recorded boots with timestamps
journalctl --no-pager --list-boots

# Last 80 lines of a specific boot (check for clean vs unclean shutdown)
journalctl -b -1 --no-pager | tail -80

# Check if a shutdown was clean (look for "Stopped Session", "Unmounting /...")
journalctl -b -1 --no-pager | grep -E "Stopping|Stopped|Unmounting|reboot|shutdown" | tail -20

# Search for crash / OOM / kernel panic indicators in previous boot
journalctl -b -1 --no-pager \
  | grep -iE "oom|out of memory|kernel panic|general protection|call trace|oops|mce|\
hardware error|tainted|watchdog|killed process|systemd-coredump|ntfs|ntfs3|\
filesystem error|remount read.only|I_O error" | head -60
```

---

## 3 — GPU Power & Status

```bash
# One-shot GPU snapshot (all relevant fields)
nvidia-smi --query-gpu=name,temperature.gpu,utilization.gpu,utilization.memory,\
power.draw,power.default_limit,memory.used,memory.total,clocks.gr,clocks.mem \
--format=csv,noheader,nounits

# Watch GPU every 1 second
watch -n1 nvidia-smi

# GPU power cap / TGP limit
nvidia-smi --query-gpu=power.limit,power.default_limit,power.min_limit,power.max_limit \
  --format=csv,noheader,nounits

# Attempt to set GPU power cap (requires ADMIN; usually blocked on Optimus laptops)
sudo nvidia-smi -pl 115

# Check NVIDIA runtime power management state
cat /sys/bus/pci/devices/0000:01:00.0/power/control           # should be "auto"
cat /sys/bus/pci/devices/0000:01:00.0/power/runtime_status    # "suspended" or "active"
cat /sys/bus/pci/devices/0000:01:00.0/power/runtime_active_time
cat /sys/bus/pci/devices/0000:01:00.0/power/runtime_suspended_time

# List all PCI power states
for d in /sys/bus/pci/devices/*/; do
  echo "$d $(cat $d/power/control 2>/dev/null) $(cat $d/power/runtime_status 2>/dev/null)"
done | grep -v "^$"
```

---

## 4 — CPU Power (RAPL)

```bash
# Current CPU package power draw (microwatts → watts)
awk '{print $1/1e6 " W"}' /sys/class/powercap/intel-rapl:0/energy_uj
# (read twice 1s apart and diff to get instantaneous watts)

# PL1 / PL2 limits
cat /sys/class/powercap/intel-rapl:0/constraint_0_power_limit_uw   # PL1
cat /sys/class/powercap/intel-rapl:0/constraint_1_power_limit_uw   # PL2
cat /sys/class/powercap/intel-rapl:0/constraint_0_max_power_uw     # PL1 max
cat /sys/class/powercap/intel-rapl:0/constraint_0_time_window_us   # PL1 window

# Combined CPU + iGPU power view
turbostat --quiet --show PkgWatt,CoreWatt,GFXWatt,RAMWatt --interval 2
```

---

## 5 — Fan Speed & EC

```bash
# Read fan RPMs via sensors
sensors | grep -i fan

# Watch all temp / fan sensors
watch -n2 sensors

# HWMon fan paths (Razer EC / EC2)
ls /sys/class/hwmon/
cat /sys/class/hwmon/hwmon*/name        # find ec or razer entries
cat /sys/class/hwmon/hwmon*/fan1_input  # RPM

# Direct sysfs fan read (adjust hwmon number as needed)
cat /sys/class/hwmon/hwmon2/fan1_input
cat /sys/class/hwmon/hwmon2/fan2_input
```

---

## 6 — Display / PSR / Flickering

```bash
# Intel Panel Self Refresh status
cat /sys/kernel/debug/dri/0/i915_edp_psr_status 2>/dev/null \
  || cat /sys/kernel/debug/dri/*/i915_edp_psr_status 2>/dev/null

# Disable PSR temporarily (root, survives until next boot/modeset)
sudo sh -c "echo 0 > /sys/kernel/debug/dri/0/i915_edp_psr_status"

# PRIME sync / offload status
xrandr --props | grep -i "prime\|sync\|vrr\|freesync"

# Check if variable refresh rate is active on the built-in panel
cat /sys/class/drm/card*/card*eDP*/vrr_capable 2>/dev/null

# Check compositor VSync / tearing settings (KDE)
kscreen-doctor -o 2>/dev/null | grep refresh

# Force PRIME sync via kernel param (add to GRUB cmdline, then rebuild initramfs)
# e.g.: intel_iommu=off nvidia.NVreg_EnableGpuFirmware=0
```

---

## 7 — Sleep / Resume Diagnostics

```bash
# Check logind inhibitor locks (see who delays sleep)
systemd-inhibit --list

# Trigger a suspend and wake manually (test)
systemctl suspend && sleep 5 && journalctl -b 0 --no-pager | grep -i "suspending\|resuming"

# Check how long suspend/resume took
journalctl -b 0 --no-pager | grep -E "PM: suspend|PM: Suspending|PM: resume|ACPI: Waking"

# Find PrepareForSleep signal in logs
journalctl -b 0 --no-pager | grep PrepareForSleep

# Check USB devices resetting on resume (potential HID issue)
journalctl -b 0 --no-pager | grep -i "usb.*reset\|usb.*disconnect\|usb.*new\|hidraw"

# Check if any process is blocking suspend
journalctl -b 0 --no-pager | grep "sleep inhibitor"

# EC / ACPI wakeup sources
cat /proc/acpi/wakeup
```

---

## 8 — HID / USB Device

```bash
# Confirm Razer HID device is present
lsusb | grep -i razer

# List HID devices
ls /dev/hidraw*
hidraw-lookup(){
  for h in /dev/hidraw*; do
    echo "$h: $(udevadm info $h 2>/dev/null | grep -E 'ID_VENDOR=|ID_MODEL=')"
  done
}
hidraw-lookup

# Watch kernel USB events
udevadm monitor --kernel --subsystem-match=usb 2>&1 | head -50
```

---

## 9 — Battery

```bash
# Current battery state via UPower
upower -i /org/freedesktop/UPower/devices/battery_BAT0

# Watch battery live
watch -n5 "upower -i /org/freedesktop/UPower/devices/battery_BAT0 | grep -E 'state|percentage|energy-rate|time'"

# Direct kernel battery sysfs
cat /sys/class/power_supply/BAT0/status
cat /sys/class/power_supply/BAT0/capacity    # %
cat /sys/class/power_supply/BAT0/energy_now  # µWh
cat /sys/class/power_supply/BAT0/energy_full
cat /sys/class/power_supply/BAT0/power_now   # µW

# Check AC adapter
cat /sys/class/power_supply/AC0/online       # 1 = plugged in
```

---

## 10 — Daemon Service Management

```bash
# Status
systemctl --user status razercontrol

# Restart daemon
systemctl --user restart razercontrol

# Stop daemon
systemctl --user stop razercontrol

# Rebuild and reinstall daemon
cd razer-laptop-control-no-dkms/razer_control_gui
cargo build --release
sudo cp target/release/daemon /usr/bin/razercontrol
sudo cp target/release/razer-settings /usr/bin/razer-settings

# Check if socket is alive
ls -la /tmp/razercontrol.sock 2>/dev/null || echo "no socket"
```

---

## 11 — Process / Memory Profiling

```bash
# Daemon memory and CPU usage
systemctl --user status razercontrol | grep -E "Memory|CPU|Tasks"

# Or via pidstat
pidstat -u -r -p $(pgrep -x razercontrol) 2

# Detailed thread view
ps -T -p $(pgrep -x razercontrol)

# Count open file descriptors (check for HID fd leak across sleep/wake)
ls -la /proc/$(pgrep -x razercontrol)/fd | wc -l
ls -la /proc/$(pgrep -x razercontrol)/fd | grep hidraw
```

---

## 12 — nvidia-smi Subprocess Frequency Verification

```bash
# Watch how often nvidia-smi is spawned (shows daemon + GUI combined)
watch -n1 "ps aux | grep nvidia-smi | grep -v grep | wc -l"

# Trace all nvidia-smi invocations (requires strace on daemon)
strace -p $(pgrep -x razercontrol) -e execve 2>&1 | grep nvidia-smi

# Alternative: count execve calls per 5 seconds via bpftrace (root)
sudo bpftrace -e 'tracepoint:syscalls:sys_enter_execve /str(args->filename) == "nvidia-smi"/ { @[comm] = count(); }' \
  --unsafe -c "sleep 10"
```
