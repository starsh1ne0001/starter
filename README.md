# Starter

A small, deliberately boring program launcher: pick a group, press **Start** or
**Stop**. That's it.

Written in Rust with [egui](https://github.com/emilk/egui) for the window,
[sysinfo](https://github.com/GuillaumeGomez/sysinfo) for the process handling and
[global-hotkey](https://github.com/tauri-apps/global-hotkey) for the key
combinations and [tray-icon](https://github.com/tauri-apps/tray-icon) for the
icon in the notification area — plus, on Windows,
[windows-sys](https://github.com/microsoft/windows-rs) for the console window
behind the launcher (see [below](#the-console-window-behind-the-launcher)).

## What it does

* Groups programs so a whole set can be started or stopped with one click.
* **One key per group, optional.** Give a group a key combination and it toggles
  the whole group from anywhere — no need to bring the window forward. The key
  decides by itself which way to go: if anything of the group is running — or
  still waiting for its start delay — it stops, otherwise it starts everything.
* **Stops programs that the launcher never started.** Stop looks the process up
  in the operating system's process list by name, so it also works for something
  you launched from the desktop — and it takes child processes with it.
* **Waits before starting or stopping a program, if it needs that.** An optional
  delay per program, per direction — for the server that has to be up before its
  client, or the client that should go first. Nothing is blocked while a wait
  runs.
* **Tells processes apart by more than their name.** A program started through a
  runner (`npx electron .`) runs under a different name than it was launched
  with, and shares that name with its own helper processes. An optional `match`
  on the command line pins down which processes really belong to the entry —
  for the display and for Stop alike.
* **Can keep running in the notification area.** Switch **Tray** on in the
  header and closing the window only takes it off the screen: the launcher stays
  up, the hotkeys keep working, and the icon is the way back (see
  [Tray mode](#tray-mode)).
* Shows whether each program is currently running (refreshed once per second,
  see [below](#cost-of-watching-what-is-running)).
* Edits its own config file: add/remove groups and programs, then press **Save**.
* Picks the executable with a native file dialog — press **Browse** next to the
  *Executable* field, or in the *New program* strip.

## Build and run

```sh
cargo run --release
```

The window appears and `launcher.toml` is created in the working directory the
first time. To use a different file:

```sh
cargo run --release -- path/to/launcher.toml
```

## Downloads

Running it does not need a Rust toolchain: the
[releases](https://github.com/starsh1ne0001/starter/releases) page carries a
ready-made `starter.exe` for Windows.

* The [development build](https://github.com/starsh1ne0001/starter/releases/tag/dev)
  is rebuilt on every push to `main` and always holds the newest state that
  passed its tests. Its version number does not move — the tag follows `main`
  instead — so one link keeps working.
* A tag gives that commit a release of its own: `git tag v0.1.0 && git push origin v0.1.0`
  builds the same binary from the tag, with the notes GitHub writes from the
  commits since the release before it. Use these whenever the file leaves your
  own machine.

Both come out of
[.github/workflows/release.yml](.github/workflows/release.yml), and neither is
published before `cargo test` and `cargo clippy --all-targets -- -D warnings`
have passed.

## Config format

Plain TOML, readable and editable by hand. The GUI writes the same format.

`program` takes either a bare name that is looked up on `PATH` (`notepad.exe`)
*or* a fully qualified path (`C:\Tools\thing\thing.exe`). A full path is used
exactly as written, so spaces in it need no quoting and no escaping.

```toml
tray = true               # optional: keep running in the notification area

[[groups]]
name = "Everyday"
hotkey = "Ctrl+Alt+E"     # optional: toggles this group from anywhere

[[groups.programs]]
name = "Notepad"          # label in the window
program = "notepad.exe"   # a name on PATH or a full path
args = ""                 # optional; quotes group an argument

[[groups.programs]]
name = "Dev server"
program = "cmd"
args = '/c "npm run dev"'
process = "node.exe"      # optional: process to look for when stopping
start_delay = 2           # optional: this long after the entry above it started
stop_delay = 1.5          # optional: the same for stopping

[[groups.programs]]
name = "Electron app"
program = "cmd"
args = "/c npx electron ."              # a runner: the real program sits below it
process = "electron.exe"                # the name the operating system reports
match = 'C:\Tools\Electron App'         # optional: must appear in its command line
```

`process` only needs to be set when the running process is named differently
from the executable — `calc.exe` is the classic example, because Windows runs it
as `CalculatorApp.exe`. Without it, the file name of `program` is used. The
comparison ignores case and the `.exe` suffix is not required if the real
process name has none.

### Waiting before starting or stopping

`start_delay` holds a program back before it is started; `stop_delay` does the
same for stopping. Both are seconds, both are optional, both take whole numbers
or fractions, and leaving the line out (or writing `0`) means "at once", which is
the normal case.

**Inside a group the delays are gaps, not offsets from the command.** An entry
starts when the entry before it has started, plus its own delay. A group of three
entries with delays `0`, `10` and `0` starts the first at once, the second ten
seconds later, and the third together with the second — never all of them at the
same instant. The first entry's delay counts from the command, and an entry
without a delay adds no gap. Stopping is staggered exactly the same way: an
entry's stop delay is the gap from the moment the entry before it was stopped.

This is deliberately **not** a dependency. The launcher does not wait for a
program to be *ready* — it only keeps the starts from arriving in parallel. So a
program that fails to start holds nothing back, and a program that needs longer
to become useful than its delay allows needs a longer delay: the launcher cannot
tell the difference between "started" and "ready". A single entry toggled on its
own has nothing before it, so there its delay counts from the press.

Two reasons to use it: the pair from the example above — a client that should not
come up at the same instant as its server — and the same in reverse, where the
client is stopped first and the server follows a moment later.

Nothing is blocked while a wait runs. The window keeps drawing, every other
entry keeps working, and the entry itself says what it is waiting for:
`starting in 2 s` or `stopping in 1 s`. It is said in words rather than shown in
colour, because no colour in this palette means "about to": green stays
"running" and grey "stopped".

One rule, unchanged: a toggle stops when something of the group is running **or
already on its way**. So pressing the same key again calls the wait off instead
of starting the program twice — and the status line says that it was called off.

### Which process belongs to an entry

Programs started through a runner or a script do not show up under their own
name. Starting `npx electron .` through the script of the program above was
measured on this machine as **five levels**:

```
electron.exe   "C:\Tools\Electron App\node_modules\electron\dist\electron.exe" .
node.exe       electron/cli.js
cmd.exe        node_modules\.bin\electron.cmd
node.exe       npx
cmd.exe        cmd /c cd /d ... && npx electron .
```

`process` answers half of that — watch `electron.exe` rather than the runner.
But a name alone is not an identity: that sample alone had **eight**
`electron.exe` processes, one main process and seven helpers
(`--type=gpu-process`, `--type=renderer`, `--type=utility`), and every other
Electron program on the machine carries the same name.

`match` is the other half. When it is set, a process only counts as this entry
if its name fits *and* the text appears in its command line, and Stop only kills
those.  The useful value is something only this program has — the folder it lives
in, which is what Electron puts into the command line of both the main process
and its helpers.

* `process = "*"` gives up the name entirely and lets the command line decide,
  for a process whose name is not known before it runs. Be careful with it: any
  program whose command line merely *mentions* the text then counts as this
  entry, so the text has to be something only this program carries.
* A path full of backslashes is easiest to write as a TOML literal string with
  single quotes, as in the example above. The launcher writes it back the same
  way.
* **The text is compared case-insensitively and does not have to be a whole
  argument** — part of one is enough.
* A process whose command line cannot be read is never assumed to match, so a
  filter can never make Stop kill something it could not check.

Command lines are only read when at least one entry has a `match`; an entry
identified by its name alone still costs nothing extra (see
[below](#cost-of-watching-what-is-running)).

### Hotkeys

`hotkey` is a key combination written the way it is meant to be read:
`Ctrl+Alt+W`, `Shift+F9`, `Win+Q`. Modifiers come first, then exactly one key
from `A`–`Z`, `0`–`9` or `F1`–`F24`; names are not case sensitive and
`Ctrl`/`Control`, `Alt`/`Option` and `Win`/`Super`/`Meta`/`Cmd` are the same
thing.

* **Optional.** A group without a `hotkey` line is the normal case.
* **Set and removed in the window.** Click the key cap next to the group name,
  press the combination, done; "Remove hotkey" takes it away again. The config
  line is the same thing in text form — deleting it removes the hotkey.
* **Modifiers are required** unless the key is a function key. Registering a bare
  `W` would take that letter away from every other program for as long as the
  launcher runs, so it is refused, with the reason shown.
* **The `Win` key is written in the config.** The window toolkit does not report
  it on Windows, so a `Win+...` combination cannot be recorded in the window —
  typing it into the config file works, and it is reserved the same way. Windows
  itself keeps some of those combinations (search, snapping), and a chord that is
  taken is reported rather than silently ignored.
* **Reserved system-wide**, so it works while something else has the focus. If
  another program already holds the combination, the launcher says so instead of
  failing quietly, and the key still works inside the window.
* **Every failure is visible.** An unreadable chord, a chord that the operating
  system refuses, or a system that has no global hotkeys at all — each is
  reported in the status line at the bottom.

## Tray mode

With the **Tray** switch in the header on — `tray = true` in the config file —
the launcher lives in the notification area, and closing its window no longer
ends it:

* **Closing only takes the window off the screen.** The program keeps running,
  so the hotkeys keep toggling their groups, waiting delays still come due, and
  the status stays as honest as it was; it is simply not on screen. Nothing about
  the window's own state is touched while it is hidden, so it comes back exactly
  as it was left.
* **The icon is the way back.** A click on it brings the window up and to the
  front. Its menu — the right button, or a long press where that is the
  convention — lists every group by name together with its hotkey and offers
  *Show window* and *Quit*, so a group can be toggled without opening anything.
* **Quit is the only thing that ends the program.** Closing the window never
  does while tray mode is on, and the *Quit* line is not answered by hiding —
  otherwise a program in tray mode could not be ended from its own icon.
* **Hotkeys keep working while the window is hidden**, which is the point of the
  whole thing. The process list is re-read at the slower background cadence and
  a hotkey still re-reads it on the spot before deciding, so the key never acts
  on a stale picture.
* **Without an icon, nothing is hidden.** Where the launcher cannot put an icon
  up, it does not take the window away either: hiding a window with no way back
  to it would be a trap. The close button then keeps doing what it always did,
  and the reason is reported in the status line.
* **The header says which of the three states it is in**, in colour, because
  what the close button does is worth knowing at a glance: the lamp beside
  **Tray** is lit green while there really is an icon in the notification area,
  amber while tray mode is on but there is nowhere to put one, and a hollow ring
  while the mode is off. Only the green one means the launcher will outlive its
  window, and the whole switch is washed in that same colour while it is on.

Turning the switch on writes a line into the config like any other setting, and
switching it off takes the icon away again and answers the close button the old
way. The icon itself is the launcher's own status lamp, drawn in code instead of
shipped as an image file, so it cannot drift away from the window's palette.

Tray mode is wired up **for Windows only.** The other platforms each want an
arrangement of their own — Linux a GTK main loop, macOS a template image for the
menu bar — and none of that is in here, so there the switch reports that it
cannot put an icon up and the window goes on closing the way any window does.

### The console window behind the launcher

A debug build is a console program, so that `cargo run` shows whatever it has to
say. Started from a file manager, an editor or a shortcut instead of from a
terminal, Windows then gives it a console off its own — a black window on the
desktop. That hardly mattered while the launcher ended together with its window;
in tray mode the program outlives its window, and the black window is left
standing with nothing behind it. It is therefore given back at startup: see
[src/console.rs](src/console.rs).

Only a console the launcher has **to itself** is given back. One shared with a
shell is left alone, because that is where its own output goes — the notes the
hotkeys write at startup and the message a panic writes. A terminal, `cargo run`
and a script handing its console down are all recognised and left untouched; a
release build has no console to give back in the first place.

## The look

A launcher is a control panel, so the visual language is borrowed from hardware:
graphite surfaces, hairline separators, engraved labels, and a status lamp that
lights up when something is running.

Three rules hold it together:

1. **Colour only ever means state.** There is no brand accent. Green is running —
and, in the header, that the launcher really has an icon in the notification
area; grey is stopped; amber is something asked for that is not happening, which
is an unsaved change and, next to **Tray**, a tray mode with nowhere to put its
icon. A program waiting for its start or stop delay gets no colour of its own —
it says `starting in 2 s`, so that green keeps meaning exactly one thing.
2. **Structure comes from hairlines and alignment, not from boxes.** No card
around each program, no border around every group of things.
3. **Two kinds of type for two kinds of content.** Prose and names are set in
the UI face; machine values — paths, arguments, process names, key combinations
— are set in the monospace face, so they line up and can be compared character
by character.

Each program is one row: a status rail down its left edge, a lamp, the name, and
the three fields it owns — all editable in place, so nothing has to be opened
first. The group's own header carries the one button that matters for the whole
group, its key cap, and a line saying how much of it is running. The one loud
element is the lamp, because state is what this tool is for.

### Typeface

Taken from the operating system at startup — **no font file is kept in this
repository**. Three roles are filled by name, per platform:

| Role | Windows | Linux | macOS |
| --- | --- | --- | --- |
| Text | Segoe UI | DejaVu Sans | Arial |
| Heavier cut | Segoe UI Semibold | DejaVu Sans Bold | Arial Bold |
| Values | Cascadia Mono, Consolas | DejaVu Sans Mono | Courier New |

The built-in fonts inside egui stay behind each of them, so a missing face
degrades instead of leaving blank boxes: if the system offers no second weight,
the headings simply stop being heavier. `cargo test` checks that the faces are
really loading, because egui falls back silently and the whole design would
quietly become something else.

## Layout of the code

| File | Purpose |
| --- | --- |
| [src/main.rs](src/main.rs) | Window setup and startup |
| [src/config.rs](src/config.rs) | TOML config model, load/save |
| [src/processes.rs](src/processes.rs) | Start, find and stop processes |
| [src/hotkey.rs](src/hotkey.rs) | Key combinations, OS registration, press handling |
| [src/tray.rs](src/tray.rs) | The notification-area icon, its menu, and the icon image |
| [src/console.rs](src/console.rs) | Giving back the console window Windows opened for us |
| [src/fonts.rs](src/fonts.rs) | Finding the system's own typefaces |
| [src/theme.rs](src/theme.rs) | Palette, type scale, egui visuals |
| [src/app.rs](src/app.rs) | The GUI |

## Cost of watching what is running

The status has to be re-read to stay honest, so that work happens on every tick:

* the refresh asks only for what is displayed (process name, parent, start time)
  instead of the full set of CPU, memory, user and command line data per
  process — **command lines are read only when an entry has a `match`**, which
  is the one thing that cannot be had for free;
* only the process names that appear in the config are looked for, and both the
  names and the counts are kept between ticks instead of being rebuilt, so a
  steady state allocates nothing for the other few hundred processes on the
  machine;
* the list is read once per second while the window has focus and once every
  four seconds while it does not — a hotkey re-reads it on the spot before it
  decides, so the slower cadence never makes a toggle act on stale information.

`cargo test --bin starter benchmark_one_poll -- --ignored --nocapture` prints
four numbers for the machine you are on:

```
per poll: everything 16.6ms, names only 6.7ms, names only kept 7.2ms, with command lines 12.0ms
```

Reading only the names that matter is the win here — that is roughly a halving.
Keeping the names and the counts between ticks is **not** measurable against
that in this benchmark; the honest description is that it removes per-tick
allocation rather than time. The change that does reduce the work per second is
the cadence: while the window is in the background the launcher spends a quarter
of what it used to.

The last number is what a `match` costs: reading command lines as well takes a
poll from about 7 ms to about 12 ms on this machine. It is paid only while at
least one entry uses a `match`, and it is still below the 16.6 ms of the full
refresh that the launcher used to do for every entry.

A start or stop delay does not add a tick either: while one is waiting, the next
frame is scheduled for the moment it is due. Nothing spins in between, and a
delay still lands while the window is in the background — or hidden, in
[tray mode](#tray-mode).

## Tests

```sh
cargo test
```

Beyond parsing, the suite covers the parts that are easy to break silently:

* **A hotkey starts and stops its group for real.** A group is given a chord,
  the chord is fed in the way the operating system would report it, and a real
  helper process has to appear and disappear — including the case where the
  counts the window is still showing are out of date.
* **Recording takes the combination, not the modifier.** The presses are fed in
  as one batch — the way the window really receives a chord that is pressed
  quickly, since it only redraws when it has to — and the group ends up with
  `Ctrl+Alt+W` rather than with `Ctrl`. Holding only modifiers records nothing
  and is not complained about.
* **A chord the system refused still works inside the window**, through the same
  batch of presses.
* **Start and stop for real.** Two helper processes are started — one through the
  launcher's own start path, one completely outside of it — and both have to be
  found and killed, including via a fully qualified path.
* **A `match` picks one of two processes with the same name.** Two sleepers are
  started that differ only in their arguments; the name alone has to see both,
  the filter exactly one, and stopping through the filter has to leave the other
  one running. Without a refresh that reads command lines this test fails, which
  is the point of it.
* **A neighbour's match does not light this entry.** Two entries share a process
  name but carry different `match` texts; a count for one of them must leave the
  other dark, and `process = "*"` has to reach the rules as written.
* **A start delay holds a real program back, and a stop delay lets it run on.**
  The program is started with a delay of most of a second, so it has to be
  missing at first and running after the wait, and it has to survive the first
  moment of a stop as well. A third test presses the same key while a start is
  waiting: the wait has to be called off, and the program must not appear
  afterwards.
* **A wait decides when the next frame is drawn.** Waiting for a delay must
  wake the window by itself; otherwise a half-second delay asked for in the
  background would land up to four seconds late. The earliest wait wins, and a
  far-off one still does not make the window redraw sooner than it has to.
* **A group staggers its starts.** Three entries with delays of nothing, 0.6 s
  and 0.6 s: right after the command exactly one is running, and all three are
  only up after more than a second. A group that fired them off together, or all
  of them 0.6 s after the command, fails this test.
* **A delay is not a dependency.** The entry in front cannot start at all; the
  delayed one behind it still starts on time, because nothing is waited for but
  the previous *start*.
* **A waiting start is not green**, because green means running.
* **Delays are cleaned before they are waited out** — `nan`, `inf` and negative
  values would all end in a panic inside `Duration::from_secs_f32`.
* **Nothing is cut off.** Frames are rendered headlessly at six window sizes,
  including the minimum, with a name, a key combination and a path that are all
  too long for their space. Overflow that reaches the window edge fails the
  test; text scrolled inside its own field, and vertical scrolling, are drawn on
  purpose and ignored.
* **The system typefaces really load**, because a missing font falls back to
  egui's own without any error.
* **Colour tracks state.** With nothing running, not a single green shape may be
  painted; with a program marked running, exactly the lamp, the rail, the group
  lamp and the group summary dot must be.
* **Tray mode shows itself in colour, and only truthfully.** The header is drawn
  in each of its three states: amber and no green while tray mode is on without
  an icon, and exactly one green lamp — the lit one in the header — while the
  icon is really up.
* **Closing the window hides it while tray mode is on.** A frame is drawn with
  the close button pressed: it has to ask for the close to be called off and for
  the window to disappear, and the launcher has to stay alive. Without an icon
  the same frame must ask for neither — a window that hides itself with no way
  back would be a trap.
* **The head keeps working with the window hidden.** Only that half of a frame
  runs while the window is off the screen, so a hotkey is fed in and nothing but
  the head is drawn: its group has to be toggled anyway.
* **The icon does what it promises.** A click on it asks for the window back and
  to the front, a line of its menu toggles the group it names, and *Quit* is not
  answered by hiding the window.
* **Tray mode is a config value like any other** — written, saved, read back,
  and left out of the file while it is off.
* **Only a console of the launcher's own is given back.** The rule behind it is
  checked directly: no console at all, a console with one process on it, and a
  console shared with a shell each fall on the side they have to.

Four checks ask the machine instead of a mock and are marked `#[ignore]`, so they
only run when asked:

```sh
# whether the system will hand the chord out at all, and who already holds it
cargo test --bin starter the_os_hands_out_the_chord -- --ignored --nocapture

# what the registry really does at startup: manager, chords, refusals
cargo test --bin starter a_live_registry_says_what_it_did -- --ignored --nocapture

# the polling benchmark above
cargo test --bin starter benchmark_one_poll -- --ignored --nocapture

# what a live program looks like to the process list: set the folder it lives in
STARTER_MATCH_PROBE='C:\Tools\My App' cargo test --bin starter \
    a_live_folder_tells_same_named_programs_apart -- --ignored --nocapture
```

The last one prints how many processes of the given name carry that folder in
their command line, how many a folder that does not exist gets, and how many the
name alone gets — while the program is running.

## License

Copyright (C) 2026 starsh1ne

GNU General Public License, version 3 or (at your option) any later version — see
[LICENSE](LICENSE) for the full text. In short: you may use, study, share and
modify this program, and anything you pass on has to stay under the same licence.
It comes with **no warranty** whatsoever. The licence Cargo records is
`GPL-3.0-or-later`; if you would rather bind it to version 3 exactly, change that
line in [Cargo.toml](Cargo.toml) and the notice here to `GPL-3.0-only`.
