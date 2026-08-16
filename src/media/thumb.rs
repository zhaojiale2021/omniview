//! Background thumbnail decoder for the seek-bar preview.
//!
//! The UI asks for a small frame at an arbitrary media time; decoding one
//! synchronously on the render thread would stall playback for hundreds of
//! milliseconds (file open + seek + decode + scale), so this module owns a
//! single worker thread.  Rapid hover requests are coalesced: while the
//! worker is decoding, newer requests overwrite the pending one, so a quick
//! drag across the bar only decodes the final hovered position.

use std::sync::mpsc;
use std::thread;

use ffmpeg_next as ffmpeg;
use ffmpeg::{codec, format, frame, media, software};

/// Thumbnail dimensions used by the UI preview.  Small enough to decode
/// quickly and upload as an egui texture without measurable overhead.
pub const THUMB_MAX_W: u32 = 160;
pub const THUMB_MAX_H: u32 = 90;

#[derive(Debug, Clone)]
pub struct Thumbnail {
    /// File the frame was decoded from.  The app discards results that no
    /// longer match the currently open file.
    pub path: String,
    /// Media time the frame was decoded for (seconds).
    pub pos: f64,
    pub width: u32,
    pub height: u32,
    /// Packed RGBA8 pixels, `width * height * 4` bytes.
    pub rgba: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct ThumbnailRequest {
    pub path: String,
    pub pos: f64,
    pub max_w: u32,
    pub max_h: u32,
}

pub type ThumbnailResult = Result<Thumbnail, String>;

/// Owns the thumbnail worker thread.  Cheap to poll every frame: `poll`
/// drains finished decodes without blocking.
pub struct ThumbnailService {
    req_tx: mpsc::Sender<ThumbnailRequest>,
    res_rx: mpsc::Receiver<ThumbnailResult>,
}

impl ThumbnailService {
    pub fn new() -> Self {
        let (req_tx, req_rx) = mpsc::channel::<ThumbnailRequest>();
        let (res_tx, res_rx) = mpsc::channel::<ThumbnailResult>();

        thread::spawn(move || thumbnail_worker(req_rx, res_tx));

        Self { req_tx, res_rx }
    }

    /// Ask for a thumbnail.  If the worker is busy, this request replaces
    /// any queued-but-not-started request for the latest-position preview.
    pub fn request(&self, path: String, pos: f64, max_w: u32, max_h: u32) {
        let _ = self.req_tx.send(ThumbnailRequest {
            path,
            pos,
            max_w,
            max_h,
        });
    }

    /// Non-blocking poll for a finished thumbnail.
    pub fn poll(&self) -> Option<ThumbnailResult> {
        self.res_rx.try_recv().ok()
    }
}

impl Default for ThumbnailService {
    fn default() -> Self {
        Self::new()
    }
}

/// Worker loop: wait for at least one request, coalesce any queued requests
/// to the newest one, decode it, and report the result.
fn thumbnail_worker(
    req_rx: mpsc::Receiver<ThumbnailRequest>,
    res_tx: mpsc::Sender<ThumbnailResult>,
) {
    let _ = ffmpeg::init(); // safe to call again; other threads do too

    let mut latest: Option<ThumbnailRequest> = None;

    loop {
        if latest.is_none() {
            match req_rx.recv() {
                Ok(req) => latest = Some(req),
                Err(_) => break, // service dropped
            }
        }

        // Coalesce: keep only the most recent request received while we
        // were either waiting or decoding the previous one.
        while let Ok(req) = req_rx.try_recv() {
            latest = Some(req);
        }

        let req = latest.take().expect("latest is Some");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            decode_thumbnail(&req.path, req.pos, req.max_w, req.max_h)
        }))
        .unwrap_or_else(|p| {
            let msg = p
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| p.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".into());
            Err(format!("thumbnail decoder panicked: {msg}"))
        });
        if res_tx.send(result).is_err() {
            break; // service dropped
        }
    }
}

/// Decode and downscale one video frame at (or just after) `pos`.
///
/// This is intentionally self-contained: it opens the file, seeks, decodes
/// a handful of frames, and returns RGBA8 pixels.  It never touches the
/// playback pipeline, so thumbnail requests cannot perturb A/V sync.
fn decode_thumbnail(
    path: &str,
    pos: f64,
    max_w: u32,
    max_h: u32,
) -> ThumbnailResult {
    ffmpeg::init().map_err(|e| format!("ffmpeg init: {e}"))?;

    let mut input = format::input(path).map_err(|e| format!("open input: {e}"))?;
    let stream = input
        .streams()
        .best(media::Type::Video)
        .ok_or_else(|| "no video stream".to_string())?;
    let stream_index = stream.index();
    let time_base = stream.time_base();
    let params = stream.parameters();

    let ctx = codec::context::Context::from_parameters(params)
        .map_err(|e| format!("video codec context: {e}"))?;
    let mut decoder = ctx
        .decoder()
        .video()
        .map_err(|e| format!("video decoder: {e}"))?;

    let src_w = decoder.width();
    let src_h = decoder.height();
    if src_w == 0 || src_h == 0 {
        return Err("video decoder has no size".to_string());
    }

    // Fit inside max_w x max_h while keeping aspect ratio; round to even
    // dimensions so scaling and texture upload stay on the happy path.
    let scale = (max_w as f64 / src_w as f64)
        .min(max_h as f64 / src_h as f64)
        .min(1.0);
    let dst_w = (((src_w as f64) * scale).round() as u32).max(2) / 2 * 2;
    let dst_h = (((src_h as f64) * scale).round() as u32).max(2) / 2 * 2;

    let mut scaler = software::scaling::Context::get(
        decoder.format(),
        src_w,
        src_h,
        format::Pixel::RGBA,
        dst_w,
        dst_h,
        software::scaling::Flags::BILINEAR,
    )
    .map_err(|e| format!("thumbnail scaler: {e}"))?;

    // Seek backwards from the target; then decode forward until the first
    // frame at/after `pos`.  AV_TIME_BASE is 1_000_000 in FFmpeg.
    let ts = (pos.max(0.0) * 1_000_000.0) as i64;
    unsafe {
        ffmpeg::ffi::av_seek_frame(
            input.as_mut_ptr(),
            -1,
            ts,
            ffmpeg::ffi::AVSEEK_FLAG_BACKWARD,
        );
    }

    let mut decoded = frame::Video::empty();
    let mut rgba_frame = frame::Video::empty();

    for (s, packet) in input.packets() {
        if s.index() != stream_index {
            continue;
        }

        if let Err(e) = decoder.send_packet(&packet) {
            if matches!(e, ffmpeg::Error::Eof) {
                break;
            }
            continue;
        }

        while decoder.receive_frame(&mut decoded).is_ok() {
            let pts_secs = decoded
                .pts()
                .or_else(|| decoded.timestamp())
                .map(|p| p as f64 * time_base.numerator() as f64 / time_base.denominator() as f64)
                .unwrap_or(0.0);

            // The first frame at/after the target (with a tiny epsilon) is
            // the preview frame.  Frames before it are from the previous
            // keyframe and are skipped.
            if pts_secs + 0.05 >= pos {
                scaler
                    .run(&decoded, &mut rgba_frame)
                    .map_err(|e| format!("thumbnail scale: {e}"))?;

                let stride = rgba_frame.stride(0);
                let bytes = rgba_frame.data(0);
                let mut rgba = Vec::with_capacity((dst_w * dst_h * 4) as usize);
                for row in 0..dst_h as usize {
                    let start = row * stride;
                    let end = start + dst_w as usize * 4;
                    if end <= bytes.len() {
                        rgba.extend_from_slice(&bytes[start..end]);
                    } else {
                        return Err("scaled frame data truncated".to_string());
                    }
                }

                return Ok(Thumbnail {
                    path: path.to_string(),
                    pos: pts_secs,
                    width: dst_w,
                    height: dst_h,
                    rgba,
                });
            }
        }
    }

    Err(format!("no video frame at {pos:.1}s"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_thumbnail_for_seek_preview() {
        let thumb = decode_thumbnail("/tmp/test_av.mp4", 1.5, 160, 90).unwrap();
        assert_eq!((thumb.width, thumb.height), (160, 90));
        assert_eq!(thumb.rgba.len(), 160 * 90 * 4);
        assert!(thumb.pos >= 1.5 - 0.05, "preview frame should be at/after target, got {}", thumb.pos);
    }
}
