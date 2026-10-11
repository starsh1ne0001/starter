//! Configuration model for the launcher.
//!
//! The whole state of the launcher is a list of groups, each holding a list of
//! programs. It is stored as TOML so that it stays readable and hand-editable
//! even though the GUI can change it too.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Name of the config file used when no path is passed on the command line.
pub const DEFAULT_FILE_NAME: &str = "launcher.toml";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub groups: Vec<Group>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Group {
    pub name: String,
    /// Optional key combination that toggles the whole group, e.g. `Ctrl+Alt+W`.
    /// Written the way it is meant to be read; empty means "no hotkey".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub hotkey: String,
    #[serde(default)]
    pub programs: Vec<Program>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Program {
    /// Label shown in the GUI.
    pub name: String,
    /// Executable to launch. Either a bare name found on `PATH` or a full path.
    pub program: String,
    /// Optional arguments, written as one string (quotes group an argument).
    #[serde(default)]
    pub args: String,
    /// Process name used to detect and stop the program. Empty means "use the
    /// file name of `program`", e.g. `notepad.exe`.
    ///
    /// `*` means "any name": the command line alone decides, which is what a
    /// program started through a runner needs.
    #[serde(default)]
    pub process: String,
    /// Optional text that has to appear in a process's command line for it to
    /// count as this program. Empty means "the process name is enough".
    ///
    /// Written as `match` in the file — the Rust keyword is the reason for the
    /// other name here.
    #[serde(default, rename = "match", skip_serializing_if = "String::is_empty")]
    pub match_on: String,
    /// Seconds to wait before this program is started, e.g. so that whatever it
    /// depends on is up first. Zero — the default, and what an entry without the
    /// line gets — means "at once".
    #[serde(default, skip_serializing_if = "is_zero")]
    pub start_delay: f32,
    /// Seconds to wait before this program is stopped, e.g. so that something
    /// else gets to shut down first. Zero means "at once".
    #[serde(default, skip_serializing_if = "is_zero")]
    pub stop_delay: f32,
}

/// `skip_serializing_if` for the delays: a wait of nothing is not written.
fn is_zero(seconds: &f32) -> bool {
    *seconds == 0.0
}

impl Program {
    /// Process name that is searched for in the OS process list.
    ///
    /// Never `*`: "any name" is a matter of seeking, not of naming, so the
    /// wildcard is left as it is written and interpreted when matching.
    pub fn process_name(&self) -> String {
        let override_name = self.process.trim();
        if !override_name.is_empty() {
            return override_name.to_string();
        }
        Path::new(self.program.trim())
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.program.trim().to_string())
    }

    /// `true` when the entry has enough information to be launched.
    pub fn is_launchable(&self) -> bool {
        !self.program.trim().is_empty()
    }
}

impl Config {
    /// Loads the config from `path`, creating a small example config if the
    /// file does not exist yet.
    pub fn load(path: &Path) -> Result<Self, String> {
        if !path.exists() {
            let config = Self::example();
            config
                .save(path)
                .map_err(|err| format!("could not create {}: {err}", path.display()))?;
            return Ok(config);
        }

        let text = std::fs::read_to_string(path)
            .map_err(|err| format!("could not read {}: {err}", path.display()))?;
        toml::from_str(&text).map_err(|err| format!("{} is not valid TOML: {err}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text = toml::to_string_pretty(self).map_err(|err| format!("could not serialize: {err}"))?;
        if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("could not create {}: {err}", parent.display()))?;
        }
        std::fs::write(path, text).map_err(|err| format!("could not write {}: {err}", path.display()))
    }

    /// Small starter config so the first start is never an empty window.
    fn example() -> Self {
        let program = |name: &str, program: &str| Program {
            name: name.to_string(),
            program: program.to_string(),
            ..Program::default()
        };

        Self {
            groups: vec![
                Group {
                    name: "Everyday".to_string(),
                    hotkey: "Ctrl+Alt+E".to_string(),
                    programs: vec![
                        program("Notepad", "notepad.exe"),
                        program("Calculator", "calc.exe"),
                    ],
                },
                Group {
                    name: "Browser".to_string(),
                    hotkey: String::new(),
                    programs: vec![program("Chrome", "chrome.exe")],
                },
            ],
        }
    }
}

/// Path to the config file: first command line argument, or the default file in
/// the current directory.
pub fn config_path_from_args() -> PathBuf {
    std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_FILE_NAME))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path() -> PathBuf {
        std::env::temp_dir()
            .join(format!("starter-config-test-{}", std::process::id()))
            .join("launcher.toml")
    }

    #[test]
    fn process_name_defaults_to_the_executable_name() {
        let program = Program {
            program: r"C:\Program Files\Notepad\notepad.exe".to_string(),
            ..Program::default()
        };
        assert_eq!(program.process_name(), "notepad.exe");

        let program = Program {
            program: "calc.exe".to_string(),
            process: " CalculatorApp.exe ".to_string(),
            ..Program::default()
        };
        assert_eq!(program.process_name(), "CalculatorApp.exe");

        // "any name" still has to survive this step untouched.
        let program = Program {
            program: "cmd".to_string(),
            process: "*".to_string(),
            ..Program::default()
        };
        assert_eq!(program.process_name(), "*");
    }

    #[test]
    fn missing_file_is_created_and_can_be_read_again() {
        let path = temp_path();
        let _ = std::fs::remove_dir_all(path.parent().unwrap());

        let created = Config::load(&path).expect("config could not be created");
        assert!(!created.groups.is_empty());

        let reloaded = Config::load(&path).expect("created config is not readable");
        assert_eq!(reloaded.groups.len(), created.groups.len());
        assert_eq!(reloaded.groups[0].programs[0].program, "notepad.exe");

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn every_field_survives_a_round_trip() {
        let config = Config {
            groups: vec![Group {
                name: "Test".to_string(),
                hotkey: "Ctrl+Alt+T".to_string(),
                programs: vec![Program {
                    name: "Tool".to_string(),
                    program: "tool.exe".to_string(),
                    args: "--fast".to_string(),
                    process: "tool-real.exe".to_string(),
                    match_on: r"C:\Tools\thing".to_string(),
                    start_delay: 2.5,
                    stop_delay: 1.0,
                }],
            }],
        };

        let text = toml::to_string_pretty(&config).expect("serializing failed");
        let parsed: Config = toml::from_str(&text).expect("serialized config does not parse");
        assert_eq!(parsed.groups[0].hotkey, "Ctrl+Alt+T");
        let program = &parsed.groups[0].programs[0];
        assert_eq!(program.name, "Tool");
        assert_eq!(program.program, "tool.exe");
        assert_eq!(program.args, "--fast");
        assert_eq!(program.process, "tool-real.exe");
        assert_eq!(program.match_on, r"C:\Tools\thing");
        assert_eq!(program.start_delay, 2.5);
        assert_eq!(program.stop_delay, 1.0);
    }

    /// A whole number of seconds is how a person writes a delay, so the file has
    /// to accept it even though the field is a float.
    #[test]
    fn a_delay_may_be_a_whole_number_or_a_fraction() {
        let text = r#"
            [[groups]]
            name = "Slow"

            [[groups.programs]]
            name = "Server"
            program = "cmd"
            start_delay = 2
            stop_delay = 0.5
        "#;

        let config: Config = toml::from_str(text).expect("hand written config does not parse");
        let program = &config.groups[0].programs[0];
        assert_eq!(program.start_delay, 2.0);
        assert_eq!(program.stop_delay, 0.5);
    }

    /// A delay of nothing is the normal case and must not clutter the file.
    #[test]
    fn a_delay_of_zero_is_not_written_to_the_file() {
        let config = Config {
            groups: vec![Group {
                name: "Plain".to_string(),
                programs: vec![
                    Program {
                        name: "Now".to_string(),
                        program: "notepad.exe".to_string(),
                        ..Program::default()
                    },
                    Program {
                        name: "Later".to_string(),
                        program: "notepad.exe".to_string(),
                        start_delay: 3.0,
                        ..Program::default()
                    },
                ],
                ..Group::default()
            }],
        };

        let text = toml::to_string_pretty(&config).expect("serializing failed");
        assert_eq!(
            text.matches("start_delay").count(),
            1,
            "only the entry that waits should carry a delay:\n{text}"
        );
        assert!(!text.contains("stop_delay"), "a zero delay was written:\n{text}");

        let again: Config = toml::from_str(&text).expect("what we wrote does not parse");
        assert_eq!(again.groups[0].programs[1].start_delay, 3.0);
        assert_eq!(again.groups[0].programs[0].start_delay, 0.0);
    }

    /// The key is written `match`, and a path in it must survive the round trip
    /// even though it is full of backslashes.
    #[test]
    fn the_match_key_is_written_and_read_by_its_name() {
        let text = r#"
            [[groups]]
            name = "Runners"

            [[groups.programs]]
            name = "Electron app"
            program = "cmd"
            args = "/c npx electron ."
            process = "electron.exe"
            match = 'C:\Tools\Sample App'
        "#;

        let config: Config = toml::from_str(text).expect("hand written config does not parse");
        let program = &config.groups[0].programs[0];
        assert_eq!(program.match_on, r"C:\Tools\Sample App");

        let written = toml::to_string_pretty(&config).expect("serializing failed");
        assert!(
            written.contains(r"match = 'C:\Tools\Sample App'"),
            "the key must stay `match`, and a path full of backslashes has to stay \
             readable instead of turning into a wall of escapes:\n{written}"
        );
        let again: Config = toml::from_str(&written).expect("what we wrote does not parse");
        assert_eq!(again.groups[0].programs[0].match_on, program.match_on);
    }

    #[test]
    fn omitted_optional_keys_are_still_accepted() {
        let text = r#"
            [[groups]]
            name = "Minimal"

            [[groups.programs]]
            name = "Tool"
            program = "tool.exe"
        "#;

        let config: Config = toml::from_str(text).expect("hand written config does not parse");
        assert_eq!(config.groups[0].hotkey, "", "a group without a hotkey is normal");
        let program = &config.groups[0].programs[0];
        assert_eq!(program.args, "");
        assert_eq!(program.process, "");
        assert_eq!(program.match_on, "");
        assert_eq!(program.process_name(), "tool.exe");
    }

    /// A program identified by its name alone must not grow an empty `match`
    /// line — an empty filter says nothing.
    #[test]
    fn an_empty_match_is_not_written_to_the_file() {
        let config = Config {
            groups: vec![Group {
                name: "Plain".to_string(),
                programs: vec![Program {
                    name: "Notepad".to_string(),
                    program: "notepad.exe".to_string(),
                    ..Program::default()
                }],
                ..Group::default()
            }],
        };

        let text = toml::to_string_pretty(&config).expect("serializing failed");
        assert!(!text.contains("match"), "an empty match was written:\n{text}");
    }

    /// A group without a hotkey must not grow a `hotkey = ""` line: the file is
    /// meant to stay readable, and an empty key says nothing.
    #[test]
    fn an_empty_hotkey_is_not_written_to_the_file() {
        let config = Config {
            groups: vec![
                Group {
                    name: "Quiet".to_string(),
                    ..Group::default()
                },
                Group {
                    name: "Loud".to_string(),
                    hotkey: "Ctrl+Alt+L".to_string(),
                    ..Group::default()
                },
            ],
        };

        let text = toml::to_string_pretty(&config).expect("serializing failed");
        assert_eq!(
            text.matches("hotkey").count(),
            1,
            "only the group that has one should carry a hotkey line:\n{text}"
        );
        assert!(!text.contains("hotkey = \"\""), "an empty hotkey was written:\n{text}");
    }
}
