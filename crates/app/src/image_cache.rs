//! Disk cache for downloaded remote images, and downscaling before they reach the renderer.
//!
//! Entries hold the processed bytes, so a cache hit skips both the download and the decode.
//! Entries expire [`TTL`] after they were written and are swept on startup.
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use image::imageops::FilterType;
use image::{ImageFormat, ImageReader, Limits};
use sha2::{Digest, Sha256};

/// How long a cached remote image stays on disk.
pub const TTL: Duration = Duration::from_secs(20 * 24 * 60 * 60);

/// Longest side, in pixels, of an image handed to the renderer.
pub const MAX_SIDE: u32 = 800;

/// Decoder limits so a hostile image cannot exhaust memory while being shrunk.
const MAX_DECODE_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone)]
pub struct ImageCache {
    dir: PathBuf,
}

impl ImageCache {
    /// Opens the cache under `data_dir` and removes entries that have expired.
    pub fn open(data_dir: &Path) -> Result<Self> {
        let dir = data_dir.join("image-cache");
        fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
        let cache = Self { dir };
        cache.sweep();
        Ok(cache)
    }

    /// Returns the cached bytes for `url`, or `None` if absent or expired.
    pub fn get(&self, url: &str) -> Option<Vec<u8>> {
        let path = self.path_for(url);
        if is_expired(&path) {
            let _ = fs::remove_file(&path);
            return None;
        }
        fs::read(&path).ok()
    }

    pub fn put(&self, url: &str, data: &[u8]) {
        let _ = fs::write(self.path_for(url), data);
    }

    /// Removes every entry older than [`TTL`].
    pub fn sweep(&self) {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.flatten() {
            if is_expired(&entry.path()) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    fn path_for(&self, url: &str) -> PathBuf {
        let digest = Sha256::digest(url.as_bytes());
        let name: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        self.dir.join(name)
    }
}

fn is_expired(path: &Path) -> bool {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age > TTL)
}

/// Shrinks an image whose longest side exceeds [`MAX_SIDE`], re-encoded as PNG.
/// Smaller images, and data that does not decode, are returned unchanged.
pub fn downscale(data: Vec<u8>) -> Vec<u8> {
    let Ok(mut reader) = ImageReader::new(Cursor::new(&data)).with_guessed_format() else {
        return data;
    };
    let mut limits = Limits::default();
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    reader.limits(limits);
    let Ok(img) = reader.decode() else {
        return data;
    };
    if img.width().max(img.height()) <= MAX_SIDE {
        return data;
    }
    let resized = img.resize(MAX_SIDE, MAX_SIDE, FilterType::Triangle);
    let mut out = Cursor::new(Vec::new());
    match resized.write_to(&mut out, ImageFormat::Png) {
        Ok(()) => out.into_inner(),
        Err(_) => data,
    }
}
