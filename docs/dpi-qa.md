# DPI / scaling QA

Manual matrix for per-monitor DPI (Per-Monitor V2), fractional scaling
(125/150/175%), and runtime monitor moves.

## What the code already guarantees

- `midi-editor.exe` embeds a manifest declaring `PerMonitorV2` (via gpui's
  `windows-manifest` feature — check with
  `grep -a PerMonitorV2 midi-editor.exe`).
- The Windows platform layer handles `WM_DPICHANGED`: it recomputes the
  scale factor and applies the suggested window rect, then repaints.
- All app geometry is in logical `px`. Every cached hit region
  (`roll_bounds`, `lane_bounds`, `mini_bounds`, `ruler_bounds`, …) is an
  `Rc<Cell<Bounds<Pixels>>>` refreshed by a prepaint listener each frame —
  a DPI change produces a repaint, so bounds can never go stale relative
  to what is on screen. Hit testing reads the same cells painting used,
  so pointer and pixels stay aligned by construction.
- Icons are SVG, all chrome is vector-painted, text is rasterized at the
  current scale — nothing bitmap-stretched in the UI.
- VST3 editor windows are separate Win32 windows hosted by
  `vst3-host`'s `PluginWindow`, which applies the newest DPI to the
  plugin's view after platform callbacks.

## Runtime indicator

The status bar shows `display scale: <n>%` — the live scale factor of the
monitor the window is on. It must match the Windows "Scale" setting for
that display. If it stays at 100% on a scaled display, the process is
being bitmap-stretched (PerMonitorV2 broken — treat as a bug).

## Matrix

Repeat each row at **100%, 125%, 150%, 175%, 200%**:

| # | Check | Pass when |
|---|-------|-----------|
| 1 | Status-bar scale chip | Shows the monitor's scale % (e.g. `150%`); updates within ~1s of moving the window to another monitor |
| 2 | Roll click/drag | Notes, select box, and marquee land exactly under the cursor — no offset |
| 3 | Note drag + resize grips | Grip and note edges track the pointer at fractional scales (esp. 125%/175%) |
| 4 | Velocity/CC lane drag | Point under cursor maps to the drawn value; hit cells align with bars |
| 5 | Menus + submenus | Open at label position, no row taller/shorter than its text; submenu opens beside parent row; popups clamp inside the window at the bottom edge |
| 6 | Minimap | Viewport rectangle matches the roll's visible span; click/drag pans to the clicked position |
| 7 | Ruler | Bar numbers/ticks unclipped; click seeks to the clicked tick |
| 8 | Track column | Row select/mute/solo/dest clicks hit the intended row; no clipped text at 200% |
| 9 | Event list | Row under cursor is the one that selects; scrolls cleanly |
| 10 | Text crispness | Menus, status bar, event rows, and roll labels are sharp (not blurry) at every scale |
| 11 | Window chrome | Title bar, resize borders, and min/max buttons respond at pointer position |
| 12 | VST3 editor | Open plugin GUI (Track > Plugin GUI): editor appears crisp and its controls hit-test correctly; check again after moving the plugin window across monitors |
| 13 | Dialogs (open/save, rename) | File dialogs honor the target monitor's scale; rename box is unclipped |

## Mixed-DPI moves

On a two-monitor setup (e.g. primary 100%, secondary 150%):

| # | Check | Pass when |
|---|-------|-----------|
| M1 | Drag window slowly across the boundary | Layout re-renders at the destination scale mid-drag; no smearing or double-scaled frame |
| M2 | Hit test immediately after the move | Clicks register at the pointer without restarting the app |
| M3 | Menu open during/after a move | Dropdown paints at the new scale, positioned under its label |
| M4 | Maximize on each monitor | Fills each monitor exactly; no border bleed |
| M5 | Change the monitor's scale while the app sits on it (Settings > Display) | App rescales live; scale chip tracks the new % |

## Notes

- Fractional scales are the interesting cases: 125%/175% produce
  non-integer logical-to-physical pixel mappings; round-off errors show
  up as 1px hit-test drift or blurry text.
- Windows only sends `WM_DPICHANGED` when the majority of the window
  moves to a differently-scaled monitor — a half-split window keeps the
  primary monitor's scale. Verify behavior at the halfway point.
