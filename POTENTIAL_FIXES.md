# Potential System Fixes — Bulk Run Sheet

Commands collected from the full investigation session (from ~20:00 March 28 onward).
Run section by section. Commands marked ⚠️ are persistent (survive reboot) and require
rebuilding grub/initramfs. All others are temporary or reversible.

---

## A — Display Flickering (PRIME / PSR)

### A1. Disable Intel PSR — **temporary test, lost on reboot**
Disabling PSR stops the iGPU selective-refresh. If flicker disappears immediately, PSR2 is the culprit.

```bash
sudo sh -c "echo 0 > /sys/kernel/debug/dri/0/i915_edp_psr_status"
```

### A2. Disable PSR permanently via kernel boot parameter ⚠️
Edit GRUB and add `i915.enable_psr=0` to `GRUB_CMDLINE_LINUX_DEFAULT`, then:

```bash
sudo grub-mkconfig -o /boot/grub/grub.cfg
sudo mkinitcpio -P
```

### A3. Disable NVIDIA GSP firmware ⚠️
Can reduce PRIME timing jitter. Add `nvidia.NVreg_EnableGpuFirmware=0` to GRUB cmdline, then run A2 commands above. May slightly reduce NVIDIA perf — remove if unstable.

### A4. Force NVIDIA GPU into runtime PM auto mode
Allows the GPU to reach D3cold at idle, reducing Dynamic Boost oscillation frequency.

```bash
echo auto | sudo tee /sys/bus/pci/devices/0000:01:00.0/power/control
echo auto | sudo tee /sys/bus/pci/devices/0000:01:00.1/power/control
cat /sys/bus/pci/devices/0000:01:00.0/power/runtime_status
```

### A5. Enable NVIDIA RTD3 (D3cold) runtime PM via modprobe ⚠️
Creates a persistent modprobe config, then rebuilds initramfs. Reboot required.

```bash
echo 'options nvidia NVreg_DynamicPowerManagement=0x02' | sudo tee /etc/modprobe.d/nvidia-power.conf
sudo mkinitcpio -P
```

### A6. Force NVIDIA DRM atomic modeset ⚠️
Ensures Wayland/PRIME gets atomic modeset enabled. Add `nvidia-drm.modeset=1` to GRUB cmdline (if not already set), then run A2. Verify first:

```bash
cat /proc/cmdline | grep -o "nvidia-drm.modeset=[0-9]"
```

---

## B — Sleep / Resume Latency

### B1. Install daemon with HID-close fix (main code fix this session) ⚠️
Rebuilds and installs the fixed daemon binary. Requires `cargo` in PATH.

```bash
cd ~/Coding/RZer_laptop_control/razer-laptop-control-no-dkms/razer_control_gui
cargo build --release 2>&1 | tail -3
sudo cp target/release/daemon /usr/bin/razercontrol
sudo cp target/release/razer-cli /usr/bin/razer-cli
systemctl --user restart razercontrol
systemctl --user status razercontrol --no-pager
```

### B2. Check for stuck sleep inhibitors
Lists everything blocking suspend. Unexpected entries here cause slow/failed suspend.

```bash
systemd-inhibit --list
```

### B3. Measure suspend/resume latency (before and after B1)

```bash
journalctl -b 0 --no-pager | grep -E "PM: suspend|PM: Suspending|PM: resume|ACPI: PM"
```

### B4. Verify no HID fd accumulation after sleep/wake
Run after a suspend+wake cycle. Should show exactly 1 hidraw entry.

```bash
ls -la /proc/$(pgrep -x razercontrol)/fd | grep hidraw
```

---

## C — Dark Display on Wake (PRIME reinit)

### C1. Install a systemd-sleep hook to force eDP connector re-probe on wake ⚠️
Creates a persistent hook run automatically after every resume.

```bash
sudo tee /usr/lib/systemd/system-sleep/prime-resume.sh << 'EOF'
#!/bin/bash
if [ "$1" = "post" ]; then
    sleep 1
    for d in /sys/class/drm/card*/card*eDP*; do
        echo detect > "$d/status" 2>/dev/null || true
    done
fi
EOF
sudo chmod +x /usr/lib/systemd/system-sleep/prime-resume.sh
```

---

## D — NTFS3 / DATA2 Corruption (after hard power-off)

### D1. Identify and repair NTFS3 partition ⚠️
Run `ntfsfix` to clear dirty bit and repair minor errors. Find the right device first.

```bash
lsblk -o NAME,FSTYPE,MOUNTPOINT | grep ntfs
```

Replace `/dev/nvme0n1pX` with the actual DATA2 device from the output above:

```bash
sudo umount /DATA2 2>/dev/null || true
sudo ntfsfix /dev/nvme0n1pX
sudo mount /DATA2
```

---

## E — Verify Everything After Reboot

Run this block after rebooting to confirm all fixes are active:

```bash
sudo cat /sys/kernel/debug/dri/0/i915_edp_psr_status | grep -i "enabled\|active"
cat /sys/bus/pci/devices/0000:01:00.0/power/control
cat /sys/bus/pci/devices/0000:01:00.0/power/runtime_status
ls -la /proc/$(pgrep -x razercontrol)/fd | grep hidraw
journalctl --user -u razercontrol -n 20 --no-pager | grep -E "PrepareForSleep|HID device ready|discover"
```

