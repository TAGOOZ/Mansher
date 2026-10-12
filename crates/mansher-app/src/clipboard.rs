//! Clipboard serve (Wayland in-process offer).
//!
//! Wayland has no system-owned pasteboard: the offer lives only as long as
//! a client serves it. Each copy spawns a thread that owns its Wayland
//! connection and blocks until the offer is superseded.

use std::path::Path;

use anyhow::Result;
use arboard::SetExtLinux;

use crate::clipwatch;

/// Copy image bytes to the clipboard. Returns the RGBA pixel hash so the
/// clipboard watcher can recognize our own copies and skip re-hanging them.
///
/// Wayland has no system-owned pasteboard (unlike macOS `pboard`): the offer
/// lives only as long as a client serves it. arboard's default `set_*` forks
/// a detached serve daemon — observed flaky here (offer gone ~2s after set,
/// 1 in 4 pastes empty). So we serve in-process instead, macOS-style: each
/// copy spawns a thread that owns its Wayland connection and blocks in
/// `set().wait()` until the offer is superseded. The old thread then exits
/// on its own, so at most two serve threads ever exist transiently.
pub(crate) enum CopyPayload {
    Image(arboard::ImageData<'static>),
    Text(String),
}

pub(crate) fn payload_for(path: &Path) -> Result<(CopyPayload, u64)> {
    match crate::thumbs::bounded_open(path) {
        Ok(img) => {
            let rgba = img.to_rgba8();
            let hash = clipwatch::hash_bytes(rgba.as_raw());
            let (w, h) = (rgba.width() as usize, rgba.height() as usize);
            let data = arboard::ImageData {
                width: w,
                height: h,
                bytes: std::borrow::Cow::Owned(rgba.into_raw()),
            };
            Ok((CopyPayload::Image(data), hash))
        }
        Err(_) => {
            let text = path.to_string_lossy().into_owned();
            let hash = clipwatch::hash_bytes(text.as_bytes());
            Ok((CopyPayload::Text(text), hash))
        }
    }
}

pub(crate) fn serve_forever(payload: CopyPayload) {
    std::thread::spawn(move || {
        let mut clip = match arboard::Clipboard::new() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("mansher: clipboard serve unavailable: {e:#}");
                return;
            }
        };
        let r = match payload {
            CopyPayload::Image(d) => clip.set().wait().image(d),
            CopyPayload::Text(t) => clip.set().wait().text(t),
        };
        if let Err(e) = r {
            eprintln!("mansher: clipboard serve ended: {e:#}");
        }
    });
}

pub(crate) fn copy_to_clipboard(path: &Path) -> Result<u64> {
    let (payload, hash) = payload_for(path)?;
    serve_forever(payload);
    Ok(hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let n = TEST_SEQ.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("mansher-app-test-{}-{n}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn test_png(w: u32, h: u32, px: [u8; 3]) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(w, h, image::Rgb(px));
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        buf.into_inner()
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

    fn huge_header_png() -> Vec<u8> {
        fn chunk(typ: &[u8; 4], data: &[u8]) -> Vec<u8> {
            let mut out = Vec::new();
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            out.extend_from_slice(typ);
            out.extend_from_slice(data);
            let mut c = Vec::from(*typ);
            c.extend_from_slice(data);
            out.extend_from_slice(&crc32(&c).to_be_bytes());
            out
        }
        let mut out = vec![137, 80, 78, 71, 13, 10, 26, 10];
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&20000u32.to_be_bytes());
        ihdr.extend_from_slice(&20000u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
        out.extend_from_slice(&chunk(b"IHDR", &ihdr));
        out.extend_from_slice(&chunk(b"IDAT", &[0x78, 0x9c, 0x03, 0, 0, 0, 0, 1]));
        out.extend_from_slice(&chunk(b"IEND", &[]));
        out
    }

    #[test]
    fn oversize_header_rejected_by_cap_with_text_fallback() {
        let dir = temp_dir("oversize");
        let p = dir.join("huge.png");
        std::fs::write(&p, huge_header_png()).unwrap();
        assert_eq!(
            crate::thumbs::bounded_open(&p),
            Err(crate::thumbs::DecodeReject::OverLimits)
        );
        let cache = RefCell::new(crate::cards::SizeCache::new());
        assert_eq!(crate::cards::photo_display_size(&cache, &p), (272, 208));
        match payload_for(&p).unwrap() {
            (CopyPayload::Text(t), _) => assert_eq!(t, p.to_string_lossy()),
            (CopyPayload::Image(_), _) => panic!("oversize file must fall back to Text"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn truncated_file_skips_with_fallbacks_no_panic() {
        let dir = temp_dir("trunc");
        let full = test_png(32, 32, [4, 5, 6]);
        let p = dir.join("cut.png");
        std::fs::write(&p, &full[..full.len() / 2]).unwrap();
        assert_eq!(
            crate::thumbs::bounded_open(&p),
            Err(crate::thumbs::DecodeReject::DecodeFailed)
        );
        // Header survives the cut, so the size box still fits 32x32 while
        // the full decode behind thumb/payload refuses it.
        let cache = RefCell::new(crate::cards::SizeCache::new());
        assert_eq!(crate::cards::photo_display_size(&cache, &p), (208, 208));
        match payload_for(&p).unwrap() {
            (CopyPayload::Text(t), _) => assert_eq!(t, p.to_string_lossy()),
            (CopyPayload::Image(_), _) => panic!("truncated file must fall back to Text"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn normal_1920x1080_passes_payload_and_size() {
        let dir = temp_dir("normal1920");
        let p = dir.join("shot.png");
        std::fs::write(&p, test_png(1920, 1080, [11, 22, 33])).unwrap();
        let img = crate::thumbs::bounded_open(&p).unwrap();
        assert_eq!((img.width(), img.height()), (1920, 1080));
        let cache = RefCell::new(crate::cards::SizeCache::new());
        assert_eq!(crate::cards::photo_display_size(&cache, &p), (272, 153));
        match payload_for(&p).unwrap() {
            (CopyPayload::Image(d), _) => assert_eq!((d.width, d.height), (1920, 1080)),
            (CopyPayload::Text(_), _) => panic!("normal screenshot must copy as Image"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
