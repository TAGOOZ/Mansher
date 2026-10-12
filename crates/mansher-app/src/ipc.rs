//! Single-instance + `--toggle` / `--preferences` IPC over a Unix socket in `$XDG_RUNTIME_DIR`.
//!
//! Compositor keybinds (Niri/Sway/Hyprland) and GNOME custom shortcuts just
//! spawn `mansher-app --toggle`; the running instance reveals or hides.
//! `--preferences` opens the Preferences window the same way, for sessions
//! with no tray. No portal, no permissions, works on every distro.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::Duration;

/// One line the listener understands. Unit-free: the wire carries words.
pub enum IpcEvent {
    Toggle,
    Preferences,
}

pub fn sock_path() -> PathBuf {
    if let Ok(rt) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(rt).join("mansher.sock");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| String::from("/tmp"));
    PathBuf::from(home).join(".cache/mansher.sock")
}

/// Ask a running instance to toggle. Returns true if delivered.
pub fn send_toggle() -> bool {
    send_line("toggle\n")
}

/// Ask a running instance to open Preferences. Returns true if delivered.
pub fn send_preferences() -> bool {
    send_line("preferences\n")
}

fn send_line(line: &str) -> bool {
    let Ok(mut s) = UnixStream::connect(sock_path()) else {
        return false;
    };
    let _ = s.set_write_timeout(Some(Duration::from_secs(2)));
    s.write_all(line.as_bytes()).is_ok()
}

/// Guard holding the bound socket fd: whoever holds it owns the path.
/// The fd moves into `start_listener` with no rebind window — never
/// reintroduce bind-then-drop probing.
pub struct PrimaryHold {
    listener: UnixListener,
}

/// Outcome of bidding for primary ownership.
pub enum PrimaryResult {
    /// This process is primary; pass the hold to `start_listener`.
    Primary(PrimaryHold),
    /// Another live instance owns the socket; toggle it and exit.
    Secondary,
    /// No live owner but the socket cannot be bound; run standalone.
    Unavailable,
}

/// Bid for primary ownership and HOLD the bound fd on success: the
/// acquire→listen handoff is a single fd, so two starters can never both
/// believe they are primary. A stale path is reclaimed only after a
/// REFUSED probe-connect proves no live owner; a live socket is never
/// unlinked.
pub fn acquire_primary() -> PrimaryResult {
    let path = sock_path();
    if ensure_parent(&path).is_err() {
        eprintln!("mansher: ipc unavailable, running standalone");
        return PrimaryResult::Unavailable;
    }
    match UnixListener::bind(&path) {
        Ok(listener) => {
            tight_perms(&path);
            PrimaryResult::Primary(PrimaryHold { listener })
        }
        Err(_) => {
            if UnixStream::connect(&path).is_ok() {
                return PrimaryResult::Secondary; // someone alive owns it
            }
            let _ = std::fs::remove_file(&path);
            match UnixListener::bind(&path) {
                Ok(listener) => {
                    tight_perms(&path);
                    PrimaryResult::Primary(PrimaryHold { listener })
                }
                Err(e) => {
                    eprintln!("mansher: ipc unavailable ({e}), running standalone");
                    PrimaryResult::Unavailable
                }
            }
        }
    }
}

/// Create the socket parent dir. The `$XDG_RUNTIME_DIR` parent is already
/// 0700 by pam_systemd convention; the `~/.cache` fallback parent gets
/// 0o700 explicitly. Pre-existing dirs keep their permissions.
fn ensure_parent(path: &Path) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    if parent.is_dir() {
        return Ok(());
    }
    if std::env::var("XDG_RUNTIME_DIR").is_err() {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(parent)
    } else {
        std::fs::create_dir_all(parent)
    }
}

/// Best-effort `chmod 0600` on the bound socket (Unix sockets honor
/// filesystem perms for local access control on Linux).
fn tight_perms(path: &Path) {
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

/// Serve Toggle/Preferences events on the HELD socket. Runs on its own thread.
pub fn start_listener(hold: PrimaryHold, tx: Sender<IpcEvent>) {
    let listener = hold.listener;
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(stream) = conn else { continue };
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            if reader.read_line(&mut line).is_ok() {
                let ev = match line.trim() {
                    "toggle" => Some(IpcEvent::Toggle),
                    "preferences" => Some(IpcEvent::Preferences),
                    _ => None,
                };
                if let Some(ev) = ev {
                    let _ = tx.send(ev);
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Mutex, MutexGuard};

    /// Env vars are process-global: serialize every test that mutates them.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

    fn lock_env() -> MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap()
    }

    fn fresh_dir(tag: &str) -> PathBuf {
        let n = TMP_SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "mansher-ipc-test-{}-{}-{}",
            std::process::id(),
            n,
            tag
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Saved `XDG_RUNTIME_DIR`/`HOME`, restored on drop while the lock is held.
    struct SavedEnv {
        runtime: Option<OsString>,
        home: Option<OsString>,
    }

    impl SavedEnv {
        fn take() -> Self {
            Self {
                runtime: std::env::var_os("XDG_RUNTIME_DIR"),
                home: std::env::var_os("HOME"),
            }
        }
    }

    impl Drop for SavedEnv {
        fn drop(&mut self) {
            match self.runtime.take() {
                Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
                None => std::env::remove_var("XDG_RUNTIME_DIR"),
            }
            match self.home.take() {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[test]
    fn send_toggle_without_listener_returns_false() {
        let _lock = lock_env();
        let _saved = SavedEnv::take();
        let dir = fresh_dir("no-listener");
        std::env::set_var("XDG_RUNTIME_DIR", &dir);
        assert!(!send_toggle());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn second_acquire_while_held_returns_secondary() {
        let _lock = lock_env();
        let _saved = SavedEnv::take();
        let dir = fresh_dir("second-acquire");
        std::env::set_var("XDG_RUNTIME_DIR", &dir);
        let PrimaryResult::Primary(hold) = acquire_primary() else {
            panic!("fresh dir must acquire primary");
        };
        let (tx, _rx) = std::sync::mpsc::channel();
        start_listener(hold, tx);
        assert!(matches!(acquire_primary(), PrimaryResult::Secondary));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_socket_reclaimed_by_acquire() {
        let _lock = lock_env();
        let _saved = SavedEnv::take();
        let dir = fresh_dir("stale");
        std::env::set_var("XDG_RUNTIME_DIR", &dir);
        let path = sock_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let stale = UnixListener::bind(&path).unwrap();
        drop(stale); // dead process: socket file remains, nobody listening
        let PrimaryResult::Primary(_hold) = acquire_primary() else {
            panic!("stale socket must be reclaimed");
        };
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn toggle_delivery_reaches_listener() {
        let _lock = lock_env();
        let _saved = SavedEnv::take();
        let dir = fresh_dir("delivery");
        std::env::set_var("XDG_RUNTIME_DIR", &dir);
        let PrimaryResult::Primary(hold) = acquire_primary() else {
            panic!("fresh dir must acquire primary");
        };
        let (tx, rx) = std::sync::mpsc::channel();
        start_listener(hold, tx);
        assert!(send_toggle());
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            IpcEvent::Toggle
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn preferences_delivery_reaches_listener() {
        let _lock = lock_env();
        let _saved = SavedEnv::take();
        let dir = fresh_dir("delivery-prefs");
        std::env::set_var("XDG_RUNTIME_DIR", &dir);
        let PrimaryResult::Primary(hold) = acquire_primary() else {
            panic!("fresh dir must acquire primary");
        };
        let (tx, rx) = std::sync::mpsc::channel();
        start_listener(hold, tx);
        assert!(send_preferences());
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            IpcEvent::Preferences
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn send_preferences_without_listener_returns_false() {
        let _lock = lock_env();
        let _saved = SavedEnv::take();
        let dir = fresh_dir("no-listener-prefs");
        std::env::set_var("XDG_RUNTIME_DIR", &dir);
        assert!(!send_preferences());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn socket_mode_0600_after_acquire() {
        let _lock = lock_env();
        let _saved = SavedEnv::take();
        let dir = fresh_dir("sock-mode");
        std::env::set_var("XDG_RUNTIME_DIR", &dir);
        let PrimaryResult::Primary(_hold) = acquire_primary() else {
            panic!("fresh dir must acquire primary");
        };
        let mode = std::fs::metadata(sock_path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fallback_parent_0700_when_created() {
        let _lock = lock_env();
        let _saved = SavedEnv::take();
        let home = fresh_dir("home");
        std::env::remove_var("XDG_RUNTIME_DIR");
        std::env::set_var("HOME", &home);
        let PrimaryResult::Primary(_hold) = acquire_primary() else {
            panic!("fallback dir must acquire primary");
        };
        let parent = sock_path().parent().unwrap().to_path_buf();
        assert_eq!(parent, home.join(".cache"));
        let mode = std::fs::metadata(&parent).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        assert!(sock_path().exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn concurrent_acquire_yields_exactly_one_primary() {
        use std::sync::Barrier;
        let _lock = lock_env();
        let _saved = SavedEnv::take();
        let dir = fresh_dir("race");
        std::env::set_var("XDG_RUNTIME_DIR", &dir);
        const N: usize = 8;
        let barrier = std::sync::Arc::new(Barrier::new(N));
        let handles: Vec<_> = (0..N)
            .map(|_| {
                let b = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    b.wait();
                    acquire_primary()
                })
            })
            .collect();
        let mut primary = 0;
        let mut secondary = 0;
        let mut _holds = Vec::new();
        for h in handles {
            match h.join().unwrap() {
                PrimaryResult::Primary(hold) => {
                    primary += 1;
                    _holds.push(hold);
                }
                PrimaryResult::Secondary => secondary += 1,
                PrimaryResult::Unavailable => panic!("acquire failed under contention"),
            }
        }
        assert_eq!(primary, 1);
        assert_eq!(secondary, N - 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sock_path_falls_back_to_home_cache() {
        let _lock = lock_env();
        let _saved = SavedEnv::take();
        let home = fresh_dir("home");
        std::env::remove_var("XDG_RUNTIME_DIR");
        std::env::set_var("HOME", &home);
        assert_eq!(sock_path(), home.join(".cache/mansher.sock"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn gnome_extension_cut_no_variant_no_tree() {
        // Cut by plan 010: the Shell half was unverifiable (Niri session,
        // headless CI, 45-48 skew), so the extension tree is gone and the
        // sensor variant with it. The events stay: the raw strip still feeds
        // HotEnter/HotLeave.
        use mansher_core::showhide::Event;

        let mut root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        root.pop(); // crates/mansher-app -> crates
        root.pop(); // crates -> repo root
        assert!(
            !root.join("extensions/gnome").exists(),
            "gnome extension tree must stay deleted"
        );
        let src = std::fs::read_to_string(root.join("crates/mansher-app/src/main.rs")).unwrap();
        assert!(
            !src.contains("GnomeExtension"),
            "detect_sensor must not construct GnomeExtension"
        );
        assert!(matches!(Event::HotEnter(0), Event::HotEnter(_)));
        assert!(matches!(Event::HotLeave, Event::HotLeave));
    }
}
