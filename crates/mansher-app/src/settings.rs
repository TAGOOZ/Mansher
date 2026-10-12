//! Settings, sounds, autostart (upstream menu-bar item parity).
//!
//! Persisted sound flag, freedesktop sound-theme playback, XDG Autostart
//! entry, and the Pictures dir. No GTK deps, so this moves first.

use std::path::PathBuf;
use std::process::Command as Proc;
use std::sync::{Arc, Mutex};

use mansher_core::layout;

#[derive(serde::Serialize, serde::Deserialize)]
struct Settings {
    #[serde(default = "sound_default")]
    sound_on: bool,
    #[serde(default = "max_items_default")]
    max_items: usize,
}

fn sound_default() -> bool {
    true
}

fn max_items_default() -> usize {
    layout::MAX_PEGS
}

fn clamp_max_items(n: usize) -> usize {
    n.clamp(4, 48)
}

fn read_settings() -> Settings {
    std::fs::read_to_string(settings_path())
        .ok()
        .and_then(|b| serde_json::from_str::<Settings>(&b).ok())
        .unwrap_or(Settings {
            sound_on: sound_default(),
            max_items: max_items_default(),
        })
}

fn write_settings(s: &Settings) {
    let path = settings_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, serde_json::to_string(s).unwrap_or_default());
}

fn settings_path() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| String::from("/tmp")))
        .join(".config/mansher/settings.json")
}

pub(crate) fn load_sound_on() -> bool {
    read_settings().sound_on
}

pub(crate) fn save_sound_on(on: bool) {
    let mut s = read_settings();
    s.sound_on = on;
    write_settings(&s);
}

pub fn load_max_items() -> usize {
    clamp_max_items(read_settings().max_items)
}

pub fn save_max_items(n: usize) -> usize {
    let mut s = read_settings();
    s.max_items = clamp_max_items(n);
    write_settings(&s);
    s.max_items
}

/// Upstream `Line.play`: Tink on hang, Pop on drop. Linux mapping through
/// the freedesktop sound theme (best-effort; missing theme/server = silent).
/// Callers check the shared flag first so a muted line spawns nothing.
fn play_sound(name: &str) {
    let _ = Proc::new("canberra-gtk-play")
        .arg("-i")
        .arg(name)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

pub(crate) fn play_if_on(name: &str, sound_on: &Arc<Mutex<bool>>) {
    let on = sound_on.lock().map(|g| *g).unwrap_or(true);
    if on {
        play_sound(name);
    }
}

pub(crate) fn autostart_desktop() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| String::from("/tmp")))
        .join(".config/autostart/mansher.desktop")
}

/// Upstream "Open at login": an XDG Autostart entry pointing at this exact
/// binary (via /proc/self, so AppImage/relocated installs keep working).
// No field codes (%f etc.) are ever passed, so plain double-quote rules apply.
// If Exec ever gains arguments, quote each arg individually, never the joined line.
pub(crate) fn quote_exec_arg(exe: &str) -> Option<String> {
    if !std::path::Path::new(exe).is_absolute() {
        return None;
    }
    if !(exe.contains(' ') || exe.contains('"') || exe.contains('\\')) {
        return Some(exe.to_owned());
    }
    let mut out = String::with_capacity(exe.len() + 2);
    out.push('"');
    for c in exe.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    Some(out)
}

pub(crate) fn set_autostart(on: bool) {
    let path = autostart_desktop();
    if !on {
        let _ = std::fs::remove_file(&path);
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| String::from("mansher-app"));
    // Refusal must never write a relative Exec; leave any previous file untouched.
    let Some(quoted) = quote_exec_arg(&exe) else {
        eprintln!("mansher: refusing to write autostart entry with relative exe path");
        return;
    };
    let body = format!(
        "[Desktop Entry]\nType=Application\nName=Mansher\nComment=Screenshots hang on a clothesline\nExec={quoted}\nHidden=false\nX-GNOME-Autostart-enabled=true\n"
    );
    let _ = std::fs::write(&path, body);
}

pub(crate) fn pictures_dir() -> PathBuf {
    directories::UserDirs::new()
        .and_then(|u| u.picture_dir().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| String::from("/tmp")))
                .join("Pictures")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEQ: AtomicU64 = AtomicU64::new(0);
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn temp_dir(tag: &str) -> PathBuf {
        let n = TEST_SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "mansher-settings-test-{}-{n}-{tag}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn with_temp_home(f: impl FnOnce()) {
        let _guard = ENV_LOCK.lock().unwrap();
        let home = temp_dir("home");
        let state = temp_dir("state");
        let old_home = std::env::var_os("HOME");
        let old_state = std::env::var_os("MANSHER_STATE_DIR");
        std::env::set_var("HOME", &home);
        std::env::set_var("MANSHER_STATE_DIR", &state);
        f();
        match old_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match old_state {
            Some(v) => std::env::set_var("MANSHER_STATE_DIR", v),
            None => std::env::remove_var("MANSHER_STATE_DIR"),
        }
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn autostart_exec_plain_path_bare() {
        assert_eq!(
            quote_exec_arg("/usr/bin/mansher-app"),
            Some(String::from("/usr/bin/mansher-app"))
        );
    }

    #[test]
    fn autostart_exec_space_path_quoted() {
        assert_eq!(
            quote_exec_arg("/home/u/My Apps/mansher-app"),
            Some(String::from("\"/home/u/My Apps/mansher-app\""))
        );
    }

    #[test]
    fn autostart_exec_quote_and_backslash_escaped() {
        assert_eq!(
            quote_exec_arg("/home/u/We\"ird\\Dir/mansher-app"),
            Some(String::from("\"/home/u/We\\\"ird\\\\Dir/mansher-app\""))
        );
    }

    #[test]
    fn autostart_exec_body_roundtrip() {
        let exe = "/home/u/My Apps/mansher-app";
        let quoted = quote_exec_arg(exe).unwrap();
        let body = format!(
            "[Desktop Entry]\nType=Application\nName=Mansher\nComment=Screenshots hang on a clothesline\nExec={quoted}\nHidden=false\nX-GNOME-Autostart-enabled=true\n"
        );
        let line = body.lines().find(|l| l.starts_with("Exec=")).unwrap();
        assert_eq!(line, "Exec=\"/home/u/My Apps/mansher-app\"");
        let value = &line["Exec=".len()..];
        let inner = &value[1..value.len() - 1];
        let mut decoded = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                decoded.push(chars.next().unwrap());
            } else {
                decoded.push(c);
            }
        }
        assert_eq!(decoded, exe);
    }

    #[test]
    fn autostart_exec_relative_refused() {
        assert_eq!(quote_exec_arg("mansher-app"), None);
        assert_eq!(quote_exec_arg("./relative/path"), None);
    }

    #[test]
    fn max_items_default_is_max_pegs_when_unset() {
        with_temp_home(|| {
            assert_eq!(load_max_items(), 24);
            assert_eq!(load_max_items(), layout::MAX_PEGS);
        });
    }

    #[test]
    fn max_items_roundtrip_persists() {
        with_temp_home(|| {
            save_max_items(12);
            assert_eq!(load_max_items(), 12);
        });
    }

    #[test]
    fn max_items_clamp_holds_at_bounds() {
        with_temp_home(|| {
            save_max_items(0);
            assert_eq!(load_max_items(), 4);
            save_max_items(999);
            assert_eq!(load_max_items(), 48);
        });
    }

    #[test]
    fn max_items_save_preserves_sound_flag() {
        with_temp_home(|| {
            save_sound_on(false);
            save_max_items(10);
            assert_eq!(load_max_items(), 10);
            assert!(!load_sound_on());
        });
    }
}
