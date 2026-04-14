# Application Icon — `razer-blade-control.svg`

## Design

The icon is a hand-crafted SVG representing a **blade/diamond** shape with a **control crosshair** motif, using Razer's signature green color palette on a dark background.

| Element | Description |
|---------|-------------|
| Background | Dark rounded rectangle (`#1a1a2e` → `#0e1628` gradient), `rx=10` corners |
| Blade | Diamond/rhombus (`M24 5 L39 24 L24 43 L9 24 Z`) with 3-stop green gradient |
| Crosshair | Horizontal + vertical dark lines through the center — "control" motif |
| Center dot | Outer dark ring (`r=2.5`) with inner green dot (`r=1.2`, `#44ffa1`) |
| Glow filter | Gaussian blur + green flood overlay for a soft luminous edge |

### Color Palette

| Token | Hex | Usage |
|-------|-----|-------|
| Green top | `#44ffa1` | Gradient start, center dot, hover accents |
| Green mid | `#00d26a` | Gradient midpoint, glow color |
| Green dark | `#009950` | Gradient bottom |
| BG dark | `#1a1a2e` | Background top-left |
| BG deeper | `#0e1628` | Background bottom-right, crosshair strokes |
| Border | `#0f3460` | Background border |

## SVG Source

```xml
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 48 48">
  <defs>
    <linearGradient id="blade" x1="0.5" y1="0" x2="0.5" y2="1">
      <stop offset="0%" stop-color="#44ffa1"/>
      <stop offset="50%" stop-color="#00d26a"/>
      <stop offset="100%" stop-color="#009950"/>
    </linearGradient>
    <linearGradient id="bg" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0%" stop-color="#1a1a2e"/>
      <stop offset="100%" stop-color="#0e1628"/>
    </linearGradient>
    <filter id="glow">
      <feGaussianBlur in="SourceAlpha" stdDeviation="1.5" result="blur"/>
      <feFlood flood-color="#00d26a" flood-opacity="0.5"/>
      <feComposite in2="blur" operator="in"/>
      <feMerge>
        <feMergeNode/>
        <feMergeNode in="SourceGraphic"/>
      </feMerge>
    </filter>
  </defs>
  <!-- Dark rounded background -->
  <rect x="1" y="1" width="46" height="46" rx="10" fill="url(#bg)" stroke="#0f3460" stroke-width="1"/>
  <!-- Green blade/diamond shape -->
  <path d="M24 5 L39 24 L24 43 L9 24 Z" fill="url(#blade)" filter="url(#glow)" opacity="0.95"/>
  <!-- Control crosshair lines -->
  <line x1="24" y1="10" x2="24" y2="38" stroke="#0e1628" stroke-width="2" stroke-linecap="round" opacity="0.7"/>
  <line x1="14" y1="24" x2="34" y2="24" stroke="#0e1628" stroke-width="2" stroke-linecap="round" opacity="0.7"/>
  <!-- Center dot -->
  <circle cx="24" cy="24" r="2.5" fill="#0e1628" opacity="0.8"/>
  <circle cx="24" cy="24" r="1.2" fill="#44ffa1"/>
</svg>
```

## Installation

The icon is installed by `install.sh` to:

```
/usr/share/icons/hicolor/scalable/apps/razer-blade-control.svg
```

Referenced by:
- `data/gui/razer-settings.desktop` → `Icon=razer-blade-control`
- `src/razer-settings/tray.rs` → `icon_name()` / `tool_tip().icon_name`
- `src/razer-settings/razer-settings.rs` → `window.set_icon_name()`
