//! Upstream `makeThumbnail` port: display-sized thumbnail cache.
//!
//! Cards show thumbs whose natural size already fits the layout photo box,
//! so rows always fit the output. Gestures keep acting on the originals.
//! Cache keys fold in mtime and length, so annotate rewrites regenerate
//! (upstream `reloadThumbnail`). Blocking calls run on the worker thread,
//! never on the UI thread.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};

pub const MAX_W: u32 = mansher_core::layout::PHOTO_MAX_W;
pub const MAX_H: u32 = mansher_core::layout::PHOTO_MAX_H;

// Caps are policy, not physics: 8k screenshots exist; raise with a test
// image at the new bound, never drop the byte cap.
pub(crate) const MAX_IMAGE_DIM: u32 = 16384;
pub(crate) const MAX_IMAGE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DecodeReject {
    OverLimits,
    DecodeFailed,
}

impl std::fmt::Display for DecodeReject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeReject::OverLimits => write!(f, "image over limits"),
            DecodeReject::DecodeFailed => write!(f, "image decode failed"),
        }
    }
}

impl std::error::Error for DecodeReject {}

/// Header gate: byte cap, then dims cap, before any full decode.
pub(crate) fn bounded_dimensions(path: &Path) -> Result<(u32, u32), DecodeReject> {
    let len = std::fs::metadata(path)
        .map_err(|_| DecodeReject::DecodeFailed)?
        .len();
    if len > MAX_IMAGE_BYTES {
        eprintln!(
            "mansher: image over limits, skipping {} ({len} bytes)",
            path.display()
        );
        return Err(DecodeReject::OverLimits);
    }
    let (w, h) = image::image_dimensions(path).map_err(|_| DecodeReject::DecodeFailed)?;
    if w > MAX_IMAGE_DIM || h > MAX_IMAGE_DIM {
        eprintln!(
            "mansher: image over limits, skipping {} ({w}x{h})",
            path.display()
        );
        return Err(DecodeReject::OverLimits);
    }
    Ok((w, h))
}

pub(crate) fn bounded_open(path: &Path) -> Result<image::DynamicImage, DecodeReject> {
    bounded_dimensions(path)?;
    image::open(path).map_err(|_| DecodeReject::DecodeFailed)
}

pub(crate) fn bounded_open_bytes(png: &[u8]) -> Result<image::DynamicImage, DecodeReject> {
    if png.len() as u64 > MAX_IMAGE_BYTES {
        eprintln!(
            "mansher: image over limits, skipping clipboard bytes ({} bytes)",
            png.len()
        );
        return Err(DecodeReject::OverLimits);
    }
    let (w, h) = image::ImageReader::new(std::io::Cursor::new(png))
        .with_guessed_format()
        .map_err(|_| DecodeReject::DecodeFailed)?
        .into_dimensions()
        .map_err(|_| DecodeReject::DecodeFailed)?;
    if w > MAX_IMAGE_DIM || h > MAX_IMAGE_DIM {
        eprintln!("mansher: image over limits, skipping clipboard bytes ({w}x{h})");
        return Err(DecodeReject::OverLimits);
    }
    image::load_from_memory(png).map_err(|_| DecodeReject::DecodeFailed)
}

pub fn thumb_dir() -> PathBuf {
    let base = std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| String::from("/tmp")))
                .join(".cache")
        });
    base.join("mansher").join("thumbs")
}

fn key_for(path: &Path) -> Option<String> {
    key_for_dims(path, MAX_W, MAX_H)
}

fn key_for_dims(path: &Path, box_w: u32, box_h: u32) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    let mut hasher = DefaultHasher::new();
    (
        path.to_string_lossy().as_ref(),
        mtime,
        meta.len(),
        box_w,
        box_h,
    )
        .hash(&mut hasher);
    Some(format!("{:016x}.png", hasher.finish()))
}

/// Blocking: fit inside the display box, save PNG, return its path.
/// None on any error (unsupported format, unreadable file).
pub fn ensure_thumb(path: &Path) -> Option<PathBuf> {
    let key = key_for(path)?;
    let dir = thumb_dir();
    let dest = dir.join(&key);
    if dest.exists() {
        return Some(dest);
    }
    let img = bounded_open(path).ok()?;
    let thumb = img.thumbnail(MAX_W, MAX_H);
    std::fs::create_dir_all(&dir).ok()?;
    // Temp file plus rename: never leave a half-written thumb behind.
    // Explicit format: the .part suffix defeats extension inference.
    let tmp = dir.join(format!("{key}.part"));
    thumb.save_with_format(&tmp, image::ImageFormat::Png).ok()?;
    std::fs::rename(&tmp, &dest).ok()?;
    Some(dest)
}

/// Lookup only, for the UI thread. Never decodes.
pub fn cached_thumb(path: &Path) -> Option<PathBuf> {
    let dest = thumb_dir().join(key_for(path)?);
    dest.exists().then_some(dest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};

    fn sample(name: &str, w: u32, h: u32, px: [u8; 3]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mansher-th-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(format!("{name}-{w}x{h}.png"));
        RgbImage::from_pixel(w, h, Rgb(px)).save(&p).unwrap();
        p
    }

    #[test]
    fn landscape_thumb_keeps_ratio_inside_box() {
        let p = sample("wide", 1920, 1080, [200, 30, 40]);
        let t = ensure_thumb(&p).unwrap();
        let (w, h) = image::image_dimensions(&t).unwrap();
        assert_eq!(w, mansher_core::layout::PHOTO_MAX_W);
        assert!(
            (h as f64 / w as f64 - 9.0 / 16.0).abs() < 0.03,
            "w={w} h={h}"
        );
    }

    #[test]
    fn deterministic_path_reused_until_rewrite() {
        let p = sample("stable", 400, 400, [10, 20, 30]);
        let a = ensure_thumb(&p).unwrap();
        assert_eq!(ensure_thumb(&p), Some(a.clone()));
        RgbImage::from_pixel(300, 400, Rgb([1, 2, 3]))
            .save(&p)
            .unwrap();
        let f = std::fs::File::options().write(true).open(&p).unwrap();
        f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(60))
            .unwrap();
        let b = ensure_thumb(&p).unwrap();
        assert_ne!(a, b, "rewrite must regenerate");
    }

    #[test]
    fn missing_file_is_none_not_panic() {
        assert!(ensure_thumb(Path::new("/nonexistent/x.png")).is_none());
        assert!(cached_thumb(Path::new("/nonexistent/x.png")).is_none());
    }

    #[test]
    fn photo_box_size_change_busts_thumb_key() {
        let p = sample("keysize", 400, 400, [7, 8, 9]);
        let old = key_for_dims(&p, 183, 140).unwrap();
        let new = key_for_dims(&p, MAX_W, MAX_H).unwrap();
        assert_ne!(old, new);
        assert_eq!(key_for(&p).unwrap(), new);
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xffff_ffffu32;
        for &b in data {
            crc ^= b as u32;
            for _ in 0..8 {
                if crc & 1 == 1 {
                    crc = (crc >> 1) ^ 0xedb8_8320;
                } else {
                    crc >>= 1;
                }
            }
        }
        !crc
    }

    fn chunk(typ: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(typ);
        out.extend_from_slice(data);
        let mut crc_in = Vec::from(*typ);
        crc_in.extend_from_slice(data);
        out.extend_from_slice(&crc32(&crc_in).to_be_bytes());
        out
    }

    fn huge_header_png() -> Vec<u8> {
        let mut out = vec![137, 80, 78, 71, 13, 10, 26, 10];
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&20000u32.to_be_bytes());
        ihdr.extend_from_slice(&20000u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
        out.extend_from_slice(&chunk(b"IHDR", &ihdr));
        // Minimal IDAT: the png decoder only reports dimensions once an
        // IDAT/fdAT chunk is present, so a header-only file errors as
        // DecodeFailed instead of OverLimits. A tiny body keeps the header
        // parse cheap while still carrying huge IHDR dims.
        out.extend_from_slice(&chunk(b"IDAT", &[0x78, 0x9c, 0x03, 0, 0, 0, 0, 1]));
        out.extend_from_slice(&chunk(b"IEND", &[]));
        out
    }

    #[test]
    fn oversize_header_rejected_by_cap_without_full_decode() {
        let bytes = huge_header_png();
        assert_eq!(bounded_open_bytes(&bytes), Err(DecodeReject::OverLimits));
        let dir = std::env::temp_dir().join(format!("mansher-th-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("huge-hdr.png");
        std::fs::write(&p, &bytes).unwrap();
        assert_eq!(bounded_open(&p), Err(DecodeReject::OverLimits));
        assert_eq!(bounded_dimensions(&p), Err(DecodeReject::OverLimits));
        assert!(ensure_thumb(&p).is_none());
    }

    #[test]
    fn truncated_file_skips_without_panic() {
        let mut full = Vec::new();
        RgbImage::from_pixel(32, 32, Rgb([9, 9, 9]))
            .write_to(
                &mut std::io::Cursor::new(&mut full),
                image::ImageFormat::Png,
            )
            .unwrap();
        let cut = full.len() / 2;
        assert_eq!(
            bounded_open_bytes(&full[..cut]),
            Err(DecodeReject::DecodeFailed)
        );
        let dir = std::env::temp_dir().join(format!("mansher-th-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("trunc.png");
        std::fs::write(&p, &full[..cut]).unwrap();
        assert_eq!(bounded_open(&p), Err(DecodeReject::DecodeFailed));
        assert!(ensure_thumb(&p).is_none());
    }

    #[test]
    fn normal_1920x1080_passes_bounded_decode() {
        let p = sample("normal1920", 1920, 1080, [11, 22, 33]);
        let img = bounded_open(&p).unwrap();
        assert_eq!((img.width(), img.height()), (1920, 1080));
        assert_eq!(bounded_dimensions(&p), Ok((1920, 1080)));
        assert!(ensure_thumb(&p).is_some());
        let bytes = std::fs::read(&p).unwrap();
        let mem = bounded_open_bytes(&bytes).unwrap();
        assert_eq!((mem.width(), mem.height()), (1920, 1080));
    }
}
