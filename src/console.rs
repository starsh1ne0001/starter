//! The console window Windows opens for a program started without one.
//!
//! A debug build is a console program — that is what the attribute at the top of
//! [`crate::main`] arranges, so that `cargo run` shows whatever the program has
//! to say. Started from a file manager or a shortcut instead of from a terminal,
//! the same build is handed a console off its own by Windows, and that console
//! is a black window behind the launcher.
//!
//! While the launcher ended together with its window this hardly showed: the
//! black window left with it. In tray mode the program outlives its window, so
//! the black window is left standing on the desktop with nothing behind it. It
//! is therefore given back at startup, before the window is built.
//!
//! Only a console this program has **to itself** is given back. A console shared
//! with a shell — the one a terminal, `cargo run`, or a script hands down — is
//! left alone: it is where output goes, and taking it away would swallow the
//! notes the hotkeys write at startup and the message a panic writes.
//!
//! Everywhere else there is nothing to do: other systems give a program no
//! second window of their own to get rid of.

/// Gives back the console window this program was given, where it was given one
/// of its own.
///
/// Called once, at the very start of the program. Does nothing at all when the
/// console is shared with something else, or when there is no console to begin
/// with.
#[cfg(windows)]
pub fn release_ours() {
    use windows_sys::Win32::System::Console::{FreeConsole, GetConsoleProcessList};

    let mut attached = [0u32; 1];
    // A one-slot buffer is enough for the question being asked: the call answers
    // with the number of processes attached either way, and more than one means
    // the console is not ours to give back.
    let count = unsafe { GetConsoleProcessList(attached.as_mut_ptr(), attached.len() as u32) };
    if !ours_alone(count) {
        return;
    }
    unsafe {
        FreeConsole();
    }
}

/// Whether a console with this many processes attached is one this program was
/// given for itself.
///
/// Taken out of the call so the rule is readable and can be checked: zero means
/// there is no console at all (the call failed, or the program was started
/// without one), and everything above one means somebody else is using it too.
#[cfg(windows)]
fn ours_alone(attached: u32) -> bool {
    attached == 1
}

/// Nothing to give back: no other system starts a program with a second window
/// of its own.
#[cfg(not(windows))]
pub fn release_ours() {}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// The one rule that keeps the developer's own console: a console we share
    /// with a shell is not ours to take away, and one that does not exist cannot
    /// be taken away either.
    #[test]
    fn only_a_console_of_our_own_is_given_back() {
        assert!(!ours_alone(0), "a program without a console has none to give");
        assert!(ours_alone(1), "a console with one process is ours");
        assert!(!ours_alone(2), "a console shared with a shell is not ours");
        assert!(!ours_alone(9), "a console shared with a shell is not ours");
    }
}
