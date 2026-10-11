//! Group hotkeys.
//!
//! One key combination per group, and it always does the same thing: it
//! *toggles* the group. Which way it toggles is decided by the group itself —
//! if any of its programs is running, the hotkey stops the group, and if
//! nothing is running, it starts everything. That way a single key covers both
//! directions and the user never has to remember which state they left it in.
//!
//! Hotkeys are optional in every sense: a group without one is normal, the
//! chord is written as a plain string in the config, and clearing that string
//! removes it.
//!
//! The combination is reserved system-wide, so it works while another program
//! has focus. That is a real reservation — the key is taken away from every
//! other program while the launcher runs — so a chord is only accepted if it
//! has a modifier or is a function key.

use std::collections::HashMap;
use std::fmt;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Mutex, OnceLock};

use eframe::egui;
use global_hotkey::hotkey::{Code, HotKey, Modifiers as GlobalModifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};

use crate::config::Group;

// ---------------------------------------------------------------------------
// Chords
// ---------------------------------------------------------------------------

/// The key half of a combination. Only keys that make sense as a shortcut are
/// modelled; anything else in the config is reported as an unknown key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    Letter(char),
    Digit(char),
    Function(u8),
}

impl Key {
    /// The name this key goes by in the key tables egui and the hotkey crate
    /// use, which is also how it is written in the config.
    fn name(self) -> String {
        match self {
            Key::Letter(letter) => letter.to_ascii_uppercase().to_string(),
            Key::Digit(digit) => digit.to_string(),
            Key::Function(number) => format!("F{number}"),
        }
    }

    /// The key as the operating system knows it.
    fn code(self) -> Option<Code> {
        let name = match self {
            Key::Letter(letter) => format!("Key{}", letter.to_ascii_uppercase()),
            Key::Digit(digit) => format!("Digit{digit}"),
            Key::Function(number) => format!("F{number}"),
        };
        name.parse().ok()
    }

    /// The key as egui knows it, for shortcuts handled inside the window.
    fn egui(self) -> Option<egui::Key> {
        egui::Key::from_name(&self.name())
    }
}

/// A key plus the modifiers held with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Chord {
    pub key: Key,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
}

impl Chord {
    /// Reads a chord from its written form, e.g. `ctrl + alt + W` or `F9`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("no key combination given".to_string());
        }

        let mut chord = Self {
            key: Key::Letter('A'),
            ctrl: false,
            alt: false,
            shift: false,
            win: false,
        };
        let mut key: Option<Key> = None;

        for token in text.split('+') {
            let token = token.trim();
            if token.is_empty() {
                return Err(format!("\"{text}\" has an empty part — write it like Ctrl+Alt+W"));
            }

            if let Some(which) = modifier(token) {
                // Modifiers first, the way the combination is printed back out.
                if key.is_some() {
                    return Err(format!(
                        "\"{text}\": the modifiers come first, like Ctrl+Alt+W"
                    ));
                }
                match which {
                    Modifier::Ctrl => chord.ctrl = true,
                    Modifier::Alt => chord.alt = true,
                    Modifier::Shift => chord.shift = true,
                    Modifier::Win => chord.win = true,
                }
                continue;
            }

            if key.is_some() {
                return Err(format!(
                    "\"{text}\" names two keys — a hotkey has exactly one"
                ));
            }
            key = Some(parse_key(token).ok_or_else(|| {
                format!("\"{token}\" is not a key I know (use A–Z, 0–9 or F1–F24)")
            })?);
        }

        let Some(key) = key else {
            return Err(format!("\"{text}\" has modifiers but no key"));
        };

        chord.key = key;
        chord.check()?;
        Ok(chord)
    }

    /// Reads a chord out of the config, where the string may also be empty:
    /// a group without a hotkey is the normal case, not an error.
    pub fn from_config(text: &str) -> Option<Self> {
        let text = text.trim();
        (!text.is_empty()).then(|| Self::parse(text).ok()).flatten()
    }

    /// The combination a key press inside the window stands for.
    pub fn from_press(key: egui::Key, modifiers: egui::Modifiers) -> Result<Self, String> {
        let name = key.name();
        let Some(key) = parse_key(name) else {
            return Err(format!(
                "\"{name}\" cannot carry a hotkey — use A–Z, 0–9 or F1–F24"
            ));
        };

        let chord = Self {
            key,
            ctrl: modifiers.ctrl,
            alt: modifiers.alt,
            shift: modifiers.shift,
            win: modifiers.mac_cmd,
        };
        chord.check()?;
        Ok(chord)
    }

    pub fn has_modifier(&self) -> bool {
        self.ctrl || self.alt || self.shift || self.win
    }

    /// Whether this combination may be reserved at all.
    ///
    /// A bare letter or digit would be taken from every other program for as
    /// long as the launcher runs. A function key is safe: nobody types it.
    fn check(&self) -> Result<(), String> {
        if self.has_modifier() || matches!(self.key, Key::Function(_)) {
            return Ok(());
        }
        Err(format!(
            "{} on its own would be swallowed in every program — hold Ctrl, Alt, Shift or Win with it",
            self.key.name()
        ))
    }

    /// The combination as reserved with the operating system.
    fn global(&self) -> Option<HotKey> {
        let key = self.key.code()?;
        let mut mods = GlobalModifiers::empty();
        if self.ctrl {
            mods |= GlobalModifiers::CONTROL;
        }
        if self.alt {
            mods |= GlobalModifiers::ALT;
        }
        if self.shift {
            mods |= GlobalModifiers::SHIFT;
        }
        if self.win {
            mods |= GlobalModifiers::SUPER;
        }
        Some(HotKey::new(Some(mods), key))
    }

    /// True when this frame pressed exactly this combination in the window.
    pub fn matches(&self, key: egui::Key, modifiers: egui::Modifiers) -> bool {
        self.key.egui() == Some(key)
            && self.ctrl == modifiers.ctrl
            && self.alt == modifiers.alt
            && self.shift == modifiers.shift
            && self.win == modifiers.mac_cmd
    }
}

impl fmt::Display for Chord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (held, name) in [
            (self.ctrl, "Ctrl"),
            (self.alt, "Alt"),
            (self.shift, "Shift"),
            (self.win, "Win"),
        ] {
            if held {
                write!(f, "{name}+")?;
            }
        }
        write!(f, "{}", self.key.name())
    }
}

/// The named modifiers, so parsing can tell a modifier from a key.
#[derive(Debug, Clone, Copy)]
enum Modifier {
    Ctrl,
    Alt,
    Shift,
    Win,
}

fn modifier(token: &str) -> Option<Modifier> {
    match token.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => Some(Modifier::Ctrl),
        "alt" | "option" => Some(Modifier::Alt),
        "shift" => Some(Modifier::Shift),
        "win" | "super" | "meta" | "cmd" | "command" => Some(Modifier::Win),
        _ => None,
    }
}

fn parse_key(token: &str) -> Option<Key> {
    let mut chars = token.chars();
    let first = chars.next()?.to_ascii_uppercase();
    let rest = chars.as_str();

    match (first, rest) {
        (letter, "") if letter.is_ascii_alphabetic() => Some(Key::Letter(letter)),
        (digit, "") if digit.is_ascii_digit() => Some(Key::Digit(digit)),
        ('F', digits) if !digits.is_empty() => {
            let number: u8 = digits.parse().ok()?;
            (1..=24).contains(&number).then_some(Key::Function(number))
        }
        _ => None,
    }
}

fn hotkey_id(chord: Chord) -> Option<u32> {
    chord.global().map(|hotkey| hotkey.id)
}

// ---------------------------------------------------------------------------
// The bridge from the operating system to the next frame
// ---------------------------------------------------------------------------

/// Pressed hotkeys, waiting to be picked up by a frame.
///
/// The OS reports them on whichever thread owns the hotkey window, so they are
/// handed over a channel and the UI is woken up explicitly; without that, a
/// hotkey pressed while the window is in the background would sit there until
/// the next scheduled repaint.
fn mailbox() -> &'static Mutex<Receiver<u32>> {
    static MAILBOX: OnceLock<Mutex<Receiver<u32>>> = OnceLock::new();
    MAILBOX.get_or_init(|| {
        let (sender, receiver) = channel();
        let _ = SENDER.set(sender);
        Mutex::new(receiver)
    })
}

static SENDER: OnceLock<Sender<u32>> = OnceLock::new();

fn sender() -> Option<&'static Sender<u32>> {
    SENDER.get()
}

// ---------------------------------------------------------------------------
// The registry
// ---------------------------------------------------------------------------

/// Hands the configured chords to the operating system and keeps track of them.
pub struct Registry {
    /// `None` when nothing is reserved with the OS. Either because this is a
    /// detached registry (tests) or because [`Registry::global`] was refused.
    manager: Option<GlobalHotKeyManager>,
    /// Set when the OS would not hand out a hotkey manager at all.
    unavailable: Option<String>,
    /// Group index -> the chord that is live for it.
    registered: HashMap<usize, Chord>,
    /// Hotkey id -> group index, so a press can be traced back to its group.
    owner: HashMap<u32, usize>,
    /// Group index -> why the chord is *not* live, for the ones that failed.
    refusals: HashMap<usize, String>,
}

impl Registry {
    /// Reserves chords system-wide. Must be called on the thread that runs the
    /// event loop; if the OS says no, hotkeys are simply unavailable and the
    /// reason is kept for the user.
    pub fn global(ctx: &egui::Context) -> Self {
        let mut registry = Self::detached();

        match GlobalHotKeyManager::new() {
            Ok(manager) => {
                registry.manager = Some(manager);

                let ctx = ctx.clone();
                GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
                    if event.state == HotKeyState::Pressed {
                        if let Some(sender) = sender() {
                            let _ = sender.send(event.id);
                        }
                        // The point of a global hotkey is that it works while
                        // something else has focus, so the window has to wake up.
                        ctx.request_repaint();
                    }
                }));
            }
            Err(err) => registry.unavailable = Some(err.to_string()),
        }

        registry
    }

    /// A registry that reserves nothing, for tests and headless use. Presses can
    /// still be fed in with [`Registry::trigger`], so the state logic stays
    /// testable without touching the keyboard of whoever runs the tests.
    pub fn detached() -> Self {
        let _ = mailbox();
        Self {
            manager: None,
            unavailable: None,
            registered: HashMap::new(),
            owner: HashMap::new(),
            refusals: HashMap::new(),
        }
    }

    /// Why system-wide hotkeys are not available, if they are not.
    pub fn unavailable(&self) -> Option<&str> {
        self.unavailable.as_deref()
    }

    /// True when this group's chord is reserved with the OS, i.e. it works from
    /// anywhere. When it is false, the same key still works inside the window.
    pub fn is_global(&self, group: usize) -> bool {
        self.manager.is_some() && self.registered.contains_key(&group)
    }

    /// Matches the registry against the config: drops chords that are gone or
    /// changed, reserves the new ones, and returns everything the user should
    /// know about. Called whenever the config changes, not every frame.
    pub fn sync(&mut self, groups: &[Group]) -> Vec<String> {
        let mut notes = Vec::new();
        let mut wanted: Vec<(usize, Chord)> = Vec::new();

        for (index, group) in groups.iter().enumerate() {
            let text = group.hotkey.trim();
            if text.is_empty() {
                continue;
            }
            match Chord::parse(text) {
                Ok(chord) => wanted.push((index, chord)),
                Err(err) => notes.push(format!("Group \"{}\": {err}", group.name)),
            }
        }

        let wanted_by_index: HashMap<usize, Chord> = wanted.iter().copied().collect();

        let stale: Vec<usize> = self
            .registered
            .iter()
            .filter(|(index, chord)| wanted_by_index.get(index) != Some(*chord))
            .map(|(index, _)| *index)
            .collect();
        for index in stale {
            if let Some(chord) = self.registered.remove(&index)
                && let (Some(manager), Some(hotkey)) = (&self.manager, chord.global())
            {
                let _ = manager.unregister(hotkey);
            }
            self.refusals.remove(&index);
        }

        // A press arrives as a bare id, so every wanted chord needs a name for
        // it — including the ones the OS refused, because those are exactly the
        // ones the window itself has to answer.
        self.owner.clear();
        for (index, chord) in &wanted {
            if let Some(id) = hotkey_id(*chord) {
                self.owner.insert(id, *index);
            }
        }

        for (index, chord) in wanted {
            if self.registered.get(&index) == Some(&chord) {
                continue;
            }
            match self.reserve(chord) {
                Ok(()) => {
                    self.registered.insert(index, chord);
                }
                Err(reason) => {
                    notes.push(format!("Group \"{}\": {reason}", groups[index].name));
                    self.refusals.insert(index, reason);
                }
            }
        }

        notes
    }

    fn reserve(&self, chord: Chord) -> Result<(), String> {
        let Some(manager) = &self.manager else {
            // Nothing to reserve with. Either the OS refused the manager (the
            // user has been told once) or this is a detached registry.
            return Ok(());
        };
        let hotkey = chord
            .global()
            .ok_or_else(|| format!("{chord} cannot be reserved system-wide"))?;
        manager
            .register(hotkey)
            .map_err(|err| format!("{chord} could not be reserved ({err})"))
    }

    /// Group indices whose hotkey was pressed since the last call.
    pub fn presses(&mut self) -> Vec<usize> {
        let mailbox = mailbox();
        let Ok(receiver) = mailbox.try_lock() else {
            return Vec::new();
        };
        let ids: Vec<u32> = receiver.try_iter().collect();
        ids.iter()
            .filter_map(|id| self.owner.get(id).copied())
            .collect()
    }

    /// As if the operating system had reported this combination. This is how
    /// the tests reach the state logic without touching a keyboard.
    #[cfg(test)]
    pub fn trigger(&self, chord: Chord) {
        if let (Some(sender), Some(id)) = (sender(), hotkey_id(chord)) {
            let _ = sender.send(id);
        }
    }
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

    /// Recording reads a key press, so the mapping from a pressed key back to a
    /// chord has to refuse exactly what parsing refuses.
    #[test]
    fn a_press_becomes_a_chord_only_when_it_may() {
        let none = egui::Modifiers::default();
        assert!(Chord::from_press(egui::Key::W, none).is_err(), "a bare letter is unsafe");
        assert!(Chord::from_press(egui::Key::Num7, none).is_err(), "a bare digit is unsafe");
        assert!(Chord::from_press(egui::Key::ArrowUp, none).is_err(), "not a key we take");

        assert_eq!(
            Chord::from_press(egui::Key::F9, none).expect("a function key may stand alone"),
            Chord::parse("F9").unwrap()
        );

        let ctrl_alt = egui::Modifiers {
            ctrl: true,
            alt: true,
            ..Default::default()
        };
        assert_eq!(
            Chord::from_press(egui::Key::W, ctrl_alt).expect("this is a fine hotkey"),
            Chord::parse("ctrl+alt+w").unwrap()
        );
    }

    /// A hotkey that is not a hotkey any more has to be ignored, not treated as
    /// an empty one.
    #[test]
    fn a_config_value_is_read_leniently() {
        assert_eq!(Chord::from_config(" ctrl+alt+w "), Chord::parse("ctrl+alt+w").ok());
        assert_eq!(Chord::from_config(""), None);
        assert_eq!(Chord::from_config("ctrl+banana"), None);
    }

    /// Whether the operating system will hand this chord out at all, and — when
    /// a running instance already holds it — whether that instance reserved it.
    /// A measurement, not part of the normal run.
    /// The whole path the program takes at startup, reporting what the system
    /// said. A measurement, not part of the normal run.
    #[test]
    #[ignore = "asks the operating system; run with --ignored --nocapture"]
    fn a_live_registry_says_what_it_did() {
        let ctx = egui::Context::default();
        let mut registry = Registry::global(&ctx);
        println!("manager unavailable: {:?}", registry.unavailable());

        let groups = [group("Keys", "ctrl+alt+f13")];
        let notes = registry.sync(&groups);
        println!("notes: {notes:?}");
        println!("reserved system-wide: {}", registry.is_global(0));
    }

    #[test]
    #[ignore = "asks the operating system; run by hand while an instance is up"]
    fn the_os_hands_out_the_chord() {
        let manager = match GlobalHotKeyManager::new() {
            Ok(manager) => manager,
            Err(err) => {
                println!("no hotkey manager on this system: {err}");
                return;
            }
        };

        let chord = Chord::parse("ctrl+alt+f13").expect("the chord should parse");
        let hotkey = chord.global().expect("the chord should have a system code");
        match manager.register(hotkey) {
            Ok(()) => println!("registered {chord} — nobody else holds it"),
            Err(err) => println!("refused {chord}: {err:?}"),
        }
        let _ = manager.unregister(hotkey);
    }

    #[test]
    fn written_forms_are_understood() {
        let cases = [
            ("ctrl+alt+w", Chord { key: Key::Letter('W'), ctrl: true, alt: true, shift: false, win: false }),
            ("Ctrl+Alt+W", Chord { key: Key::Letter('W'), ctrl: true, alt: true, shift: false, win: false }),
            (" shift + F9 ", Chord { key: Key::Function(9), ctrl: false, alt: false, shift: true, win: false }),
            ("win+q", Chord { key: Key::Letter('Q'), ctrl: false, alt: false, shift: false, win: true }),
            ("super+1", Chord { key: Key::Digit('1'), ctrl: false, alt: false, shift: false, win: true }),
            ("F24", Chord { key: Key::Function(24), ctrl: false, alt: false, shift: false, win: false }),
        ];

        for (text, expected) in cases {
            let parsed = Chord::parse(text).unwrap_or_else(|err| panic!("{text} was rejected: {err}"));
            assert_eq!(parsed, expected, "{text} parsed as {parsed}");
        }
    }

    #[test]
    fn a_chord_prints_the_way_it_is_written() {
        assert_eq!(Chord::parse("ctrl+alt+w").unwrap().to_string(), "Ctrl+Alt+W");
        assert_eq!(Chord::parse("shift+f9").unwrap().to_string(), "Shift+F9");
        assert_eq!(Chord::parse("win+q").unwrap().to_string(), "Win+Q");
    }

    /// Everything that would quietly take a key away from the rest of the
    /// system, or that is simply not a key, must be refused — with a reason the
    /// user can act on.
    #[test]
    fn dangerous_and_unknown_chords_are_refused() {
        for text in [
            "",
            "   ",
            "w",              // no modifier: this is just the letter W
            "1",              // same for a digit
            "ctrl",           // modifiers only
            "ctrl+w+alt",     // two keys
            "ctrl+",          // empty part
            "ctrl+banana",    // not a key
            "ctrl+f25",       // beyond the range
            "ctrl+f0",
        ] {
            let error = Chord::parse(text).expect_err(&format!("{text:?} should have been refused"));
            assert!(!error.trim().is_empty(), "{text:?} was refused without a reason");
        }
    }

    /// The two key tables are pulled in from two different crates; a key that
    /// exists in one and not the other would fail silently at runtime.
    #[test]
    fn every_key_exists_in_both_key_tables() {
        let mut keys: Vec<Key> = ('A'..='Z').map(Key::Letter).collect();
        keys.extend(('0'..='9').map(Key::Digit));
        keys.extend((1..=24).map(Key::Function));

        for key in keys {
            assert!(key.code().is_some(), "{key:?} has no system code");
            assert!(key.egui().is_some(), "{key:?} has no egui key");
        }

        // The supported range is what the config may name; keyboards go to F24
        // and the tables behind both crates go further, so this is the boundary
        // that keeps them from disagreeing.
        assert!(parse_key("F24").is_some());
        assert!(parse_key("F25").is_none());
        assert!(parse_key("F0").is_none());
    }

    #[test]
    fn a_hotkey_is_traced_back_to_its_group() {
        let mut registry = Registry::detached();
        registry.sync(&[group("Work", "ctrl+alt+1"), group("Play", "ctrl+alt+2")]);

        registry.trigger(Chord::parse("ctrl+alt+2").unwrap());
        assert_eq!(registry.presses(), vec![1], "the wrong group was reported");
        assert!(registry.presses().is_empty(), "a press was reported twice");
    }

    #[test]
    fn changing_or_clearing_a_hotkey_takes_effect() {
        let mut registry = Registry::detached();
        registry.sync(&[group("Work", "ctrl+alt+1")]);

        let old = Chord::parse("ctrl+alt+1").unwrap();
        let new = Chord::parse("ctrl+alt+2").unwrap();

        // Clearing it: the group is still there, the key is not.
        registry.sync(&[group("Work", "")]);
        registry.trigger(old);
        assert!(registry.presses().is_empty(), "a cleared hotkey still fires");

        // Setting a different one: only the new key fires.
        registry.sync(&[group("Work", "ctrl+alt+2")]);
        registry.trigger(old);
        registry.trigger(new);
        assert_eq!(registry.presses(), vec![0]);
    }

    #[test]
    fn a_broken_chord_is_reported_and_ignored() {
        let mut registry = Registry::detached();
        let notes = registry.sync(&[group("Work", "ctrl+banana")]);

        assert_eq!(notes.len(), 1, "the broken chord was not reported: {notes:?}");
        assert!(notes[0].contains("banana"), "the note is not helpful: {notes:?}");
        assert!(notes[0].contains("Work"), "the note does not name the group");

        registry.trigger(Chord::parse("ctrl+alt+1").unwrap());
        assert!(registry.presses().is_empty(), "an unrelated key fired a group");
    }

    /// A detached registry reserves nothing, so nothing may claim to be
    /// system-wide — that claim is what decides the in-window fallback.
    #[test]
    fn a_detached_registry_reserves_nothing() {
        let mut registry = Registry::detached();
        registry.sync(&[group("Work", "ctrl+alt+1")]);
        assert!(!registry.is_global(0));
        assert!(registry.unavailable().is_none());
    }
}
