//! Appearance: colour palette (follow Windows, dark, light or a custom background) and the opacity
//! of the bar and the detail panel.
//!
//! Settings live next to the others in HKCU\Software\TaskbarMonitor: `Theme` (0 auto, 1 dark, 2 light,
//! 3 custom), `CustomColor` (COLORREF), `BarOpacity` and `PanelOpacity` (percent).

use std::cell::Cell;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::InvalidateRect;
use windows::Win32::UI::Controls::Dialogs::{ChooseColorW, CC_FULLOPEN, CC_RGBINIT, CHOOSECOLORW};
use windows::Win32::UI::WindowsAndMessaging::{SetLayeredWindowAttributes, LWA_ALPHA};

use crate::gdi::{blend, rgb};
use crate::overlay::{read_dword, write_setting};
use crate::{OVERLAY, POPUP};

const THEME_VALUE: PCWSTR = w!("Theme");
const CUSTOM_VALUE: PCWSTR = w!("CustomColor");
const PERSONALIZE_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    /// Follow the Windows taskbar theme.
    Auto,
    Dark,
    Light,
    Custom,
}

impl Theme {
    fn from_setting(v: Option<u32>) -> Self {
        match v {
            Some(1) => Theme::Dark,
            Some(2) => Theme::Light,
            Some(3) => Theme::Custom,
            _ => Theme::Auto,
        }
    }
}

/// Every colour the bar and the panel draw with.
#[derive(Clone, Copy)]
pub struct Palette {
    pub bar_bg: COLORREF,
    pub bar_label: COLORREF,
    /// Value below the warning threshold; also the panel's primary text.
    pub text: COLORREF,
    pub warn: COLORREF,
    pub hot: COLORREF,
    pub update_dot: COLORREF,
    pub bg: COLORREF,
    pub panel: COLORREF,
    pub track: COLORREF,
    pub div: COLORREF,
    pub dim: COLORREF,
    pub cpu: COLORREF,
    pub ram: COLORREF,
    pub gpu: COLORREF,
    pub vram: COLORREF,
}

const DARK: Palette = Palette {
    bar_bg: rgb(0x1c, 0x1c, 0x1c),
    bar_label: rgb(0xb4, 0xb4, 0xb4),
    text: rgb(0xf2, 0xf2, 0xf2),
    warn: rgb(0xff, 0xd1, 0x66),
    hot: rgb(0xff, 0x6b, 0x6b),
    update_dot: rgb(0x4f, 0xa3, 0xff),
    bg: rgb(0x20, 0x20, 0x20),
    panel: rgb(0x2a, 0x2a, 0x2a),
    track: rgb(0x3a, 0x3a, 0x3a),
    div: rgb(0x34, 0x34, 0x34),
    dim: rgb(0x9a, 0x9a, 0x9a),
    cpu: rgb(0x4f, 0xa3, 0xff),
    ram: rgb(0xb0, 0x7c, 0xff),
    gpu: rgb(0x3d, 0xd6, 0x8c),
    vram: rgb(0xf5, 0xa5, 0x24),
};

/// Modelled on the Windows 11 light taskbar and flyouts. The load colours and the metric accents are
/// darker than their dark-theme twins, which wash out on a light background.
const LIGHT: Palette = Palette {
    bar_bg: rgb(0xf3, 0xf3, 0xf3),
    bar_label: rgb(0x55, 0x55, 0x55),
    text: rgb(0x1a, 0x1a, 0x1a),
    warn: rgb(0x94, 0x62, 0x00),
    hot: rgb(0xc4, 0x2b, 0x1c),
    update_dot: rgb(0x00, 0x67, 0xc0),
    bg: rgb(0xf3, 0xf3, 0xf3),
    panel: rgb(0xe6, 0xe6, 0xe6),
    track: rgb(0xd6, 0xd6, 0xd6),
    div: rgb(0xda, 0xda, 0xda),
    dim: rgb(0x5f, 0x5f, 0x5f),
    cpu: rgb(0x00, 0x67, 0xc0),
    ram: rgb(0x7a, 0x3f, 0xd1),
    gpu: rgb(0x0e, 0x7c, 0x4a),
    vram: rgb(0xb3, 0x5a, 0x00),
};

thread_local! {
    /// Built on first use and after anything that can change it, see `invalidate`.
    static PALETTE: Cell<Option<Palette>> = const { Cell::new(None) };
    /// The colour dialog's 16 custom swatches, kept for the session.
    static SWATCHES: Cell<[COLORREF; 16]> = const { Cell::new([COLORREF(0x00ff_ffff); 16]) };
}

pub fn theme() -> Theme {
    Theme::from_setting(read_dword(crate::overlay::SETTINGS_KEY, THEME_VALUE))
}

fn custom_color() -> COLORREF {
    // Until one is picked, start from the dark bar colour
    COLORREF(read_dword(crate::overlay::SETTINGS_KEY, CUSTOM_VALUE).unwrap_or(DARK.bar_bg.0) & 0x00ff_ffff)
}

/// The Windows taskbar theme. Absent value (older builds, fresh profiles) means dark.
fn windows_uses_light() -> bool {
    read_dword(PERSONALIZE_KEY, w!("SystemUsesLightTheme")) == Some(1)
}

pub fn palette() -> Palette {
    if let Some(p) = PALETTE.get() {
        return p;
    }
    let p = match theme() {
        Theme::Dark => DARK,
        Theme::Light => LIGHT,
        Theme::Auto => {
            if windows_uses_light() {
                LIGHT
            } else {
                DARK
            }
        }
        Theme::Custom => custom(custom_color()),
    };
    PALETTE.set(Some(p));
    p
}

/// Relative luminance (WCAG), 0 = black, 1 = white.
fn luminance(c: COLORREF) -> f32 {
    let lin = |shift: u32| {
        let v = ((c.0 >> shift) & 0xff) as f32 / 255.0;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(0) + 0.7152 * lin(8) + 0.0722 * lin(16)
}

/// Palette for a user-chosen background: the light or the dark palette, whichever has the better
/// contrast with it (black and white are equally contrasted at a luminance of about 0.179), with the
/// panel surfaces nudged from the background toward the text colour like the built-in palettes do.
fn custom(bg: COLORREF) -> Palette {
    let base = if luminance(bg) > 0.179 { LIGHT } else { DARK };
    Palette {
        bar_bg: bg,
        bg,
        panel: blend(base.text, bg, 0.05),
        track: blend(base.text, bg, 0.14),
        div: blend(base.text, bg, 0.10),
        ..base
    }
}

/// Forget the palette and repaint both windows. The bar repaints every second anyway, but the panel
/// only while open, and a change should show at once.
pub fn invalidate() {
    PALETTE.set(None);
    unsafe {
        let _ = InvalidateRect(Some(OVERLAY.get()), None, false);
        let _ = InvalidateRect(Some(POPUP.get()), None, false);
    }
}

pub fn set_theme(t: Theme) {
    write_setting(THEME_VALUE, t as u32);
    invalidate();
}

/// Handle WM_SETTINGCHANGE: Windows announces a light/dark switch with "ImmersiveColorSet".
pub fn on_setting_change(lp: LPARAM) {
    if lp.0 == 0 {
        return;
    }
    let area = unsafe { PCWSTR(lp.0 as *const u16).to_string() };
    if area.is_ok_and(|a| a == "ImmersiveColorSet") {
        invalidate();
    }
}

/// Let the user pick the background colour. Returns false if cancelled, leaving the theme alone.
pub fn pick_custom(owner: HWND) -> bool {
    let mut swatches = SWATCHES.get();
    let mut cc = CHOOSECOLORW {
        lStructSize: size_of::<CHOOSECOLORW>() as u32,
        hwndOwner: owner,
        rgbResult: custom_color(),
        lpCustColors: swatches.as_mut_ptr(),
        Flags: CC_RGBINIT | CC_FULLOPEN,
        ..Default::default()
    };
    // Modal: the nested message loop keeps the bar ticking
    if !unsafe { ChooseColorW(&mut cc) }.as_bool() {
        return false;
    }
    SWATCHES.set(swatches);
    write_setting(CUSTOM_VALUE, cc.rgbResult.0);
    set_theme(Theme::Custom);
    true
}

/// The two windows whose opacity can be changed.
#[derive(Clone, Copy)]
pub enum Surface {
    Bar,
    Panel,
}

impl Surface {
    fn value(self) -> PCWSTR {
        match self {
            Surface::Bar => w!("BarOpacity"),
            Surface::Panel => w!("PanelOpacity"),
        }
    }

    /// 96% of 255 rounds to 245, the bar's alpha before it became adjustable.
    pub fn default_percent(self) -> u32 {
        match self {
            Surface::Bar => 96,
            Surface::Panel => 100,
        }
    }

    /// Choices offered in the menu. The bar lists its default too, so it is never an invisible
    /// state with nothing checked.
    pub fn levels(self) -> &'static [u32] {
        match self {
            Surface::Bar => &[100, 96, 90, 80, 70, 60],
            Surface::Panel => &[100, 90, 80, 70, 60],
        }
    }

    fn hwnd(self) -> HWND {
        match self {
            Surface::Bar => OVERLAY.get(),
            Surface::Panel => POPUP.get(),
        }
    }

    pub fn percent(self) -> u32 {
        // A hand-edited value must not make the window invisible
        read_dword(crate::overlay::SETTINGS_KEY, self.value()).map_or(self.default_percent(), |v| v.clamp(20, 100))
    }

    /// Push the stored opacity to the window. Both windows are layered; the bar is recreated at
    /// times, so this also runs from its `create`.
    pub fn apply(self, hwnd: HWND) {
        let alpha = ((self.percent() * 255 + 50) / 100) as u8;
        unsafe {
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA);
        }
    }

    pub fn set_percent(self, pct: u32) {
        write_setting(self.value(), pct);
        self.apply(self.hwnd());
    }
}
