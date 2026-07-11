//! HLS segment uploader for YouTube HLS ingest.
//!
//! Watches a local directory where the HLS sink writes `.ts` segments and
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Maximum retries per segment upload before giving up for that poll cycle.
const MAX_UPLOAD_RETRIES: u32 = 3;
/// Base delay between retries (doubles each attempt).
const RETRY_BASE_DELAY: Duration = Duration::from_millis(250);

/// Configuration for the HLS uploader.
pub struct HlsUploaderConfig {
    /// Local directory where the HLS sink writes segments + playlist.
    pub segment_dir: PathBuf,
    /// Base URL for uploads (everything up to and including `file=`).
    pub base_url: String,
    /// Name of the playlist file (e.g. "playlist.m3u8").
    pub playlist_filename: String,
    /// Segment filenames that start a real timeline gap (a DeliveredStream
    /// gate resume — see `strata_pipeline.rs:install_delivered_stream_gate`).
    /// The uploader marks these with `#EXT-X-DISCONTINUITY` before upload;
    /// the set only grows, so the uploader tracks which entries are still
    /// within the live playlist's sliding window itself.
    pub discontinuous_segments: Arc<Mutex<HashSet<String>>>,
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
    let mut discontinuity_state = DiscontinuityState::default();
    let mut media_sequence_state = MediaSequenceState::default();

    eprintln!(
        "HLS uploader: watching {} → {}",
        config.segment_dir.display(),
        config.base_url
    );

    while !stop.load(Ordering::Relaxed) {
        let mut new_uploaded = false;

        // Scan for .ts segment files
        {
            let new_segments = find_new_segments(&config.segment_dir, &uploaded, false);

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
                upload_playlist_with_retry(
                    &agent,
                    config,
                    &playlist_path,
                    &mut discontinuity_state,
                    &mut media_sequence_state,
                );
            }
        }

        std::thread::sleep(Duration::from_millis(250));
    }

    // Final: upload any remaining segments, then playlist
    {
        let remaining = find_new_segments(&config.segment_dir, &uploaded, true);
        for name in remaining {
            let path = config.segment_dir.join(&name);
            if upload_file_with_retry(&agent, &config.base_url, &name, &path) {
                uploaded.insert(name);
            }
        }
    }
    if playlist_path.exists() {
        upload_playlist_with_retry(
            &agent,
            config,
            &playlist_path,
            &mut discontinuity_state,
            &mut media_sequence_state,
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
    upload_bytes(agent, base_url, filename, &body)
}

/// Upload in-memory bytes to `{base_url}{filename}` via HTTP PUT.
/// Returns `true` on success.
fn upload_bytes(agent: &ureq::Agent, base_url: &str, filename: &str, body: &[u8]) -> bool {
    let url = format!("{base_url}{filename}");
    let content_type = content_type_for_hls(filename);

    match agent
        .put(&url)
        .header("Content-Type", content_type)
        .send(body)
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

/// Read the playlist hlssink3 just wrote, tag any segment in
/// `config.discontinuous_segments` with `#EXT-X-DISCONTINUITY`, and upload
/// the rewritten copy (with retry). The on-disk file is left untouched —
/// hlssink3 owns and rewrites it every segment, so our edits only ever live
/// in the uploaded copy.
fn upload_playlist_with_retry(
    agent: &ureq::Agent,
    config: &HlsUploaderConfig,
    playlist_path: &Path,
    state: &mut DiscontinuityState,
    seq_state: &mut MediaSequenceState,
) -> bool {
    let text = match std::fs::read_to_string(playlist_path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!(
                "HLS uploader: failed to read {}: {}",
                playlist_path.display(),
                e
            );
            return false;
        }
    };
    let discontinuous = config.discontinuous_segments.lock().unwrap();
    let rewritten = rewrite_playlist_discontinuities(&text, &discontinuous, state);
    drop(discontinuous);
    let rewritten = rewrite_media_sequence(&rewritten, seq_state);

    for attempt in 0..MAX_UPLOAD_RETRIES {
        if upload_bytes(
            agent,
            &config.base_url,
            &config.playlist_filename,
            rewritten.as_bytes(),
        ) {
            return true;
        }
        if attempt + 1 < MAX_UPLOAD_RETRIES {
            let delay = RETRY_BASE_DELAY * 2u32.saturating_pow(attempt);
            std::thread::sleep(delay);
        }
    }
    eprintln!(
        "HLS uploader: giving up on {} after {} attempts",
        config.playlist_filename, MAX_UPLOAD_RETRIES
    );
    false
}

/// Tracks, across polling cycles, which segments currently in the live
/// playlist's sliding window are marked discontinuous, and how many tagged
/// segments have already rolled out of that window (the playlist's
/// `#EXT-X-DISCONTINUITY-SEQUENCE`).
#[derive(Default)]
struct DiscontinuityState {
    in_playlist: HashSet<String>,
    sequence: u32,
}

/// Insert `#EXT-X-DISCONTINUITY` before the `#EXTINF:` of any segment named
/// in `discontinuous`, and maintain `#EXT-X-DISCONTINUITY-SEQUENCE` as tagged
/// segments roll out of the playlist's sliding window. `hlssink3` has no
/// concept of an application-driven discontinuity (it never emits either
/// tag itself), so this is reconstructed from the plain playlist text on
/// every upload.
fn rewrite_playlist_discontinuities(
    text: &str,
    discontinuous: &HashSet<String>,
    state: &mut DiscontinuityState,
) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let current_segments: HashSet<&str> = lines
        .iter()
        .copied()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();

    // Segments that rolled out of the window since the last poll count
    // toward the discontinuity sequence; segments newly visible and tagged
    // join the live set.
    let rolled_off = state
        .in_playlist
        .iter()
        .filter(|name| !current_segments.contains(name.as_str()))
        .count();
    state.sequence += rolled_off as u32;
    state
        .in_playlist
        .retain(|name| current_segments.contains(name.as_str()));
    for &name in &current_segments {
        if discontinuous.contains(name) {
            state.in_playlist.insert(name.to_string());
        }
    }

    let mut out = String::with_capacity(text.len() + 64);
    for (i, line) in lines.iter().enumerate() {
        if *line == "#EXTM3U" {
            out.push_str(line);
            out.push('\n');
            if state.sequence > 0 || !state.in_playlist.is_empty() {
                out.push_str(&format!(
                    "#EXT-X-DISCONTINUITY-SEQUENCE:{}\n",
                    state.sequence
                ));
            }
            continue;
        }
        if line.starts_with("#EXTINF:")
            && let Some(&uri) = lines.get(i + 1)
            && state.in_playlist.contains(uri)
        {
            out.push_str("#EXT-X-DISCONTINUITY\n");
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Assigns each segment a continuous media-sequence number so the uploaded
/// playlist's `#EXT-X-MEDIA-SEQUENCE` never goes backwards across watchdog
/// pipeline rebuilds. A fresh hlssink3 restarts its own numbering at 0 each
/// generation, but the uploader keeps publishing to the same live playlist
/// URL — a backwards media-sequence jump violates RFC 8216 §6.2.2 and an
/// ingest server may silently treat the stream as broken. Segments are
/// keyed by filename (unique across generations via the `seg-gNNNN-` prefix),
/// so numbering survives the reset; entries are pruned to the current
/// playlist window to bound memory.
#[derive(Default)]
struct MediaSequenceState {
    assigned: std::collections::HashMap<String, u64>,
    next: u64,
    seeded: bool,
}

/// Rewrite `#EXT-X-MEDIA-SEQUENCE` with the uploader's continuous numbering
/// (see [`MediaSequenceState`]). New segments take numbers in playlist order,
/// so within a generation the output matches hlssink3's own numbering; after
/// a rebuild the sequence keeps counting up instead of resetting to 0. The
/// forward jump this leaves at a rebuild is spec-legal (clients treat skipped
/// numbers as missed segments), and the first segment of each generation is
/// already tagged `#EXT-X-DISCONTINUITY` via `discontinuous_segments`.
///
/// hlssink3 omits the tag entirely until segments roll off its window, which
/// an ingest server reads as an implicit sequence of 0 — a backwards jump on
/// every rebuild (YouTube requires the tag and monotonic increase). When the
/// tag is missing it is inserted before the first `#EXTINF`.
fn rewrite_media_sequence(text: &str, state: &mut MediaSequenceState) -> String {
    let uris: Vec<&str> = text
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    if uris.is_empty() {
        return text.to_string();
    }
    if !state.seeded {
        // Adopt hlssink3's numbering from the first playlist seen, so the
        // uploaded copy is identical to the on-disk one until a rebuild.
        state.next = parse_media_sequence(text).unwrap_or(0);
        state.seeded = true;
    }
    for &uri in &uris {
        if !state.assigned.contains_key(uri) {
            state.assigned.insert(uri.to_string(), state.next);
            state.next += 1;
        }
    }
    let first_seq = state.assigned[uris[0]];
    let window: HashSet<&str> = uris.iter().copied().collect();
    state
        .assigned
        .retain(|name, _| window.contains(name.as_str()));

    let has_tag = text
        .lines()
        .any(|l| l.starts_with("#EXT-X-MEDIA-SEQUENCE:"));
    let mut out = String::with_capacity(text.len() + 40);
    let mut inserted = false;
    for line in text.lines() {
        if line.starts_with("#EXT-X-MEDIA-SEQUENCE:") {
            out.push_str(&format!("#EXT-X-MEDIA-SEQUENCE:{first_seq}\n"));
            continue;
        }
        if !has_tag && !inserted && line.starts_with("#EXTINF:") {
            out.push_str(&format!("#EXT-X-MEDIA-SEQUENCE:{first_seq}\n"));
            inserted = true;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Parse the `#EXT-X-MEDIA-SEQUENCE` value from playlist text.
fn parse_media_sequence(text: &str) -> Option<u64> {
    text.lines().find_map(|l| {
        l.strip_prefix("#EXT-X-MEDIA-SEQUENCE:")?
            .trim()
            .parse()
            .ok()
    })
}

/// Return the appropriate MIME type for an HLS file.
///
/// `.m3u8` playlists get `application/vnd.apple.mpegurl`; everything else
/// (`.ts` segments) gets `video/mp2t`.
pub(crate) fn content_type_for_hls(filename: &str) -> &'static str {
    if filename.ends_with(".m3u8") {
        "application/vnd.apple.mpegurl"
    } else {
        "video/mp2t"
    }
}

/// Scan `dir` for `.ts` segment files not yet in `uploaded`, returning them
/// sorted by name.
///
/// Only non-empty files are considered — the HLS sink creates the segment file
/// before writing any data, so a zero-byte file is still open for writing.
///
/// When `include_latest` is `false` (live polling mode), the segment with the
/// highest name is also excluded because it may still be open for writing.
/// The HLS sink always finalises segment N before creating segment N+1, so a
/// segment is guaranteed complete as soon as a successor exists.
///
/// Set `include_latest = true` only during final shutdown cleanup, when the
/// pipeline is already stopped and no further segments will be created.
pub(crate) fn find_new_segments(
    dir: &Path,
    uploaded: &HashSet<String>,
    include_latest: bool,
) -> Vec<String> {
    let mut segments = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".ts")
                && !uploaded.contains(&name)
                && entry.metadata().map(|m| m.len() > 0).unwrap_or(false)
            {
                segments.push(name);
            }
        }
    }
    segments.sort();
    if !include_latest {
        // Without a successor we can't confirm the latest segment is fully
        // written, so hold it back until the next poll cycle.
        segments.pop();
    }
    segments
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
    use std::fs;

    // ── hls_base_url ────────────────────────────────────────────────────

    #[test]
    fn test_hls_base_url_trailing_file_eq() {
        let url = "https://a.upload.youtube.com/http_upload_hls?cid=abc&copy=0&file=";
        assert_eq!(hls_base_url(url), url);
    }

    #[test]
    fn test_hls_base_url_strips_after_file_eq() {
        let url = "https://a.upload.youtube.com/http_upload_hls?cid=abc&copy=0&file=something";
        assert_eq!(
            hls_base_url(url),
            "https://a.upload.youtube.com/http_upload_hls?cid=abc&copy=0&file="
        );
    }

    #[test]
    fn test_hls_base_url_no_file_param_returns_whole_url() {
        let url = "https://example.com/upload";
        assert_eq!(hls_base_url(url), url);
    }

    #[test]
    fn test_hls_base_url_empty_string() {
        assert_eq!(hls_base_url(""), "");
    }

    #[test]
    fn test_hls_base_url_multiple_file_params_uses_last() {
        let url = "https://example.com/?file=first&other=1&file=second";
        // rfind should pick the *last* `file=`
        assert_eq!(
            hls_base_url(url),
            "https://example.com/?file=first&other=1&file="
        );
    }

    // ── tmpfs_segment_dir ───────────────────────────────────────────────

    #[test]
    fn test_tmpfs_segment_dir_contains_suffix() {
        let dir = tmpfs_segment_dir("strata-test-123");
        assert!(dir.to_string_lossy().contains("strata-test-123"));
    }

    #[test]
    fn test_tmpfs_segment_dir_prefers_dev_shm_on_linux() {
        let dir = tmpfs_segment_dir("test");
        // On Linux CI/dev containers, /dev/shm should exist
        if Path::new("/dev/shm").is_dir() {
            assert!(dir.starts_with("/dev/shm"));
        } else {
            // Fallback path — just ensure it's reasonable
            assert!(dir.to_string_lossy().contains("test"));
        }
    }

    // ── content_type_for_hls ────────────────────────────────────────────

    #[test]
    fn test_content_type_playlist() {
        assert_eq!(
            content_type_for_hls("playlist.m3u8"),
            "application/vnd.apple.mpegurl"
        );
    }

    #[test]
    fn test_content_type_segment() {
        assert_eq!(content_type_for_hls("segment00001.ts"), "video/mp2t");
    }

    #[test]
    fn test_content_type_unknown_extension() {
        // Anything that isn't .m3u8 is treated as a transport stream
        assert_eq!(content_type_for_hls("data.bin"), "video/mp2t");
    }

    // ── find_new_segments ───────────────────────────────────────────────

    use std::sync::atomic::AtomicU32;

    static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn make_temp_dir() -> PathBuf {
        let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("strata-test-{}-{}", std::process::id(), id));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_find_new_segments_empty_dir() {
        let dir = make_temp_dir();
        let uploaded = HashSet::new();
        let result = find_new_segments(&dir, &uploaded, true);
        assert!(result.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_find_new_segments_returns_only_ts_files() {
        let dir = make_temp_dir();
        fs::write(dir.join("segment00000.ts"), b"data").unwrap();
        fs::write(dir.join("segment00001.ts"), b"data").unwrap();
        fs::write(dir.join("playlist.m3u8"), b"#EXTM3U").unwrap();
        fs::write(dir.join("notes.txt"), b"misc").unwrap();

        let uploaded = HashSet::new();
        let result = find_new_segments(&dir, &uploaded, true);
        assert_eq!(result, vec!["segment00000.ts", "segment00001.ts"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_find_new_segments_excludes_already_uploaded() {
        let dir = make_temp_dir();
        fs::write(dir.join("segment00000.ts"), b"data").unwrap();
        fs::write(dir.join("segment00001.ts"), b"data").unwrap();
        fs::write(dir.join("segment00002.ts"), b"data").unwrap();

        let mut uploaded = HashSet::new();
        uploaded.insert("segment00000.ts".to_string());
        uploaded.insert("segment00002.ts".to_string());

        let result = find_new_segments(&dir, &uploaded, true);
        assert_eq!(result, vec!["segment00001.ts"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_find_new_segments_returned_sorted() {
        let dir = make_temp_dir();
        // Create in reverse order — result should still be sorted
        fs::write(dir.join("segment00005.ts"), b"d").unwrap();
        fs::write(dir.join("segment00001.ts"), b"d").unwrap();
        fs::write(dir.join("segment00003.ts"), b"d").unwrap();

        let result = find_new_segments(&dir, &HashSet::new(), true);
        assert_eq!(
            result,
            vec!["segment00001.ts", "segment00003.ts", "segment00005.ts"]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_find_new_segments_nonexistent_dir() {
        let dir = PathBuf::from("/tmp/strata-nonexistent-dir-12345");
        let result = find_new_segments(&dir, &HashSet::new(), true);
        assert!(result.is_empty());
    }

    #[test]
    fn test_find_new_segments_skips_zero_byte() {
        let dir = make_temp_dir();
        fs::write(dir.join("segment00000.ts"), b"").unwrap(); // 0-byte: still open
        fs::write(dir.join("segment00001.ts"), b"data").unwrap();
        // Zero-byte file skipped in both modes
        let result_live = find_new_segments(&dir, &HashSet::new(), false);
        let result_final = find_new_segments(&dir, &HashSet::new(), true);
        // Live: 00000 is 0-byte (skip), 00001 is latest with no successor (skip)
        assert!(result_live.is_empty());
        // Final: 00000 is 0-byte (skip), 00001 has data (include)
        assert_eq!(result_final, vec!["segment00001.ts"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_find_new_segments_live_mode_skips_latest() {
        let dir = make_temp_dir();
        fs::write(dir.join("segment00000.ts"), b"data").unwrap();
        fs::write(dir.join("segment00001.ts"), b"data").unwrap();
        fs::write(dir.join("segment00002.ts"), b"data").unwrap();
        // Live mode: all but the latest (00002 has no confirmed successor)
        let result = find_new_segments(&dir, &HashSet::new(), false);
        assert_eq!(result, vec!["segment00000.ts", "segment00001.ts"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_find_new_segments_live_mode_single_segment_returns_empty() {
        let dir = make_temp_dir();
        fs::write(dir.join("segment00000.ts"), b"data").unwrap();
        // Only one non-empty segment with no successor — can't confirm it's finalised
        let result = find_new_segments(&dir, &HashSet::new(), false);
        assert!(result.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    // ── rewrite_playlist_discontinuities ────────────────────────────────

    fn sample_playlist(segments: &[&str]) -> String {
        let mut p = String::from(
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:0\n",
        );
        for seg in segments {
            p.push_str("#EXTINF:1.000,\n");
            p.push_str(seg);
            p.push('\n');
        }
        p
    }

    #[test]
    fn rewrite_untagged_playlist_is_unchanged_besides_passthrough() {
        let playlist = sample_playlist(&["segment00000.ts", "segment00001.ts"]);
        let discontinuous = HashSet::new();
        let mut state = DiscontinuityState::default();
        let out = rewrite_playlist_discontinuities(&playlist, &discontinuous, &mut state);
        assert_eq!(out, playlist);
        assert_eq!(state.sequence, 0);
    }

    #[test]
    fn rewrite_tags_discontinuity_before_matching_segment() {
        let playlist = sample_playlist(&["segment00000.ts", "segment00001.ts", "segment00002.ts"]);
        let discontinuous: HashSet<String> = ["segment00001.ts".to_string()].into_iter().collect();
        let mut state = DiscontinuityState::default();
        let out = rewrite_playlist_discontinuities(&playlist, &discontinuous, &mut state);

        let lines: Vec<&str> = out.lines().collect();
        let tagged_idx = lines
            .iter()
            .position(|&l| l == "#EXT-X-DISCONTINUITY")
            .unwrap();
        assert_eq!(lines[tagged_idx + 1], "#EXTINF:1.000,");
        assert_eq!(lines[tagged_idx + 2], "segment00001.ts");
        // Untouched segments get no tag.
        assert_eq!(
            lines
                .iter()
                .filter(|&&l| l == "#EXT-X-DISCONTINUITY")
                .count(),
            1
        );
        // No segment has yet rolled off, so the sequence is present but zero.
        assert!(out.contains("#EXT-X-DISCONTINUITY-SEQUENCE:0"));
    }

    #[test]
    fn rewrite_increments_sequence_as_tagged_segment_rolls_off_window() {
        let discontinuous: HashSet<String> = ["segment00000.ts".to_string()].into_iter().collect();
        let mut state = DiscontinuityState::default();

        // First poll: segment00000 is tagged and still in the window.
        let p1 = sample_playlist(&["segment00000.ts", "segment00001.ts"]);
        let out1 = rewrite_playlist_discontinuities(&p1, &discontinuous, &mut state);
        assert!(out1.contains("#EXT-X-DISCONTINUITY-SEQUENCE:0"));
        let lines1: Vec<&str> = out1.lines().collect();
        let seg_idx = lines1.iter().position(|&l| l == "segment00000.ts").unwrap();
        assert_eq!(lines1[seg_idx - 1], "#EXTINF:1.000,");
        assert_eq!(lines1[seg_idx - 2], "#EXT-X-DISCONTINUITY");

        // Second poll: segment00000 has slid out of the sliding window.
        let p2 = sample_playlist(&["segment00001.ts", "segment00002.ts"]);
        let out2 = rewrite_playlist_discontinuities(&p2, &discontinuous, &mut state);
        assert!(out2.contains("#EXT-X-DISCONTINUITY-SEQUENCE:1"));
        assert!(!out2.contains("#EXT-X-DISCONTINUITY\n"));
    }

    // ── rewrite_media_sequence ──────────────────────────────────────────

    fn playlist_with_sequence(seq: u64, segments: &[&str]) -> String {
        let mut p = format!(
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:{seq}\n"
        );
        for seg in segments {
            p.push_str("#EXTINF:1.000,\n");
            p.push_str(seg);
            p.push('\n');
        }
        p
    }

    #[test]
    fn media_sequence_within_one_generation_matches_hlssink() {
        let mut state = MediaSequenceState::default();
        let p1 = playlist_with_sequence(0, &["seg-g0000-00000.ts", "seg-g0000-00001.ts"]);
        assert_eq!(rewrite_media_sequence(&p1, &mut state), p1);
        // Window slides: hlssink3 bumps the sequence, ours must agree.
        let p2 = playlist_with_sequence(1, &["seg-g0000-00001.ts", "seg-g0000-00002.ts"]);
        assert_eq!(rewrite_media_sequence(&p2, &mut state), p2);
    }

    #[test]
    fn media_sequence_adopts_initial_value_mid_stream() {
        let mut state = MediaSequenceState::default();
        let p = playlist_with_sequence(7, &["seg-g0000-00007.ts", "seg-g0000-00008.ts"]);
        assert_eq!(rewrite_media_sequence(&p, &mut state), p);
    }

    /// The watchdog-rebuild regression: a new generation's hlssink3 resets
    /// `#EXT-X-MEDIA-SEQUENCE` to 0, but the uploaded playlist must keep
    /// counting forward — a backwards jump violates RFC 8216 §6.2.2.
    #[test]
    fn media_sequence_continues_across_generation_reset() {
        let mut state = MediaSequenceState::default();
        // Generation 0, window has slid to sequence 3.
        let p1 = playlist_with_sequence(
            3,
            &[
                "seg-g0000-00003.ts",
                "seg-g0000-00004.ts",
                "seg-g0000-00005.ts",
            ],
        );
        rewrite_media_sequence(&p1, &mut state);
        // Rebuild: generation 1 restarts at raw sequence 0.
        let p2 = playlist_with_sequence(0, &["seg-g0001-00000.ts", "seg-g0001-00001.ts"]);
        let out = rewrite_media_sequence(&p2, &mut state);
        assert!(
            out.contains("#EXT-X-MEDIA-SEQUENCE:6"),
            "sequence must continue after the last assigned number, got:\n{out}"
        );
        assert!(!out.contains("#EXT-X-MEDIA-SEQUENCE:0"));
        // And keep advancing normally as the new generation's window slides.
        let p3 = playlist_with_sequence(1, &["seg-g0001-00001.ts", "seg-g0001-00002.ts"]);
        let out = rewrite_media_sequence(&p3, &mut state);
        assert!(out.contains("#EXT-X-MEDIA-SEQUENCE:7"), "got:\n{out}");
    }

    fn playlist_without_sequence(segments: &[&str]) -> String {
        let mut p = String::from("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:1\n");
        for seg in segments {
            p.push_str("#EXTINF:1.000,\n");
            p.push_str(seg);
            p.push('\n');
        }
        p
    }

    /// hlssink3 omits `#EXT-X-MEDIA-SEQUENCE` until its window slides, which
    /// reads as an implicit 0 — the tag must be inserted, both on the very
    /// first playlist (YouTube requires it) and after a rebuild (where the
    /// implicit 0 is a backwards jump).
    #[test]
    fn media_sequence_tag_inserted_when_missing() {
        let mut state = MediaSequenceState::default();
        let p1 = playlist_without_sequence(&["seg-g0000-00000.ts"]);
        let out = rewrite_media_sequence(&p1, &mut state);
        let lines: Vec<&str> = out.lines().collect();
        let extinf_idx = lines.iter().position(|l| l.starts_with("#EXTINF")).unwrap();
        assert_eq!(
            lines[extinf_idx - 1],
            "#EXT-X-MEDIA-SEQUENCE:0",
            "got:\n{out}"
        );

        // Window slides, tag appears; numbering must agree with hlssink3.
        let p2 = playlist_with_sequence(1, &["seg-g0000-00001.ts", "seg-g0000-00002.ts"]);
        assert_eq!(rewrite_media_sequence(&p2, &mut state), p2);

        // Rebuild: fresh generation with no tag must continue, not reset to 0.
        let p3 = playlist_without_sequence(&["seg-g0001-00000.ts"]);
        let out = rewrite_media_sequence(&p3, &mut state);
        assert!(out.contains("#EXT-X-MEDIA-SEQUENCE:3"), "got:\n{out}");
        assert!(!out.contains("#EXT-X-MEDIA-SEQUENCE:0"));
    }

    #[test]
    fn media_sequence_empty_playlist_passthrough() {
        let mut state = MediaSequenceState::default();
        let p = "#EXTM3U\n#EXT-X-VERSION:3\n";
        assert_eq!(rewrite_media_sequence(p, &mut state), p);
    }

    // ── HlsUploaderHandle lifecycle ─────────────────────────────────────

    #[test]
    fn test_uploader_start_and_immediate_stop() {
        let dir = make_temp_dir();
        let handle = start_hls_uploader(HlsUploaderConfig {
            segment_dir: dir.clone(),
            base_url: "https://localhost:0/file=".to_string(),
            playlist_filename: "playlist.m3u8".to_string(),
            discontinuous_segments: Arc::new(Mutex::new(HashSet::new())),
        });
        // Signal stop — should not hang
        handle.stop();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_uploader_stop_on_drop() {
        let dir = make_temp_dir();
        {
            let _handle = start_hls_uploader(HlsUploaderConfig {
                segment_dir: dir.clone(),
                base_url: "https://localhost:0/file=".to_string(),
                playlist_filename: "playlist.m3u8".to_string(),
                discontinuous_segments: Arc::new(Mutex::new(HashSet::new())),
            });
            // handle dropped here — should signal stop and join cleanly
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
