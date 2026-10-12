//! The icon in the notification area — the launcher's face while its window is
//! closed.
//!
//! Tray mode has two halves, and they are deliberately independent:
//!
//! * **Closing no longer quits.** The close button takes the window off the
//!   screen; the event loop keeps running, so the status stays honest, waiting
//!   delays still come due, and a hotkey still toggles its group.
//! * **A way back.** The icon is what brings the window back, and its menu what
//!   toggles a group or quits the program. Without that way back, hiding the
//!   window would be a trap — which is why [`Tray::is_showing`] is asked before
//!   anything is hidden, and why nothing is hidden where there is no icon.
//!
//! The icon is only put up on Windows. Elsewhere [`Backend::open`] says so
//! instead of pretending, tray mode then does nothing at all, and the window
//! keeps closing the way a window normally does. The other platforms each want
//! an arrangement of their own — a GTK main loop on Linux, a template image on
//! macOS — and none of that can be tried out from here.
//!
//! Nothing in here knows what a group *does*; the icon only reports what was
//! clicked (see [`TrayEvent`]) and the launcher decides.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Mutex, OnceLock};

use eframe::egui;

use crate::config::Group;

// ---------------------------------------------------------------------------
// What the icon can ask for
// ---------------------------------------------------------------------------

/// One thing the user asked for through the icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    /// Bring the window back: a click on the icon itself.
    Show,
    /// Toggle one group of the config: that group's line in the menu.
    ToggleGroup(usize),
    /// Quit the program: the last line of the menu.
    Quit,
}

/// Clicks and menu picks, waiting to be picked up by a frame.
///
/// They arrive on whichever thread the operating system reported them on, so
/// they are handed over a channel and the window is woken up explicitly —
/// the same arrangement as the hotkeys, and for the same reason: while the
/// window is hidden nothing is drawn, and without the wake-up the click would
/// sit in the channel until something else happened to cause a frame.
fn mailbox() -> &'static Mutex<Receiver<TrayEvent>> {
    static MAILBOX: OnceLock<Mutex<Receiver<TrayEvent>>> = OnceLock::new();
    MAILBOX.get_or_init(|| {
        let (sender, receiver) = channel();
        let _ = SENDER.set(sender);
        Mutex::new(receiver)
    })
}

static SENDER: OnceLock<Sender<TrayEvent>> = OnceLock::new();

fn sender() -> Option<&'static Sender<TrayEvent>> {
    SENDER.get()
}

/// Hands one event over to the next frame and wakes it up.
fn wake(ctx: &egui::Context, event: TrayEvent) {
    if let Some(sender) = sender() {
        let _ = sender.send(event);
    }
    ctx.request_repaint();
}

// ---------------------------------------------------------------------------
// The tray
// ---------------------------------------------------------------------------

/// One line of the menu that is worth keeping: a group, as the menu reads it.
/// The index it sits at in this list is the group's index in the config.
type MenuEntry = (String, String);

/// The tray icon of this program, for as long as there is one.
pub struct Tray {
    /// The icon and its menu, once they are up. `None` while tray mode is off,
    /// and wherever the platform has no notification area to put them in.
    backend: Option<Backend>,
    /// A tray that must never put an icon up, whatever the config says: the
    /// tests, which would otherwise fill the notification area of whoever runs
    /// them, and the tests alone.
    never: bool,
    /// Whether the icon is on screen right now. This is what the window asks
    /// before it hides itself: no icon, no hiding.
    showing: bool,
    /// What the menu currently reads, so it is only rebuilt when it would say
    /// something else.
    shown: Option<Vec<MenuEntry>>,
    /// Whether tray mode was on the last time this was looked at. Turning it on
    /// is the moment to report a missing notification area; every frame after
    /// that would be nagging.
    wanted: bool,
    /// A mailbox of this tray's own, in tests only.
    ///
    /// A running program has one mailbox and one tray, so its clicks go through
    /// [`mailbox`]. Tests are many trays at once in one process, and a mailbox
    /// they shared would hand one test's click to another test's frame — which
    /// is exactly what happened while this field did not exist. [`Tray::inbox`]
    /// is the one place that decides which mailbox is meant.
    #[cfg(test)]
    own: Option<(Sender<TrayEvent>, Mutex<Receiver<TrayEvent>>)>,
}

impl Tray {
    /// The tray of this program. Nothing appears yet: the icon is put up the
    /// first time [`Tray::sync`] says that tray mode is on.
    pub fn install() -> Self {
        let _ = mailbox();
        Self {
            backend: None,
            never: false,
            showing: false,
            shown: None,
            wanted: false,
            #[cfg(test)]
            own: None,
        }
    }

    /// A tray that never puts an icon up, for the tests.
    ///
    /// It behaves like a tray whose icon could not be created: no icon, and the
    /// window never hides itself. The tests get this one so that running them
    /// cannot put icons into the notification area of whoever runs them — the
    /// same reason [`crate::hotkey::Registry`] has a detached form that reserves
    /// no keys.
    #[cfg(test)]
    pub fn detached() -> Self {
        let _ = mailbox();
        let (sender, receiver) = channel();
        Self {
            backend: None,
            never: true,
            showing: false,
            shown: None,
            wanted: false,
            own: Some((sender, Mutex::new(receiver))),
        }
    }

    /// A tray that behaves as if its icon were up, for the tests that check what
    /// the window does about being closed. Nothing is ever put up: the answer is
    /// given, not earned.
    #[cfg(test)]
    pub fn pretending() -> Self {
        Self {
            showing: true,
            wanted: true,
            ..Self::detached()
        }
    }

    /// Whether there is really an icon to come back through.
    pub fn is_showing(&self) -> bool {
        self.showing
    }

    /// Brings the icon in line with `enabled` and with the groups it offers, and
    /// returns what the user has to be told about it.
    ///
    /// Called once per frame, so it does nothing at all while nothing changes:
    /// the menu is compared by what it reads, and the icon is only asked to
    /// appear or disappear when that actually changes. That way a rename lands
    /// on the menu immediately without the menu being rebuilt for every
    /// keystroke.
    pub fn sync(&mut self, ctx: &egui::Context, enabled: bool, groups: &[Group]) -> Vec<String> {
        let turned_on = enabled && !self.wanted;
        self.wanted = enabled;

        if self.never {
            return Vec::new();
        }

        if !enabled {
            if self.showing
                && let Some(backend) = &self.backend
            {
                // A hidden icon is not a way back, so the window stops hiding
                // itself on the very next frame.
                let _ = backend.icon.set_visible(false);
            }
            self.showing = false;
            return Vec::new();
        }

        let reading = menu_reading(groups);

        let Some(backend) = &self.backend else {
            return match Backend::open(ctx, &reading) {
                Ok(backend) => {
                    self.backend = Some(backend);
                    self.shown = Some(reading);
                    self.showing = true;
                    Vec::new()
                }
                Err(reason) => {
                    self.showing = false;
                    // Said once, when tray mode is turned on: repeating it every
                    // frame would drown the status line.
                    if turned_on {
                        vec![format!(
                            "{reason} Closing the window therefore keeps quitting the launcher."
                        )]
                    } else {
                        Vec::new()
                    }
                }
            };
        };

        if self.shown.as_ref() != Some(&reading) {
            backend.set_menu(&reading);
            self.shown = Some(reading);
        }
        if !self.showing {
            let _ = backend.icon.set_visible(true);
            self.showing = true;
        }
        Vec::new()
    }

    /// The mailbox this tray reads from: its own in a test, the program's one
    /// anywhere else.
    fn inbox(&self) -> &Mutex<Receiver<TrayEvent>> {
        #[cfg(test)]
        if let Some((_, own)) = &self.own {
            return own;
        }
        mailbox()
    }

    /// What was asked for through the icon since the last call.
    ///
    /// Never blocks and never fails: a frame that cannot get at the mailbox just
    /// sees nothing this time and the events are still there for the next one.
    pub fn events(&self) -> Vec<TrayEvent> {
        let Ok(receiver) = self.inbox().try_lock() else {
            return Vec::new();
        };
        receiver.try_iter().collect()
    }

    /// As if the icon had been clicked. This is how the tests reach the state
    /// logic without a notification area to click on, on the same mailbox their
    /// frames then read.
    #[cfg(test)]
    pub fn trigger(&self, event: TrayEvent) {
        if let Some((own, _)) = &self.own {
            let _ = own.send(event);
            return;
        }
        if let Some(sender) = sender() {
            let _ = sender.send(event);
        }
    }
}

/// What the menu offers, reduced to what it reads: the groups in order, each
/// with the hotkey that goes behind its name. Comparing this is what keeps the
/// menu from being rebuilt when nothing about it changed.
fn menu_reading(groups: &[Group]) -> Vec<MenuEntry> {
    groups
        .iter()
        .map(|group| (group.name.clone(), group.hotkey.trim().to_string()))
        .collect()
}

// ---------------------------------------------------------------------------
// The icon itself: Windows
// ---------------------------------------------------------------------------

/// The menu item ids. Plain strings, because that is all the operating system
/// hands back when something is picked.
const SHOW_ID: &str = "starter:show";
const QUIT_ID: &str = "starter:quit";
/// Prefix of a group's item id, followed by the group's index.
const GROUP_ID: &str = "starter:group:";

/// The icon and the menu behind it.
#[cfg(windows)]
struct Backend {
    icon: tray_icon::TrayIcon,
}

#[cfg(windows)]
impl Backend {
    /// Puts the icon up.
    ///
    /// Has to be called from the thread that runs the event loop: the icon is a
    /// window of its own, and its clicks and its menu arrive as messages on that
    /// thread. `ctx` is kept by the handlers so that a click can wake a window
    /// that is not drawing.
    fn open(ctx: &egui::Context, reading: &[MenuEntry]) -> Result<Self, String> {
        set_handlers(ctx);

        let icon = tray_icon::TrayIconBuilder::new()
            .with_id("starter")
            .with_icon(icon_image(32)?)
            .with_tooltip("Starter")
            // A click on the icon is the shortest way back to the window, so it
            // is not answered with the menu: the menu is on the right button.
            .with_menu_on_left_click(false)
            .with_menu(Box::new(build_menu(reading)?))
            .build()
            .map_err(|err| format!("the notification-area icon could not be put up ({err})"))?;

        Ok(Self { icon })
    }

    /// Replaces the menu, for when the groups changed.
    fn set_menu(&self, reading: &[MenuEntry]) {
        let Ok(menu) = build_menu(reading) else {
            // A menu that cannot be built leaves the old one in place: a stale
            // line in the menu is a smaller problem than an icon whose menu
            // stopped working.
            return;
        };
        self.icon.set_menu(Some(Box::new(menu)));
    }
}

/// Wires the icon's clicks and the menu's picks to the mailbox.
///
/// Both handlers are global, so this is done once per program; setting them
/// again after the icon was taken down and put up a second time simply replaces
/// them.
#[cfg(windows)]
fn set_handlers(ctx: &egui::Context) {
    let clicked = ctx.clone();
    tray_icon::TrayIconEvent::set_event_handler(Some(move |event: tray_icon::TrayIconEvent| {
        // Both a single click and a double click mean "bring it back": the
        // window has nothing else to do with a click.
        if let tray_icon::TrayIconEvent::Click {
            button: tray_icon::MouseButton::Left,
            button_state: tray_icon::MouseButtonState::Up,
            ..
        }
        | tray_icon::TrayIconEvent::DoubleClick {
            button: tray_icon::MouseButton::Left,
            ..
        } = event
        {
            wake(&clicked, TrayEvent::Show);
        }
    }));

    let ctx = ctx.clone();
    tray_icon::menu::MenuEvent::set_event_handler(Some(move |event: tray_icon::menu::MenuEvent| {
        let id = event.id.0.as_str();
        let asked = if id == SHOW_ID {
            Some(TrayEvent::Show)
        } else if id == QUIT_ID {
            Some(TrayEvent::Quit)
        } else {
            id.strip_prefix(GROUP_ID)
                .and_then(|index| index.parse().ok())
                .map(TrayEvent::ToggleGroup)
        };
        if let Some(asked) = asked {
            wake(&ctx, asked);
        }
    }));
}

/// The menu the icon opens: the way back to the window, the groups, and the end.
#[cfg(windows)]
fn build_menu(reading: &[MenuEntry]) -> Result<tray_icon::menu::Menu, String> {
    use tray_icon::menu::{Menu, MenuItem, PredefinedMenuItem};

    let menu = Menu::new();
    let add = |item: &dyn tray_icon::menu::IsMenuItem| {
        menu.append(item)
            .map_err(|err| format!("the notification-area menu could not be built ({err})"))
    };

    add(&MenuItem::with_id(SHOW_ID, "Show window", true, None))?;
    if !reading.is_empty() {
        add(&PredefinedMenuItem::separator())?;
    }
    for (index, (name, hotkey)) in reading.iter().enumerate() {
        // The hotkey is written behind the name rather than registered as an
        // accelerator: the combination is already reserved by the launcher
        // itself, and the menu only has to remind the user of it.
        let text = if hotkey.is_empty() {
            name.clone()
        } else {
            format!("{name}   ({hotkey})")
        };
        add(&MenuItem::with_id(
            format!("{GROUP_ID}{index}"),
            text,
            true,
            None,
        ))?;
    }
    add(&PredefinedMenuItem::separator())?;
    add(&MenuItem::with_id(QUIT_ID, "Quit", true, None))?;

    Ok(menu)
}

/// The picture the notification area shows: the launcher's own status lamp, lit.
///
/// Drawn here instead of shipped as a file, for the same reason the window has
/// no images: one small mark, nothing binary to keep in step with the code. The
/// colours are the window's, so the icon cannot drift away from it — a lit lamp
/// on a graphite plate, which reads on a light taskbar and on a dark one.
#[cfg(windows)]
fn icon_image(size: u32) -> Result<tray_icon::Icon, String> {
    use crate::theme;

    let mut rgba = vec![0u8; (size * size * 4) as usize];
    let center = size as f32 / 2.0 - 0.5;
    let half = size as f32 / 2.0 - 1.0;
    let lamp = size as f32 * 0.26;
    let halo = size as f32 * 0.40;
    let ring = size as f32 * 0.045;

    for y in 0..size {
        for x in 0..size {
            let point = (x as f32 - center, y as f32 - center);
            let mut pixel = [0.0f32; 4];

            // The plate: a square with the window's machined corners, so the mark
            // has a body of its own rather than floating on the taskbar.
            over(
                &mut pixel,
                theme::SURFACE,
                coverage(rounded_box(point, half, size as f32 * 0.21)),
            );
            // The lamp's halo, then the dot inside it — the same two rings the
            // window draws next to something that is running.
            let distance = (point.0 * point.0 + point.1 * point.1).sqrt();
            over(
                &mut pixel,
                theme::mix(theme::SURFACE, theme::LIVE, 0.55),
                coverage((distance - halo).abs() - ring),
            );
            over(&mut pixel, theme::LIVE, coverage(distance - lamp));

            let at = ((y * size + x) * 4) as usize;
            for channel in 0..4 {
                rgba[at + channel] = (pixel[channel].clamp(0.0, 1.0) * 255.0).round() as u8;
            }
        }
    }

    tray_icon::Icon::from_rgba(rgba, size, size)
        .map_err(|err| format!("the notification-area icon could not be drawn ({err})"))
}

/// Distance from a point to a rounded box centred on the origin: negative
/// inside it, positive outside, and in pixels either way.
#[cfg(windows)]
fn rounded_box(point: (f32, f32), half: f32, radius: f32) -> f32 {
    let out = (
        (point.0.abs() - (half - radius)).max(0.0),
        (point.1.abs() - (half - radius)).max(0.0),
    );
    (out.0 * out.0 + out.1 * out.1).sqrt() - radius
}

/// How much of one pixel a shape covers, from its distance to the pixel's
/// centre. One pixel of softness at the edge is what keeps a circle from turning
/// into a staircase at the sixteen pixels Windows really shows.
#[cfg(windows)]
fn coverage(distance: f32) -> f32 {
    (0.5 - distance).clamp(0.0, 1.0)
}

/// Puts one shape on top of what is already in the pixel.
#[cfg(windows)]
fn over(pixel: &mut [f32; 4], color: egui::Color32, alpha: f32) {
    if alpha <= 0.0 {
        return;
    }
    let source = [
        color.r() as f32 / 255.0,
        color.g() as f32 / 255.0,
        color.b() as f32 / 255.0,
    ];
    let below = pixel[3];
    let above = alpha + below * (1.0 - alpha);
    for channel in 0..3 {
        pixel[channel] = (source[channel] * alpha + pixel[channel] * below * (1.0 - alpha)) / above;
    }
    pixel[3] = above;
}

// ---------------------------------------------------------------------------
// The icon itself: everywhere else
// ---------------------------------------------------------------------------

/// There is no icon to put up, and no pretending there is one: the reason is
/// reported when tray mode is switched on, and the window goes on closing the
/// way any window does.
#[cfg(not(windows))]
struct Backend;

#[cfg(not(windows))]
impl Backend {
    fn open(_ctx: &egui::Context, _reading: &[MenuEntry]) -> Result<Self, String> {
        Err("This build has no notification-area icon".to_string())
    }

    fn set_menu(&self, _reading: &[MenuEntry]) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(name: &str, hotkey: &str) -> Group {
        Group {
            name: name.to_string(),
            hotkey: hotkey.to_string(),
            ..Group::default()
        }
    }

    /// What the menu reads is exactly what the config says, in the same order —
    /// the index in this list is the group's index everywhere else.
    #[test]
    fn the_menu_reads_the_groups_in_order() {
        let reading = menu_reading(&[
            group("Everyday", "Ctrl+Alt+E"),
            group("Quiet", "  "),
            group("Editor", "Ctrl+Alt+K"),
        ]);

        assert_eq!(
            reading,
            vec![
                ("Everyday".to_string(), "Ctrl+Alt+E".to_string()),
                ("Quiet".to_string(), String::new()),
                ("Editor".to_string(), "Ctrl+Alt+K".to_string()),
            ]
        );
    }

    /// A tray that puts nothing up is what the tests and the platforms without a
    /// notification area get, and it must stay quiet: no icon, no complaint, and
    /// above all no hiding of the window.
    #[test]
    fn a_detached_tray_stays_out_of_the_way() {
        let ctx = egui::Context::default();
        let mut tray = Tray::detached();

        assert!(
            tray.sync(&ctx, true, &[group("Everyday", "Ctrl+Alt+E")]).is_empty(),
            "a detached tray must not report anything"
        );
        assert!(!tray.is_showing(), "a detached tray shows no icon");
        assert!(tray.events().is_empty());
    }

    /// Events arrive from the operating system on another thread, and a frame
    /// picks them up one by one — in order, and only once.
    #[test]
    fn events_survive_the_handover_and_are_only_delivered_once() {
        let tray = Tray::detached();
        // A test tray reads from a mailbox of its own, so the tests running
        // beside this one cannot leave anything in it — nor take anything out.
        assert!(tray.events().is_empty(), "a fresh tray already had events");

        tray.trigger(TrayEvent::ToggleGroup(2));
        tray.trigger(TrayEvent::Show);

        assert_eq!(tray.events(), vec![TrayEvent::ToggleGroup(2), TrayEvent::Show]);
        assert!(tray.events().is_empty(), "an event was reported twice");
    }
}
