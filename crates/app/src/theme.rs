//! Color themes: every semantic color the UI uses, in one place.
//!
//! `Theme::dark()` reproduces the hand-tuned dark palette this app has
//! always used; `Theme::high_contrast()` swaps it for a Windows-style
//! high-contrast palette (black surfaces, bright borders/text). The active
//! theme follows the OS "high contrast" accessibility setting unless the
//! user forces one in the View menu (`GlobalPrefs.hc`).
//!
//! All colors are `0xRRGGBB` (or `0xRRGGBBAA` where an alpha byte is baked
//! in, matching the `rgb()`/`rgba()` call sites in render.rs).

/// The full set of semantic colors used by the UI.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Theme {
    /// menu bar, transport bar, status bar
    pub bg_bar: u32,
    /// side panels, dropdown menus, dialogs
    pub bg_panel: u32,
    /// open/raised surfaces (menu item under an open menu, enabled snap)
    pub bg_raised: u32,
    /// piano-roll / minimap canvas backdrop
    pub bg_canvas: u32,
    /// controller-lane strip backdrop
    pub bg_lane: u32,
    /// black-key rows on the roll
    pub bg_key: u32,
    /// ordinary pitch-row separators on the roll
    pub grid_row: u32,
    /// C-octave row separators (slightly stronger)
    pub grid_oct: u32,
    /// bar lines (strongest grid line; beat lines use `border`)
    pub grid_bar: u32,
    /// window root behind all panels
    pub bg_root: u32,
    /// track column and unselected track rows
    pub bg_row: u32,
    /// selected track row
    pub bg_row_sel: u32,
    /// track-row hover
    pub bg_row_hover: u32,
    /// LCD-style readouts (position, tempo)
    pub bg_input: u32,
    /// small chips (status bar, lane-mode, snap-off)
    pub bg_chip: u32,
    /// chip hover
    pub bg_chip_hover: u32,
    /// generic control hover (menu items, icon buttons)
    pub bg_hover: u32,
    /// top-level menubar item hover
    pub bg_menu_hover: u32,
    /// tooltip bubble
    pub bg_tooltip: u32,
    /// disabled/off control surface
    pub bg_off: u32,
    /// separators and panel edges
    pub border: u32,
    /// tooltip/dialog borders that must read clearly
    pub border_strong: u32,
    /// primary accent: markers, active chips, links, focused outline
    pub accent: u32,
    /// secondary accent: minimap viewport edges
    pub accent_dim: u32,
    /// active toggle-button background
    pub accent_bg: u32,
    /// active toggle-button / snap-on border
    pub accent_edge: u32,
    /// file drag-over wash
    pub accent_drop: u32,
    /// primary text
    pub text: u32,
    /// secondary text (menu shortcuts read a bit dimmer via text_faint)
    pub text_dim: u32,
    /// headers, status line, column labels
    pub text_muted: u32,
    /// menu shortcuts, badges, submenu arrows
    pub text_faint: u32,
    /// section heads inside menus
    pub text_head: u32,
    /// dim app title
    pub text_link: u32,
    /// inactive icon color
    pub icon_off: u32,
    /// "off" state text/icons + ruler tick marks
    pub state_off: u32,
    /// muted track name
    pub text_muted_name: u32,
    /// muted track-color swatch (unmuted rows show the track color)
    pub swatch_off: u32,
    /// emphasized text (open menubar item, playing-state digits)
    pub text_bright: u32,
    /// channel indicator on track rows
    pub ch_text: u32,
    /// event-list text
    pub events_text: u32,
    /// LCD digits and menu checkmarks
    pub lcd: u32,
    /// playhead / playback-running indicator
    pub ok: u32,
    /// dirty marker, solo-on text
    pub warn: u32,
    /// mute-on text, diagnostics link
    pub warn_alt: u32,
    /// dangling notes (no note-off) and errors
    pub danger: u32,
    /// controller-lane points
    pub lane: u32,
    /// lane connector lines (with alpha)
    pub lane_fill: u32,
    /// selected-note/lane-point highlight
    pub sel: u32,
    /// marquee rubber-band fill (with alpha)
    pub sel_fill: u32,
    /// alt-drag ghost note fill (with alpha)
    pub ghost_fill: u32,
    /// minimap viewport wash (with alpha)
    pub viewport_fill: u32,
    /// modal backdrop dim (with alpha)
    pub scrim: u32,
    /// blend target for inactive-track notes on the roll
    pub dim_target: u32,
    /// blend target for inactive-track notes on the minimap
    pub mini_dim: u32,
    /// in-scale row highlight on the roll (#40)
    pub scale_row: u32,
    /// subtle hover wash for rows (with alpha)
    pub hover_wash: u32,
    /// playhead line (with alpha)
    pub ok_fill: u32,
    /// per-track note colors (track index mod 8)
    pub track_colors: [u32; 8],
}

impl Theme {
    /// The palette this app has always used.
    pub(crate) const fn dark() -> Self {
        Self {
            bg_bar: 0x0f0f15,
            bg_panel: 0x17171d,
            bg_raised: 0x20202c,
            bg_canvas: 0x111118,
            bg_lane: 0x14141a,
            bg_key: 0x1a1a21,
            grid_row: 0x232329,
            grid_oct: 0x2e2e3a,
            grid_bar: 0x3d3d52,
            bg_root: 0x1b1b22,
            bg_row: 0x1b1b24,
            bg_row_sel: 0x2a2a3a,
            bg_row_hover: 0x252532,
            bg_input: 0x0b0b11,
            bg_chip: 0x2a2a35,
            bg_chip_hover: 0x3a3a48,
            bg_hover: 0x2f2f42,
            bg_menu_hover: 0x1d1d28,
            bg_tooltip: 0x26262e,
            bg_off: 0x141419,
            border: 0x2a2a35,
            border_strong: 0x3c3c4a,
            accent: 0x9fd0ff,
            accent_dim: 0x4f7fb0,
            accent_bg: 0x2b3d4f,
            accent_edge: 0x3d5a75,
            accent_drop: 0x16202e,
            text: 0xd8d8e0,
            text_dim: 0x9999aa,
            text_muted: 0x77778a,
            text_faint: 0x666677,
            text_head: 0x7a7a90,
            text_link: 0x7a86a8,
            icon_off: 0x9a9ab0,
            state_off: 0x55556a,
            text_muted_name: 0x707080,
            swatch_off: 0x555560,
            text_bright: 0xffffff,
            ch_text: 0x7070a0,
            events_text: 0xb8b8c8,
            lcd: 0x8fd0a0,
            ok: 0x50ff9f,
            warn: 0xffd24f,
            warn_alt: 0xffb454,
            danger: 0xff4f4f,
            lane: 0x4fd0ff,
            lane_fill: 0x4fd0ff88,
            sel: 0xffffff,
            sel_fill: 0x4f8cff33,
            ghost_fill: 0x80808044,
            viewport_fill: 0x9fd0ff1c,
            scrim: 0x00000066,
            dim_target: 0x12121a,
            mini_dim: 0x111118,
            scale_row: 0x1e2436,
            hover_wash: 0xffffff12,
            ok_fill: 0x50ff9f88,
            track_colors: [
                0x4f8cff, 0xff8c4f, 0x4fd08c, 0xd04fff, 0xffd24f, 0x4fd0ff, 0xff4f7a, 0x9dff4f,
            ],
        }
    }

    /// Windows-style high-contrast palette: black surfaces, bright borders,
    /// saturated accents. Every background/foreground pair clears WCAG AA
    /// (enforced by `contrast_audit` in tests).
    pub(crate) const fn high_contrast() -> Self {
        Self {
            bg_bar: 0x000000,
            bg_panel: 0x000000,
            bg_raised: 0x1a1a1a,
            bg_canvas: 0x000000,
            bg_lane: 0x000000,
            bg_key: 0x101010,
            grid_row: 0x303030,
            grid_oct: 0x404040,
            grid_bar: 0x808080,
            bg_root: 0x000000,
            bg_row: 0x000000,
            bg_row_sel: 0x004040,
            bg_row_hover: 0x1a1a1a,
            bg_input: 0x000000,
            bg_chip: 0x1a1a1a,
            bg_chip_hover: 0x333333,
            bg_hover: 0x2a2a2a,
            bg_menu_hover: 0x1a1a1a,
            bg_tooltip: 0x000000,
            bg_off: 0x000000,
            border: 0xc0c0c0,
            border_strong: 0xffffff,
            accent: 0x00ffff,
            accent_dim: 0x00a0a0,
            accent_bg: 0x003040,
            accent_edge: 0x00ffff,
            accent_drop: 0x003030,
            text: 0xffffff,
            text_dim: 0xd0d0d0,
            text_muted: 0xb0b0b0,
            text_faint: 0xa0a0a0,
            text_head: 0xb0b0b0,
            text_link: 0x00d0d0,
            icon_off: 0xc0c0c0,
            state_off: 0x909090,
            text_muted_name: 0xa0a0a0,
            swatch_off: 0x404040,
            text_bright: 0xffffff,
            ch_text: 0xb0b0b0,
            events_text: 0xe0e0e0,
            lcd: 0x40ff80,
            ok: 0x00ff00,
            warn: 0xffff00,
            warn_alt: 0xffa000,
            danger: 0xff4040,
            lane: 0x00ffff,
            lane_fill: 0x00ffffaa,
            sel: 0xffff00,
            sel_fill: 0xffff0044,
            ghost_fill: 0xffffff66,
            viewport_fill: 0x00ffff33,
            scrim: 0x000000aa,
            dim_target: 0x000000,
            mini_dim: 0x000000,
            scale_row: 0x002a2a,
            hover_wash: 0xffffff44,
            ok_fill: 0x00ff00aa,
            // saturated, widely-separated hues stay distinct in HC
            track_colors: [
                0x00ffff, 0xffff00, 0x00ff00, 0xff00ff, 0xff8000, 0x8080ff, 0xff4040, 0x80ff80,
            ],
        }
    }

    /// Light palette — same token slots as `dark`, re-balanced for light
    /// surfaces (accents darkened to keep WCAG AA on white).
    pub(crate) const fn light() -> Self {
        Self {
            bg_bar: 0xefeff3,
            bg_panel: 0xf7f7fa,
            bg_raised: 0xffffff,
            bg_canvas: 0xe9e9ef,
            bg_lane: 0xececf2,
            bg_key: 0xf4f4f8,
            grid_row: 0xcfcfda,
            grid_oct: 0xbfbfcf,
            grid_bar: 0x9898b0,
            bg_root: 0xefeff4,
            bg_row: 0xefeff4,
            bg_row_sel: 0xd2d2e4,
            bg_row_hover: 0xe2e2ec,
            bg_input: 0xffffff,
            bg_chip: 0xe0e0e8,
            bg_chip_hover: 0xceceda,
            bg_hover: 0xdadae6,
            bg_menu_hover: 0xe9e9f0,
            bg_tooltip: 0xffffff,
            bg_off: 0xf0f0f4,
            border: 0xc8c8d2,
            border_strong: 0xa8a8b8,
            accent: 0x1a5fb0,
            accent_dim: 0x4a70a8,
            accent_bg: 0xd0e0f5,
            accent_edge: 0x1a5fb0,
            accent_drop: 0xc8dcf0,
            text: 0x1b1b22,
            text_dim: 0x4a4a5e,
            text_muted: 0x5f5f75,
            text_faint: 0x8585a0,
            text_head: 0x66667d,
            text_link: 0x1a5fb0,
            icon_off: 0x565670,
            state_off: 0xa8a8ba,
            text_muted_name: 0x7c7c98,
            swatch_off: 0xb8b8c8,
            text_bright: 0x000000,
            ch_text: 0x7a7aa0,
            events_text: 0x2a2a35,
            lcd: 0x1a7a40,
            ok: 0x0a7a35,
            warn: 0x8a5a00,
            warn_alt: 0x945200,
            danger: 0xc02020,
            lane: 0x087090,
            lane_fill: 0x08709055,
            sel: 0x1a5fb0,
            sel_fill: 0x1a5fb033,
            ghost_fill: 0x50505044,
            viewport_fill: 0x1a5fb01c,
            scrim: 0x00000040,
            dim_target: 0xe9e9ef,
            mini_dim: 0xe9e9ef,
            scale_row: 0xd8e2f5,
            hover_wash: 0x00000010,
            ok_fill: 0x0a7a3588,
            track_colors: [
                0x2f6fdf, 0xc05a10, 0x1a9050, 0x9030c0, 0x9a7800, 0x1090a8, 0xd02060, 0x60a010,
            ],
        }
    }

    /// Resolved theme: the `hc_pref` override wins; otherwise `mode`
    /// decides, with `System` following the OS light/dark flag the caller
    /// read from `window.appearance()`.
    pub(crate) fn resolve(hc_pref: Option<bool>, mode: ThemeMode, sys_dark: bool) -> Self {
        match hc_pref {
            Some(true) => return Self::high_contrast(),
            Some(false) => {}
            None if high_contrast_on() => return Self::high_contrast(),
            None => {}
        }
        match mode {
            ThemeMode::Dark => Self::dark(),
            ThemeMode::Light => Self::light(),
            ThemeMode::System => {
                if sys_dark {
                    Self::dark()
                } else {
                    Self::light()
                }
            }
        }
    }
}

/// Appearance preference — persisted in prefs.json under `theme`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ThemeMode {
    /// Follow the OS light/dark setting (default).
    System,
    Dark,
    Light,
}

impl ThemeMode {
    pub(crate) fn from_pref(s: Option<&str>) -> Self {
        match s {
            Some("dark") => Self::Dark,
            Some("light") => Self::Light,
            _ => Self::System,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }
}

/// Shared layout metrics for the chrome (toolbars, menus, popups) — the
/// fixed sizes and the type ramp live here instead of ad-hoc px literals
/// scattered through render code.
pub(crate) mod metrics {
    /// 26px square toolbar icon buttons
    pub const ICON_BTN: f32 = 26.0;
    /// 24px dropdown rows, 18px section headers
    pub const MENU_ROW: f32 = 24.0;
    pub const MENU_HEAD: f32 = 18.0;
    /// check-column width inside a menu row
    pub const CHECK_W: f32 = 14.0;
    /// toolbar separator height
    pub const VSEP_H: f32 = 20.0;
    /// chrome type ramp (px): tiny labels → body
    pub const TEXT_XS: f32 = 9.5;
    pub const TEXT_SM: f32 = 10.0;
    pub const TEXT_MD: f32 = 11.0;
    pub const TEXT_LG: f32 = 12.0;
}

/// Windows "high contrast" accessibility flag (SystemParametersInfoW /
/// SPI_GETHIGHCONTRAST). False on failure or non-Windows.
#[cfg(target_os = "windows")]
pub(crate) fn high_contrast_on() -> bool {
    #[repr(C)]
    struct HighContrastW {
        cb_size: u32,
        flags: u32,
        lpsz_default_scheme: *mut u16,
    }
    const SPI_GETHIGHCONTRAST: u32 = 0x0042;
    const HCF_HIGHCONTRASTON: u32 = 0x0000_0001;
    unsafe extern "system" {
        fn SystemParametersInfoW(
            ui_action: u32,
            ui_param: u32,
            pv_param: *mut core::ffi::c_void,
            f_win_ini: u32,
        ) -> i32;
    }
    let mut hc = HighContrastW {
        cb_size: size_of::<HighContrastW>() as u32,
        flags: 0,
        lpsz_default_scheme: core::ptr::null_mut(),
    };
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            0,
            (&mut hc as *mut HighContrastW).cast(),
            0,
        )
    };
    ok != 0 && hc.flags & HCF_HIGHCONTRASTON != 0
}

// The editor is a single-window app drawn on the UI thread; helpers that
// build chrome (menus, chips, icon buttons) don't carry `self`, so the
// active theme is stashed here for the duration of a render pass.
thread_local! {
    static CURRENT: std::cell::Cell<Theme> = const { std::cell::Cell::new(Theme::dark()) };
}

/// Called once at the top of `render` before any chrome is built.
pub(crate) fn set_current(th: Theme) {
    CURRENT.with(|c| c.set(th));
}

/// Theme active for this render pass — used by free-standing helpers.
pub(crate) fn current() -> Theme {
    CURRENT.with(|c| c.get())
}

/// WCAG relative luminance of an `0xRRGGBB` sRGB color (0.0 black, 1.0 white).
#[cfg(test)]
pub(crate) fn luminance(c: u32) -> f64 {
    fn lin(v: f64) -> f64 {
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    }
    let r = lin(((c >> 16) & 0xff) as f64 / 255.0);
    let g = lin(((c >> 8) & 0xff) as f64 / 255.0);
    let b = lin((c & 0xff) as f64 / 255.0);
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

/// WCAG contrast ratio between two opaque `0xRRGGBB` colors (1.0..=21.0).
#[cfg(test)]
pub(crate) fn contrast(fg: u32, bg: u32) -> f64 {
    let (a, b) = (luminance(fg), luminance(bg));
    let (hi, lo) = if a >= b { (a, b) } else { (b, a) };
    (hi + 0.05) / (lo + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every foreground/background pairing the UI relies on for reading:
    /// body text, dimmed labels, accents, LCD digits, and state colors on
    /// each surface they sit on, grouped by how critical legibility is.
    /// (foreground field, background field) getter pair for one surface.
    type FgBg = (fn(&Theme) -> u32, fn(&Theme) -> u32);
    /// Primary content text — must clear WCAG AA 4.5:1 in every theme.
    const TEXT_PAIRS: &[FgBg] = &[
        (|t: &Theme| t.text, |t: &Theme| t.bg_panel),
        (|t: &Theme| t.text, |t: &Theme| t.bg_row),
        (|t: &Theme| t.text, |t: &Theme| t.bg_row_sel),
        (|t: &Theme| t.text, |t: &Theme| t.bg_bar),
        (|t: &Theme| t.text, |t: &Theme| t.bg_tooltip),
        (|t: &Theme| t.text, |t: &Theme| t.bg_raised),
        (|t: &Theme| t.text_dim, |t: &Theme| t.bg_panel),
        (|t: &Theme| t.text_dim, |t: &Theme| t.bg_tooltip),
        (|t: &Theme| t.text_link, |t: &Theme| t.bg_bar),
        (|t: &Theme| t.events_text, |t: &Theme| t.bg_panel),
        (|t: &Theme| t.lcd, |t: &Theme| t.bg_input),
        (|t: &Theme| t.lcd, |t: &Theme| t.bg_panel),
        (|t: &Theme| t.accent, |t: &Theme| t.bg_panel),
        (|t: &Theme| t.accent, |t: &Theme| t.bg_lane),
        (|t: &Theme| t.accent, |t: &Theme| t.bg_chip),
        (|t: &Theme| t.warn, |t: &Theme| t.bg_bar),
        (|t: &Theme| t.warn, |t: &Theme| t.bg_row),
        (|t: &Theme| t.warn_alt, |t: &Theme| t.bg_row),
        (|t: &Theme| t.warn_alt, |t: &Theme| t.bg_panel),
        (|t: &Theme| t.danger, |t: &Theme| t.bg_panel),
        (|t: &Theme| t.lane, |t: &Theme| t.bg_lane),
        (|t: &Theme| t.icon_off, |t: &Theme| t.bg_panel),
        (|t: &Theme| t.accent, |t: &Theme| t.accent_bg),
        (|t: &Theme| t.text, |t: &Theme| t.bg_canvas),
        (|t: &Theme| t.text_bright, |t: &Theme| t.bg_raised),
        (|t: &Theme| t.icon_off, |t: &Theme| t.bg_raised),
    ];

    /// Secondary/muted labels — 3:1 floor in the legacy dark palette
    /// (WCAG large-text level; AA for them is tracked by the HC theme).
    const SECONDARY_PAIRS: &[FgBg] = &[
        (|t: &Theme| t.text_muted, |t: &Theme| t.bg_panel),
        (|t: &Theme| t.text_muted, |t: &Theme| t.bg_bar),
        (|t: &Theme| t.text_muted, |t: &Theme| t.bg_row),
        (|t: &Theme| t.text_head, |t: &Theme| t.bg_panel),
        (|t: &Theme| t.text_muted_name, |t: &Theme| t.bg_row),
        (|t: &Theme| t.ch_text, |t: &Theme| t.bg_row),
    ];

    /// Off-state glyphs: exempt from contrast minimums in dark mode
    /// (disabled/secondary affordances) but fully legible in HC.
    const DISABLED_PAIRS: &[FgBg] = &[
        (|t: &Theme| t.state_off, |t: &Theme| t.bg_panel),
        (|t: &Theme| t.state_off, |t: &Theme| t.bg_off),
    ];

    /// Meaningful 1px+ cues — 3:1 in every theme (WCAG non-text).
    const CUE_PAIRS: &[FgBg] = &[
        (|t: &Theme| t.accent, |t: &Theme| t.bg_canvas),
        (|t: &Theme| t.ok, |t: &Theme| t.bg_canvas),
        (|t: &Theme| t.sel, |t: &Theme| t.bg_canvas),
        (|t: &Theme| t.danger, |t: &Theme| t.bg_canvas),
        (|t: &Theme| t.lane, |t: &Theme| t.bg_canvas),
        (|t: &Theme| t.accent_dim, |t: &Theme| t.bg_canvas),
    ];

    /// Decorative hairlines: deliberately subtle in the dark palette,
    /// but must become real separators in HC.
    const HAIRLINE_PAIRS: &[FgBg] = &[
        (|t: &Theme| t.border, |t: &Theme| t.bg_panel),
        (|t: &Theme| t.border, |t: &Theme| t.bg_canvas),
        (|t: &Theme| t.border_strong, |t: &Theme| t.bg_tooltip),
        (|t: &Theme| t.accent_edge, |t: &Theme| t.accent_bg),
        (|t: &Theme| t.accent_edge, |t: &Theme| t.bg_panel),
    ];

    fn check(t: &Theme, pairs: &[FgBg], floor: f64, name: &str) {
        for (i, (fg, bg)) in pairs.iter().enumerate() {
            let r = contrast(fg(t), bg(t));
            assert!(r >= floor, "{name} pair {i} only {r:.2}:1");
        }
    }

    #[test]
    fn dark_theme_text_clears_aa() {
        let t = Theme::dark();
        check(&t, TEXT_PAIRS, 4.5, "dark text");
        check(&t, SECONDARY_PAIRS, 3.0, "dark secondary");
        check(&t, CUE_PAIRS, 3.0, "dark cue");
    }

    #[test]
    fn hc_theme_text_clears_aa() {
        let t = Theme::high_contrast();
        check(&t, TEXT_PAIRS, 4.5, "hc text");
        check(&t, SECONDARY_PAIRS, 4.5, "hc secondary");
        check(&t, DISABLED_PAIRS, 4.5, "hc disabled");
        check(&t, CUE_PAIRS, 3.0, "hc cue");
        check(&t, HAIRLINE_PAIRS, 3.0, "hc hairline");
    }

    #[test]
    fn light_theme_text_clears_aa() {
        let t = Theme::light();
        check(&t, TEXT_PAIRS, 4.5, "light text");
        check(&t, SECONDARY_PAIRS, 3.0, "light secondary");
        check(&t, CUE_PAIRS, 3.0, "light cue");
    }

    #[test]
    fn hc_theme_beats_dark_floor() {
        // The accessible palette must read *at least* as well as default.
        for pairs in [
            TEXT_PAIRS,
            SECONDARY_PAIRS,
            DISABLED_PAIRS,
            CUE_PAIRS,
            HAIRLINE_PAIRS,
        ] {
            for (fg, bg) in pairs {
                let t_hc = Theme::high_contrast();
                let t_dk = Theme::dark();
                let hc = contrast(fg(&t_hc), bg(&t_hc));
                let dk = contrast(fg(&t_dk), bg(&t_dk));
                assert!(hc >= dk - 0.01, "{hc:.2} < {dk:.2}");
            }
        }
    }

    #[test]
    fn resolve_prefers_explicit_hc_override() {
        assert_eq!(
            Theme::resolve(Some(true), ThemeMode::Dark, true),
            Theme::high_contrast()
        );
        assert_eq!(
            Theme::resolve(Some(true), ThemeMode::Light, false),
            Theme::high_contrast()
        );
    }

    #[test]
    fn resolve_maps_modes() {
        // Some(false) pins HC off so mode selection is deterministic.
        assert_eq!(
            Theme::resolve(Some(false), ThemeMode::Dark, false),
            Theme::dark()
        );
        assert_eq!(
            Theme::resolve(Some(false), ThemeMode::Light, true),
            Theme::light()
        );
        assert_eq!(
            Theme::resolve(Some(false), ThemeMode::System, true),
            Theme::dark()
        );
        assert_eq!(
            Theme::resolve(Some(false), ThemeMode::System, false),
            Theme::light()
        );
    }

    #[test]
    fn theme_mode_pref_roundtrips() {
        for m in [ThemeMode::System, ThemeMode::Dark, ThemeMode::Light] {
            assert_eq!(ThemeMode::from_pref(Some(m.name())), m);
        }
        assert_eq!(ThemeMode::from_pref(None), ThemeMode::System);
        assert_eq!(ThemeMode::from_pref(Some("junk")), ThemeMode::System);
    }
}
