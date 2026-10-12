//! File actions (no gio dep: xdg-open, freedesktop Trash, image).
//!
//! Open/annotate via spawned viewers, freedesktop Trash moves, and
//! Keep-in-Pictures moves. Pure filesystem moves with explicit dirs so
//! tests pass temp dirs and production passes real dirs.

use std::path::{Path, PathBuf};
use std::process::Command as Proc;

use anyhow::Result;
use mansher_core::{unique_path, unique_stem_name_occupied};

pub(crate) fn open_path(path: &Path) {
    let _ = Proc::new("xdg-open").arg(path).spawn();
}

pub(crate) fn open_folder(path: &Path) {
    if let Some(parent) = path.parent() {
        let _ = Proc::new("xdg-open").arg(parent).spawn();
    }
}

/// Long-press: upstream opens MarkupUI. We try native annotators, else viewer.
pub(crate) fn annotate_path(path: &Path) {
    let path_str = path.to_string_lossy().into_owned();
    if Proc::new("swappy").arg("-f").arg(&path_str).spawn().is_ok() {
        return;
    }
    if Proc::new("drawing").arg(&path_str).spawn().is_ok() {
        return;
    }
    open_path(path);
}

fn data_home_dir() -> PathBuf {
    std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| String::from("/tmp")))
                .join(".local/share")
        })
}

/// Move a file to the freedesktop Trash ($XDG_DATA_HOME/Trash).
pub(crate) fn trash_file(path: &Path) -> Result<()> {
    trash_file_in(path, &data_home_dir())
}

/// Trash move with an explicit base dir (tests pass temp dirs; production
/// callers pass `data_home_dir()` via `trash_file`).
///
/// Freedesktop Trash spec conformance: only regular files (symlinks and dirs
/// refused — `symlink_metadata` so a symlink to a file is refused, not
/// followed), the collision loop claims both `files/` and `info/` names, the
/// `Path=` value is percent-encoded, and fresh `files/`/`info/` dirs get
/// mode 0o700. Failures return `Err` with no `.trashinfo` left behind.
pub(crate) fn trash_file_in(path: &Path, data_home: &Path) -> Result<()> {
    let ft = std::fs::symlink_metadata(path)?.file_type();
    if !ft.is_file() {
        anyhow::bail!(
            "mansher: refusing to trash non-regular file {}",
            path.display()
        );
    }
    let trash = data_home.join("Trash");
    ensure_private_dir(&trash.join("files"))?;
    ensure_private_dir(&trash.join("info"))?;
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| String::from("screenshot"));
    let stem = Path::new(&name)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| name.clone());
    let ext = Path::new(&name)
        .extension()
        .map(|s| format!(".{}", s.to_string_lossy()))
        .unwrap_or_default();
    let candidate = unique_stem_name_occupied(&stem, ext.strip_prefix('.').unwrap_or(&ext), |c| {
        trash.join("files").join(c).exists()
            || trash.join("info").join(format!("{c}.trashinfo")).exists()
    });
    let deletion = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
    let info = format!(
        "[Trash Info]\nPath={}\nDeletionDate={}\n",
        trash_encode_path(path),
        deletion
    );
    std::fs::write(
        trash.join("info").join(format!("{candidate}.trashinfo")),
        info,
    )?;
    std::fs::rename(path, trash.join("files").join(&candidate))?;
    Ok(())
}

/// Percent-encode a Trash `Path=` value: keep URI unreserved chars and `/`,
/// encode all other bytes. Non-UTF8 names go through lossy conversion first
/// (same as the rest of this file) — reported assumption, not a new scheme.
fn trash_encode_path(path: &Path) -> String {
    let s = path.to_string_lossy();
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Create `dir` (and parents) with mode 0o700 when new; pre-existing dirs
/// keep their permissions deliberately.
fn ensure_private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    if dir.is_dir() {
        return Ok(());
    }
    std::fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(dir)?;
    Ok(())
}

/// Keep-in-Pictures move with an explicit destination dir (tests pass temp
/// dirs; the menu handler passes `pictures_dir()`): unique-name then `rename`,
/// mirroring the inline handler logic. Across filesystems (`EXDEV`) falls
/// back to copy + verified remove-source.
pub(crate) fn keep_in_dir(path: &Path, dest_dir: &Path) -> Result<PathBuf> {
    let fallback = String::from("screenshot.png");
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or(fallback);
    let dest = unique_path(dest_dir, &name);
    match std::fs::rename(path, &dest) {
        Ok(()) => Ok(dest),
        Err(e) if e.raw_os_error() == Some(EXDEV) => copy_then_remove(path, &dest),
        Err(e) => Err(e.into()),
    }
}

/// `EXDEV` (invalid cross-device link): `rename` across filesystems.
const EXDEV: i32 = 18;

/// Copy fallback for cross-filesystem Keep: copy the bytes, verify by
/// re-reading both lengths, and remove the source ONLY on a verified copy.
/// Anything uncertain leaves everything in place and logs.
fn copy_then_remove(src: &Path, dest: &Path) -> Result<PathBuf> {
    let copied = std::fs::copy(src, dest)?;
    let src_len = std::fs::metadata(src)?.len();
    let dest_len = std::fs::metadata(dest)?.len();
    if copied != src_len || src_len != dest_len {
        anyhow::bail!(
            "mansher: keep copy unverified, source kept at {}",
            src.display()
        );
    }
    std::fs::remove_file(src)?;
    Ok(dest.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let n = TEST_SEQ.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("mansher-app-test-{}-{n}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn trash_roundtrip_moves_file_and_writes_trashinfo() {
        let work = temp_dir("trash-rt");
        let src = work.join("shot.png");
        std::fs::write(&src, b"pixels").unwrap();
        trash_file_in(&src, &work).unwrap();
        assert!(!src.exists());
        assert!(work.join("Trash/files/shot.png").exists());
        let body = std::fs::read_to_string(work.join("Trash/info/shot.png.trashinfo")).unwrap();
        assert!(body.starts_with("[Trash Info]\n"));
        assert!(body.contains(&format!("Path={}\n", src.display())));
        assert!(body.contains("DeletionDate="));
        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn trash_collision_renumbers_second_file() {
        let work = temp_dir("trash-collide");
        std::fs::create_dir_all(work.join("Trash/files")).unwrap();
        std::fs::write(work.join("Trash/files/shot.png"), b"old").unwrap();
        let src = work.join("shot.png");
        std::fs::write(&src, b"new").unwrap();
        trash_file_in(&src, &work).unwrap();
        let second = work.join("Trash/files/shot 2.png");
        assert!(second.exists());
        assert_eq!(std::fs::read(&second).unwrap(), b"new");
        assert!(work.join("Trash/info/shot 2.png.trashinfo").exists());
        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn trash_missing_source_returns_error() {
        let work = temp_dir("trash-missing");
        let missing = work.join("ghost.png");
        assert!(trash_file_in(&missing, &work).is_err());
        // Plan 005: failures leave no orphan .trashinfo behind.
        assert!(!work.join("Trash/info/ghost.png.trashinfo").exists());
        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn trash_symlink_refused() {
        let work = temp_dir("trash-symlink");
        let target = work.join("real.png");
        std::fs::write(&target, b"pixels").unwrap();
        let link = work.join("link.png");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(trash_file_in(&link, &work).is_err());
        assert!(target.exists());
        assert!(!work.join("Trash/files/link.png").exists());
        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn trash_dir_refused() {
        let work = temp_dir("trash-dir");
        let sub = work.join("subdir");
        std::fs::create_dir_all(&sub).unwrap();
        assert!(trash_file_in(&sub, &work).is_err());
        assert!(sub.exists());
        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn trash_info_collision_renumbers_candidate() {
        let work = temp_dir("trash-info-collide");
        // files/ is free but info/ already claims the name: candidate moves on.
        std::fs::create_dir_all(work.join("Trash/info")).unwrap();
        std::fs::write(work.join("Trash/info/shot.png.trashinfo"), b"old").unwrap();
        let src = work.join("shot.png");
        std::fs::write(&src, b"new").unwrap();
        trash_file_in(&src, &work).unwrap();
        assert!(work.join("Trash/files/shot 2.png").exists());
        assert!(work.join("Trash/info/shot 2.png.trashinfo").exists());
        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn trash_path_value_is_percent_encoded() {
        let work = temp_dir("trash-encode");
        let src = work.join("my shot.png");
        std::fs::write(&src, b"pixels").unwrap();
        trash_file_in(&src, &work).unwrap();
        let body = std::fs::read_to_string(work.join("Trash/info/my shot.png.trashinfo")).unwrap();
        assert!(body.contains("my%20shot.png"));
        assert!(!body.contains("my shot.png"));
        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn trash_creates_dirs_mode_700() {
        use std::os::unix::fs::PermissionsExt;
        let work = temp_dir("trash-mode");
        let src = work.join("shot.png");
        std::fs::write(&src, b"pixels").unwrap();
        trash_file_in(&src, &work).unwrap();
        for d in ["Trash/files", "Trash/info"] {
            let mode = std::fs::metadata(work.join(d))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700, "{d} has wrong mode");
        }
        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn keep_roundtrip_moves_file_to_dest_dir() {
        let work = temp_dir("keep-rt");
        let dest_dir = work.join("pics");
        std::fs::create_dir_all(&dest_dir).unwrap();
        let src = work.join("shot.png");
        std::fs::write(&src, b"pixels").unwrap();
        let dest = keep_in_dir(&src, &dest_dir).unwrap();
        assert_eq!(dest, dest_dir.join("shot.png"));
        assert!(!src.exists());
        assert_eq!(std::fs::read(&dest).unwrap(), b"pixels");
        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn keep_collision_renumbers_second_file() {
        let work = temp_dir("keep-collide");
        let dest_dir = work.join("pics");
        std::fs::create_dir_all(&dest_dir).unwrap();
        std::fs::write(dest_dir.join("shot.png"), b"old").unwrap();
        let src = work.join("shot.png");
        std::fs::write(&src, b"new").unwrap();
        let dest = keep_in_dir(&src, &dest_dir).unwrap();
        assert_eq!(dest, dest_dir.join("shot 2.png"));
        assert_eq!(std::fs::read(&dest).unwrap(), b"new");
        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn keep_copy_fallback_copies_and_removes_source() {
        let work = temp_dir("keep-copy");
        let src = work.join("shot.png");
        std::fs::write(&src, b"pixels").unwrap();
        let dest = work.join("pics").join("shot.png");
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        let out = copy_then_remove(&src, &dest).unwrap();
        assert_eq!(out, dest);
        assert!(!src.exists());
        assert_eq!(std::fs::read(&dest).unwrap(), b"pixels");
        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn keep_copy_fallback_missing_source_errors() {
        let work = temp_dir("keep-copy-missing");
        let missing = work.join("ghost.png");
        let dest = work.join("pics").join("ghost.png");
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        assert!(copy_then_remove(&missing, &dest).is_err());
        assert!(!dest.exists());
        let _ = std::fs::remove_dir_all(&work);
    }
}
