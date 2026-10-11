//! Everything that talks to the operating system: starting programs, finding
//! running ones and stopping them.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::io;
use std::process::Command;

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

/// Case-insensitive comparison key for process names.
pub fn process_key(name: &str) -> String {
    name.trim().to_ascii_lowercase()
}

/// What identifies the processes belonging to one entry: a process name, and
/// optionally a piece of text that has to appear in its command line.
///
/// The name alone is often not enough. A program that is started through a
/// runner or a script ends up as a process named after the *runtime* rather than
/// after the program itself — `npx electron .` becomes `cmd.exe` → `node.exe` →
/// `electron.exe` — and Electron alone runs another seven processes that all
/// carry the very same name for its renderers and helpers. A needle is what
/// tells those apart, both from each other and from somebody else's
/// `electron.exe`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Target {
    /// Lowercased name as the operating system reports it. Empty means "any
    /// name", which the config writes as `*`.
    name: String,
    /// Text that must appear in the command line, as the user wrote it.
    needle: String,
    /// The same text, lowercased: comparing allocates enough already.
    needle_lower: String,
}

impl Target {
    pub fn new(name: &str, match_on: &str) -> Self {
        let name = process_key(name);
        let needle = match_on.trim().to_string();
        Self {
            name: if name == "*" { String::new() } else { name },
            needle_lower: needle.to_ascii_lowercase(),
            needle,
        }
    }

    /// `true` when telling this entry apart needs command lines to be read.
    ///
    /// Reading them costs an extra system call per process on the machine, so
    /// an entry that is identified by its name alone must not pay for them.
    pub fn needs_command_line(&self) -> bool {
        !self.needle.is_empty()
    }

    /// How the entry's processes are described to the user.
    pub fn describe(&self) -> String {
        match (self.name.as_str(), self.needle.as_str()) {
            ("", "") => "nothing".to_string(),
            ("", needle) => format!("any process with \"{needle}\" in the command line"),
            (name, "") => name.to_string(),
            (name, needle) => {
                format!("a {name} with \"{needle}\" in the command line")
            }
        }
    }

    /// Whether one process is an instance of this target.
    ///
    /// A process whose command line cannot be read is *not* assumed to match: a
    /// filter that cannot be checked must never let Stop kill the wrong thing.
    fn matches(&self, name: &str, command_line: &[OsString]) -> bool {
        if !self.name.is_empty() && !name.eq_ignore_ascii_case(&self.name) {
            return false;
        }
        if self.needle.is_empty() {
            // "any name" with no filter would match every process on the
            // machine. That is never what an entry means, so it matches none.
            return !self.name.is_empty();
        }
        command_line
            .iter()
            .any(|part| part.to_string_lossy().to_ascii_lowercase().contains(&self.needle_lower))
    }
}

/// Splits an argument string into arguments, keeping quoted parts together.
///
/// `--path "C:\Program Files" -v` becomes `["--path", "C:\Program Files", "-v"]`.
pub fn split_args(input: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut quoted = false;

    for character in input.chars() {
        match character {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            c if c.is_whitespace() && !quoted => {
                if started {
                    args.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            c => {
                current.push(c);
                started = true;
            }
        }
    }

    if started {
        args.push(current);
    }
    args
}

/// Starts a program and returns its process id.
pub fn spawn(program: &str, args: &str) -> io::Result<u32> {
    Command::new(program.trim())
        .args(split_args(args))
        .spawn()
        .map(|child| child.id())
}

/// Thin wrapper around [`System`] that answers the only two questions the GUI
/// asks: "is this running?" and "please stop it".
pub struct ProcessManager {
    system: System,
    /// Whether the last refresh also read command lines. [`Target::matches`]
    /// sees an empty command line otherwise, so this is what keeps a filter from
    /// silently matching nothing.
    command_lines: bool,
}

impl ProcessManager {
    pub fn new() -> Self {
        // `System::new()` instead of `new_all()`: we never look at CPU, memory,
        // disks or users, so there is no reason to collect them.
        let mut system = System::new();
        Self::refresh_names_only(&mut system);
        Self {
            system,
            command_lines: false,
        }
    }

    /// Re-reads the process list. Called once per UI tick.
    ///
    /// This is the only way to refresh, and it always takes the targets that
    /// are about to be counted or stopped: that way the command lines a filter
    /// needs cannot be forgotten.
    pub fn refresh_for(&mut self, targets: &[Target]) {
        let command_lines = targets.iter().any(Target::needs_command_line);
        let kind = if command_lines {
            ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always)
        } else {
            ProcessRefreshKind::nothing()
        };
        self.system
            .refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
        self.command_lines = command_lines;
    }

    /// Process names, parents and start times come for free with the process
    /// list; everything else (CPU, memory, user, command line) would be an
    /// extra system call per process and is never displayed.
    fn refresh_names_only(system: &mut System) {
        system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing(),
        );
    }

    /// Counts the running instances of every target into `counts`.
    ///
    /// Both the targets and the counts are owned by the caller and kept between
    /// ticks: the targets are rebuilt only when the config changes, and the
    /// counts are reset rather than reallocated. A steady state therefore
    /// allocates nothing, which matters because this walks every process on the
    /// machine once per tick. The name is compared first and the command line
    /// only for the few processes whose name already fits.
    pub fn count_running(&self, targets: &[Target], counts: &mut HashMap<Target, usize>) {
        for count in counts.values_mut() {
            *count = 0;
        }

        debug_assert!(
            self.command_lines || !targets.iter().any(Target::needs_command_line),
            "a target with a command-line filter was counted without reading command lines"
        );

        if targets.is_empty() {
            return;
        }

        for process in self.system.processes().values() {
            let Some(name) = process.name().to_str() else {
                continue;
            };
            // One running process belongs to one entry, so the first target
            // that fits takes it and the rest are not asked again.
            let Some(target) = targets
                .iter()
                .find(|target| target.matches(name, process.cmd()))
            else {
                continue;
            };
            match counts.get_mut(target) {
                Some(count) => *count += 1,
                None => {
                    counts.insert(target.clone(), 1);
                }
            }
        }
    }

    /// [`ProcessManager::count_running`] with a refresh and a map of its own,
    /// for callers that have neither to keep — tests and one-off queries.
    #[cfg(test)]
    pub fn running_counts(&mut self, targets: &[Target]) -> HashMap<Target, usize> {
        self.refresh_for(targets);
        let mut counts: HashMap<Target, usize> = HashMap::with_capacity(targets.len());
        self.count_running(targets, &mut counts);
        counts
    }
}

/// Tests that start and stop real processes take this lock first.
///
/// Stopping happens *by name*, so two such tests running at the same time would
/// each undo the other's work. It lives here rather than in the test module
/// because the UI tests start processes too.
#[cfg(test)]
pub(crate) fn process_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static RUNNING_PROGRAMS: std::sync::Mutex<()> = std::sync::Mutex::new(());
    RUNNING_PROGRAMS
        .lock()
        .unwrap_or_else(|err| err.into_inner())
}

impl ProcessManager {
/// Stops every process belonging to `target`, plus their children.
///
/// Works for programs started elsewhere, because it only looks at the
/// operating system's process list. Returns how many processes were killed.
/// The caller is expected to re-read the process list afterwards.
    pub fn kill(&mut self, target: &Target) -> usize {
        self.refresh_for(std::slice::from_ref(target));

        let mut victims: HashSet<Pid> = self
            .system
            .processes()
            .iter()
            .filter(|(_, process)| {
                process
                    .name()
                    .to_str()
                    .is_some_and(|name| target.matches(name, process.cmd()))
            })
            .map(|(pid, _)| *pid)
            .collect();

        if victims.is_empty() {
            return 0;
        }

        // A stopped program usually leaves helper processes behind, so take the
        // whole tree with it.
        loop {
            let mut added = false;
            for (pid, process) in self.system.processes() {
                if process.parent().is_some_and(|parent| victims.contains(&parent))
                    && victims.insert(*pid)
                {
                    added = true;
                }
            }
            if !added {
                break;
            }
        }

        let mut killed = 0;
        for pid in victims {
            if let Some(process) = self.system.process(pid)
                && process.kill()
            {
                killed += 1;
            }
        }

        killed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    #[ignore = "measurement, run with --ignored --nocapture"]
    fn benchmark_one_poll() {
        use std::time::Instant;
        const ROUNDS: u32 = 20;

        // How it was: refresh everything, then key every process on the machine.
        let mut system = System::new_all();
        let start = Instant::now();
        for _ in 0..ROUNDS {
            system.refresh_processes(ProcessesToUpdate::All, true);
            let mut counts: HashMap<String, usize> = HashMap::new();
            for process in system.processes().values() {
                if let Some(name) = process.name().to_str() {
                    *counts.entry(process_key(name)).or_insert(0) += 1;
                }
            }
        }
        let before = start.elapsed() / ROUNDS;

        let names = ["notepad.exe", "chrome.exe", "code.exe", "steam.exe"];
        let mut manager = ProcessManager::new();

        // The same narrowly-scoped lookup, but throwing away the targets and
        // the counts on every tick instead of keeping them.
        let start = Instant::now();
        for _ in 0..ROUNDS {
            let targets: Vec<Target> = names.iter().map(|name| by_name(name)).collect();
            manager.refresh_for(&targets);
            let mut counts: HashMap<Target, usize> = HashMap::new();
            manager.count_running(&targets, &mut counts);
        }
        let uncached = start.elapsed() / ROUNDS;

        // How it is now: nothing is built per tick.
        let targets: Vec<Target> = names.iter().map(|name| by_name(name)).collect();
        let mut counts: HashMap<Target, usize> = HashMap::with_capacity(targets.len());
        let start = Instant::now();
        for _ in 0..ROUNDS {
            manager.refresh_for(&targets);
            manager.count_running(&targets, &mut counts);
        }
        let after = start.elapsed() / ROUNDS;

        // What one entry costs that has to look at command lines as well.
        let filtered = vec![Target::new("electron.exe", "node_modules")];
        let mut counts: HashMap<Target, usize> = HashMap::with_capacity(1);
        let start = Instant::now();
        for _ in 0..ROUNDS {
            manager.refresh_for(&filtered);
            manager.count_running(&filtered, &mut counts);
        }
        let filtered = start.elapsed() / ROUNDS;

        println!(
            "\nper poll: everything {before:?}, names only {uncached:?}, names only kept {after:?}, \
             with command lines {filtered:?}\n"
        );
    }

    /// A program that stays alive long enough to be found and stopped.
    /// Kept short on purpose: everything it prints to stdout ends up in the
    /// test log, because a child inherits the parent's handles.
    #[cfg(windows)]
    const SLEEPER: (&str, &str, &str) = ("ping", "-n 15 127.0.0.1", "ping.exe");
    #[cfg(unix)]
    const SLEEPER: (&str, &str, &str) = ("sleep", "60", "sleep");

    /// The rule an entry without a `match` line uses: the name, nothing else.
    fn by_name(name: &str) -> Target {
        Target::new(name, "")
    }

    fn running(manager: &mut ProcessManager, name: &str) -> usize {
        let target = by_name(name);
        manager
            .running_counts(std::slice::from_ref(&target))
            .get(&target)
            .copied()
            .unwrap_or(0)
    }

    /// Asks this machine's real process list whether a running program can be
    /// told apart by where it lives, even though other programs carry the same
    /// process name.
    ///
    /// Run it by hand while the program is running; `STARTER_MATCH_PROBE` is the
    /// folder it lives in and `STARTER_MATCH_PROCESS` its process name
    /// (`electron.exe` by default).
    ///
    /// ```sh
    /// STARTER_MATCH_PROBE='C:\Tools\My App' cargo test --bin starter \
    ///     a_live_folder_tells_same_named_programs_apart -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "asks a running program on this machine"]
    fn a_live_folder_tells_same_named_programs_apart() {
        let folder = std::env::var("STARTER_MATCH_PROBE")
            .expect("set STARTER_MATCH_PROBE to the folder the running program lives in");
        let name = std::env::var("STARTER_MATCH_PROCESS")
            .unwrap_or_else(|_| "electron.exe".to_string());
        let mut manager = ProcessManager::new();

        let mine = Target::new(&name, &folder);
        let nowhere = Target::new(&name, "starter-folder-that-does-not-exist");
        let by_name = Target::new(&name, "");

        for target in [&mine, &nowhere, &by_name] {
            let count = manager
                .running_counts(std::slice::from_ref(target))
                .get(target)
                .copied()
                .unwrap_or(0);
            println!("{}  ->  {count}", target.describe());
        }

        let found = manager
            .running_counts(std::slice::from_ref(&mine))
            .get(&mine)
            .copied()
            .unwrap_or(0);
        assert!(
            found > 0,
            "nothing named {name} with {folder} in its command line is running"
        );
    }

    /// Two programs with the same process name, differing only in their command
    /// line — the case a name alone cannot decide. Each is given as program,
    /// arguments, process name and a piece of the arguments to recognise it by.
    #[cfg(windows)]
    fn same_name_different_arguments() -> [(&'static str, &'static str, &'static str, &'static str); 2]
    {
        [
            ("ping", "-n 15 127.0.0.1", "ping.exe", "127.0.0.1"),
            ("ping", "-n 15 127.0.0.2", "ping.exe", "127.0.0.2"),
        ]
    }

    #[cfg(unix)]
    fn same_name_different_arguments() -> [(&'static str, &'static str, &'static str, &'static str); 2]
    {
        [
            ("sleep", "60", "sleep", "60"),
            ("sleep", "75", "sleep", "75"),
        ]
    }

    #[test]
    fn a_target_normalizes_what_it_was_given() {
        let target = Target::new(" Notepad.EXE ", " C:\\Tools\\App ");
        assert!(target.needs_command_line());
        assert!(
            target.matches("notepad.exe", &[OsString::from(r"C:\TOOLS\APP\app.js")]),
            "name and command line are compared case-insensitively"
        );
        assert!(!target.matches("other.exe", &[OsString::from(r"C:\Tools\App\app.js")]));
        assert!(
            !target.matches("notepad.exe", &[]),
            "an unreadable command line must not be assumed to match"
        );

        assert!(!Target::new("notepad.exe", "   ").needs_command_line());
        assert!(!Target::new("", "").matches("anything.exe", &[]));
    }

    /// `process = "*"` lets the command line alone decide, for a program whose
    /// process name is not known before it runs.
    #[test]
    fn a_star_name_lets_the_command_line_decide() {
        let target = Target::new("*", "node_modules");
        assert!(target.matches(
            "electron.exe",
            &[OsString::from(r"C:\app\node_modules\electron\dist\electron.exe")]
        ));
        assert!(!target.matches("chrome.exe", &[OsString::from(r"C:\Chrome\chrome.exe")]));
        assert!(!target.matches("electron.exe", &[]));
    }

    /// Absolute paths are normal, so the second sleeper comes from one.
    #[cfg(windows)]
    fn absolute_sleeper() -> (String, String, String) {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
        let exe = std::path::Path::new(&root)
            .join("System32")
            .join("ping.exe");
        (
            exe.display().to_string(),
            "-n 15 127.0.0.1".to_string(),
            "ping.exe".to_string(),
        )
    }

    #[cfg(unix)]
    fn absolute_sleeper() -> (String, String, String) {
        ("/bin/sleep".to_string(), "60".to_string(), "sleep".to_string())
    }

    /// Waits until the OS reports `expected` instances of `name`.
    fn wait_for(manager: &mut ProcessManager, name: &str, expected: usize) -> usize {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let count = running(manager, name);
            if count == expected || Instant::now() > deadline {
                return count;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    #[test]
    fn splits_plain_arguments() {
        assert_eq!(split_args(""), Vec::<String>::new());
        assert_eq!(split_args("  "), Vec::<String>::new());
        assert_eq!(split_args("-v --debug"), vec!["-v", "--debug"]);
    }

    #[test]
    fn keeps_quoted_arguments_together() {
        assert_eq!(
            split_args(r#"--path "C:\Program Files" -v"#),
            vec!["--path", r"C:\Program Files", "-v"]
        );
    }

    #[test]
    fn process_names_are_compared_case_insensitively() {
        assert_eq!(process_key(" Notepad.EXE "), "notepad.exe");
    }

    /// The important promise of the launcher: programs can be stopped even
    /// when the launcher did not start them.
    #[test]
    fn stops_processes_it_did_not_start() {
        let _serially = process_test_lock();
        let (program, args, process_name) = SLEEPER;
        let mut manager = ProcessManager::new();
        let before = wait_for(&mut manager, process_name, 0);

        // One instance through the launcher's own start path ...
        spawn(program, args).expect("launcher could not start the sleeper");
        // ... and one started completely outside of it.
        let mut external = std::process::Command::new(program)
            .args(split_args(args))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("could not start the external sleeper");

        assert_eq!(
            wait_for(&mut manager, process_name, before + 2),
            before + 2,
            "spawned processes never showed up in the process list"
        );

        let killed = manager.kill(&by_name(process_name));
        assert!(killed >= 2, "expected both processes to be killed, got {killed}");
        assert_eq!(
            wait_for(&mut manager, process_name, before),
            before,
            "processes survived the stop request"
        );

        // Reap the child we own so the test leaves nothing behind.
        let _ = external.wait();
    }

    /// A fully qualified path must work from start to stop — including the
    /// process name the launcher derives from it.
    #[test]
    fn full_paths_are_accepted() {
        use crate::config::Program;

        let _serially = process_test_lock();
        let (program, args, process_name) = absolute_sleeper();
        assert!(
            std::path::Path::new(&program).is_absolute(),
            "test setup is wrong: {program}"
        );
        assert!(
            std::path::Path::new(&program).exists(),
            "test setup is wrong, {program} does not exist"
        );

        let mut manager = ProcessManager::new();
        let before = wait_for(&mut manager, &process_name, 0);

        spawn(&program, &args).expect("a full path must be launchable");
        assert_eq!(
            wait_for(&mut manager, &process_name, before + 1),
            before + 1,
            "the program started from a full path never showed up"
        );

        // This is the link that makes Stop work for full paths: the name the
        // config derives from the path is the name the OS reports.
        let entry = Program {
            program: program.clone(),
            ..Program::default()
        };
        assert_eq!(entry.process_name(), process_name);

        let killed = manager.kill(&by_name(&process_name));
        assert!(killed >= 1, "nothing was killed for {process_name}");
        assert_eq!(wait_for(&mut manager, &process_name, before), before);
    }

    /// The promise of a `match` line: of two processes that share a name, only
    /// the one whose command line carries the text is seen — and only that one
    /// is stopped.
    #[test]
    fn a_command_line_filter_picks_one_of_two_same_named_processes() {
        let _serially = process_test_lock();
        let [first, second] = same_name_different_arguments();
        let (first_program, first_args, name, _) = first;
        let (second_program, second_args, _, needle) = second;
        let mut manager = ProcessManager::new();
        let before = wait_for(&mut manager, name, 0);

        // One instance through the launcher's own start path ...
        spawn(first_program, first_args).expect("launcher could not start the first sleeper");
        // ... and one started completely outside of it, with the other argument.
        let mut external = std::process::Command::new(second_program)
            .args(split_args(second_args))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("could not start the external sleeper");

        assert_eq!(
            wait_for(&mut manager, name, before + 2),
            before + 2,
            "both sleepers should be running"
        );

        // By name alone, both of them fit.
        let by_name = by_name(name);
        assert_eq!(
            manager
                .running_counts(std::slice::from_ref(&by_name))
                .get(&by_name)
                .copied()
                .unwrap_or(0),
            before + 2
        );

        // With the filter, exactly one does — and it is the right one.
        let filtered = Target::new(name, needle);
        assert_eq!(
            manager
                .running_counts(std::slice::from_ref(&filtered))
                .get(&filtered)
                .copied()
                .unwrap_or(0),
            1,
            "the filter did not pick exactly one process"
        );

        // Stopping through the filter must leave the other program alone.
        assert_eq!(
            manager.kill(&filtered),
            1,
            "the filter stopped the wrong number of processes"
        );
        assert_eq!(
            wait_for(&mut manager, name, before + 1),
            before + 1,
            "the filtered stop took the wrong process with it"
        );

        let killed = manager.kill(&by_name);
        assert!(killed >= 1, "nothing was killed for {name}");
        assert_eq!(wait_for(&mut manager, name, before), before);

        // Reap the child we own so the test leaves nothing behind.
        let _ = external.wait();
    }
}
