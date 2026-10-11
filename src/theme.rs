//! The look of the launcher.
//!
//! A launcher is a control panel, so the visual language comes from hardware:
//! graphite surfaces, hairline separators, engraved labels, and a status lamp
//! that lights up when something is running.
//!
//! Three rules keep it from turning into decoration:
//!
//! 1. **Colour only ever means state.** There is no brand accent; green is
//!    running, grey is stopped, amber is "you have unsaved changes".
//! 2. **Structure comes from hairlines and alignment, not from boxes.** No card
//!    per entry, no border around every group of things.
//! 3. **Two kinds of type, two kinds of content.** Prose and names are set in
//!    the system UI face in two weights; machine values — paths, arguments,
//!    process names, key combinations — are set in the system monospace face, so
//!    they line up and can be compared character by character.
//!
//! The faces themselves come from the operating system (see [`crate::fonts`]);
//! none are shipped with the program.

use std::sync::Arc;

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Stroke,
    TextStyle, Vec2,
};

use crate::fonts;

// ---------------------------------------------------------------------------
// Palette: a blue-shifted graphite, deliberately not near-black.
// ---------------------------------------------------------------------------

/// Window background behind the panels.
pub const BG: Color32 = Color32::from_rgb(0x0D, 0x10, 0x14);
/// Side panels and bars.
pub const SURFACE: Color32 = Color32::from_rgb(0x14, 0x18, 0x1E);
/// Selected or hovered surfaces.
pub const RAISED: Color32 = Color32::from_rgb(0x1D, 0x22, 0x2A);
/// Background of an editable field: darker than its surroundings, so a value
/// reads as inset into the surface rather than stuck onto it.
pub const FIELD: Color32 = Color32::from_rgb(0x0F, 0x13, 0x1A);

/// Hairlines between rows and panels.
pub const LINE: Color32 = Color32::from_rgb(0x24, 0x2A, 0x33);

pub const TEXT: Color32 = Color32::from_rgb(0xE8, 0xEC, 0xF1);
pub const MUTED: Color32 = Color32::from_rgb(0x97, 0xA1, 0xAE);
pub const FAINT: Color32 = Color32::from_rgb(0x61, 0x6B, 0x78);

/// A program that is running.
pub const LIVE: Color32 = Color32::from_rgb(0x57, 0xC0, 0x8A);
/// A program that is not running.
pub const IDLE: Color32 = Color32::from_rgb(0x4C, 0x55, 0x61);
/// Attention, never decoration.
pub const ALERT: Color32 = Color32::from_rgb(0xD9, 0xA2, 0x4B);

/// One radius for everything, small enough to read as a machined edge rather
/// than a rounded card.
pub const RADIUS: u8 = 4;

/// Width of the status rail on the left of every entry.
pub const RAIL: f32 = 2.5;

/// Blends two colours. Used only for the lamp's halo, never for state itself:
/// which colour something *is* stays a decision, not a gradient.
pub fn mix(from: Color32, to: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let channel = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
    Color32::from_rgb(
        channel(from.r(), to.r()),
        channel(from.g(), to.g()),
        channel(from.b(), to.b()),
    )
}

// ---------------------------------------------------------------------------
// Type
// ---------------------------------------------------------------------------

const OS_TEXT: &str = "os-text";
const OS_STRONG: &str = "os-strong";
const OS_MONO: &str = "os-mono";
const STRONG: &str = "strong";

/// The heavier family, for titles, names and buttons.
fn strong_family() -> FontFamily {
    FontFamily::Name(STRONG.into())
}

/// A heavier cut of the UI face. Falls back to the regular cut when the system
/// has no second weight, so callers never have to check.
pub fn semibold(size: f32) -> FontId {
    FontId::new(size, strong_family())
}

/// The regular UI face, for prose and values.
pub fn regular(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}

/// The monospace face, for values that are read as machines write them.
pub fn mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Monospace)
}

/// Type scale: sizes step in roughly a 1.2 ratio so hierarchy reads at a glance.
pub const DISPLAY: f32 = 19.0; // the program's own name
pub const HEADING: f32 = 17.0; // the selected group's name
pub const TITLE: f32 = 15.0; // entry names
pub const BODY: f32 = 13.5; // values, buttons
pub const LABEL: f32 = 12.0; // field labels
pub const CAPTION: f32 = 11.5; // counts, paths, status line

// ---------------------------------------------------------------------------
// Applying it
// ---------------------------------------------------------------------------

/// Installs fonts, spacing, type scale and colours on a context.
///
/// Called once from the app and once from the tests, so both see the same
/// layout — the tests would otherwise measure a UI that never exists.
pub fn apply(ctx: &egui::Context) {
    ctx.set_fonts(fonts());

    // Dark on purpose, whatever the desktop is set to: this palette has no
    // light counterpart, and following the system would silently change it.
    ctx.set_theme(egui::ThemePreference::Dark);

    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Heading, semibold(DISPLAY)),
            (TextStyle::Body, regular(BODY)),
            (TextStyle::Button, semibold(BODY)),
            (TextStyle::Small, regular(CAPTION)),
            (TextStyle::Monospace, mono(BODY)),
        ]
        .into();

        style.spacing.item_spacing = Vec2::new(8.0, 6.0);
        style.spacing.button_padding = Vec2::new(11.0, 4.0);
        style.spacing.interact_size = Vec2::new(24.0, 26.0);
        style.spacing.menu_margin = Margin::same(6);
        style.spacing.window_margin = Margin::same(10);
        style.spacing.scroll.bar_width = 9.0;
        style.spacing.scroll.bar_inner_margin = 3.0;
        style.spacing.scroll.bar_outer_margin = 2.0;

        style.visuals = visuals();
    });
}

/// The system faces, in front of the ones egui carries.
///
/// The built-in fonts stay behind every family: they cover what the system face
/// may lack (emoji, symbols, and the whole family if the OS has nothing to
/// offer), so a missing system font degrades instead of leaving blank boxes.
fn fonts() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    let faces = fonts::os_faces();

    let mut proportional = Vec::new();
    if let Some(face) = &faces.text {
        fonts.font_data.insert(
            OS_TEXT.to_owned(),
            Arc::new(FontData::from_owned(face.bytes.clone())),
        );
        proportional.push(OS_TEXT.to_owned());
    }
    proportional.extend(
        fonts
            .families
            .get(&FontFamily::Proportional)
            .cloned()
            .unwrap_or_default(),
    );

    let mut strong = Vec::new();
    if let Some(face) = &faces.strong {
        fonts.font_data.insert(
            OS_STRONG.to_owned(),
            Arc::new(FontData::from_owned(face.bytes.clone())),
        );
        strong.push(OS_STRONG.to_owned());
    }
    strong.extend(proportional.clone());

    let mut monospace = Vec::new();
    if let Some(face) = &faces.mono {
        fonts.font_data.insert(
            OS_MONO.to_owned(),
            Arc::new(FontData::from_owned(face.bytes.clone())),
        );
        monospace.push(OS_MONO.to_owned());
    }
    monospace.extend(
        fonts
            .families
            .get(&FontFamily::Monospace)
            .cloned()
            .unwrap_or_default(),
    );

    fonts
        .families
        .insert(FontFamily::Proportional, proportional);
    fonts.families.insert(strong_family(), strong);
    fonts.families.insert(FontFamily::Monospace, monospace);

    fonts
}

fn visuals() -> egui::Visuals {
    let mut visuals = egui::Visuals::dark();

    visuals.panel_fill = SURFACE;
    visuals.window_fill = SURFACE;
    visuals.extreme_bg_color = FIELD;
    visuals.faint_bg_color = BG;
    visuals.window_stroke = Stroke::new(1.0, LINE);
    visuals.window_corner_radius = CornerRadius::same(6);
    visuals.selection = egui::style::Selection {
        bg_fill: RAISED,
        stroke: Stroke::new(1.0, TEXT),
    };

    // Fields and buttons: filled, never outlined in their resting state.
    visuals.widgets.inactive = egui::style::WidgetVisuals {
        bg_fill: RAISED,
        weak_bg_fill: RAISED,
        bg_stroke: Stroke::NONE,
        corner_radius: CornerRadius::same(RADIUS),
        fg_stroke: Stroke::new(1.0, TEXT),
        expansion: 0.0,
    };
    visuals.widgets.hovered = egui::style::WidgetVisuals {
        bg_fill: Color32::from_rgb(0x26, 0x2C, 0x36),
        weak_bg_fill: Color32::from_rgb(0x26, 0x2C, 0x36),
        bg_stroke: Stroke::new(1.0, LINE),
        corner_radius: CornerRadius::same(RADIUS),
        fg_stroke: Stroke::new(1.0, TEXT),
        expansion: 0.0,
    };
    visuals.widgets.active = egui::style::WidgetVisuals {
        bg_fill: Color32::from_rgb(0x2E, 0x35, 0x41),
        weak_bg_fill: Color32::from_rgb(0x2E, 0x35, 0x41),
        bg_stroke: Stroke::new(1.0, MUTED),
        corner_radius: CornerRadius::same(RADIUS),
        fg_stroke: Stroke::new(1.0, TEXT),
        expansion: 0.0,
    };
    visuals.widgets.open = visuals.widgets.active;
    visuals.widgets.noninteractive = egui::style::WidgetVisuals {
        bg_fill: SURFACE,
        weak_bg_fill: SURFACE,
        bg_stroke: Stroke::new(1.0, LINE),
        corner_radius: CornerRadius::same(RADIUS),
        fg_stroke: Stroke::new(1.0, MUTED),
        expansion: 0.0,
    };

    visuals
}
