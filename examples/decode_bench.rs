//! Headless decode benchmark: measures the production video path
//! (ffmpeg software decode + swscale to RGBA) without a window.
//!
//! Usage: cargo run --release --example decode_bench -- <file> [--decode-only|--nv12]

use std::time::Instant;

use ffmpeg_next as ffmpeg;
use ffmpeg_next::{format, frame, media, software};

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: decode_bench <file> [--decode-only]");
        std::process::exit(2);
    };
    let mode_arg = args.next();
    let mode = mode_arg
        .as_deref()
        .map(|s| s.trim_start_matches('-'))
        .unwrap_or("rgba");

    ffmpeg::init().expect("ffmpeg init");

    let mut input = format::input(&path).expect("open input");
    let stream = input
        .streams()
        .best(media::Type::Video)
        .expect("no video stream");
    let stream_index = stream.index();
    let time_base = stream.time_base();
    let rate = stream.rate();
    let fps = if rate.denominator() > 0 {
        rate.numerator() as f64 / rate.denominator() as f64
    } else {
        30.0
    };
    let duration = stream.duration() as f64
        * time_base.numerator() as f64
        / time_base.denominator() as f64;

    let ctx = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
        .expect("codec context");
    let mut decoder = ctx.decoder().video().expect("video decoder");
    let (width, height) = (decoder.width(), decoder.height());

    let target_pixel = match mode {
        "decode-only" => None,
        "nv12" => Some(format::Pixel::NV12),
        _ => Some(format::Pixel::RGBA),
    };
    let mut scaler = target_pixel.map(|pix| {
        software::scaling::Context::get(
            decoder.format(),
            width,
            height,
            pix,
            width,
            height,
            software::scaling::Flags::BILINEAR,
        )
        .expect("swscale")
    });
    let mut out = frame::Video::empty();

    println!(
        "{path}: {width}x{height} {fps:.2}fps, {duration:.1}s, format={:?}",
        decoder.format()
    );

    let mut frames = 0u64;
    let mut bytes = 0u64;
    let t0 = Instant::now();
    let mut last_report = t0;
    let mut last_frames = 0u64;

    for (stream_, packet) in input.packets() {
        if stream_.index() != stream_index {
            continue;
        }
        if decoder.send_packet(&packet).is_err() {
            continue;
        }
        let mut decoded = frame::Video::empty();
        while decoder.receive_frame(&mut decoded).is_ok() {
            if let Some(sc) = scaler.as_mut()
                && sc.run(&decoded, &mut out).is_ok() {
                    for p in 0..out.planes() {
                        bytes += out.data(p).len() as u64;
                    }
                }
            frames += 1;
        }

        if last_report.elapsed().as_secs_f64() >= 1.0 {
            let df = frames - last_frames;
            let dt = last_report.elapsed().as_secs_f64();
            println!(
                "  running: {frames} frames, live {:.1} fps (last {dt:.1}s: {df} frames)",
                frames as f64 / t0.elapsed().as_secs_f64()
            );
            last_report = Instant::now();
            last_frames = frames;
        }
    }
    let _ = decoder.send_eof();
    let mut decoded = frame::Video::empty();
    while decoder.receive_frame(&mut decoded).is_ok() {
        if let Some(sc) = scaler.as_mut()
            && sc.run(&decoded, &mut out).is_ok() {
                for p in 0..out.planes() {
                    bytes += out.data(p).len() as u64;
                }
            }
        frames += 1;
    }

    let dt = t0.elapsed().as_secs_f64();
    let real_fps = frames as f64 / dt;
    let target = fps;
    let pct = real_fps / target * 100.0;
    let mode = match mode {
        "decode-only" => "decode-only",
        "nv12" => "decode+NV12 (new production path)",
        _ => "decode+RGBA (old production path)",
    };
    println!(
        "RESULT [{mode}]: {frames} frames in {dt:.2}s = {real_fps:.1} fps ({pct:.0}% of {target:.1} target), {:.1} MB converted",
        bytes as f64 / 1e6
    );
    if real_fps + 0.05 < target {
        println!("VERDICT: NOT smooth — decode path cannot keep up with content rate");
    } else {
        println!("VERDICT: decode path keeps up with content rate");
    }
}
