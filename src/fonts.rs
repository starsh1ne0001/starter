//! Typefaces, read from the operating system at startup.
//!
//! No font file lives in this repository. What the design asks for is a text
//! face, a heavier companion so headings can carry weight, and a monospace face
//! for machine values — paths, arguments, process names. Those are looked up in
//! the OS font directories; whatever is missing falls back to the fonts that
//! ship inside egui, so a wrong guess degrades rather than breaks.
//!
//! The probes are per platform because that is where the answers differ: Segoe
//! UI on Windows, DejaVu on most Linux installs, the system Arial on macOS.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The faces the design wants, already read into memory.
#[derive(Debug, Default)]
pub struct Faces {
    pub text: Option<Face>,
    /// A heavier companion to `text`; `None` means headings fall back to `text`.
    pub strong: Option<Face>,
    pub mono: Option<Face>,
}

#[derive(Debug, Clone)]
pub struct Face {
    pub bytes: Vec<u8>,
}

/// The faces of this machine, read once.
pub fn os_faces() -> &'static Faces {
    static FACES: OnceLock<Faces> = OnceLock::new();
    FACES.get_or_init(load)
}

pub fn load() -> Faces {
    let dirs = font_dirs();
    Faces {
        text: first_usable(&dirs, TEXT_FACES),
        strong: first_usable(&dirs, STRONG_FACES),
        mono: first_usable(&dirs, MONO_FACES),
    }
}

/// Directories that hold font files, most likely first.
fn font_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();

    #[cfg(windows)]
    {
        // %WINDIR% is the documented location; the fallback matters when the
        // variable is missing, which happens in stripped-down environments.
        let windows = std::env::var_os("WINDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        dirs.push(windows.join("Fonts"));
        // Fonts a user installed for themselves only, without admin rights.
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            dirs.push(PathBuf::from(local).join("Microsoft").join("Windows").join("Fonts"));
        }
    }

    #[cfg(target_os = "linux")]
    {
        dirs.extend(
            [
                "/usr/share/fonts/truetype/dejavu",
                "/usr/share/fonts/truetype/liberation",
                "/usr/share/fonts/truetype/noto",
                "/usr/share/fonts/TTF",
                "/usr/share/fonts",
                "/usr/local/share/fonts",
            ]
            .iter()
            .map(PathBuf::from),
        );
        if let Some(home) = std::env::var_os("HOME") {
            dirs.push(PathBuf::from(home).join(".local/share/fonts"));
        }
    }

    #[cfg(target_os = "macos")]
    {
        dirs.extend(
            [
                "/System/Library/Fonts/Supplemental",
                "/System/Library/Fonts",
                "/Library/Fonts",
            ]
            .iter()
            .map(PathBuf::from),
        );
        if let Some(home) = std::env::var_os("HOME") {
            dirs.push(PathBuf::from(home).join("Library/Fonts"));
        }
    }

    dirs
}

// File names to try, per role. Not every one of these exists on every machine;
// the first that does wins, and the order expresses the preference.

#[cfg(windows)]
const TEXT_FACES: &[&str] = &["segoeui.ttf", "tahoma.ttf", "arial.ttf"];
#[cfg(windows)]
const STRONG_FACES: &[&str] = &["seguisb.ttf", "segoeuib.ttf", "arialbd.ttf"];
#[cfg(windows)]
const MONO_FACES: &[&str] = &["CascadiaMono.ttf", "consola.ttf", "cour.ttf"];

#[cfg(target_os = "linux")]
const TEXT_FACES: &[&str] = &["DejaVuSans.ttf", "LiberationSans-Regular.ttf"];
#[cfg(target_os = "linux")]
const STRONG_FACES: &[&str] = &["DejaVuSans-Bold.ttf", "LiberationSans-Bold.ttf"];
#[cfg(target_os = "linux")]
const MONO_FACES: &[&str] = &["DejaVuSansMono.ttf", "LiberationMono-Regular.ttf"];

#[cfg(target_os = "macos")]
const TEXT_FACES: &[&str] = &["Arial.ttf", "Verdana.ttf"];
#[cfg(target_os = "macos")]
const STRONG_FACES: &[&str] = &["Arial Bold.ttf", "Verdana Bold.ttf"];
#[cfg(target_os = "macos")]
const MONO_FACES: &[&str] = &["Courier New.ttf"];

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
const TEXT_FACES: &[&str] = &[];
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
const STRONG_FACES: &[&str] = &[];
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
const MONO_FACES: &[&str] = &[];

fn first_usable(dirs: &[PathBuf], names: &[&str]) -> Option<Face> {
    for name in names {
        for dir in dirs {
            if let Some(face) = read_face(&dir.join(name)) {
                return Some(face);
            }
        }
    }
    None
}

fn read_face(path: &Path) -> Option<Face> {
    let bytes = std::fs::read(path).ok()?;
    is_single_face(&bytes).then_some(Face { bytes })
}

/// True for the formats the text stack can actually parse.
///
/// Font collections (`ttcf`) are rejected on purpose: they hold several faces
/// and picking the right one is not something we can do from the header alone,
/// so it is better to fall through to the next candidate.
fn is_single_face(bytes: &[u8]) -> bool {
    matches!(
        bytes.first_chunk::<4>(),
        Some([0x00, 0x01, 0x00, 0x00]) | Some(b"true") | Some(b"OTTO")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file that is not a font must be rejected rather than handed to the
    /// text stack, where it would fail later and far less clearly.
    #[test]
    fn only_real_single_face_fonts_are_accepted() {
        assert!(is_single_face(&[0x00, 0x01, 0x00, 0x00, 0x00]));
        assert!(is_single_face(b"true    "));
        assert!(is_single_face(b"OTTO    "));

        // A collection, an empty file, a text file.
        assert!(!is_single_face(b"ttcf"));
        assert!(!is_single_face(b""));
        assert!(!is_single_face(b"not a font at all"));
    }

    /// The probe list must not name the same file twice in one role.
    #[test]
    fn candidate_lists_are_ordered_and_unique() {
        for list in [TEXT_FACES, STRONG_FACES, MONO_FACES] {
            let mut seen = std::collections::HashSet::new();
            for name in list {
                assert!(seen.insert(*name), "{name} is listed twice");
            }
        }
    }

    /// On Windows the text and strong faces are part of the OS, so the design
    /// can rely on them; if this ever fails, the whole type hierarchy has
    /// quietly collapsed to a single weight.
    #[cfg(windows)]
    #[test]
    fn the_system_offers_both_weights() {
        let faces = os_faces();
        let text = faces.text.as_ref().expect("no text face was found");
        let strong = faces
            .strong
            .as_ref()
            .expect("no semibold or bold face was found among the candidates");

        // The two roles have to end up as different faces; the same file under
        // two names would look like a working design while nothing gets heavier.
        assert_ne!(
            text.bytes, strong.bytes,
            "the text and strong roles resolved to the same file"
        );
    }
}
