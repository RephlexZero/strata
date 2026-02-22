//! HLS segment uploader for YouTube HLS ingest.
//!
//! Watches a local directory where `hlssink2` writes `.ts` segments and
//! `.m3u8` playlists, then uploads each new file to the YouTube HLS HTTP
//! endpoint via HTTPS PUT.
//!
//! **Ordering guarantee:** the playlist is only uploaded after every new
//! segment referenced by it has been successfully PUT.  YouTube requires
//! segments to exist before the playlist references them.
//!
//! **Segment directory:** callers should place the directory on a RAM-backed
//! filesystem (`/dev/shm` on Linux) to avoid flash/eMMC wear on SBCs.
//! Use [`tmpfs_segment_dir`] to get a suitable path.
//!
//! YouTube HLS URL format:
//!   `https://a.upload.youtube.com/http_upload_hls?cid=STREAM_KEY&copy=0&file=`
//!
//! Each file is uploaded by appending its filename to the base URL:
//!   PUT `{base_url}{filename}` with the file body.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Maximum retries per segment upload before giving up for that poll cycle.
const MAX_UPLOAD_RETRIES: u32 = 3;
/// Base delay between retries (doubles each attempt).
const RETRY_BASE_DELAY: Duration = Duration::from_millis(250);

/// Configuration for the HLS uploader.
pub struct HlsUploaderConfig {
    /// Local directory where hlssink2 writes segments + playlist.
    pub segment_dir: PathBuf,
    /// Base URL for uploads (everything up to and including `file=`).
    pub base_url: String,
    /// Name of the playlist file (e.g. "playlist.m3u8").
    pub playlist_filename: String,
}

/// Handle to a running HLS uploader thread.
pub struct HlsUploaderHandle {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl HlsUploaderHandle {
    /// Signal the uploader to stop and wait for the thread to finish.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for HlsUploaderHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Return a RAM-backed directory suitable for HLS segments.
///
/// Prefers `/dev/shm` (Linux tmpfs, always RAM-backed) to avoid
/// flash/eMMC wear on SBCs.  Falls back to `std::env::temp_dir()`
/// (usually `/tmp`) if `/dev/shm` is not available.
pub fn tmpfs_segment_dir(suffix: &str) -> PathBuf {
    let shm = Path::new("/dev/shm");
    if shm.is_dir() {
        shm.join(suffix)
    } else {
        std::env::temp_dir().join(suffix)
    }
}

/// Start the HLS uploader in a background thread.
///
/// Returns a handle that stops the uploader when dropped.
pub fn start_hls_uploader(config: HlsUploaderConfig) -> HlsUploaderHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = stop.clone();

    let thread = std::thread::Builder::new()
        .name("hls-upload".into())
        .spawn(move || {
            uploader_loop(&config, &stop_clone);
        })
        .expect("failed to spawn HLS uploader thread");

    HlsUploaderHandle {
        stop,
        thread: Some(thread),
    }
}

/// Main polling loop: scan for new segments, upload them, then upload the
/// playlist only when at least one new segment was successfully uploaded.
fn uploader_loop(config: &HlsUploaderConfig, stop: &AtomicBool) {
    let mut uploaded: HashSet<String> = HashSet::new();
    let agent = ureq::Agent::new_with_defaults();
    let playlist_path = config.segment_dir.join(&config.playlist_filename);

    eprintln!(
        "HLS uploader: watching {} → {}",
        config.segment_dir.display(),
        config.base_url
    );

    while !stop.load(Ordering::Relaxed) {
        let mut new_uploaded = false;

        // Scan for .ts segment files
        if let Ok(entries) = std::fs::read_dir(&config.segment_dir) {
            let mut new_segments = Vec::new();
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".ts") && !uploaded.contains(&name) {
                    new_segments.push(name);
                }
            }

            // Sort by name to upload in order (segment00000.ts, segment00001.ts, ...)
            new_segments.sort();

            for name in new_segments {
                let path = config.segment_dir.join(&name);
                if upload_file_with_retry(&agent, &config.base_url, &name, &path) {
                    uploaded.insert(name);
                    new_uploaded = true;
                }
                // If stop was requested mid-upload, break out
                if stop.load(Ordering::Relaxed) {
                    break;
                }
            }

            // Upload playlist ONLY after at least one new segment succeeded.
            // This guarantees YouTube has the segment data before the
            // playlist references it.
            if new_uploaded && playlist_path.exists() {
                upload_file_with_retry(
                    &agent,
                    &config.base_url,
                    &config.playlist_filename,
                    &playlist_path,
                );
            }
        }

        std::thread::sleep(Duration::from_millis(500));
    }

    // Final: upload any remaining segments, then playlist
    if let Ok(entries) = std::fs::read_dir(&config.segment_dir) {
        let mut remaining: Vec<String> = entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                if name.ends_with(".ts") && !uploaded.contains(&name) {
                    Some(name)
                } else {
                    None
                }
            })
            .collect();
        remaining.sort();
        for name in remaining {
            let path = config.segment_dir.join(&name);
            if upload_file_with_retry(&agent, &config.base_url, &name, &path) {
                uploaded.insert(name);
            }
        }
    }
    if playlist_path.exists() {
        upload_file_with_retry(
            &agent,
            &config.base_url,
            &config.playlist_filename,
            &playlist_path,
        );
    }

    eprintln!(
        "HLS uploader: stopped ({} segments uploaded)",
        uploaded.len()
    );
}

/// Upload a file with exponential-backoff retry.
fn upload_file_with_retry(
    agent: &ureq::Agent,
    base_url: &str,
    filename: &str,
    path: &Path,
) -> bool {
    for attempt in 0..MAX_UPLOAD_RETRIES {
        if upload_file(agent, base_url, filename, path) {
            return true;
        }
        if attempt + 1 < MAX_UPLOAD_RETRIES {
            let delay = RETRY_BASE_DELAY * 2u32.saturating_pow(attempt);
            eprintln!(
                "HLS uploader: retrying {} in {}ms (attempt {}/{})",
                filename,
                delay.as_millis(),
                attempt + 2,
                MAX_UPLOAD_RETRIES
            );
            std::thread::sleep(delay);
        }
    }
    eprintln!(
        "HLS uploader: giving up on {} after {} attempts",
        filename, MAX_UPLOAD_RETRIES
    );
    false
}

/// Upload a single file to `{base_url}{filename}` via HTTP PUT.
/// Returns `true` on success.
fn upload_file(agent: &ureq::Agent, base_url: &str, filename: &str, path: &Path) -> bool {
    let body = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("HLS uploader: failed to read {}: {}", path.display(), e);
            return false;
        }
    };

    let url = format!("{base_url}{filename}");
    let content_type = if filename.ends_with(".m3u8") {
        "application/vnd.apple.mpegurl"
    } else {
        "video/mp2t"
    };

    match agent
        .put(&url)
        .header("Content-Type", content_type)
        .send(&body[..])
    {
        Ok(resp) => {
            let status = resp.status().as_u16();
            if (200..300).contains(&status) {
                true
            } else {
                eprintln!("HLS uploader: PUT {} returned HTTP {}", filename, status);
                false
            }
        }
        Err(e) => {
            eprintln!("HLS uploader: PUT {} failed: {}", filename, e);
            false
        }
    }
}

/// Detect whether a relay URL is an HLS upload endpoint.
///
/// Returns `true` if the URL looks like a YouTube HLS ingest URL
/// (HTTPS with `http_upload_hls` in the path, ending with `file=`).
pub fn is_hls_url(url: &str) -> bool {
    url.starts_with("https://") && url.contains("http_upload_hls") && url.contains("file=")
}

/// Parse the HLS base URL from a relay URL.
///
/// The base URL is everything up to and including `file=`.
/// Segment/playlist filenames are appended to this.
pub fn hls_base_url(url: &str) -> &str {
    // The URL should end with `file=` — if not, use the whole thing.
    if let Some(idx) = url.rfind("file=") {
        &url[..idx + 5]
    } else {
        url
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_hls_url() {
        assert!(is_hls_url(
            "https://a.upload.youtube.com/http_upload_hls?cid=abc&copy=0&file="
        ));
        assert!(!is_hls_url("rtmp://a.rtmp.youtube.com/live2/key"));
        assert!(!is_hls_url("https://example.com/upload"));
    }

    #[test]
    fn test_hls_base_url() {
        let url = "https://a.upload.youtube.com/http_upload_hls?cid=abc&copy=0&file=";
        assert_eq!(hls_base_url(url), url);

        let url2 = "https://a.upload.youtube.com/http_upload_hls?cid=abc&copy=0&file=something";
        assert_eq!(
            hls_base_url(url2),
            "https://a.upload.youtube.com/http_upload_hls?cid=abc&copy=0&file="
        );
    }
}
