use std::path::{Path, PathBuf};

pub enum Status {
    Ok,
    Warn,
    Missing,
}

impl Status {
    fn label(&self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Warn => "warn",
            Status::Missing => "missing",
        }
    }
}

pub struct Check {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
}

pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    pub fn collect() -> Report {
        Report {
            checks: vec![
                wayland_check(),
                x11_check(),
                compositor_check(),
                clipboard_check(),
                capture_check(),
                inbox_check(),
                pictures_check(),
                desktop_check(),
                autostart_check(),
                sound_check(),
            ],
        }
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        for c in &self.checks {
            out.push_str(&format!("{} {}: {}\n", c.status.label(), c.name, c.detail));
        }
        out
    }
}

fn which(name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.contains('/') {
        return None;
    }
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(name);
        let Ok(meta) = std::fs::metadata(&candidate) else {
            continue;
        };
        if !meta.file_type().is_file() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o111 == 0 {
                continue;
            }
        }
        return Some(candidate);
    }
    None
}

fn wayland_display() -> Option<String> {
    std::env::var("WAYLAND_DISPLAY")
        .ok()
        .filter(|s| !s.is_empty())
}

fn wayland_socket(display: &str) -> Option<PathBuf> {
    let p = Path::new(display);
    if p.is_absolute() {
        Some(p.to_path_buf())
    } else {
        std::env::var_os("XDG_RUNTIME_DIR").map(|r| PathBuf::from(r).join(display))
    }
}

fn wayland_present() -> bool {
    wayland_display()
        .and_then(|d| wayland_socket(&d))
        .is_some_and(|p| p.exists())
}

fn wayland_check() -> Check {
    match wayland_display() {
        None => Check {
            name: "wayland",
            status: Status::Missing,
            detail: String::from("WAYLAND_DISPLAY unset"),
        },
        Some(display) => match wayland_socket(&display) {
            None => Check {
                name: "wayland",
                status: Status::Missing,
                detail: String::from("XDG_RUNTIME_DIR unset, cannot verify socket"),
            },
            Some(sock) if sock.exists() => Check {
                name: "wayland",
                status: Status::Ok,
                detail: format!(
                    "WAYLAND_DISPLAY={display} socket {} present",
                    sock.display()
                ),
            },
            Some(sock) => Check {
                name: "wayland",
                status: Status::Missing,
                detail: format!(
                    "WAYLAND_DISPLAY={display} socket {} missing",
                    sock.display()
                ),
            },
        },
    }
}

fn x11_check() -> Check {
    match std::env::var("DISPLAY").ok().filter(|s| !s.is_empty()) {
        Some(display) => Check {
            name: "x11",
            status: Status::Ok,
            detail: format!("DISPLAY={display}"),
        },
        // Wayland-only sessions legitimately leave DISPLAY unset.
        None if wayland_present() => Check {
            name: "x11",
            status: Status::Ok,
            detail: String::from("DISPLAY unset (wayland session)"),
        },
        None => Check {
            name: "x11",
            status: Status::Warn,
            detail: String::from("DISPLAY unset, no wayland"),
        },
    }
}

fn compositor_check() -> Check {
    if std::env::var_os("NIRI_SOCKET").is_some() {
        return Check {
            name: "compositor",
            status: Status::Ok,
            detail: String::from("niri (guess from NIRI_SOCKET)"),
        };
    }
    if std::env::var_os("SWAYSOCK").is_some() {
        return Check {
            name: "compositor",
            status: Status::Ok,
            detail: String::from("sway (guess from SWAYSOCK)"),
        };
    }
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
        return Check {
            name: "compositor",
            status: Status::Ok,
            detail: String::from("hyprland (guess from HYPRLAND_INSTANCE_SIGNATURE)"),
        };
    }
    if let Ok(desktop) = std::env::var("XDG_CURRENT_DESKTOP").map(|s| s.to_lowercase()) {
        if !desktop.is_empty() {
            return Check {
                name: "compositor",
                status: Status::Ok,
                detail: format!("{desktop} (guess from XDG_CURRENT_DESKTOP)"),
            };
        }
    }
    if std::env::var_os("GNOME_DESKTOP_SESSION_ID").is_some() {
        return Check {
            name: "compositor",
            status: Status::Ok,
            detail: String::from("gnome (guess from GNOME_DESKTOP_SESSION_ID)"),
        };
    }
    if std::env::var_os("KDE_SESSION_VERSION").is_some() {
        return Check {
            name: "compositor",
            status: Status::Ok,
            detail: String::from("kde (guess from KDE_SESSION_VERSION)"),
        };
    }
    Check {
        name: "compositor",
        status: Status::Warn,
        detail: String::from("unknown (no niri, sway, hyprland, desktop, gnome, kde marker)"),
    }
}

fn clipboard_check() -> Check {
    if wayland_display().is_some() {
        let paste = which("wl-paste").is_some();
        let copy = which("wl-copy").is_some();
        if paste && copy {
            Check {
                name: "clipboard",
                status: Status::Ok,
                detail: String::from("wl-copy, wl-paste on PATH"),
            }
        } else {
            let mut missing = Vec::new();
            if !copy {
                missing.push("wl-copy");
            }
            if !paste {
                missing.push("wl-paste");
            }
            Check {
                name: "clipboard",
                status: Status::Missing,
                detail: format!("missing {} on PATH", missing.join(", ")),
            }
        }
    } else if which("xclip").is_some() {
        Check {
            name: "clipboard",
            status: Status::Ok,
            detail: String::from("xclip on PATH"),
        }
    } else {
        Check {
            name: "clipboard",
            status: Status::Missing,
            detail: String::from("xclip missing on PATH"),
        }
    }
}

fn capture_check() -> Check {
    const TOOLS: [&str; 7] = [
        "grim",
        "slurp",
        "gnome-screenshot",
        "spectacle",
        "flameshot",
        "scrot",
        "maim",
    ];
    let found: Vec<&str> = TOOLS
        .iter()
        .copied()
        .filter(|t| which(t).is_some())
        .collect();
    if found.is_empty() {
        Check {
            name: "capture",
            status: Status::Warn,
            detail: String::from(
                "none of grim, slurp, gnome-screenshot, spectacle, flameshot, scrot, maim on PATH",
            ),
        }
    } else {
        Check {
            name: "capture",
            status: Status::Ok,
            detail: format!("found: {}", found.join(", ")),
        }
    }
}

fn inbox_check() -> Check {
    match mansher_core::inbox_dir() {
        Ok(dir) => {
            // Read-only writability probe: never create a test file in the inbox.
            let writable = std::fs::metadata(&dir)
                .map(|m| !m.permissions().readonly())
                .unwrap_or(false);
            if dir.is_dir() && writable {
                Check {
                    name: "inbox",
                    status: Status::Ok,
                    detail: format!("{} present, writable", dir.display()),
                }
            } else {
                Check {
                    name: "inbox",
                    status: Status::Missing,
                    detail: format!("{} not writable", dir.display()),
                }
            }
        }
        Err(e) => Check {
            name: "inbox",
            status: Status::Missing,
            detail: format!("creation failed: {e:#}"),
        },
    }
}

fn pictures_check() -> Check {
    let dirs = mansher_core::external_watch_dirs();
    if let Some(hit) = dirs.iter().find(|p| p.ends_with("Pictures/Screenshots")) {
        return Check {
            name: "pictures",
            status: Status::Ok,
            detail: format!("{} present", hit.display()),
        };
    }
    let expected = directories::UserDirs::new()
        .and_then(|u| u.picture_dir().map(|p| p.join("Screenshots")))
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| String::from("/tmp")))
                .join("Pictures/Screenshots")
        });
    Check {
        name: "pictures",
        status: Status::Warn,
        detail: format!("{} absent", expected.display()),
    }
}

fn desktop_check() -> Check {
    let dir = directories::UserDirs::new()
        .map(|u| u.home_dir().join("Desktop"))
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| String::from("/tmp")))
                .join("Desktop")
        });
    if dir.is_dir() {
        Check {
            name: "desktop",
            status: Status::Ok,
            detail: format!("{} present", dir.display()),
        }
    } else {
        Check {
            name: "desktop",
            status: Status::Warn,
            detail: format!("{} absent", dir.display()),
        }
    }
}

fn autostart_check() -> Check {
    let path = crate::settings::autostart_desktop();
    if path.is_file() {
        Check {
            name: "autostart",
            status: Status::Ok,
            detail: format!("{} present", path.display()),
        }
    } else {
        Check {
            name: "autostart",
            status: Status::Ok,
            detail: format!("off (no file at {})", path.display()),
        }
    }
}

fn sound_check() -> Check {
    if which("canberra-gtk-play").is_some() {
        Check {
            name: "sound",
            status: Status::Ok,
            detail: String::from("canberra-gtk-play on PATH"),
        }
    } else {
        Check {
            name: "sound",
            status: Status::Warn,
            detail: String::from("canberra-gtk-play absent from PATH"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEQ: AtomicU64 = AtomicU64::new(0);
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn temp_dir(tag: &str) -> PathBuf {
        let n = TEST_SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "mansher-doctor-test-{}-{n}-{tag}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn which_finds_fake_executable_and_misses_absent() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = temp_dir("which");
        let fake = dir.join("mansher-fake-tool-xyz");
        std::fs::write(&fake, b"#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let old_path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![dir.clone()];
        paths.extend(std::env::split_paths(&old_path));
        let joined = std::env::join_paths(paths).unwrap();
        std::env::set_var("PATH", &joined);
        let found = which("mansher-fake-tool-xyz");
        let missing = which("mansher-doctor-absent-xyz-12345");
        std::env::set_var("PATH", &old_path);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(found, Some(fake));
        assert_eq!(missing, None);
    }

    #[test]
    fn render_emits_ok_line_shape() {
        let report = Report {
            checks: vec![Check {
                name: "wayland",
                status: Status::Ok,
                detail: String::from("test-detail"),
            }],
        };
        assert_eq!(report.render(), "ok wayland: test-detail\n");
    }

    #[test]
    fn all_status_variants_render() {
        let report = Report {
            checks: vec![
                Check {
                    name: "a",
                    status: Status::Ok,
                    detail: String::from("d1"),
                },
                Check {
                    name: "b",
                    status: Status::Warn,
                    detail: String::from("d2"),
                },
                Check {
                    name: "c",
                    status: Status::Missing,
                    detail: String::from("d3"),
                },
            ],
        };
        assert_eq!(report.render(), "ok a: d1\nwarn b: d2\nmissing c: d3\n");
    }
}
