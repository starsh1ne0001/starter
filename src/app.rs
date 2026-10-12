//! The GUI: a group list on the left, one row per program on the right.
//!
//! Three rules keep the layout from ever cutting content off:
//!
//! * rows wrap (`horizontal_wrapped`) instead of running off the right edge,
//! * everything that must stay on one line reserves width measured from the real
//!   widgets rather than from a guessed pixel number, and
//! * every field width is derived from `ui.available_width()`.
//!
//! The parts that only draw (the group rows in the list, the key caps) are
//! painted by hand, where a text that does not fit is elided instead of clipped.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::text::{LayoutJob, TextFormat, TextWrapping};
use eframe::egui::{
    self, Align2, Color32, CornerRadius, Margin, Rect, RichText, Stroke, Vec2,
};

use crate::config::{Config, Group, Program};
use crate::hotkey::{Chord, Registry};
use crate::processes::{self, ProcessManager, Target};
use crate::theme;
use crate::tray::{Tray, TrayEvent};

/// How often the process list is re-read while the window has focus. The status
/// has to be honest to the second there.
const REFRESH_FOCUSED: Duration = Duration::from_secs(1);

/// The same while the window is in the background. A global hotkey re-reads the
/// list on the spot before it decides, so a slower background cadence never
/// makes a hotkey act on stale information — it only keeps the launcher out of
/// the way of whatever else the machine is doing.
const REFRESH_BACKGROUND: Duration = Duration::from_secs(4);

/// Width of the label column in front of the entry fields.
const LABEL_WIDTH: f32 = 66.0;

/// Width of the per-program "Start"/"Stop" toggle, fixed so the button does not
/// change size when the state flips.
const PROGRAM_TOGGLE: f32 = 62.0;

/// Width of the per-group toggle, which carries a longer word.
const GROUP_TOGGLE: f32 = 104.0;

/// Everything a frame wants to do, collected while drawing and applied
/// afterwards. This keeps the drawing code free of borrow-checker puzzles.
#[derive(Debug)]
enum Action {
    Save,
    Reload,
    SelectGroup(usize),
    AddGroup,
    RemoveGroup(usize),
    AddProgram(usize),
    RemoveProgram(usize, usize),
    ToggleProgram(usize, usize),
    ToggleGroup(usize),
    Record(usize),
    SetHotkey(usize, Chord),
    ClearHotkey(usize),
    CancelRecording,
    /// Turn tray mode on or off.
    SetTray(bool),
}

/// What toggling a group means, given how much of it is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GroupToggle {
    StartAll,
    StopAll,
}

/// One rule, used by the group button and by the hotkey alike: as soon as
/// anything of the group is running — or already on its way — toggling stops the
/// group; only a group with nothing running and nothing queued is started.
fn group_toggle(busy: bool) -> GroupToggle {
    if busy {
        GroupToggle::StopAll
    } else {
        GroupToggle::StartAll
    }
}

/// How many of one group's programs are running, from the rules that were built
/// when the config last changed.
fn live_in_group(running: &HashMap<Target, usize>, targets: Option<&[Target]>) -> usize {
    targets
        .map(|targets| {
            targets
                .iter()
                .filter(|target| running.get(*target).is_some_and(|count| *count > 0))
                .count()
        })
        .unwrap_or(0)
}

/// A start or a stop, before or after its delay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Start,
    Stop,
}

impl Step {
    fn verb(self) -> &'static str {
        match self {
            Step::Start => "start",
            Step::Stop => "stop",
        }
    }
}

/// Nothing waits longer than this, whatever the config says.
const MAX_STEP_DELAY: f32 = 3600.0;

/// The delay an entry asks for, in a shape that can be waited out.
///
/// A hand-written file can hold anything, including `nan` and `inf`, and on both
/// of those `Duration::from_secs_f32` panics. The value is cleaned here rather
/// than trusted.
fn delay_of(program: &Program, step: Step) -> f32 {
    let seconds = match step {
        Step::Start => program.start_delay,
        Step::Stop => program.stop_delay,
    };
    if seconds.is_finite() {
        seconds.clamp(0.0, MAX_STEP_DELAY)
    } else {
        0.0
    }
}

/// A wait the way a person says it: whole seconds from one upwards, a tenth of a
/// second below that.
fn delay_text(seconds: f32) -> String {
    if seconds >= 1.0 {
        format!("{:.0} s", seconds.ceil())
    } else {
        format!("{:.1} s", seconds)
    }
}

/// A start or a stop that is waiting for its delay to pass.
///
/// Delays are never slept through. The event loop keeps running and the step is
/// taken when it is due, so the window stays responsive, other entries keep
/// working, and something on its way still shows as such.
#[derive(Debug, Clone, Copy)]
struct Pending {
    due: Instant,
    group: usize,
    index: usize,
    step: Step,
}

impl Pending {
    fn remaining(&self, now: Instant) -> Duration {
        self.due.saturating_duration_since(now)
    }
}

/// How long until the next frame is wanted: the regular refresh, or the moment
/// the earliest waiting step is due, whichever comes first.
///
/// This is what makes a delay land on time while the window is in the
/// background, where the regular refresh is four seconds away — and what keeps a
/// far-off wait from making the window redraw sooner than it has to.
fn next_frame_delay(interval: Duration, pending: &[Pending], now: Instant) -> Duration {
    pending
        .iter()
        .map(|step| step.remaining(now))
        .min()
        .map_or(interval, |until_next| interval.min(until_next))
}

/// What became of one requested step.
#[derive(Debug)]
enum Outcome {
    /// It happened; the text says what to tell the user about it.
    Done(String),
    /// It is waiting for its delay.
    Queued(String),
    /// It could not happen; the text says why.
    Failed(String),
}

/// What one program row has to show: how much of it runs, and what is waiting
/// for its delay.
#[derive(Debug, Clone, Copy, Default)]
struct EntryState {
    running: Option<usize>,
    waiting: Option<(Step, Duration)>,
}

impl EntryState {
    /// `true` while the entry runs or is on its way to starting: that is what
    /// decides whether the button starts or stops.
    fn busy(self) -> bool {
        self.running.is_some() || matches!(self.waiting, Some((Step::Start, _)))
    }
}

/// What is known about one entry right now, from the rules and the queue.
fn entry_state(
    running: &HashMap<Target, usize>,
    target: Option<&Target>,
    pending: &[Pending],
    group: usize,
    index: usize,
) -> EntryState {
    EntryState {
        running: target
            .and_then(|target| running.get(target))
            .copied()
            .filter(|count| *count > 0),
        waiting: pending
            .iter()
            .filter(|step| step.group == group && step.index == index)
            .min_by_key(|step| step.due)
            .map(|step| (step.step, step.remaining(Instant::now()))),
    }
}

/// Whether anything of a group is running, or already on its way. This is what
/// decides the direction of a group toggle.
fn group_is_busy(
    running: &HashMap<Target, usize>,
    targets: Option<&[Target]>,
    pending: &[Pending],
    group: usize,
) -> bool {
    live_in_group(running, targets) > 0
        || pending
            .iter()
            .any(|step| step.group == group && step.step == Step::Start)
}

/// How often the process list is re-read.
fn refresh_interval(focused: bool) -> Duration {
    if focused {
        REFRESH_FOCUSED
    } else {
        REFRESH_BACKGROUND
    }
}

pub struct LauncherApp {
    config: Config,
    config_path: PathBuf,
    manager: ProcessManager,
    hotkeys: Registry,
    /// The icon in the notification area, for as long as tray mode is on. What
    /// it is asked for arrives here, and what it shows is kept in step here.
    tray: Tray,
    /// Running instances per identification rule.
    running: HashMap<Target, usize>,
    /// The rule that identifies each configured program, in the same order as
    /// the groups and their programs. Rebuilt when the config changes, never per
    /// tick.
    targets: Vec<Vec<Target>>,
    /// The same rules, flattened, in the shape the process list wants them.
    all_targets: Vec<Target>,
    /// Set when [`LauncherApp::targets`] no longer matches the config.
    targets_stale: bool,
    /// Starts and stops waiting for their delay, earliest first.
    pending: Vec<Pending>,
    last_refresh: Instant,
    selected_group: usize,
    dirty: bool,
    message: String,
    /// Set when the icon's menu asked to quit.
    ///
    /// One close has to be told from the others: in tray mode the close button
    /// only hides the window, and that is the last thing a program that was
    /// asked to quit should do.
    quitting: bool,

    /// Group whose hotkey is currently being recorded, if any.
    recording: Option<usize>,
    /// Why the last key press could not become a hotkey.
    recording_note: String,

    // Buffers for the "new group" / "new program" rows.
    new_group: String,
    new_name: String,
    new_program: String,
}

impl LauncherApp {
    /// `hotkeys` and `tray` decide what reaches outside the window: the program
    /// passes a live registry and a live tray, tests pass detached ones that
    /// reserve nothing and put nothing up.
    pub fn new(config_path: PathBuf, hotkeys: Registry, tray: Tray) -> Self {
        let (config, message) = match Config::load(&config_path) {
            Ok(config) => (config, String::new()),
            Err(err) => (Config::default(), err),
        };

        let manager = ProcessManager::new();

        let mut app = Self {
            config,
            config_path,
            running: HashMap::new(),
            targets: Vec::new(),
            all_targets: Vec::new(),
            targets_stale: true,
            pending: Vec::new(),
            last_refresh: Instant::now(),
            selected_group: 0,
            dirty: false,
            message,
            hotkeys,
            tray,
            quitting: false,
            recording: None,
            recording_note: String::new(),
            manager,
            new_group: String::new(),
            new_name: String::new(),
            new_program: String::new(),
        };
        app.refresh_now();
        app.sync_hotkeys();
        if app.message.is_empty()
            && let Some(reason) = app.hotkeys.unavailable()
        {
            app.message = format!(
                "System-wide hotkeys are not available ({reason}); they still work while this window is focused."
            );
        }
        app
    }

    /// Re-reads the process list and the running counts.
    fn refresh_now(&mut self) {
        // The rules the config asks about are the same every tick, so they are
        // only rebuilt when the config actually changed. This has to happen
        // before the refresh: whether command lines have to be read at all is
        // decided by the rules.
        self.rebuild_targets_if_stale();
        self.manager.refresh_for(&self.all_targets);
        // The counts are written into the map that already exists instead of a
        // fresh one, so a steady state allocates nothing at all.
        self.manager
            .count_running(&self.all_targets, &mut self.running);
        self.last_refresh = Instant::now();
    }

    fn rebuild_targets_if_stale(&mut self) {
        if !self.targets_stale {
            return;
        }

        self.targets.clear();
        self.all_targets.clear();
        for group in &self.config.groups {
            let mut row = Vec::with_capacity(group.programs.len());
            for program in &group.programs {
                row.push(Target::new(&program.process_name(), &program.match_on));
            }
            self.all_targets.extend(row.iter().cloned());
            self.targets.push(row);
        }

        // Entries for programs that are gone would otherwise keep their key for
        // the lifetime of the window.
        self.running
            .retain(|target, _| self.all_targets.contains(target));
        self.targets_stale = false;
    }

    fn save(&mut self) {
        match self.config.save(&self.config_path) {
            Ok(()) => {
                self.dirty = false;
                self.message = format!("Saved {}.", self.config_path.display());
            }
            Err(err) => self.message = err,
        }
    }

    fn reload(&mut self) {
        match Config::load(&self.config_path) {
            Ok(config) => {
                self.config = config;
                self.dirty = false;
                self.selected_group = 0;
                self.message = format!("Reloaded {}.", self.config_path.display());
            }
            Err(err) => self.message = err,
        }
        // A reload can replace every group and program, so nothing that was
        // waiting for its delay can still be trusted to mean the same entry.
        self.pending.clear();
        self.targets_stale = true;
        self.sync_hotkeys();
        self.refresh_now();
    }

    /// Hands the configured combinations to the operating system again and
    /// reports anything the user should know about.
    ///
    /// Called when the config changes, never per frame.
    fn sync_hotkeys(&mut self) {
        let notes = self.hotkeys.sync(&self.config.groups);
        if !notes.is_empty() {
            self.message = notes.join(" ");
        }
    }

    // -----------------------------------------------------------------------
    // Doing things
    // -----------------------------------------------------------------------

    /// Starts one program and returns what to tell the user about it.
    ///
    /// It deliberately does *not* re-read the process list: starting a group of
    /// ten would otherwise cost ten full reads.
    fn start_one(&mut self, group: usize, index: usize) -> Result<String, String> {
        let Some(program) = self.program(group, index) else {
            return Ok(String::new());
        };
        match processes::spawn(&program.program, &program.args) {
            Ok(pid) => Ok(format!("Started {} (pid {pid}).", program.name)),
            Err(err) => Err(format!("Could not start {}: {err}", program.name)),
        }
    }

    /// Stops every process of one program and returns what to tell the user.
    fn stop_one(&mut self, group: usize, index: usize) -> String {
        let Some(program) = self.program(group, index) else {
            return String::new();
        };
        let target = Target::new(&program.process_name(), &program.match_on);
        match self.manager.kill(&target) {
            0 => format!("{} was not running ({}).", program.name, target.describe()),
            count => format!("Stopped {} — {count} process(es) killed.", program.name),
        }
    }

    fn toggle_program(&mut self, group: usize, index: usize) {
        let step = if self.entry_state(group, index).busy() {
            Step::Stop
        } else {
            Step::Start
        };
        self.message = self.act(group, index, step);
        self.refresh_now();
    }

    /// One rule for the group button and for the hotkey: stop if anything of
    /// this group is running or already on its way, otherwise start everything.
    ///
    /// The process list is re-read first, so the decision is made on the state
    /// as it is *now* — a hotkey pressed while the window was in the background
    /// must not act on a minute-old picture.
    fn toggle_group(&mut self, group: usize) {
        self.refresh_now();
        match group_toggle(self.group_is_busy(group)) {
            GroupToggle::StartAll => self.start_group(group),
            GroupToggle::StopAll => self.stop_group(group),
        }
    }

    /// Starts or stops one program, or queues it when its delay says so, and
    /// returns what to tell the user about it.
    fn act(&mut self, group: usize, index: usize, step: Step) -> String {
        match self.apply_step(group, index, step) {
            Outcome::Done(note) | Outcome::Queued(note) => note,
            Outcome::Failed(reason) => reason,
        }
    }

    /// Starts or stops one program: at once when its moment has come, and at
    /// `at` otherwise.
    ///
    /// The moment is absolute rather than a wait, because a group hands its
    /// entries times that are already lined up one behind the other.
    fn apply_step_at(&mut self, group: usize, index: usize, step: Step, at: Instant) -> Outcome {
        let Some(program) = self.program(group, index) else {
            return Outcome::Done(String::new());
        };

        // A command replaces whatever this entry was waiting for. A wait for the
        // same direction would otherwise pile up, and — the one that must never
        // happen — a program told to stop must not start afterwards anyway.
        self.pending.retain(|queued| {
            !(queued.group == group
                && queued.index == index
                && (queued.step == step || step == Step::Stop))
        });

        let now = Instant::now();
        if at > now {
            self.pending.push(Pending {
                due: at,
                group,
                index,
                step,
            });
            return Outcome::Queued(format!(
                "{} will {} in {}.",
                program.name,
                step.verb(),
                delay_text(at.duration_since(now).as_secs_f32())
            ));
        }

        match self.perform(group, index, step) {
            Some(Ok(note)) => Outcome::Done(note),
            Some(Err(reason)) => Outcome::Failed(reason),
            None => Outcome::Done(String::new()),
        }
    }

    /// [`LauncherApp::apply_step_at`] with the entry's own delay: on its own, the
    /// clock starts when the command does.
    fn apply_step(&mut self, group: usize, index: usize, step: Step) -> Outcome {
        let delay = self
            .program(group, index)
            .map_or(0.0, |program| delay_of(&program, step));
        self.apply_step_at(
            group,
            index,
            step,
            Instant::now() + Duration::from_secs_f32(delay),
        )
    }

    /// Does the step right now, whatever the config says about delays.
    ///
    /// `None` when the entry is gone: the config can change while a step waits.
    fn perform(&mut self, group: usize, index: usize, step: Step) -> Option<Result<String, String>> {
        self.program(group, index)?;
        Some(match step {
            Step::Start => self.start_one(group, index),
            Step::Stop => Ok(self.stop_one(group, index)),
        })
    }

    /// Takes every step whose delay has passed.
    ///
    /// Called once per frame, never as a wait: the window has to keep drawing
    /// and the other entries have to keep working while something is on its way.
    fn run_due_pending(&mut self) {
        let now = Instant::now();
        if !self.pending.iter().any(|step| step.due <= now) {
            return;
        }

        // Earliest first, so the order a group was asked for is kept.
        self.pending.sort_by_key(|step| step.due);
        let mut ready: Vec<Pending> = Vec::new();
        self.pending.retain(|step| {
            if step.due <= now {
                ready.push(*step);
                false
            } else {
                true
            }
        });

        let mut notes: Vec<String> = Vec::new();
        for step in ready {
            match self.perform(step.group, step.index, step.step) {
                Some(Ok(note)) | Some(Err(note)) if !note.is_empty() => notes.push(note),
                _ => {}
            }
        }
        if !notes.is_empty() {
            self.message = notes.join(" ");
        }
        self.refresh_now();
    }

    /// Calls off queued steps: one entry's, or the whole group's. Returns how
    /// many were waiting.
    fn cancel_pending(&mut self, group: usize, index: Option<usize>) -> usize {
        let before = self.pending.len();
        self.pending
            .retain(|step| !(step.group == group && index.is_none_or(|index| step.index == index)));
        before - self.pending.len()
    }

    fn start_group(&mut self, group: usize) {
        let Some(name) = self.config.groups.get(group).map(|group| group.name.clone()) else {
            return;
        };
        let programs = self.config.groups[group].programs.len();
        if programs == 0 {
            self.message = format!("\"{name}\" has no programs yet.");
            return;
        }

        // The entries start one behind the other, never all at the same instant:
        // each one's delay is the gap from the moment the entry before it
        // started. The first one's gap counts from this command, and an entry
        // without a delay adds no gap — it starts together with the one before
        // it, which is what a delay of zero means.
        let mut started = 0usize;
        let mut waiting: Vec<String> = Vec::new();
        let mut failures: Vec<String> = Vec::new();
        let mut at = Instant::now();
        for index in 0..programs {
            let wait = self
                .program(group, index)
                .map_or(0.0, |program| delay_of(&program, Step::Start));
            at += Duration::from_secs_f32(wait);

            match self.apply_step_at(group, index, Step::Start, at) {
                Outcome::Done(_) => started += 1,
                Outcome::Queued(note) => waiting.push(note),
                Outcome::Failed(reason) => failures.push(reason),
            }
        }

        // A wait is named, not just counted: which program is held back, and for
        // how long, is the whole point of it.
        let mut message = format!("Started {started} of {programs} program(s) of \"{name}\".");
        if !waiting.is_empty() {
            message = format!("{message} {}", waiting.join(" "));
        }
        if !failures.is_empty() {
            message = format!("{message} {}", failures.join(" "));
        }
        self.message = message;
        self.refresh_now();
    }

    fn stop_group(&mut self, group: usize) {
        let Some(name) = self.config.groups.get(group).map(|group| group.name.clone()) else {
            return;
        };
        let running = self.group_running(group);
        // A group that is being stopped must not start anything, so whatever is
        // still waiting for its start delay is called off.
        let cancelled = self.cancel_pending(group, None);

        // Stopping is staggered the same way: an entry's stop delay is the gap
        // from the moment the entry before it was stopped.
        let mut waiting: Vec<String> = Vec::new();
        let mut at = Instant::now();
        for index in 0..self.config.groups[group].programs.len() {
            let wait = self
                .program(group, index)
                .map_or(0.0, |program| delay_of(&program, Step::Stop));
            at += Duration::from_secs_f32(wait);

            if let Outcome::Queued(note) = self.apply_step_at(group, index, Step::Stop, at) {
                waiting.push(note);
            }
        }

        let mut message = if running == 0 {
            format!("Nothing of \"{name}\" was running.")
        } else {
            format!(
                "Stopped {running} of {} program(s) of \"{name}\".",
                self.config.groups[group].programs.len()
            )
        };
        if !waiting.is_empty() {
            message = format!("{message} {}", waiting.join(" "));
        }
        if cancelled > 0 {
            message = format!("{message} {cancelled} waiting step(s) called off.");
        }
        self.message = message;
        self.refresh_now();
    }

    fn program(&self, group: usize, index: usize) -> Option<Program> {
        self.config.groups.get(group)?.programs.get(index).cloned()
    }

    /// Everything known about one entry: what runs, and what is waiting.
    fn entry_state(&self, group: usize, index: usize) -> EntryState {
        entry_state(
            &self.running,
            self.targets.get(group).and_then(|row| row.get(index)),
            &self.pending,
            group,
            index,
        )
    }

    /// Whether anything of a group runs or is on its way.
    fn group_is_busy(&self, group: usize) -> bool {
        group_is_busy(
            &self.running,
            self.targets.get(group).map(Vec::as_slice),
            &self.pending,
            group,
        )
    }

    /// How many of a group's programs are running right now. This is what the
    /// group lamp shows and what decides the direction of a toggle.
    fn group_running(&self, group: usize) -> usize {
        self.targets
            .get(group)
            .map(|targets| {
                targets
                    .iter()
                    .filter(|target| self.running.get(*target).is_some_and(|count| *count > 0))
                    .count()
            })
            .unwrap_or(0)
    }

    /// Moves the config from "the file changed" to "the file is dirty": the
    /// names to watch and the reserved hotkeys are refreshed here and nowhere
    /// else.
    fn config_changed(&mut self) {
        self.dirty = true;
        self.targets_stale = true;
    }

    fn apply(&mut self, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::Save => self.save(),
                Action::Reload => self.reload(),
                Action::SelectGroup(index) => self.selected_group = index,

                Action::AddGroup => {
                    let name = self.new_group.trim();
                    let name = if name.is_empty() { "New group" } else { name }.to_string();
                    self.config.groups.push(Group {
                        name,
                        ..Group::default()
                    });
                    self.new_group.clear();
                    self.selected_group = self.config.groups.len() - 1;
                    self.config_changed();
                }

                Action::RemoveGroup(index) => {
                    if index < self.config.groups.len() {
                        let removed = self.config.groups.remove(index);
                        self.message = format!("Removed group {}.", removed.name);
                        self.selected_group =
                            self.selected_group.min(self.config.groups.len().saturating_sub(1));
                        // Queued steps of that group are gone with it, and the
                        // groups behind it move up.
                        self.pending.retain(|step| step.group != index);
                        for step in &mut self.pending {
                            if step.group > index {
                                step.group -= 1;
                            }
                        }
                        self.config_changed();
                        // Its hotkey is gone with it and must be released.
                        self.sync_hotkeys();
                    }
                }

                Action::AddProgram(group) => {
                    let program = Program {
                        name: if self.new_name.trim().is_empty() {
                            self.new_program.trim().to_string()
                        } else {
                            self.new_name.trim().to_string()
                        },
                        program: self.new_program.trim().to_string(),
                        ..Program::default()
                    };

                    if program.is_launchable()
                        && let Some(group) = self.config.groups.get_mut(group)
                    {
                        group.programs.push(program);
                        self.new_name.clear();
                        self.new_program.clear();
                        self.config_changed();
                    } else {
                        self.message = "A program needs at least an executable.".to_string();
                    }
                }

                Action::RemoveProgram(group, index) => {
                    let removed = self.config.groups.get_mut(group).and_then(|entry| {
                        (index < entry.programs.len()).then(|| entry.programs.remove(index))
                    });
                    if let Some(removed) = removed {
                        self.message = format!("Removed {}.", removed.name);
                        // Queued steps belong to the entry they were asked for:
                        // this one's are gone with it, and the entries behind it
                        // move up.
                        self.pending
                            .retain(|step| !(step.group == group && step.index == index));
                        for step in &mut self.pending {
                            if step.group == group && step.index > index {
                                step.index -= 1;
                            }
                        }
                        self.config_changed();
                    }
                }

                Action::ToggleProgram(group, index) => self.toggle_program(group, index),
                Action::ToggleGroup(group) => self.toggle_group(group),

                Action::Record(group) => {
                    self.recording = Some(group);
                    self.recording_note.clear();
                }
                Action::CancelRecording => {
                    self.recording = None;
                    self.recording_note.clear();
                }

                Action::SetHotkey(group, chord) => {
                    if let Some(target) = self.config.groups.get_mut(group) {
                        target.hotkey = chord.to_string();
                        let name = target.name.clone();
                        self.recording = None;
                        self.recording_note.clear();
                        self.message = format!("\"{name}\" toggles with {chord}.");
                        self.dirty = true;
                        self.sync_hotkeys();
                    }
                }

                Action::ClearHotkey(group) => {
                    if let Some(target) = self.config.groups.get_mut(group) {
                        target.hotkey.clear();
                        let name = target.name.clone();
                        self.recording = None;
                        self.recording_note.clear();
                        self.message = format!("\"{name}\" has no hotkey any more.");
                        self.dirty = true;
                        self.sync_hotkeys();
                    }
                }

                Action::SetTray(on) => {
                    self.config.tray = on;
                    self.dirty = true;
                    self.message = if on {
                        "Tray mode on: closing the window hides it instead of quitting."
                    } else {
                        "Tray mode off: closing the window quits the launcher."
                    }
                    .to_string();
                    // Whether the icon really appears is decided where it is put
                    // up, on the next frame — and if it cannot appear, that is
                    // said instead of this.
                }
            }
        }
    }

    /// Turns the hotkeys the operating system reported into actions.
    ///
    /// Kept separate from drawing so the tests can drive it without a window.
    fn hotkey_actions(&mut self) -> Vec<Action> {
        self.hotkeys
            .presses()
            .into_iter()
            .map(Action::ToggleGroup)
            .collect()
    }

    /// Everything the icon asked for, turned into actions, plus the work of
    /// keeping the icon in step with the config.
    ///
    /// Like [`LauncherApp::hotkey_actions`], this is kept out of the drawing
    /// code: nothing here needs a window, and none of it needs a notification
    /// area to exist.
    fn tray_actions(&mut self, ctx: &egui::Context) -> Vec<Action> {
        // Called every frame, and free when nothing changed: the menu is only
        // rebuilt when it would read differently. This is what makes a renamed
        // group appear in the menu without every path that can rename it having
        // to remember the icon.
        let notes = self.tray.sync(ctx, self.config.tray, &self.config.groups);
        if !notes.is_empty() {
            self.message = notes.join(" ");
        }

        let mut actions = Vec::new();
        for event in self.tray.events() {
            match event {
                TrayEvent::Show => {
                    // Back and in front: whoever clicked the icon wants to see
                    // the window, not merely own it.
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                TrayEvent::ToggleGroup(group) => actions.push(Action::ToggleGroup(group)),
                TrayEvent::Quit => {
                    self.quitting = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
        actions
    }

    /// The window's close button.
    ///
    /// With the icon in the notification area, closing only takes the window off
    /// the screen: the launcher keeps running, the hotkeys keep working, and the
    /// icon is the way back. Two closings are let through untouched — the one
    /// the icon's menu asked for, which is what "Quit" means, and the one where
    /// there is no icon at all, because hiding a window with no way back to it
    /// would be a trap.
    fn handle_close(&mut self, ctx: &egui::Context) {
        if self.quitting || !self.tray.is_showing() {
            return;
        }
        if !ctx.input(|input| input.viewport().close_requested()) {
            return;
        }

        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        self.message =
            "Hidden; still running — the notification area brings it back.".to_string();
    }

    /// Combinations that the system did not take still work while the window
    /// has focus. When they are reserved system-wide the key never reaches this
    /// window, so there is nothing to double-fire.
    fn in_window_actions(&self, ui: &egui::Ui) -> Vec<Action> {
        if self.recording.is_some() {
            return Vec::new();
        }

        let pressed = key_presses(ui.ctx());
        if pressed.is_empty() {
            return Vec::new();
        }

        let mut actions = Vec::new();
        for (index, group) in self.config.groups.iter().enumerate() {
            if self.hotkeys.is_global(index) {
                continue;
            }
            let Some(chord) = Chord::from_config(&group.hotkey) else {
                continue;
            };
            if pressed.iter().any(|(key, held)| chord.matches(*key, *held)) {
                actions.push(Action::ToggleGroup(index));
            }
        }
        actions
    }
}

// ---------------------------------------------------------------------------
// Small pieces of the visual language
// ---------------------------------------------------------------------------

/// A status lamp: a lit dot with a halo when on, a hollow ring when off.
/// The one loud element in the design, because state is what this tool is for.
///
/// Only the halo fades in; the dot itself is the state and never a blend, so
/// green always means running rather than "becoming running".
fn lamp(ui: &mut egui::Ui, live: bool, id: egui::Id) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(16.0), egui::Sense::hover());
    let center = rect.center();
    let glow = ui.ctx().animate_bool_with_time(id, live, 0.22);
    let painter = ui.painter();

    if live {
        painter.circle_filled(center, 3.0, theme::LIVE);
        painter.circle_stroke(
            center,
            6.0,
            Stroke::new(1.0, theme::mix(theme::SURFACE, theme::LIVE, 0.5 * glow)),
        );
    } else {
        painter.circle_stroke(center, 3.0, Stroke::new(1.2, theme::IDLE));
    }
}

/// A small dot, used for the unsaved-changes note and the group summary.
fn dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(11.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 2.5, color);
}

/// Text on a single line, elided with an ellipsis instead of cut off at the
/// edge. Used by the rows that are painted by hand.
fn one_line(
    painter: &egui::Painter,
    text: &str,
    font: egui::FontId,
    color: Color32,
    width: f32,
) -> Arc<egui::Galley> {
    let mut job = LayoutJob::single_section(
        text.to_owned(),
        TextFormat {
            font_id: font,
            color,
            ..Default::default()
        },
    );
    job.wrap = TextWrapping {
        max_width: width.max(1.0),
        max_rows: 1,
        break_anywhere: false,
        overflow_character: Some('…'),
    };
    painter.layout_job(job)
}

fn caption(text: &str) -> RichText {
    RichText::new(text)
        .font(theme::regular(theme::CAPTION))
        .color(theme::FAINT)
}

fn section_label(text: &str) -> RichText {
    RichText::new(text)
        .font(theme::regular(theme::LABEL))
        .color(theme::FAINT)
}

/// Width a piece of text really needs.
fn text_width(ui: &egui::Ui, text: &str, font: egui::FontId) -> f32 {
    ui.painter()
        .layout_no_wrap(text.to_owned(), font, theme::TEXT)
        .size()
        .x
}

/// Width a text button really needs, so a row can reserve exactly that much
/// instead of guessing and pushing the button off the edge.
fn button_width(ui: &egui::Ui, text: &str) -> f32 {
    text_width(ui, text, theme::semibold(theme::BODY)) + 2.0 * ui.spacing().button_padding.x
}

/// The width of one of the fixed-size buttons, which never shrinks below the
/// width its longest label needs.
fn fixed_button_width(ui: &egui::Ui, longest_label: &str, minimum: f32) -> f32 {
    button_width(ui, longest_label).max(minimum)
}

/// The width of the one field in a row that gives way when space runs short.
///
/// It is never wider than `maximum` and never narrower than `minimum` — and
/// because the rows wrap, reaching the floor means the widgets behind it move to
/// the next line instead of being pushed out of the window.
fn flexible_width(available: f32, reserved: f32, minimum: f32, maximum: f32) -> f32 {
    (available - reserved).clamp(minimum, maximum)
}

/// The small grey label in front of a field, always the same width and flush
/// with the field, so the values of all rows line up in one column.
fn label_column(ui: &mut egui::Ui, label: &str) {
    ui.allocate_ui_with_layout(
        Vec2::new(LABEL_WIDTH, ui.spacing().interact_size.y),
        egui::Layout::right_to_left(egui::Align::Center),
        |ui| {
            ui.label(section_label(label));
        },
    );
}

/// A field for a value the machine writes: paths, arguments, process names.
fn value_field<'a>(value: &'a mut String, hint: &str, width: f32) -> egui::TextEdit<'a> {
    egui::TextEdit::singleline(value)
        .hint_text(RichText::new(hint).font(theme::mono(theme::BODY)).color(theme::FAINT))
        .font(theme::mono(theme::BODY))
        .desired_width(width)
}

/// A labelled single-line field that grows to the right edge of `ui`.
/// Returns `true` when the text was changed.
fn labelled_field(ui: &mut egui::Ui, label: &str, value: &mut String, hint: &str) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        label_column(ui, label);
        let width = (ui.available_width() - ui.spacing().item_spacing.x).max(60.0);
        changed = ui.add(value_field(value, hint, width)).changed();
    });
    changed
}

/// Like [`labelled_field`], but with a **Browse** button that opens the
/// operating system's file picker.
fn path_field(ui: &mut egui::Ui, label: &str, value: &mut String, hint: &str) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        label_column(ui, label);

        let reserved = button_width(ui, "Browse") + ui.spacing().item_spacing.x;
        let width = (ui.available_width() - reserved).max(60.0);
        changed = ui.add(value_field(value, hint, width)).changed();

        if ui
            .button("Browse")
            .on_hover_text("Pick the program with a file dialog")
            .clicked()
            && let Some(path) = pick_program_file()
        {
            *value = path.display().to_string();
            changed = true;
        }
    });
    changed
}

/// A key cap: the shape of a key on a keyboard, used wherever a combination is
/// shown. Drawn rather than written so it reads as something you press.
fn keycap(ui: &mut egui::Ui, text: &str, hint: &str) -> egui::Response {
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), theme::mono(theme::LABEL), theme::FAINT);
    let size = Vec2::new(galley.size().x + 16.0, 22.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());

    let (fill, stroke, color) = if response.hovered() {
        (theme::RAISED, theme::MUTED, theme::TEXT)
    } else {
        (theme::FIELD, theme::LINE, theme::MUTED)
    };
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(3), fill);
    painter.rect_stroke(
        rect,
        CornerRadius::same(3),
        Stroke::new(1.0, stroke),
        egui::StrokeKind::Inside,
    );
    painter.galley(rect.center() - galley.size() / 2.0, galley, color);

    response.on_hover_text(hint)
}

/// The tray annunciator: a lamp, one word, one thing to click.
///
/// The word is what the switch does; the lamp is what it does it *to*, and it is
/// the same lamp the entries use, because colour has exactly one job here. Lit
/// green means there really is an icon in the notification area; amber means
/// tray mode is on but there is nothing there to bring the window back; a hollow
/// ring means the mode is off. Behind the word sits a wash of that same state
/// colour, so an active tray mode reads from across the window without green
/// having to mean a second thing.
fn tray_switch(ui: &mut egui::Ui, mode: bool, icon_up: bool) -> egui::Response {
    let state = if icon_up {
        Some(theme::LIVE)
    } else if mode {
        Some(theme::ALERT)
    } else {
        None
    };
    let color = state.unwrap_or(theme::TEXT);

    let galley = ui
        .painter()
        .layout_no_wrap("Tray".to_owned(), theme::semibold(theme::BODY), color);

    let padding = Vec2::new(7.0, 3.0);
    let lamp = 16.0;
    let gap = 3.0;
    let size = Vec2::new(
        padding.x * 2.0 + lamp + gap + galley.size().x,
        (galley.size().y + padding.y * 2.0).max(ui.spacing().interact_size.y),
    );
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let painter = ui.painter();

    if let Some(color) = state {
        // Hovering lifts the wash, the way every other control here answers the
        // pointer. The colour is mixed with the surface rather than used neat,
        // so the lamp stays the one thing that *is* the state.
        let strength = if response.hovered() { 0.26 } else { 0.16 };
        painter.rect_filled(
            rect,
            CornerRadius::same(theme::RADIUS),
            theme::mix(theme::SURFACE, color, strength),
        );
    } else if response.hovered() {
        painter.rect_filled(rect, CornerRadius::same(theme::RADIUS), theme::RAISED);
    }

    let center = egui::pos2(rect.left() + padding.x + lamp / 2.0, rect.center().y);
    match state {
        Some(color) => {
            let glow = ui
                .ctx()
                .animate_bool_with_time(egui::Id::new("tray-lamp"), true, 0.22);
            painter.circle_filled(center, 3.0, color);
            painter.circle_stroke(
                center,
                6.0,
                Stroke::new(1.0, theme::mix(theme::SURFACE, color, 0.5 * glow)),
            );
        }
        None => {
            painter.circle_stroke(center, 3.0, Stroke::new(1.2, theme::IDLE));
        }
    }

    painter.galley(
        egui::pos2(
            rect.left() + padding.x + lamp + gap,
            rect.center().y - galley.size().y / 2.0,
        ),
        galley,
        color,
    );

    response
}

/// Opens the operating system's file picker for choosing an executable.
fn pick_program_file() -> Option<PathBuf> {
    let dialog = rfd::FileDialog::new().set_title("Choose a program");
    #[cfg(windows)]
    let dialog = dialog.add_filter("Programs", &["exe"]);
    dialog.pick_file()
}

/// `C:\Tools\thing.exe` -> `thing`
fn file_stem(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// One row of the group list: what the group is called, how it is toggled, and
/// how much of it is running.
fn group_row(
    ui: &mut egui::Ui,
    name: &str,
    chord: Option<Chord>,
    programs: usize,
    running: usize,
    selected: bool,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), 34.0),
        egui::Sense::click(),
    );
    let painter = ui.painter();

    if selected {
        painter.rect_filled(rect, CornerRadius::same(theme::RADIUS), theme::RAISED);
        painter.rect_filled(
            Rect::from_min_size(rect.min, Vec2::new(theme::RAIL, rect.height())),
            CornerRadius::ZERO,
            theme::TEXT,
        );
    } else if response.hovered() {
        painter.rect_filled(
            rect,
            CornerRadius::same(theme::RADIUS),
            theme::mix(theme::RAISED, theme::SURFACE, 0.5),
        );
    }

    // The group's own lamp: is anything in it running?
    let lamp_center = egui::pos2(rect.left() + 15.0, rect.center().y);
    if running > 0 {
        painter.circle_filled(lamp_center, 3.0, theme::LIVE);
    } else {
        painter.circle_stroke(lamp_center, 3.0, Stroke::new(1.2, theme::IDLE));
    }

    // Right-hand end: the number of programs, and the key cap in front of it.
    let count = programs.to_string();
    let count_width = text_width(ui, &count, theme::regular(theme::CAPTION));
    painter.text(
        egui::pos2(rect.right() - 10.0, rect.center().y),
        Align2::RIGHT_CENTER,
        &count,
        theme::regular(theme::CAPTION),
        theme::FAINT,
    );
    let mut name_right = rect.right() - 10.0 - count_width - 8.0;

    if let Some(chord) = chord {
        let text = chord.to_string();
        let chip_width = text_width(ui, &text, theme::mono(theme::CAPTION)) + 10.0;
        let chip = Rect::from_center_size(
            egui::pos2(name_right - chip_width / 2.0, rect.center().y),
            Vec2::new(chip_width, 18.0),
        );
        // The name keeps a minimum of room; a long chord on a narrow list is
        // left out rather than squeezing the name to nothing.
        if chip.left() > rect.left() + 74.0 {
            painter.rect_filled(chip, CornerRadius::same(3), theme::FIELD);
            painter.rect_stroke(
                chip,
                CornerRadius::same(3),
                Stroke::new(1.0, theme::LINE),
                egui::StrokeKind::Inside,
            );
            painter.text(
                chip.center(),
                Align2::CENTER_CENTER,
                &text,
                theme::mono(theme::CAPTION),
                theme::FAINT,
            );
            name_right = chip.left() - 8.0;
        }
    }

    let name_left = rect.left() + 27.0;
    let color = if selected { theme::TEXT } else { theme::MUTED };
    let font = if selected {
        theme::semibold(theme::BODY)
    } else {
        theme::regular(theme::BODY)
    };
    let galley = one_line(painter, name, font, color, (name_right - name_left).max(20.0));
    let height = galley.size().y;
    painter.galley(
        egui::pos2(name_left, rect.center().y - height / 2.0),
        galley,
        color,
    );

    response
}

/// One program: what it is, whether it runs, and everything editable in place.
/// Returns `true` when the user edited one of its fields.
fn program_entry(
    ui: &mut egui::Ui,
    group: usize,
    index: usize,
    program: &mut Program,
    state: EntryState,
    actions: &mut Vec<Action>,
) -> bool {
    let mut edited = false;
    // Only a process means running: something waiting for its delay is not.
    let live = state.running.is_some();

    // The rail and the hairline span the row's box, taken before the frame
    // adds its margins, so they can never reach past it.
    let row_box = ui.available_rect_before_wrap().x_range();

    let row = egui::Frame::new()
        .inner_margin(Margin {
            left: 14,
            right: 4,
            top: 9,
            bottom: 9,
        })
        .show(ui, |ui| {
            // Fill the width so the fields reach the right edge of the row.
            ui.set_width(ui.available_width());

            ui.horizontal_wrapped(|ui| {
                lamp(ui, live, egui::Id::new(("lamp", group, index)));

                // A wait is said in words, because no colour in this palette
                // means "about to": green stays "running" and grey "stopped".
                let (status_text, status_color) = match state.waiting {
                    Some((step, remaining)) => (
                        format!(
                            "{} in {}",
                            match step {
                                Step::Start => "starting",
                                Step::Stop => "stopping",
                            },
                            delay_text(remaining.as_secs_f32())
                        ),
                        theme::MUTED,
                    ),
                    None => match state.running {
                        Some(count) => (format!("running ({count})"), theme::LIVE),
                        None => ("stopped".to_string(), theme::FAINT),
                    },
                };

                // Reserve exactly what the status text and the two buttons
                // need, so the name field gives up the space first.
                let reserved = text_width(ui, &status_text, theme::regular(theme::LABEL))
                    + fixed_button_width(ui, "Start", PROGRAM_TOGGLE)
                    + text_width(ui, "×", theme::regular(theme::BODY))
                    + 2.0 * ui.spacing().button_padding.x
                    + 6.0 * ui.spacing().item_spacing.x;

                let name_width = flexible_width(ui.available_width(), reserved, 56.0, 260.0);
                edited |= ui
                    .add(
                        egui::TextEdit::singleline(&mut program.name)
                            .hint_text("Name")
                            .font(theme::semibold(theme::TITLE))
                            .desired_width(name_width),
                    )
                    .changed();

                ui.label(
                    RichText::new(&status_text)
                        .font(theme::regular(theme::LABEL))
                        .color(status_color),
                );

                // The button says what pressing it does. Something on its way
                // to starting is stopped by the next press, which calls the wait
                // off — the alternative would start it twice.
                let busy = state.busy();
                let ready = program.is_launchable();
                if ui
                    .add_enabled(
                        ready,
                        egui::Button::new(if busy { "Stop" } else { "Start" })
                            .min_size(Vec2::new(PROGRAM_TOGGLE, 0.0)),
                    )
                    .on_hover_text(if busy {
                        "Kill the running processes, or call the wait off"
                    } else {
                        "Launch the program"
                    })
                    .clicked()
                {
                    actions.push(Action::ToggleProgram(group, index));
                }

                if ui
                    .add(egui::Button::new(
                        RichText::new("×")
                            .font(theme::regular(theme::BODY))
                            .color(theme::FAINT),
                    ))
                    .on_hover_text("Take this entry out of the config")
                    .clicked()
                {
                    actions.push(Action::RemoveProgram(group, index));
                }
            });

            ui.add_space(4.0);
            edited |= path_field(ui, "Executable", &mut program.program, "notepad.exe or a full path");
            edited |= labelled_field(ui, "Arguments", &mut program.args, r#"optional, e.g. --profile "work""#);
            edited |= labelled_field(
                ui,
                "Process",
                &mut program.process,
                "optional; the executable's file name by default",
            );
            edited |= labelled_field(
                ui,
                "Match",
                &mut program.match_on,
                "optional; must appear in its command line, e.g. a folder",
            );

            // Waits are machine values, so they are set in the value face and
            // read as numbers rather than as text. The row wraps like every
            // other, so a narrow window pushes it to the next line instead of
            // running out of the row.
            ui.horizontal_wrapped(|ui| {
                label_column(ui, "Delays");
                ui.style_mut().override_font_id = Some(theme::mono(theme::BODY));
                edited |= ui
                    .add(
                        egui::DragValue::new(&mut program.start_delay)
                            .speed(0.1)
                            .range(0.0..=MAX_STEP_DELAY)
                            .max_decimals(1)
                            .suffix(" s"),
                    )
                    .on_hover_text("Wait this long before starting it; 0 means at once")
                    .changed();
                ui.label(caption("before start"));
                ui.add_space(8.0);
                edited |= ui
                    .add(
                        egui::DragValue::new(&mut program.stop_delay)
                            .speed(0.1)
                            .range(0.0..=MAX_STEP_DELAY)
                            .max_decimals(1)
                            .suffix(" s"),
                    )
                    .on_hover_text("Wait this long before stopping it; 0 means at once")
                    .changed();
                ui.label(caption("before stop"));
            });
        });

    // The rail carries the state down the full height of the row.
    let rect = row.response.rect;
    let painter = ui.painter();
    painter.rect_filled(
        Rect::from_min_size(rect.min, Vec2::new(theme::RAIL, rect.height())),
        CornerRadius::ZERO,
        if live { theme::LIVE } else { theme::IDLE },
    );
    painter.hline(row_box, rect.max.y, Stroke::new(1.0, theme::LINE));

    edited
}

/// The window that records a new hotkey for a group.
fn recording_modal(
    ctx: &egui::Context,
    group_name: &str,
    current: Option<Chord>,
    note: &str,
    group: usize,
    actions: &mut Vec<Action>,
) {
    egui::Modal::new(egui::Id::new("hotkey_recording")).show(ctx, |ui| {
        ui.set_width(330.0);

        ui.label(
            RichText::new(format!("Hotkey for \"{group_name}\""))
                .font(theme::semibold(theme::HEADING)),
        );
        ui.add_space(8.0);

        match current {
            Some(chord) => {
                let _ = keycap(ui, &chord.to_string(), "The combination that toggles this group");
            }
            None => {
                ui.label(caption("No hotkey yet."));
            }
        }

        ui.add_space(8.0);
        ui.label(caption(
            "Press the combination you want — for example Ctrl+Alt+W. Esc cancels.",
        ));
        if !note.is_empty() {
            ui.add_space(6.0);
            ui.label(
                RichText::new(note)
                    .font(theme::regular(theme::LABEL))
                    .color(theme::ALERT),
            );
        }

        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if current.is_some() && ui.button("Remove hotkey").clicked() {
                actions.push(Action::ClearHotkey(group));
            }
            if ui.button("Cancel").clicked() {
                actions.push(Action::CancelRecording);
            }
        });
    });
}

impl eframe::App for LauncherApp {
    /// Runs before every [`LauncherApp::paint`] — and, while the window is off
    /// the screen, instead of it: eframe runs no ui pass at all for a window
    /// that is hidden, and calls this half on its own so that the program behind
    /// it stays alive. That is what keeps a hotkey working and a delay coming
    /// due in tray mode.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let actions = self.head(ctx);
        self.apply(actions);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let actions = self.paint(ui);
        self.apply(actions);
    }
}

impl LauncherApp {
    /// One whole frame: the head and then the window.
    ///
    /// This is what the tests drive. eframe itself does the two halves
    /// separately, because it keeps running the head while the window is hidden
    /// — see [`eframe::App`] above.
    #[cfg(test)]
    fn draw(&mut self, ui: &mut egui::Ui) {
        let mut actions = self.head(ui.ctx());
        actions.extend(self.paint(ui));
        self.apply(actions);
    }

    /// Everything of a frame that is not drawing: the process list, the steps
    /// still waiting for their delay, the hotkeys, the icon and the close
    /// button. Returns what the user asked for through any of them.
    ///
    /// Kept apart from [`LauncherApp::paint`] because of what eframe does with a
    /// hidden window: no ui pass, this half only. Anything that has to keep
    /// working with the window off the screen belongs here.
    fn head(&mut self, ctx: &egui::Context) -> Vec<Action> {
        let focused = ctx.input(|input| input.focused);
        let interval = refresh_interval(focused);
        if self.last_refresh.elapsed() >= interval {
            self.refresh_now();
        }
        // A delay has to come due even while the window is hidden, so the next
        // frame is scheduled for the earliest one — never later than the regular
        // refresh.
        self.run_due_pending();
        ctx.request_repaint_after(next_frame_delay(interval, &self.pending, Instant::now()));

        self.handle_close(ctx);

        let mut actions = self.hotkey_actions();
        actions.extend(self.tray_actions(ctx));
        actions
    }

    /// Builds the window. Kept separate from [`eframe::App`] so it can be
    /// exercised headlessly in tests.
    fn paint(&mut self, ui: &mut egui::Ui) -> Vec<Action> {
        // Only here, never in the head: this reads the keys that were typed into
        // the window, and a frame that runs with the window hidden has no keys
        // of its own — it would be looking at the ones from the last visible
        // frame and re-fire them.
        let mut actions = self.in_window_actions(ui);

        self.top_bar(ui, &mut actions);
        self.status_bar(ui);
        self.sidebar(ui, &mut actions);
        self.group_view(ui, &mut actions);

        if let Some(group) = self.recording
            && let Some(target) = self.config.groups.get(group)
        {
            let name = target.name.clone();
            let current = Chord::from_config(&target.hotkey);
            recording_modal(ui.ctx(), &name, current, &self.recording_note, group, &mut actions);
        }

        // While the recording window is open the keys are the point, so the
        // window keeps redrawing quickly instead of waiting for the next status
        // refresh — otherwise a press takes up to a second to show up.
        if self.recording.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }

        // Anything the user just pressed into the recording window becomes the
        // group's hotkey — and anything that cannot be one is explained.
        if let Some(group) = self.recording
            && let Some(&(key, held)) = key_presses(ui.ctx()).last()
        {
            if key == egui::Key::Escape {
                actions.push(Action::CancelRecording);
            } else {
                match Chord::from_press(key, held) {
                    Ok(chord) => actions.push(Action::SetHotkey(group, chord)),
                    Err(reason) => self.recording_note = reason,
                }
            }
        }

        actions
    }

    fn top_bar(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        // Read before the fields are borrowed apart below: whether there is
        // really an icon up is what the Tray annunciator answers with — and it is
        // the same question the close button asks.
        let icon_up = self.tray.is_showing();
        let Self {
            config,
            config_path,
            dirty,
            ..
        } = self;

        egui::Panel::top(egui::Id::new("header")).show(ui, |ui| {
            ui.add_space(9.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Starter").font(theme::semibold(theme::DISPLAY)));

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    for (label, hint, action) in [
                        ("Reload", "Read the config file again", Action::Reload),
                        ("Save", "Write the config file", Action::Save),
                    ] {
                        if ui
                            .selectable_label(false, RichText::new(label).font(theme::regular(theme::BODY)))
                            .on_hover_text(hint)
                            .clicked()
                        {
                            actions.push(action);
                        }
                    }
                    // Tray mode: whether closing the window ends the program or
                    // only takes it off the screen. An annunciator rather than a
                    // plain switch, because it changes what the close button
                    // does and has to be readable at a glance.
                    let tray = config.tray;
                    let hint = match (tray, icon_up) {
                        (true, true) => "Closing the window only hides it; the icon in the notification area brings it back",
                        (true, false) => "Tray mode is on, but this build has no notification-area icon: closing the window still quits",
                        (false, _) => "Keep running in the notification area when the window is closed",
                    };
                    if tray_switch(ui, tray, icon_up)
                        .on_hover_text(hint)
                        .clicked()
                    {
                        actions.push(Action::SetTray(!tray));
                        // The icon is put up by the next frame — the head does
                        // that — so the next frame is asked for now instead of
                        // at the next scheduled refresh.
                        ui.ctx().request_repaint();
                    }

                    if *dirty {
                        ui.label(caption("unsaved"));
                        dot(ui, theme::ALERT);
                    }
                });
            });
            ui.add(
                egui::Label::new(
                    RichText::new(config_path.display().to_string())
                        .font(theme::mono(theme::CAPTION))
                        .color(theme::FAINT),
                )
                .truncate(),
            );
            ui.add_space(7.0);
            ui.separator();
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        let Self { message, hotkeys, .. } = self;
        let unavailable = hotkeys.unavailable().map(str::to_owned);
        let text = if message.is_empty() {
            "Ready.".to_string()
        } else {
            message.clone()
        };

        egui::Panel::bottom(egui::Id::new("status")).show(ui, |ui| {
            ui.add_space(5.0);
            ui.separator();
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let note = if unavailable.is_some() {
                    "hotkeys: only while this window is focused"
                } else {
                    ""
                };
                let reserved = if note.is_empty() {
                    0.0
                } else {
                    text_width(ui, note, theme::regular(theme::CAPTION)) + ui.spacing().item_spacing.x
                };
                let width = (ui.available_width() - reserved).max(40.0);
                ui.add_sized(
                    [width, ui.spacing().interact_size.y],
                    egui::Label::new(caption(&text)).truncate(),
                );
                if let Some(reason) = unavailable {
                    ui.label(caption(note)).on_hover_text(reason);
                }
            });
            ui.add_space(1.0);
        });
    }

    fn sidebar(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let Self {
            config,
            selected_group,
            new_group,
            running,
            targets,
            ..
        } = self;

        egui::Panel::left(egui::Id::new("groups"))
            .resizable(true)
            .default_size(224.0)
            // Beyond this the group list would eat the panel the programs live
            // in; a name and a key cap fit in far less.
            .max_size(300.0)
            .show(ui, |ui| {
                // Controls first, so they can never be pushed off the panel.
                egui::Panel::bottom(egui::Id::new("group_controls")).show(ui, |ui| {
                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(8.0);
                    ui.add(value_field(new_group, "New group", ui.available_width()));
                    ui.add_space(4.0);
                    if ui
                        .add_sized(
                            [ui.available_width(), ui.spacing().interact_size.y],
                            egui::Button::new("Add group"),
                        )
                        .clicked()
                    {
                        actions.push(Action::AddGroup);
                    }
                    if ui
                        .add_enabled(
                            !config.groups.is_empty(),
                            egui::Button::new(
                                RichText::new("Remove group")
                                    .font(theme::regular(theme::BODY))
                                    .color(theme::FAINT),
                            )
                            .min_size(Vec2::new(ui.available_width(), 0.0)),
                        )
                        .on_hover_text("Takes the group out of the config, not off the disk")
                        .clicked()
                    {
                        actions.push(Action::RemoveGroup(*selected_group));
                    }
                    ui.add_space(8.0);
                });

                egui::ScrollArea::vertical()
                    .id_salt("group_list")
                    .show(ui, |ui| {
                        ui.add_space(10.0);
                        ui.label(section_label("Groups"));
                        ui.add_space(5.0);

                        if config.groups.is_empty() {
                            ui.add_space(4.0);
                            ui.label(caption("None yet — add one below."));
                        }

                        for (index, group) in config.groups.iter().enumerate() {
                            let live = live_in_group(
                                running,
                                targets.get(index).map(Vec::as_slice),
                            );
                            let clickable = group_row(
                                ui,
                                &group.name,
                                Chord::from_config(&group.hotkey),
                                group.programs.len(),
                                live,
                                *selected_group == index,
                            );
                            if clickable.clicked() {
                                actions.push(Action::SelectGroup(index));
                            }
                        }
                        ui.add_space(6.0);
                    });
            });
    }

    fn group_view(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let Self {
            config,
            selected_group,
            dirty,
            targets_stale,
            running,
            targets,
            pending,
            new_name,
            new_program,
            ..
        } = self;

        egui::CentralPanel::default().show(ui, |ui| {
            let Some(index) = (!config.groups.is_empty())
                .then(|| (*selected_group).min(config.groups.len() - 1))
            else {
                ui.centered_and_justified(|ui| {
                    ui.label(caption("No groups yet — add one on the left."));
                });
                return;
            };
            *selected_group = index;

            // Adding a program lives in a pinned strip, so it can never be
            // scrolled out of view.
            egui::Panel::bottom(egui::Id::new("new_program")).show(ui, |ui| {
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(8.0);
                ui.label(section_label("Add a program"));
                ui.add_space(5.0);

                ui.horizontal(|ui| {
                    label_column(ui, "Name");
                    let width = ui.available_width();
                    ui.add(value_field(new_name, "optional", width));
                });

                ui.horizontal(|ui| {
                    label_column(ui, "Executable");
                    let reserved = button_width(ui, "Browse")
                        + button_width(ui, "Add")
                        + 2.0 * ui.spacing().item_spacing.x;
                    let width = (ui.available_width() - reserved).max(60.0);
                    ui.add(value_field(new_program, "notepad.exe or a full path", width));

                    if ui.button("Browse").clicked()
                        && let Some(path) = pick_program_file()
                    {
                        if new_name.trim().is_empty() {
                            new_name.push_str(&file_stem(&path));
                        }
                        *new_program = path.display().to_string();
                    }
                    if ui.button("Add").clicked() {
                        actions.push(Action::AddProgram(index));
                    }
                });

                ui.add_space(3.0);
                ui.label(caption(
                    "Arguments, the process name and the match are edited on the entry itself.",
                ));
                ui.add_space(8.0);
            });

            let group = &config.groups[index];
            let chord = Chord::from_config(&group.hotkey);
            let live = live_in_group(running, targets.get(index).map(Vec::as_slice));
            let busy = group_is_busy(
                running,
                targets.get(index).map(Vec::as_slice),
                pending,
                index,
            );
            let programs = group.programs.len();
            // The button says what pressing it does, and something on its way
            // counts as busy — otherwise a second press would start it twice.
            let toggling = group_toggle(busy);

            egui::Panel::top(egui::Id::new("group_header")).show(ui, |ui| {
                ui.add_space(10.0);
                ui.horizontal_wrapped(|ui| {
                    let hotkey_text = chord.map_or_else(|| "Hotkey".to_string(), |chord| chord.to_string());
                    // Exactly what the key cap and the group button take, so the
                    // name gives way first — and if the floor is reached, the two
                    // of them wrap onto the next line rather than leave the box.
                    let reserved = text_width(ui, &hotkey_text, theme::mono(theme::LABEL))
                        + 16.0
                        + fixed_button_width(ui, "Start group", GROUP_TOGGLE)
                        + 2.0 * ui.spacing().item_spacing.x;
                    let width = flexible_width(ui.available_width(), reserved, 64.0, 360.0);

                    if ui
                        .add(
                            egui::TextEdit::singleline(&mut config.groups[index].name)
                                .hint_text("Group name")
                                .font(theme::semibold(theme::HEADING))
                                .frame(egui::Frame::new())
                                .desired_width(width),
                        )
                        .on_hover_text("Rename this group")
                        .changed()
                    {
                        *dirty = true;
                    }

                    let hint = if chord.is_some() {
                        "Change or remove this group's hotkey"
                    } else {
                        "Give this group a key combination that toggles it"
                    };
                    if keycap(ui, &hotkey_text, hint).clicked() {
                        actions.push(Action::Record(index));
                    }

                    if ui
                        .add(
                            egui::Button::new(match toggling {
                                GroupToggle::StartAll => "Start group",
                                GroupToggle::StopAll => "Stop group",
                            })
                            .min_size(Vec2::new(GROUP_TOGGLE, 0.0)),
                        )
                        .on_hover_text(match toggling {
                            GroupToggle::StartAll => "Launch every program of this group",
                            GroupToggle::StopAll => "Stop every running program of this group",
                        })
                        .clicked()
                    {
                        actions.push(Action::ToggleGroup(index));
                    }
                });

                ui.add_space(5.0);
                ui.horizontal(|ui| {
                    dot(ui, if live > 0 { theme::LIVE } else { theme::IDLE });
                    let summary = match (programs, live) {
                        (0, _) => "No programs yet.".to_string(),
                        (total, 0) if busy => {
                            format!("{total} program(s) — waiting for a delay.")
                        }
                        (total, 0) => format!("{total} program(s) — nothing running."),
                        (total, live) => format!("{live} of {total} program(s) running."),
                    };
                    ui.label(caption(&summary));
                });
                ui.add_space(9.0);
                ui.separator();
            });

            egui::ScrollArea::vertical()
                .id_salt("program_list")
                .show(ui, |ui| {
                    let group = &mut config.groups[index];

                    if group.programs.is_empty() {
                        ui.add_space(10.0);
                        ui.label(caption("No programs in this group yet."));
                    }

                    for (program_index, program) in group.programs.iter_mut().enumerate() {
                        // The rule for this entry was built from the config; when
                        // the config was only just edited it may not be there yet,
                        // and the entry simply reads as stopped until it is.
                        let state = entry_state(
                            running,
                            targets.get(index).and_then(|row| row.get(program_index)),
                            pending,
                            index,
                            program_index,
                        );
                        if program_entry(ui, index, program_index, program, state, actions) {
                            *dirty = true;
                            *targets_stale = true;
                        }
                    }
                });
        });
    }
}

/// The keys pressed this frame that could carry a hotkey, each with the
/// modifiers that were held for it.
fn key_presses(ctx: &egui::Context) -> Vec<(egui::Key, egui::Modifiers)> {
    ctx.input(|input| {
        input
            .events
            .iter()
            .filter_map(|event| match event {
                egui::Event::Key {
                    key,
                    pressed: true,
                    repeat: false,
                    modifiers,
                    ..
                } => (!is_modifier(*key)).then_some((*key, *modifiers)),
                _ => None,
            })
            .collect()
    })
}

/// A key whose only job is to modify others. It arrives as a key press of its
/// own, and it is never the answer to "which combination?":
///
/// * the window redraws only when it has to — while it is idle that is once a
///   second — so holding `Ctrl`, `Alt` and pressing `W` quickly delivers all
///   three presses in *one* frame, and
/// * the modifiers have to come from the press itself rather than from the
///   context's modifier state, which by the end of such a batch is already back
///   to whatever happened last (a key release, for instance).
fn is_modifier(key: egui::Key) -> bool {
    use egui::Key::{
        AltLeft, AltRight, ControlLeft, ControlRight, ShiftLeft, ShiftRight, SuperLeft, SuperRight,
    };
    matches!(
        key,
        ShiftLeft | ShiftRight | ControlLeft | ControlRight | AltLeft | AltRight | SuperLeft
            | SuperRight
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every test gets its own directory: they run in parallel, and sharing one
    /// file would let them read and delete each other's config.
    fn temp_config(tag: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("starter-ui-test-{}-{tag}", std::process::id()));
        let path = dir.join("launcher.toml");
        let _ = std::fs::remove_dir_all(&dir);
        (dir, path)
    }

    /// A context with the real theme, so the tests measure the UI that ships.
    fn themed_context() -> egui::Context {
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        ctx
    }

    fn frame_input(size: egui::Vec2) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, size)),
            ..Default::default()
        }
    }

    /// Fails when something is cut off at the window's left or right edge,
    /// which is exactly what "not everything is visible" looks like.
    ///
    /// Two kinds of overflow are ignored on purpose:
    ///
    /// * **Vertical**, because that is just scrolling.
    /// * Shapes clipped by an *inner* clip rectangle. A value that is longer
    ///   than its field is scrolled inside that field, and the panel background
    ///   is painted a few pixels past the scroll area; both are drawn on
    ///   purpose and both have their clip rectangle well inside the window. A
    ///   widget that is pushed out of the window is different: there the clip
    ///   rectangle *is* the window edge, and that is what is caught here.
    fn assert_nothing_is_cut_off(app: &mut LauncherApp, size: egui::Vec2) {
        let ctx = themed_context();
        let window = Rect::from_min_size(egui::Pos2::ZERO, size);
        let slack = 2.0; // strokes and rounding may stick out a hair
        let mut offenders: Vec<String> = Vec::new();
        let mut painted = 0usize;

        // A few frames, because panels settle their sizes over time.
        for _ in 0..3 {
            let output = ctx.run_ui(frame_input(size), |ui| app.draw(ui));

            for clipped in &output.shapes {
                let bounds = clipped.shape.visual_bounding_rect();
                if bounds.width() <= 0.0 || !bounds.is_finite() {
                    continue;
                }
                painted += 1;

                let clip = clipped.clip_rect;
                let at_window_edge = clip.left() <= window.left() + slack
                    || clip.right() >= window.right() - slack;
                let overflows = bounds.left() < clip.left() - slack
                    || bounds.right() > clip.right() + slack;

                if overflows && at_window_edge {
                    offenders.push(format!("{bounds:?} sticks out of the window {window:?}"));
                }
            }

            output.drop_without_applying_deltas();
        }

        assert!(painted > 0, "nothing was painted at {size:?}");
        assert!(
            offenders.is_empty(),
            "content is cut off horizontally at {size:?}: {offenders:#?}"
        );
    }

    /// How much geometry was painted in a given colour, by shape kind.
    ///
    /// Coloured text is not counted: egui bakes the colour of a `RichText` into
    /// the text galley, so it cannot be recovered from the shape.
    #[derive(Debug, Default, PartialEq)]
    struct Painted {
        circles: usize,
        rects: usize,
    }

    fn painted_with(shapes: &[egui::epaint::ClippedShape], color: Color32) -> Painted {
        fn walk(shape: &egui::Shape, color: Color32, painted: &mut Painted) {
            match shape {
                egui::Shape::Vec(items) => {
                    for item in items {
                        walk(item, color, painted);
                    }
                }
                egui::Shape::Circle(circle)
                    if circle.fill == color || circle.stroke.color == color =>
                {
                    painted.circles += 1;
                }
                egui::Shape::Rect(rect)
                    if rect.fill == color || rect.stroke.color == color =>
                {
                    painted.rects += 1;
                }
                _ => {}
            }
        }

        let mut painted = Painted::default();
        for clipped in shapes {
            walk(&clipped.shape, color, &mut painted);
        }
        painted
    }

    /// Runs a few frames and returns what the last one painted.
    fn run_frames(
        ctx: &egui::Context,
        app: &mut LauncherApp,
        frames: usize,
    ) -> Vec<egui::epaint::ClippedShape> {
        let mut shapes = Vec::new();
        for _ in 0..frames {
            // Keep the automatic refresh from wiping a state the test set up.
            app.last_refresh = Instant::now();
            let output = ctx.run_ui(frame_input(Vec2::new(880.0, 600.0)), |ui| app.draw(ui));
            shapes = output.shapes.clone();
            output.drop_without_applying_deltas();
        }
        shapes
    }

    /// Writes a config, so a test can decide exactly what the launcher sees.
    fn write_config(path: &Path, config: &Config) {
        config.save(path).expect("test config could not be written");
    }

    /// The sleeper the process tests use: short enough that a leaked one is gone
    /// by itself, long enough to be found and stopped. The last value is a piece
    /// of its command line, for the tests that identify a process by more than
    /// its name.
    #[cfg(windows)]
    fn sleeper() -> (String, String, String, String) {
        (
            "ping".to_string(),
            "-n 15 127.0.0.1".to_string(),
            "ping.exe".to_string(),
            "127.0.0.1".to_string(),
        )
    }
    #[cfg(unix)]
    fn sleeper() -> (String, String, String, String) {
        (
            "sleep".to_string(),
            "60".to_string(),
            "sleep".to_string(),
            "60".to_string(),
        )
    }

    /// The row's own arithmetic: the field that gives way must not be the reason
    /// a button ends up outside the window.
    #[test]
    fn the_field_gives_way_instead_of_pushing_buttons_out() {
        // What the key cap and the group button take on a narrow row.
        let reserved = 240.0;

        for available in [0.0, 60.0, 120.0, 239.0, 240.0, 304.0, 400.0, 620.0, 1400.0] {
            let width = flexible_width(available, reserved, 64.0, 360.0);

            assert!(
                (64.0..=360.0).contains(&width),
                "a field may not be {width} wide"
            );
            if width + reserved > available {
                // Down at the floor the row is out of room, and that is exactly
                // when it wraps instead of squeezing the buttons.
                assert_eq!(width, 64.0, "only the floor may overflow, at {available}");
            }
        }
    }

    #[test]
    fn draws_a_full_frame() {
        let (dir, path) = temp_config("full-frame");
        let mut app = LauncherApp::new(path, Registry::detached(), Tray::detached());
        let ctx = themed_context();

        for _ in 0..3 {
            let output = ctx.run_ui(frame_input(Vec2::new(880.0, 600.0)), |ui| app.draw(ui));
            output.drop_without_applying_deltas();
        }

        assert_eq!(app.config.groups.len(), 2, "example config should be loaded");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The system faces are loaded from disk; if that ever fails, egui silently
    /// falls back to its own and the whole type hierarchy quietly collapses.
    #[test]
    fn the_system_typefaces_are_really_used() {
        let ctx = themed_context();
        let mut widths = Vec::new();

        let output = ctx.run_ui(frame_input(Vec2::new(600.0, 400.0)), |ui| {
            for font in [
                theme::regular(14.0),
                theme::semibold(14.0),
                theme::mono(14.0),
            ] {
                let galley = ui
                    .painter()
                    .layout_no_wrap("Starter Handbuch".to_owned(), font, Color32::WHITE);
                widths.push(galley.size().x);
            }
        });
        output.drop_without_applying_deltas();

        let [regular, strong, mono] = widths[..] else {
            panic!("the fonts were not measured: {widths:?}");
        };
        assert!(regular > 0.0, "no text was measured at all");

        // On Windows both weights and a monospace face are part of the OS; the
        // candidate list in `fonts` is what has to find them.
        #[cfg(windows)]
        {
            assert_ne!(
                regular, strong,
                "regular and heavy measured the same width, so only one face is loading"
            );
            assert_ne!(
                regular, mono,
                "the monospace face is not loading, so values cannot line up"
            );
        }
    }

    /// The palette promises that colour only ever means state. This is the
    /// promise it would be easiest to break without noticing.
    #[test]
    fn colour_tracks_state_and_nothing_else() {
        let (dir, path) = temp_config("colour");
        write_config(
            &path,
            &Config {
                groups: vec![Group {
                    name: "Test".to_string(),
                    programs: vec![Program {
                        name: "Impossible".to_string(),
                        program: "starter-test-never-running.exe".to_string(),
                        ..Program::default()
                    }],
                    ..Group::default()
                }],
                ..Config::default()
            },
        );
        let mut app = LauncherApp::new(path, Registry::detached(), Tray::detached());
        let ctx = themed_context();

        let stopped = painted_with(&run_frames(&ctx, &mut app, 3), theme::LIVE);
        assert_eq!(
            stopped,
            Painted::default(),
            "nothing is running, so nothing may be green"
        );

        // Pretend the program is running. Exactly four things turn green, and
        // each of them says "this is running":
        let target = app.targets[0][0].clone();
        app.running.insert(target, 1);

        let running = painted_with(&run_frames(&ctx, &mut app, 3), theme::LIVE);
        assert_eq!(
            running.circles, 3,
            "expected the entry lamp, the group lamp and the group summary dot"
        );
        assert_eq!(running.rects, 1, "expected the entry's status rail");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn nothing_is_cut_off_at_any_window_size() {
        let (dir, path) = temp_config("clipping");
        write_config(
            &path,
            &Config {
                groups: vec![Group {
                    name: "A group with a fairly long name".to_string(),
                    hotkey: "Ctrl+Alt+Shift+W".to_string(),
                    programs: vec![Program {
                        name: "A program with a long name".to_string(),
                        program: r"C:\Program Files\Something Quite Long\program.exe".to_string(),
                        args: "--profile default".to_string(),
                        process: "program.exe".to_string(),
                        match_on: r"C:\Program Files\Something Quite Long\node_modules".to_string(),
                        start_delay: 30.0,
                        stop_delay: 12.5,
                    }],
                }],
                ..Config::default()
            },
        );
        let mut app = LauncherApp::new(path, Registry::detached(), Tray::detached());

        for size in [
            Vec2::new(880.0, 600.0), // default window
            Vec2::new(620.0, 400.0), // smallest window we allow
            Vec2::new(224.0 + 320.0, 300.0), // sidebar at its widest share
            Vec2::new(1400.0, 900.0),
            Vec2::new(620.0, 900.0), // tall and narrow
            Vec2::new(1400.0, 400.0), // wide and short
        ] {
            assert_nothing_is_cut_off(&mut app, size);
        }

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_group_toggles_by_its_state() {
        assert_eq!(group_toggle(false), GroupToggle::StartAll);
        assert_eq!(group_toggle(true), GroupToggle::StopAll);
    }

    /// Hand-written delays can be anything, including values that would make
    /// `Duration::from_secs_f32` panic.
    #[test]
    fn delays_are_cleaned_before_they_are_waited_out() {
        let program = Program {
            start_delay: f32::NAN,
            stop_delay: f32::INFINITY,
            ..Program::default()
        };
        assert_eq!(delay_of(&program, Step::Start), 0.0, "`nan` must not be waited out");
        assert_eq!(delay_of(&program, Step::Stop), 0.0);

        let program = Program {
            start_delay: -5.0,
            stop_delay: 10_000.0,
            ..Program::default()
        };
        assert_eq!(delay_of(&program, Step::Start), 0.0, "a negative wait is no wait");
        assert_eq!(delay_of(&program, Step::Stop), MAX_STEP_DELAY);

        assert_eq!(delay_text(0.4), "0.4 s");
        assert_eq!(delay_text(2.0), "2 s");
        assert_eq!(delay_text(2.5), "3 s", "a wait is never rounded down");
    }

    /// A wait has to wake the window by itself. Without this, a delay asked for
    /// in the background would land up to four seconds late.
    #[test]
    fn a_wait_decides_when_the_next_frame_is_drawn() {
        let background = Duration::from_secs(4);
        let now = Instant::now();
        let step = |after: Duration| Pending {
            due: now + after,
            group: 0,
            index: 0,
            step: Step::Start,
        };

        assert_eq!(next_frame_delay(background, &[], now), background);

        let soon = step(Duration::from_millis(300));
        assert_eq!(
            next_frame_delay(background, &[soon], now),
            Duration::from_millis(300),
            "the window has to wake up for the wait, background or not"
        );

        let later = step(Duration::from_secs(30));
        assert_eq!(
            next_frame_delay(background, &[later, soon], now),
            Duration::from_millis(300),
            "the earliest wait decides"
        );
        assert_eq!(
            next_frame_delay(background, &[later], now),
            background,
            "a far-off wait must not make the window redraw sooner than it has to"
        );
    }

    /// Draws frames until `condition` holds, and gives up after ten seconds.
    ///
    /// A delay is a real wait, so a test has to let the clock pass while the
    /// window keeps drawing — which is exactly what the launcher does.
    fn wait_until(
        ctx: &egui::Context,
        app: &mut LauncherApp,
        condition: impl Fn(&LauncherApp) -> bool,
        what: &str,
    ) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if condition(app) {
                return;
            }
            run_frames(ctx, app, 1);
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("{what} (last message: {})", app.message);
    }

    /// A group whose one program waits `start_delay` before it starts and
    /// `stop_delay` before it stops.
    fn config_with_delays(tag: &str, start_delay: f32, stop_delay: f32) -> (PathBuf, PathBuf) {
        let (dir, path) = temp_config(tag);
        let (program, args, process, _) = sleeper();
        write_config(
            &path,
            &Config {
                groups: vec![Group {
                    name: "Slow".to_string(),
                    hotkey: String::new(),
                    programs: vec![Program {
                        name: "Lingering".to_string(),
                        program,
                        args,
                        process,
                        start_delay,
                        stop_delay,
                        ..Program::default()
                    }],
                }],
                ..Config::default()
            },
        );
        (dir, path)
    }

    /// A start delay is a wait, not a stall: the window keeps drawing, the wait
    /// is reported, and the program starts when its time has come.
    #[test]
    fn a_start_delay_holds_a_program_back() {
        let _guard = processes::process_test_lock();
        let (dir, path) = config_with_delays("start-delay", 0.8, 0.0);
        let mut app = LauncherApp::new(path, Registry::detached(), Tray::detached());
        let ctx = themed_context();

        app.apply(vec![Action::ToggleGroup(0)]);
        assert_eq!(
            app.group_running(0),
            0,
            "the delay was skipped: {}",
            app.message
        );
        assert!(
            app.message.contains("will start in"),
            "a wait has to be reported: {}",
            app.message
        );
        assert!(
            app.entry_state(0, 0).busy(),
            "something on its way counts as busy, or a second press would start it twice"
        );

        wait_until(
            &ctx,
            &mut app,
            |app| app.group_running(0) == 1,
            "the delayed start never happened",
        );

        // The same rule as ever: now that something runs, the toggle stops it.
        app.apply(vec![Action::ToggleGroup(0)]);
        wait_until(
            &ctx,
            &mut app,
            |app| app.group_running(0) == 0,
            "the sleeper was not stopped",
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Three sleepers that share their process name and differ in their
    /// arguments, so each entry can be recognised on its own.
    #[cfg(windows)]
    fn three_distinct_sleepers() -> [(String, String, String, String); 3] {
        [1, 2, 3].map(|last| {
            (
                "ping".to_string(),
                format!("-n 15 127.0.0.{last}"),
                "ping.exe".to_string(),
                format!("127.0.0.{last}"),
            )
        })
    }

    #[cfg(unix)]
    fn three_distinct_sleepers() -> [(String, String, String, String); 3] {
        ["60", "75", "90"].map(|seconds| {
            (
                "sleep".to_string(),
                seconds.to_string(),
                "sleep".to_string(),
                seconds.to_string(),
            )
        })
    }

    /// A group staggers its starts: an entry goes when the one before it has
    /// started, plus its own delay — not all together when the command arrives,
    /// and not waiting for anything to be *ready* either.
    #[test]
    fn a_group_staggers_its_starts() {
        let _guard = processes::process_test_lock();
        let (dir, path) = temp_config("staggered");

        let programs: Vec<Program> = three_distinct_sleepers()
            .iter()
            .enumerate()
            .map(|(index, (program, args, process, match_on))| Program {
                name: format!("Sleeper {index}"),
                program: program.clone(),
                args: args.clone(),
                process: process.clone(),
                match_on: match_on.clone(),
                // The first goes at once, the others follow half a second apart.
                start_delay: if index == 0 { 0.0 } else { 0.6 },
                ..Program::default()
            })
            .collect();

        write_config(
            &path,
            &Config {
                groups: vec![Group {
                    name: "Ladder".to_string(),
                    hotkey: String::new(),
                    programs,
                }],
                ..Config::default()
            },
        );
        let mut app = LauncherApp::new(path, Registry::detached(), Tray::detached());
        let ctx = themed_context();

        let commanded = Instant::now();
        app.apply(vec![Action::ToggleGroup(0)]);
        assert_eq!(
            app.group_running(0),
            1,
            "only the first entry may start at once: {}",
            app.message
        );

        wait_until(
            &ctx,
            &mut app,
            |app| app.group_running(0) == 2,
            "the second entry never started",
        );
        assert!(
            app.group_running(0) < 3,
            "the third entry must not start together with the second: {}",
            app.message
        );

        wait_until(
            &ctx,
            &mut app,
            |app| app.group_running(0) == 3,
            "the third entry never started",
        );
        assert!(
            commanded.elapsed() >= Duration::from_secs(1),
            "the entries did not wait for each other, only for the command: {:?}",
            commanded.elapsed()
        );

        app.apply(vec![Action::ToggleGroup(0)]);
        wait_until(
            &ctx,
            &mut app,
            |app| app.group_running(0) == 0,
            "the group was not stopped",
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A delay is not a dependency either: it counts from the moment the entry
    /// before it started, not from that program being up or even working. An
    /// entry that cannot start at all holds nothing back.
    #[test]
    fn a_delay_does_not_wait_for_the_other_programs() {
        let _guard = processes::process_test_lock();
        let (dir, path) = temp_config("delay-alone");
        let (program, args, process, _) = sleeper();
        write_config(
            &path,
            &Config {
                groups: vec![Group {
                    name: "Together".to_string(),
                    hotkey: String::new(),
                    programs: vec![
                        // The entry before it fails, so nothing is ever up.
                        Program {
                            name: "Impossible".to_string(),
                            program: "starter-test-never-runs.exe".to_string(),
                            ..Program::default()
                        },
                        Program {
                            name: "Late".to_string(),
                            program,
                            args,
                            process,
                            start_delay: 0.8,
                            ..Program::default()
                        },
                    ],
                }],
                ..Config::default()
            },
        );
        let mut app = LauncherApp::new(path, Registry::detached(), Tray::detached());
        let ctx = themed_context();

        app.apply(vec![Action::ToggleGroup(0)]);
        assert!(
            app.message.contains("Could not start Impossible"),
            "the impossible entry has to fail: {}",
            app.message
        );
        assert_eq!(
            app.group_running(0),
            0,
            "the wait is still running: {}",
            app.message
        );

        wait_until(
            &ctx,
            &mut app,
            |app| app.group_running(0) == 1,
            "the delayed program never started, although nothing of the group was up",
        );

        app.apply(vec![Action::ToggleGroup(0)]);
        wait_until(
            &ctx,
            &mut app,
            |app| app.group_running(0) == 0,
            "the sleeper was not stopped",
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The other direction: a stop delay leaves the program running until its
    /// time has come, and says so.
    #[test]
    fn a_stop_delay_lets_a_program_run_on() {
        let _guard = processes::process_test_lock();
        let (dir, path) = config_with_delays("stop-delay", 0.0, 0.8);
        let mut app = LauncherApp::new(path, Registry::detached(), Tray::detached());
        let ctx = themed_context();

        app.apply(vec![Action::ToggleGroup(0)]);
        wait_until(
            &ctx,
            &mut app,
            |app| app.group_running(0) == 1,
            "the sleeper never started",
        );

        app.apply(vec![Action::ToggleGroup(0)]);
        assert_eq!(
            app.group_running(0),
            1,
            "the stop should not have happened yet: {}",
            app.message
        );
        assert!(
            app.message.contains("will stop in"),
            "a wait has to be reported: {}",
            app.message
        );

        wait_until(
            &ctx,
            &mut app,
            |app| app.group_running(0) == 0,
            "the delayed stop never happened",
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The one thing that must never happen: a wait that is called off and then
    /// starts anyway.
    #[test]
    fn stopping_calls_a_pending_start_off() {
        let _guard = processes::process_test_lock();
        let (dir, path) = config_with_delays("call-off", 0.7, 0.0);
        let mut app = LauncherApp::new(path, Registry::detached(), Tray::detached());
        let ctx = themed_context();

        app.apply(vec![Action::ToggleGroup(0)]);
        assert!(app.entry_state(0, 0).busy(), "the start should be waiting");

        // The same key again: something on its way is stopped, which means the
        // wait is called off instead of the program being started twice.
        app.apply(vec![Action::ToggleGroup(0)]);
        assert!(
            !app.entry_state(0, 0).busy(),
            "the wait was not called off: {}",
            app.message
        );
        assert!(
            app.message.contains("called off"),
            "calling it off has to be visible: {}",
            app.message
        );

        // Well past the delay it asked for, the program must not be there.
        std::thread::sleep(Duration::from_millis(900));
        run_frames(&ctx, &mut app, 2);
        assert_eq!(
            app.group_running(0),
            0,
            "the called-off start happened anyway"
        );

        // One entry on its own has to obey the same rule: Start queues, Stop
        // calls the wait off — through the entry's own button, not the group's.
        app.apply(vec![Action::ToggleProgram(0, 0)]);
        assert!(
            app.entry_state(0, 0).busy(),
            "the entry's start should be waiting: {}",
            app.message
        );
        app.apply(vec![Action::ToggleProgram(0, 0)]);
        assert!(
            !app.entry_state(0, 0).busy(),
            "the entry's wait was not called off: {}",
            app.message
        );

        std::thread::sleep(Duration::from_millis(900));
        run_frames(&ctx, &mut app, 2);
        assert_eq!(
            app.group_running(0),
            0,
            "the called-off start of one entry happened anyway"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The palette promises that green means running. Something waiting for its
    /// start delay is not running, so it must not be painted as if it were.
    #[test]
    fn a_waiting_start_is_not_green() {
        let (dir, path) = temp_config("waiting-colour");
        write_config(
            &path,
            &Config {
                groups: vec![Group {
                    name: "Waiting".to_string(),
                    hotkey: String::new(),
                    programs: vec![Program {
                        name: "Later".to_string(),
                        program: "starter-test-never-running.exe".to_string(),
                        start_delay: 2.0,
                        ..Program::default()
                    }],
                }],
                ..Config::default()
            },
        );
        let mut app = LauncherApp::new(path, Registry::detached(), Tray::detached());
        let ctx = themed_context();

        run_frames(&ctx, &mut app, 1);
        app.pending.push(Pending {
            due: Instant::now() + Duration::from_secs(2),
            group: 0,
            index: 0,
            step: Step::Start,
        });

        let painted = painted_with(&run_frames(&ctx, &mut app, 3), theme::LIVE);
        assert_eq!(
            painted,
            Painted::default(),
            "green means running, and a waiting start is not running"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    /// Two entries that share a process name but not their `match`: one running
    /// process may only light up the entry it really belongs to.
    #[test]
    fn a_neighbours_match_does_not_light_this_entry() {
        let (dir, path) = temp_config("match-neighbours");
        write_config(
            &path,
            &Config {
                groups: vec![Group {
                    name: "Runners".to_string(),
                    hotkey: String::new(),
                    programs: vec![
                        Program {
                            name: "Mine".to_string(),
                            program: "cmd".to_string(),
                            args: "/c npx electron .".to_string(),
                            process: "electron.exe".to_string(),
                            match_on: r"C:\Tools\Mine".to_string(),
                            ..Program::default()
                        },
                        Program {
                            name: "Theirs".to_string(),
                            program: "cmd".to_string(),
                            args: "/c npx electron .".to_string(),
                            process: "electron.exe".to_string(),
                            match_on: r"C:\Tools\Theirs".to_string(),
                            ..Program::default()
                        },
                        Program {
                            name: "Any name".to_string(),
                            program: "cmd".to_string(),
                            args: "/c npx electron .".to_string(),
                            process: "*".to_string(),
                            match_on: r"C:\Tools\Mine\node_modules".to_string(),
                            ..Program::default()
                        },
                    ],
                }],
                ..Config::default()
            },
        );
        let mut app = LauncherApp::new(path, Registry::detached(), Tray::detached());

        // Every entry got its own rule, in the order of the config.
        assert_eq!(app.targets.len(), 1, "one group");
        assert_eq!(app.targets[0].len(), 3, "one rule per entry");
        assert_eq!(
            app.targets[0][0],
            Target::new("electron.exe", r"C:\Tools\Mine"),
            "the rule has to come from the entry's own fields"
        );
        assert!(
            app.targets[0][2].describe().starts_with("any process with"),
            "`process = \"*\"` means the command line alone decides: {}",
            app.targets[0][2].describe()
        );
        assert_eq!(app.entry_state(0, 0).running, None, "nothing is running yet");

        // Only the second entry's rule sees the eight processes Electron runs.
        let theirs = app.targets[0][1].clone();
        app.running.insert(theirs, 8);

        assert_eq!(
            app.entry_state(0, 1).running,
            Some(8),
            "the entry that owns the processes must be lit"
        );
        assert_eq!(
            app.entry_state(0, 0).running,
            None,
            "a process belonging to the neighbour must not light this entry"
        );
        assert_eq!(app.group_running(0), 1, "exactly one of the three runs");

        let _ = std::fs::remove_dir_all(dir);
    }

    /// The whole point of a group hotkey: one key, and the direction is decided
    /// by whether anything of the group is running.
    ///
    /// This uses a real process, because that is the only way the decision can
    /// be wrong in a way the user would notice.
    #[test]
    fn a_hotkey_starts_the_group_and_then_stops_it() {
        let _guard = processes::process_test_lock();
        let (dir, path) = temp_config("hotkey-toggle");
        let (program, args, process, match_on) = sleeper();

        write_config(
            &path,
            &Config {
                groups: vec![Group {
                    name: "Toggle".to_string(),
                    hotkey: "Ctrl+Alt+9".to_string(),
                    programs: vec![
                        // Identified by a piece of its command line as well, so
                        // the hotkey has to read command lines to decide.
                        Program {
                            name: "Sleeper".to_string(),
                            program,
                            args,
                            process,
                            match_on,
                            ..Program::default()
                        },
                        // A program that cannot run, to prove that one running
                        // program of the group is already enough to stop it.
                        Program {
                            name: "Impossible".to_string(),
                            program: "starter-test-never-runs.exe".to_string(),
                            ..Program::default()
                        },
                    ],
                }],
                ..Config::default()
            },
        );

        let mut app = LauncherApp::new(path, Registry::detached(), Tray::detached());
        let chord = Chord::parse("Ctrl+Alt+9").expect("the test chord should parse");

        // Nothing is running, so the hotkey starts the group.
        app.hotkeys.trigger(chord);
        let actions = app.hotkey_actions();
        assert!(
            matches!(actions.as_slice(), [Action::ToggleGroup(0)]),
            "the hotkey did not reach its group: {actions:?}"
        );
        app.apply(actions);
        assert_eq!(
            app.group_running(0),
            1,
            "the group should have exactly one running program: {}",
            app.message
        );
        assert!(app.message.contains("Started 1 of 2"), "{}", app.message);

        // One of the two is running, so the same key now stops the group — and
        // it must not trust counts it read a while ago.
        app.running.clear();
        app.last_refresh = Instant::now();
        app.hotkeys.trigger(chord);
        let actions = app.hotkey_actions();
        app.apply(actions);

        assert_eq!(app.group_running(0), 0, "the group should be stopped: {}", app.message);
        assert!(app.message.contains("Stopped 1 of 2"), "{}", app.message);

        let _ = std::fs::remove_dir_all(dir);
    }

    fn key_event(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    /// The events of one frame in which `Ctrl+Alt+<key>` is pressed in one go.
    ///
    /// This is what the window really receives: pressing a combination quickly
    /// delivers the modifier keys as presses of their own, in the same batch as
    /// the key that finishes it, and the context's modifier state is back to
    /// nothing by the time the frame is handled. The window only redraws when it
    /// has to, so that is the normal case rather than an edge case.
    fn chord_events(key: egui::Key) -> Vec<egui::Event> {
        let ctrl = egui::Modifiers {
            ctrl: true,
            ..Default::default()
        };
        let ctrl_alt = egui::Modifiers {
            ctrl: true,
            alt: true,
            ..Default::default()
        };
        vec![
            egui::Event::ModifiersChanged(ctrl),
            key_event(egui::Key::ControlLeft, ctrl),
            egui::Event::ModifiersChanged(ctrl_alt),
            key_event(egui::Key::AltLeft, ctrl_alt),
            key_event(key, ctrl_alt),
            egui::Event::ModifiersChanged(egui::Modifiers::default()),
        ]
    }

    /// Draws one frame with exactly these events.
    fn frame_with(app: &mut LauncherApp, ctx: &egui::Context, events: Vec<egui::Event>) {
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(880.0, 600.0))),
            events,
            ..Default::default()
        };
        let output = ctx.run_ui(input, |ui| app.draw(ui));
        output.drop_without_applying_deltas();
    }

    /// Writes a config with a single group, so a test can decide what the
    /// launcher finds and what it can toggle.
    fn one_group_config(tag: &str, group: Group) -> (PathBuf, PathBuf) {
        let (dir, path) = temp_config(tag);
        write_config(&path, &Config {
            groups: vec![group],
            ..Config::default()
        });
        (dir, path)
    }

    /// Recording is the one part of the window the user drives with the
    /// keyboard, and it is easy to break silently: the window opens and the keys
    /// go nowhere.
    #[test]
    fn recording_takes_the_next_key_combination() {
        let (dir, path) = one_group_config(
            "recording",
            Group {
                name: "Keys".to_string(),
                ..Group::default()
            },
        );
        let mut app = LauncherApp::new(path, Registry::detached(), Tray::detached());
        let ctx = themed_context();

        // What a click on the key cap produces.
        app.apply(vec![Action::Record(0)]);
        assert_eq!(app.recording, Some(0), "the recording window did not open");

        frame_with(&mut app, &ctx, chord_events(egui::Key::W));

        assert_eq!(
            app.config.groups[0].hotkey, "Ctrl+Alt+W",
            "the pressed combination was not taken over"
        );
        assert!(app.recording.is_none(), "the recording window stayed open");
        assert!(
            app.recording_note.is_empty(),
            "a working combination should not be complained about: {}",
            app.recording_note
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Holding only the modifiers is not an answer to "which combination?".
    /// Complaining about it would be confusing, and recording one of them would
    /// be wrong.
    #[test]
    fn holding_only_modifiers_is_not_a_combination() {
        let (dir, path) = one_group_config(
            "recording-modifiers",
            Group {
                name: "Keys".to_string(),
                ..Group::default()
            },
        );
        let mut app = LauncherApp::new(path, Registry::detached(), Tray::detached());
        let ctx = themed_context();
        app.apply(vec![Action::Record(0)]);

        let ctrl = egui::Modifiers {
            ctrl: true,
            ..Default::default()
        };
        frame_with(
            &mut app,
            &ctx,
            vec![
                egui::Event::ModifiersChanged(ctrl),
                key_event(egui::Key::ControlLeft, ctrl),
                key_event(egui::Key::AltLeft, ctrl),
            ],
        );

        assert_eq!(app.config.groups[0].hotkey, "", "a modifier was recorded");
        assert_eq!(app.recording, Some(0), "the recording window should stay open");
        assert!(app.recording_note.is_empty(), "{}", app.recording_note);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A combination the system would not reserve still has to work inside the
    /// window — which is the same press and the same batch of events.
    #[test]
    fn a_chord_the_system_did_not_take_still_works_in_the_window() {
        let (dir, path) = one_group_config(
            "in-window",
            Group {
                name: "Keys".to_string(),
                hotkey: "Ctrl+Alt+W".to_string(),
                programs: vec![Program {
                    name: "Nope".to_string(),
                    program: "starter-test-never-runs.exe".to_string(),
                    ..Program::default()
                }],
            },
        );
        let mut app = LauncherApp::new(path, Registry::detached(), Tray::detached());
        let ctx = themed_context();
        assert!(!app.hotkeys.is_global(0), "a detached registry reserves nothing");

        frame_with(&mut app, &ctx, chord_events(egui::Key::W));

        assert!(
            app.message.contains("Could not start"),
            "the in-window hotkey did not reach its group: {}",
            app.message
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Setting and clearing a hotkey is deliberately the plainest thing in the
    /// config: a string that is written and deleted.
    #[test]
    fn a_hotkey_is_set_and_cleared_through_the_config() {
        let (dir, path) = temp_config("hotkey-edit");
        let mut app = LauncherApp::new(path.clone(), Registry::detached(), Tray::detached());
        let chord = Chord::parse("ctrl+alt+w").expect("the chord should parse");

        app.apply(vec![Action::SetHotkey(0, chord)]);
        assert_eq!(app.config.groups[0].hotkey, "Ctrl+Alt+W");
        assert!(app.dirty, "changing a hotkey has to be saved");
        assert!(app.recording.is_none(), "the recording window should be closed");

        // It survives a save and a reload, because it is just a config value.
        app.apply(vec![Action::Save]);
        app.apply(vec![Action::Reload]);
        assert_eq!(app.config.groups[0].hotkey, "Ctrl+Alt+W");

        app.apply(vec![Action::ClearHotkey(0)]);
        assert_eq!(app.config.groups[0].hotkey, "");
        assert!(app.dirty);

        app.apply(vec![Action::Save]);
        let text = std::fs::read_to_string(&path).expect("the config should exist");
        assert!(
            !text.contains("hotkey"),
            "a cleared hotkey must not be left in the file:\n{text}"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    /// A group that is running must be stopped even when the counts the window
    /// is showing are old, because a hotkey can be pressed at any time.
    #[test]
    fn the_background_cadence_is_slower_but_never_stale() {
        assert_eq!(refresh_interval(true), REFRESH_FOCUSED);
        assert_eq!(refresh_interval(false), REFRESH_BACKGROUND);
        assert!(REFRESH_BACKGROUND > REFRESH_FOCUSED);
    }

    // -----------------------------------------------------------------------
    // Tray mode
    // -----------------------------------------------------------------------

    /// Draws one frame and returns the viewport commands it asked for — how a
    /// test sees "hide the window", "put it back" or "quit". `closing` says
    /// whether the close button was what this frame is about.
    fn frame_commands(
        app: &mut LauncherApp,
        ctx: &egui::Context,
        closing: bool,
    ) -> Vec<egui::ViewportCommand> {
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(880.0, 600.0))),
            viewports: [(
                egui::ViewportId::ROOT,
                egui::ViewportInfo {
                    events: if closing {
                        vec![egui::ViewportEvent::Close]
                    } else {
                        Vec::new()
                    },
                    ..Default::default()
                },
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        let output = ctx.run_ui(input, |ui| app.draw(ui));
        let commands = output.viewport_output[&egui::ViewportId::ROOT]
            .commands
            .clone();
        output.drop_without_applying_deltas();
        commands
    }

    /// One group with a hotkey and a single program that cannot possibly start.
    /// Toggling it is visible in the note the launcher writes, which is how a
    /// test without a screen can tell that a key press or a menu line reached
    /// its group.
    fn impossible_group(tag: &str) -> (PathBuf, PathBuf) {
        one_group_config(
            tag,
            Group {
                name: "Keys".to_string(),
                hotkey: "Ctrl+Alt+W".to_string(),
                programs: vec![Program {
                    name: "Nope".to_string(),
                    program: "starter-test-never-runs.exe".to_string(),
                    ..Program::default()
                }],
            },
        )
    }

    /// With the icon up, the close button only takes the window off the screen:
    /// the frame calls the close off and asks for the window to disappear.
    #[test]
    fn closing_the_window_only_hides_it_in_tray_mode() {
        let (dir, path) = impossible_group("close-to-tray");
        let mut app = LauncherApp::new(path, Registry::detached(), Tray::pretending());
        app.config.tray = true;
        let ctx = themed_context();

        let commands = frame_commands(&mut app, &ctx, true);

        assert!(
            commands.contains(&egui::ViewportCommand::CancelClose),
            "the window closed although the icon was up: {commands:?}"
        );
        assert!(
            commands.contains(&egui::ViewportCommand::Visible(false)),
            "the window was not taken off the screen: {commands:?}"
        );
        assert!(
            app.message.contains("Hidden"),
            "the user was not told where the window went: {}",
            app.message
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Without an icon there is nothing to come back through, so the close goes
    /// through as it always did. That is also what every platform whose
    /// notification area is not wired up gets.
    #[test]
    fn without_an_icon_the_window_closes_as_usual() {
        let (dir, path) = impossible_group("close-without-icon");
        let mut app = LauncherApp::new(path, Registry::detached(), Tray::detached());
        app.config.tray = true;
        let ctx = themed_context();

        let commands = frame_commands(&mut app, &ctx, true);

        assert!(
            !commands.contains(&egui::ViewportCommand::CancelClose),
            "the window was kept alive although there is no way back to it: {commands:?}"
        );
        assert!(
            !commands.contains(&egui::ViewportCommand::Visible(false)),
            "the window was hidden with no way back to it: {commands:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The half of a frame that is not drawing is what keeps running while the
    /// window is hidden — eframe calls exactly that half then. This is the path
    /// a hotkey takes when there is nothing on screen to press it in.
    #[test]
    fn a_hotkey_still_reaches_its_group_with_the_window_hidden() {
        let (dir, path) = impossible_group("hidden-hotkey");
        let mut app = LauncherApp::new(path, Registry::detached(), Tray::pretending());
        app.config.tray = true;
        let ctx = themed_context();
        let chord = Chord::parse("ctrl+alt+w").expect("the chord should parse");

        app.hotkeys.trigger(chord);
        // No ui pass at all: the window is off the screen.
        let actions = app.head(&ctx);
        app.apply(actions);

        assert!(
            app.message.contains("Could not start"),
            "the hotkey did not reach its group with the window hidden: {}",
            app.message
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The icon is the way back, and the way to a group without opening
    /// anything: a click asks for the window, a line in the menu toggles its
    /// group.
    #[test]
    fn the_icon_brings_the_window_back_and_toggles_groups() {
        let (dir, path) = impossible_group("tray-actions");
        let mut app = LauncherApp::new(path, Registry::detached(), Tray::pretending());
        app.config.tray = true;
        let ctx = themed_context();

        app.tray.trigger(TrayEvent::Show);
        let commands = frame_commands(&mut app, &ctx, false);
        assert!(
            commands.contains(&egui::ViewportCommand::Visible(true)),
            "a click on the icon did not ask for the window back: {commands:?}"
        );
        assert!(
            commands.contains(&egui::ViewportCommand::Focus),
            "the window came back but not to the front: {commands:?}"
        );

        app.tray.trigger(TrayEvent::ToggleGroup(0));
        frame_commands(&mut app, &ctx, false);
        assert!(
            app.message.contains("Could not start"),
            "the menu line did not reach its group: {}",
            app.message
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Quit means quit: the close that follows is not answered the way the close
    /// button is, or a program in tray mode could never be ended from the icon.
    #[test]
    fn quitting_from_the_icon_is_not_answered_by_hiding_the_window() {
        let (dir, path) = impossible_group("tray-quit");
        let mut app = LauncherApp::new(path, Registry::detached(), Tray::pretending());
        app.config.tray = true;
        let ctx = themed_context();

        app.tray.trigger(TrayEvent::Quit);
        let commands = frame_commands(&mut app, &ctx, false);
        assert!(
            commands.contains(&egui::ViewportCommand::Close),
            "the icon's Quit did not ask to close anything: {commands:?}"
        );

        let commands = frame_commands(&mut app, &ctx, true);
        assert!(
            !commands.contains(&egui::ViewportCommand::CancelClose),
            "a quit was answered by hiding the window instead of ending the program"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The switch in the header is a plain config value: written, saved, read
    /// back, and left out of the file while it is off.
    #[test]
    fn tray_mode_is_switched_through_the_config() {
        let (dir, path) = temp_config("tray-switch");
        let mut app = LauncherApp::new(path.clone(), Registry::detached(), Tray::detached());

        app.apply(vec![Action::SetTray(true)]);
        assert!(app.config.tray, "the switch did not take");
        assert!(app.dirty, "changing the switch has to be saved");
        assert!(app.message.contains("Tray mode on"), "{}", app.message);

        app.apply(vec![Action::Save]);
        app.apply(vec![Action::Reload]);
        assert!(
            app.config.tray,
            "the switch did not survive a save and a reload"
        );

        app.apply(vec![Action::SetTray(false)]);
        assert!(!app.config.tray);
        assert!(app.message.contains("Tray mode off"), "{}", app.message);
        app.apply(vec![Action::Save]);

        let text = std::fs::read_to_string(&path).expect("the config should exist");
        assert!(
            !text.contains("tray"),
            "a switched-off mode was left in the file:\n{text}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Tray mode has to be visible in the window's own language, and that
    /// language is colour. The header lights up while the launcher really has an
    /// icon in the notification area — and it does not claim that while there is
    /// nowhere to put one, which is the difference between "this survives its
    /// window" and "this was asked for and did not happen".
    #[test]
    fn tray_mode_shows_itself_in_colour() {
        let (dir, path) = impossible_group("tray-colour");
        let ctx = themed_context();

        // Off: the annunciator is there, quietly, and nothing in the header is
        // lit. This is also the state every other drawing test is in.
        let mut app = LauncherApp::new(path.clone(), Registry::detached(), Tray::detached());
        assert_eq!(
            painted_with(&run_frames(&ctx, &mut app, 3), theme::LIVE),
            Painted::default(),
            "tray mode is off, so nothing may be green"
        );

        // On, but with no icon to show for it: amber, never green.
        let mut app = LauncherApp::new(path.clone(), Registry::detached(), Tray::detached());
        app.config.tray = true;
        let framed = run_frames(&ctx, &mut app, 3);
        assert_eq!(
            painted_with(&framed, theme::LIVE).circles,
            0,
            "nothing runs in the background without an icon, so nothing may be green"
        );
        assert_eq!(
            painted_with(&framed, theme::ALERT).circles,
            1,
            "the header has to say that tray mode is on but has nowhere to go"
        );

        // With the icon really up: a lit lamp, and nothing is wrong.
        let mut app = LauncherApp::new(path, Registry::detached(), Tray::pretending());
        app.config.tray = true;
        let framed = run_frames(&ctx, &mut app, 3);
        assert_eq!(
            painted_with(&framed, theme::LIVE).circles,
            1,
            "the lit lamp in the header is the one green circle while nothing is running"
        );
        assert_eq!(
            painted_with(&framed, theme::ALERT).circles,
            0,
            "a tray that has its icon must not be reported as a problem"
        );

        let _ = std::fs::remove_dir_all(dir);
    }
}
