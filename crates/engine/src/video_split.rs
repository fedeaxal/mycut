//! Stream-copy splitting of an exported video: cut one MP4/MOV into several files by time
//! ranges without re-decoding or re-encoding — sample bytes are copied as-is, only the sample
//! tables are re-laid-out (via the same `filmcraft-isobmff` demuxer/muxer the exporter writes
//! with).
//!
//! The「匹配源 – 分P导出」dialog takes this path when「整段导出」and「分P导出」are both
//! checked: the whole video renders once (captions, effects and quality all baked in), and
//! every part is then copied out of that file by [`split_stream_copy`] — an order of magnitude
//! faster than rendering each part again, and every part matches the whole in quality.
//!
//! How the boundaries land (the physics of not re-encoding, same as `ffmpeg -c copy`):
//! - Video is cut contiguously in decode order and every part starts on a sync sample — a
//!   stream entered mid-GOP cannot be decoded on its own. A requested start falls back to the
//!   closest keyframe at or before it, so part boundaries may lead the request by up to one
//!   keyframe interval (2 s by default; per-part durations are far larger).
//! - Audio follows the video: a part's audio starts where the part's first video frame is
//!   presented, so picture and sound stay in sync even when the video fell back to a keyframe
//!   (AAC packets are ~21 ms, so the audio side lands within one packet).
//! - Other tracks (timecode, data) are not copied.

use std::path::{Path, PathBuf};

use filmcraft_export::Progress;
use filmcraft_isobmff::{Brand, ByteSource as _, FourCc, Mp4File, Mp4Writer, Track, TrackConfig, TrackKind, WriteSample, WriterOptions, open};

/// One output of a split: the half-open time range in seconds and where to write it.
#[derive(Clone, Debug, PartialEq)]
pub struct SplitPart {
    pub start_seconds: f64,
    pub end_seconds: f64,
    pub path: PathBuf,
}

/// Copies the samples of one track in `[a, b)` (decode order) into `w`. Returns the bytes
/// copied. `first_pts` aligns the track's composition timeline so the part starts at 0.
/// `base` is the byte count already written before this track — progress reports `base + n`.
fn copy_track(
    mp4: &Mp4File,
    file: &std::fs::File,
    track_index: usize,
    (a, b): (usize, usize),
    w: &mut Mp4Writer<std::fs::File>,
    progress: Option<&Progress>,
    base: u64,
) -> Result<u64, String> {
    let t = &mp4.tracks[track_index];
    if b <= a || t.samples.is_empty() {
        return Ok(0);
    }
    // 1-based description index of the part's first sample; one entry per track is the norm
    // (every file this exporter writes), fall back to the first entry.
    let desc = t.samples[a].description_index.max(1) as usize - 1;
    let entry = t
        .entries
        .get(desc)
        .or_else(|| t.entries.first())
        .cloned()
        .ok_or_else(|| format!("track {} has no sample entry", track_index))?;
    // The output stream re-accumulates dts from zero, so a sample's output pts is its source
    // pts minus the part's first dts; align the composition timeline to that minimum.
    let first_dts = t.samples[a].dts;
    let first_pts = t.samples[a..b].iter().map(|s| s.pts - first_dts).min().unwrap_or(0).max(0);
    let mut cfg = TrackConfig::new(entry, t.timescale);
    cfg.language = t.language.clone();
    cfg.media_start = Some(first_pts);
    let out_track = w.add_track(cfg).map_err(|e| e.to_string())?;
    let mut buf: Vec<u8> = Vec::new();
    let mut copied = 0u64;
    for (n, s) in t.samples[a..b].iter().enumerate() {
        buf.clear();
        buf.resize(s.size as usize, 0);
        file.read_at(s.offset, &mut buf).map_err(|e| e.to_string())?;
        let sample = WriteSample {
            data: &buf,
            duration: s.duration,
            composition_offset: (s.pts - s.dts).clamp(i32::MIN as i64, i32::MAX as i64) as i32,
            is_sync: s.is_sync,
        };
        w.write_sample(out_track, sample).map_err(|e| e.to_string())?;
        copied += s.size as u64;
        // progress every 16 samples: an atomic store per frame is noise, and so is the
        // cancel check — keep both rare enough not to show up on hour-long files.
        if n % 16 == 15 && let Some(p) = progress {
            if p.cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return Err("cancelled".into());
            }
            p.done.store(base + copied, std::sync::atomic::Ordering::Relaxed);
        }
    }
    if let Some(p) = progress {
        p.done.store(base + copied, std::sync::atomic::Ordering::Relaxed);
    }
    Ok(copied)
}

/// Decode-order cut points of the video track for every part: part k covers `[cuts[k], cuts[k+1])`.
/// Each requested start falls back to the closest sync sample at or before it (no fallback for
/// files with no sync table — every sample is one); the first part always starts at 0.
fn video_cuts(t: &Track, starts: &[f64]) -> Vec<usize> {
    let last = t.samples.len();
    let mut cuts = Vec::with_capacity(starts.len() + 1);
    for (k, &sec) in starts.iter().enumerate() {
        let cut = if k == 0 {
            0
        } else {
            let ts = (sec * f64::from(t.timescale)).round() as i64;
            let at = t.sample_at_pts(ts).unwrap_or(last);
            if t.sync_samples().is_empty() { at } else { t.sync_sample_before(at) }
        };
        // A fallback can land on (or behind) the previous cut when a part is shorter than a
        // GOP; clamp so every part keeps at least one sample instead of vanishing.
        cuts.push(cut.max(*cuts.last().unwrap_or(&0)).min(last));
    }
    cuts.push(last);
    cuts
}

/// The per-part sample ranges of every copied track, computed up front so the video cuts stay
/// contiguous across parts (part k ends exactly where part k+1 begins) and the audio follows
/// the video's actual (keyframe-aligned) boundaries.
struct Plan {
    track: usize,
    /// One `(first, last_exclusive)` per part.
    ranges: Vec<(usize, usize)>,
}

fn plan_tracks(mp4: &Mp4File, parts: &[SplitPart]) -> Result<Vec<Plan>, String> {
    let starts: Vec<f64> = parts.iter().map(|p| p.start_seconds).collect();
    let video = mp4.tracks.iter().position(|t| t.kind == TrackKind::Video && !t.pcm_chunked && !t.samples.is_empty());
    let Some(v) = video else { return Err("no video track to split".into()) };
    let vt = &mp4.tracks[v];
    let cuts = video_cuts(vt, &starts);
    // Presentation-time boundary of each part in track timescale: the smallest pts in the
    // part's video range; the audio of part k runs from boundary k to boundary k+1 (the last
    // part's audio runs to the end of its track).
    let bounds: Vec<i64> = cuts
        .iter()
        .map(|&c| vt.samples.get(c).map(|s| s.pts).unwrap_or(i64::MAX))
        .collect();
    let mut plans = vec![Plan {
        track: v,
        ranges: cuts.windows(2).map(|w| (w[0], w[1])).collect(),
    }];
    for (i, t) in mp4.tracks.iter().enumerate() {
        if i == v || t.pcm_chunked || t.samples.is_empty() || t.kind != TrackKind::Audio {
            continue;
        }
        // the bounds are in the video track's timescale; convert to this track's
        let at = |sec: f64| {
            let ts = (sec * f64::from(t.timescale)).round() as i64;
            t.samples.partition_point(|s| s.pts < ts)
        };
        let to_sec = |ts: i64| ts as f64 / f64::from(vt.timescale);
        let ranges = (0..parts.len())
            .map(|k| {
                let a = if bounds[k] == i64::MAX { t.samples.len() } else { at(to_sec(bounds[k])) };
                let b = if k + 1 < parts.len() && bounds[k + 1] != i64::MAX { at(to_sec(bounds[k + 1])) } else { t.samples.len() };
                (a, b.max(a))
            })
            .collect();
        plans.push(Plan { track: i, ranges });
    }
    Ok(plans)
}

/// Writes one part: a fresh MP4/MOV with the same brands, sample entries and timescales as the
/// source, carrying the planned sample ranges. Returns the bytes written.
fn write_part(
    mp4: &Mp4File,
    file: &std::fs::File,
    plans: &[Plan],
    index: usize,
    part: &SplitPart,
    progress: Option<&Progress>,
) -> Result<u64, String> {
    let brand = if mp4.major_brand == FourCc(*b"qt  ") { Brand::Mov } else { Brand::Mp4 };
    // read+write: `finish_faststart` shifts the moov in place, which reads the file back
    let out = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&part.path).map_err(|e| e.to_string())?;
    let mut w = Mp4Writer::new(out, WriterOptions::new(brand)).map_err(|e| e.to_string())?;
    let mut bytes = 0u64;
    for plan in plans {
        let (a, b) = plan.ranges[index];
        if b <= a {
            continue;
        }
        bytes += copy_track(mp4, file, plan.track, (a, b), &mut w, progress, bytes)?;
    }
    w.finish_faststart().map_err(|e| e.to_string())?;
    Ok(bytes)
}

/// Splits `source` into every part (stream copy, no re-encode). `progress`, when given, gets
/// `total` = the copyable sample bytes up front and `done` = bytes written so far; a cancelled
/// progress stops after the part being written with the error `"cancelled"`. Parts are written
/// in order; a failure leaves the parts before it on disk (they are valid files).
pub fn split_stream_copy(source: &Path, parts: &[SplitPart], progress: Option<&Progress>) -> Result<(), String> {
    if parts.is_empty() {
        return Ok(());
    }
    for p in parts {
        if p.end_seconds <= p.start_seconds || p.start_seconds < 0.0 {
            return Err(format!("invalid part range {}–{}", p.start_seconds, p.end_seconds));
        }
    }
    let file = std::fs::File::open(source).map_err(|e| format!("{}: {e}", source.display()))?;
    let mp4 = open(&file).map_err(|e| format!("{}: {e}", source.display()))?;
    let plans = plan_tracks(&mp4, parts)?;
    if let Some(p) = progress {
        let mut total = 0u64;
        for plan in &plans {
            for &(a, b) in &plan.ranges {
                total += mp4.tracks[plan.track].samples[a..b].iter().map(|s| s.size as u64).sum::<u64>();
            }
        }
        p.total.store(total, std::sync::atomic::Ordering::Relaxed);
    }
    for (index, part) in parts.iter().enumerate() {
        if let Some(p) = progress {
            if p.cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return Err("cancelled".into());
            }
            *p.status.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = format!("part {}/{}", index + 1, parts.len());
        }
        write_part(&mp4, &file, &plans, index, part, progress)?;
    }
    Ok(())
}

/// Runs [`split_stream_copy`] as a background job (the export queue's job list shows it; the
/// label should start with "Export" so the export screen treats the session as busy). Returns
/// the job id. `Report.path` is the source, `extra_files` the written parts.
pub fn spawn_split(s: &mut crate::Session, source: String, parts: Vec<SplitPart>, label: String, wait: bool) -> crate::Result<u64> {
    let id = s.jobs.iter().map(|j| j.id).max().unwrap_or(0) + 1;
    let job = crate::Job { id, label, progress: Default::default(), result: Default::default() };
    let (prog, res) = (job.progress.clone(), job.result.clone());
    let run = move || {
        let t0 = web_time::Instant::now();
        let r = split_stream_copy(Path::new(&source), &parts, Some(&prog));
        // The render pipeline sets `finished` itself at the end (`filmcraft_export::run`); the
        // splitter must too, or the UI's job bar keeps showing "Exporting…" forever.
        prog.finished.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Err(e) = &r {
            *prog.error.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(e.clone());
        }
        let report = r.map(|()| {
            *prog.status.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = format!("Done in {:.1}s", t0.elapsed().as_secs_f64());
            filmcraft_export::Report {
                extra_files: parts.iter().map(|p| p.path.to_string_lossy().to_string()).collect(),
                path: source,
                frames: 0,
                seconds: 0.0,
                bytes: prog.done.load(std::sync::atomic::Ordering::Relaxed),
                render_fps: 0.0,
            }
        });
        *res.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(report);
    };
    let run = crate::export_tools::guard_job(job.progress.clone(), job.result.clone(), run);
    s.jobs.push(job);
    if wait || cfg!(target_arch = "wasm32") {
        run();
    } else if let Err(e) = std::thread::Builder::new().name("filmcraft-split".into()).spawn(run) {
        return Err(crate::EngineError::Other(e.to_string()));
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use filmcraft_isobmff::{AvcConfig, SampleEntry};

    /// A 6 s test file: H.264 video (30 fps, keyframe every 2 s, non-trivial composition
    /// offsets) + AAC (48 kHz). Payloads encode (track, index) so byte-equality is checkable
    /// after the split.
    fn make_source(path: &Path) {
        let out = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(path).unwrap();
        let mut w = Mp4Writer::new(out, WriterOptions::new(Brand::Mp4)).unwrap();
        let v = w
            .add_track(TrackConfig::new(SampleEntry::avc(AvcConfig::new(vec![vec![0x67, 0x64]], vec![vec![0x68]], 4), 320, 240), 30))
            .unwrap();
        let a = w.add_track(TrackConfig::new(SampleEntry::aac(vec![0x11, 0x90], 2, 48000), 48000)).unwrap();
        for i in 0..180u32 {
            // keyframe every second; composition offset cycles so ctts is non-trivial
            let sync = i % 30 == 0;
            let cto = if sync { 0 } else { (i % 3) as i32 };
            let data = vec![1u8, (i >> 8) as u8, i as u8, 0xAB];
            w.write_sample(v, WriteSample { data: &data, duration: 1, composition_offset: cto, is_sync: sync }).unwrap();
        }
        for i in 0..282u32 {
            let data = vec![2u8, (i >> 8) as u8, i as u8, 0xCD];
            w.write_sample(a, WriteSample { data: &data, duration: 1024, composition_offset: 0, is_sync: true }).unwrap();
        }
        w.finish_faststart().unwrap();
    }

    #[test]
    fn parts_start_on_keyframes_and_carry_identical_bytes() {
        let dir = std::env::temp_dir().join(format!("split-basic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("whole.mp4");
        make_source(&source);

        let parts = vec![
            SplitPart { start_seconds: 0.0, end_seconds: 2.0, path: dir.join("P1.mp4") },
            SplitPart { start_seconds: 2.0, end_seconds: 4.0, path: dir.join("P2.mp4") },
            SplitPart { start_seconds: 4.0, end_seconds: 6.0, path: dir.join("P3.mp4") },
        ];
        split_stream_copy(&source, &parts, None).unwrap();

        let whole_file = std::fs::File::open(&source).unwrap();
        let whole = open(&whole_file).unwrap();
        for (k, part) in parts.iter().enumerate() {
            let f = std::fs::File::open(&part.path).unwrap();
            let mp4 = open(&f).unwrap();
            let (vt, at) = (mp4.track_of_kind(TrackKind::Video).unwrap(), mp4.track_of_kind(TrackKind::Audio).unwrap());
            let (wvt, wat) = (whole.track_of_kind(TrackKind::Video).unwrap(), whole.track_of_kind(TrackKind::Audio).unwrap());
            assert_eq!(mp4.tracks[vt].samples.len(), 60, "part {}: 2s of 30fps video", k + 1);
            assert_eq!(mp4.tracks[at].samples.len(), 94, "part {}: 2s of AAC packets", k + 1);
            assert!(mp4.tracks[vt].samples[0].is_sync, "part {} starts on a keyframe", k + 1);
            // bytes and timing identical to the same span of the source
            let offset = k * 60;
            for (i, s) in mp4.tracks[vt].samples.iter().enumerate() {
                assert_eq!(mp4.read_sample(&f, vt, i).unwrap(), whole.read_sample(&whole_file, wvt, offset + i).unwrap());
                assert_eq!(s.duration, whole.tracks[wvt].samples[offset + i].duration);
                assert_eq!(s.is_sync, whole.tracks[wvt].samples[offset + i].is_sync);
            }
            for (i, s) in mp4.tracks[at].samples.iter().enumerate() {
                assert_eq!(mp4.read_sample(&f, at, i).unwrap(), whole.read_sample(&whole_file, wat, k * 94 + i).unwrap());
                let _ = s;
            }
            // presentation restarts at zero and never goes negative
            let pts: Vec<i64> = (0..mp4.tracks[vt].samples.len()).filter_map(|i| mp4.tracks[vt].presentation_pts(i)).collect();
            assert_eq!(*pts.iter().min().unwrap(), 0, "part {} video starts at 0", k + 1);
            let apts: Vec<i64> = (0..mp4.tracks[at].samples.len()).filter_map(|i| mp4.tracks[at].presentation_pts(i)).collect();
            assert_eq!(*apts.iter().min().unwrap(), 0, "part {} audio starts at 0", k + 1);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_mid_gop_range_falls_back_to_the_keyframe_with_audio_following() {
        let dir = std::env::temp_dir().join(format!("split-gop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("whole.mp4");
        make_source(&source);
        // 0.5s in, inside the first GOP: the part falls back to the keyframe at 0
        let parts = vec![
            SplitPart { start_seconds: 0.5, end_seconds: 3.0, path: dir.join("P1.mp4") },
            SplitPart { start_seconds: 3.0, end_seconds: 6.0, path: dir.join("P2.mp4") },
        ];
        split_stream_copy(&source, &parts, None).unwrap();
        let f = std::fs::File::open(&parts[0].path).unwrap();
        let mp4 = open(&f).unwrap();
        let (vt, at) = (mp4.track_of_kind(TrackKind::Video).unwrap(), mp4.track_of_kind(TrackKind::Audio).unwrap());
        assert!(mp4.tracks[vt].samples[0].is_sync);
        assert_eq!(mp4.tracks[vt].samples.len(), 90, "P1 runs from the keyframe at 0s to the one at 3s");
        // the audio covers the same span (3s ≈ 140.6 packets → 141), so A/V stay in sync
        let audio_seconds = mp4.tracks[at].samples.len() as f64 * 1024.0 / 48000.0;
        assert!((audio_seconds - 3.0).abs() < 0.05, "audio spans the video's actual start, got {audio_seconds}s");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The `export.split` command end to end: parse params, run the job, report the parts.
    #[test]
    fn the_export_split_command_runs_as_a_job() {
        let dir = std::env::temp_dir().join(format!("split-cmd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("whole.mp4");
        make_source(&source);

        let mut s = crate::Session::default();
        let v = s
            .execute(
                "export.split",
                serde_json::json!({
                    "source": source.to_string_lossy(),
                    "parts": [
                        {"startSeconds": 0.0, "endSeconds": 3.0, "path": dir.join("A.mp4").to_string_lossy()},
                        {"startSeconds": 3.0, "endSeconds": 6.0, "path": dir.join("B.mp4").to_string_lossy()},
                    ],
                    "wait": true,
                }),
            )
            .unwrap();
        let job = v["job"].as_u64().unwrap();
        let j = s.jobs.iter().find(|j| j.id == job).unwrap();
        // finished must be set on success too, or the UI's job bar shows "Exporting…" forever
        assert!(j.progress.finished.load(std::sync::atomic::Ordering::Relaxed));
        assert!(j.progress.status.lock().unwrap().starts_with("Done in"));
        let report = j.result.lock().unwrap().clone().unwrap().unwrap();
        assert_eq!(report.extra_files.len(), 2);
        assert!(dir.join("A.mp4").is_file() && dir.join("B.mp4").is_file());
        // bad params surface as command errors
        assert!(s.execute("export.split", serde_json::json!({"source": source.to_string_lossy()})).is_err());
        assert!(s
            .execute("export.split", serde_json::json!({"source": source.to_string_lossy(), "parts": [{"endSeconds": 1.0}]}))
            .is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancel_and_errors_surface() {
        let dir = std::env::temp_dir().join(format!("split-err-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // invalid range
        let bad = vec![SplitPart { start_seconds: 3.0, end_seconds: 1.0, path: dir.join("x.mp4") }];
        assert!(split_stream_copy(&dir.join("missing.mp4"), &bad, None).is_err());
        // cancelled progress stops before the first part
        let source = dir.join("whole.mp4");
        make_source(&source);
        let progress = Progress::default();
        progress.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        let parts = vec![SplitPart { start_seconds: 0.0, end_seconds: 2.0, path: dir.join("P1.mp4") }];
        assert_eq!(split_stream_copy(&source, &parts, Some(&progress)).unwrap_err(), "cancelled");
        assert!(!parts[0].path.exists(), "nothing written before the first part");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
