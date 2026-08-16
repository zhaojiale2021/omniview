//! Decoder-level corruption check: after seek, does the decoder emit frames
//! with missing references / corrupt flags / broken PTS order?
//!
//! Usage: cargo run --release --example dec_corrupt -- <file> <seek_pos>

use std::time::Instant;

use ffmpeg_next as ffmpeg;
use ffmpeg_next::{format, frame, media, software};

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: dec_corrupt <file> <pos>");
    let pos: f64 = std::env::args()
        .nth(2)
        .map(|s| s.parse().unwrap())
        .unwrap_or(30.0);
    ffmpeg::init().unwrap();

    let mut input = format::input(&path).expect("open");
    let stream = input
        .streams()
        .best(media::Type::Video)
        .expect("video stream");
    let stream_index = stream.index();
    let time_base = stream.time_base();
    let ctx = ffmpeg::codec::context::Context::from_parameters(stream.parameters()).unwrap();
    let mut decoder = ctx.decoder().video().unwrap();

    // seek backward to keyframe
    let ts = (pos * 1_000_000.0) as i64;
    let rc = unsafe {
        ffmpeg::ffi::av_seek_frame(
            input.as_mut_ptr(),
            stream_index as i32,
            ts,
            ffmpeg::ffi::AVSEEK_FLAG_BACKWARD,
        )
    };
    println!("seek rc={rc}");

    let mut scaler = software::scaling::Context::get(
        decoder.format(),
        decoder.width(),
        decoder.height(),
        format::Pixel::NV12,
        decoder.width(),
        decoder.height(),
        software::scaling::Flags::BILINEAR,
    )
    .unwrap();
    let mut nv12 = frame::Video::empty();

    let mut count = 0u64;
    let mut corrupt = 0u64;
    let mut missing_pts = 0u64;
    let mut pts_out_of_order = 0u64;
    let mut last_pts: Option<i64> = None;
    let mut first_after_seek = true;

    let t0 = Instant::now();
    for (s, pkt) in input.packets() {
        if s.index() != stream_index {
            continue;
        }
        if decoder.send_packet(&pkt).is_err() {
            continue;
        }
        let mut decoded = frame::Video::empty();
        while decoder.receive_frame(&mut decoded).is_ok() {
            count += 1;
            let flags = decoded.flags();
            if flags.contains(ffmpeg::util::frame::flag::Flags::CORRUPT) {
                corrupt += 1;
            }
            let pts = decoded.timestamp().or(decoded.pts());
            match pts {
                Some(p) => {
                    if let Some(lp) = last_pts
                        && p < lp
                    {
                        pts_out_of_order += 1;
                    }
                    last_pts = Some(p);
                    let pts_secs =
                        p as f64 * time_base.numerator() as f64 / time_base.denominator() as f64;
                    if first_after_seek {
                        println!(
                            "first frame after seek: pts={:.3}s corrupt={} key={}",
                            pts_secs,
                            flags.contains(ffmpeg::util::frame::flag::Flags::CORRUPT),
                            decoded.is_key()
                        );
                        first_after_seek = false;
                    }
                    if count < 30 && pts_secs < pos + 0.6 {
                        println!(
                            "  frame {count}: pts={pts_secs:.3}s corrupt={} key={}",
                            flags.contains(ffmpeg::util::frame::flag::Flags::CORRUPT),
                            decoded.is_key()
                        );
                    }
                }
                None => missing_pts += 1,
            }
            // scale it to force decode completion
            let _ = scaler.run(&decoded, &mut nv12);
            if count >= 100 {
                break;
            }
        }
        if count >= 100 {
            break;
        }
    }
    println!(
        "frames={count} corrupt={corrupt} missing_pts={missing_pts} out_of_order={pts_out_of_order} time={:.1}s",
        t0.elapsed().as_secs_f64()
    );
}
