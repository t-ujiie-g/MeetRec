//! 開発用：生の H.264（Annex B、AUD 区切り）と ADTS AAC を MeetRec の muxer で MP4 にする。
//! muxer の出力を ffprobe 等で検証するためのもの（アプリ本体では使わない）。
//!
//! ```sh
//! ffmpeg -f lavfi -i testsrc=size=640x360:rate=10 -t 8 -c:v libx264 -bf 0 -g 40 \
//!   -x264-params aud=1 -bsf:v h264_mp4toannexb v.h264
//! ffmpeg -f lavfi -i sine=frequency=440:sample_rate=48000 -t 8 -ac 2 -c:a aac a.aac
//! cargo run -p meetrec-core --example mux_raw -- v.h264 a.aac out.mp4 [crash]
//! ```

use std::fs::{self, File};

use meetrec_core::h264;
use meetrec_core::mp4::{FragWriter, TrackKind, TrackSpec, VIDEO_TIMESCALE};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let video = fs::read(&args[1]).unwrap();
    let audio = fs::read(&args[2]).unwrap();
    let crash = args.get(4).is_some_and(|a| a == "crash");

    // AUD（type 9）でアクセスユニットに分ける。
    let mut units: Vec<Vec<u8>> = Vec::new();
    for nal in h264::nal_units(&video) {
        if nal[0] & 0x1f == 9 || units.is_empty() {
            units.push(Vec::new());
        }
        let u = units.last_mut().unwrap();
        u.extend_from_slice(&[0, 0, 0, 1]);
        u.extend_from_slice(nal);
    }
    let aus: Vec<h264::AccessUnit> = units.iter().map(|u| h264::annexb_to_avcc(u)).collect();
    let sps = aus.iter().find_map(|a| a.sps.clone()).expect("SPS");
    let pps = aus.iter().find_map(|a| a.pps.clone()).expect("PPS");

    // ADTS を raw AAC パケットに分ける。
    let mut packets = Vec::new();
    let mut i = 0;
    while i + 7 <= audio.len() {
        let len = ((audio[i + 3] as usize & 3) << 11)
            | ((audio[i + 4] as usize) << 3)
            | (audio[i + 5] as usize >> 5);
        let header = if audio[i + 1] & 1 == 1 { 7 } else { 9 };
        packets.push(audio[i + header..i + len].to_vec());
        i += len;
    }

    let tracks = vec![
        TrackSpec {
            kind: TrackKind::Video {
                width: 640,
                height: 360,
                sps,
                pps,
            },
            timescale: VIDEO_TIMESCALE,
            enabled: true,
        },
        TrackSpec {
            kind: TrackKind::Audio {
                sample_rate: 48_000,
                channels: 2,
                asc: meetrec_core::audio::DEFAULT_ASC.to_vec(),
                bitrate: 96_000,
            },
            timescale: 48_000,
            enabled: true,
        },
    ];
    let mut w = FragWriter::new(File::create(&args[3]).unwrap(), tracks).unwrap();
    // 映像は 10fps。ただし 20 フレーム目の後に 3 秒の「画面が変化しない」区間を入れる。
    let mut next_audio = 0usize;
    let mut last_flush = 0u64;
    let mut end_ms = 0;
    for (n, au) in aus.iter().enumerate() {
        let ms = n as u64 * 100 + if n > 20 { 3000 } else { 0 };
        while next_audio < packets.len() && (next_audio as u64 * 1024 * 1000 / 48_000) < ms {
            // 音声だけが進む区間でもフラグメントを切る。
            let a_ms = next_audio as u64 * 1024 * 1000 / 48_000;
            if a_ms - last_flush >= 2000 {
                w.flush(a_ms).unwrap();
                last_flush = a_ms;
            }
            w.write_fixed(1, 1024, &packets[next_audio]);
            next_audio += 1;
        }
        w.write_timed(0, ms * 90, au.key, &au.avcc).unwrap();
        end_ms = ms + 100;
    }
    if crash {
        // finish せずに終了（未確定の最後のフラグメントは失われる）。
        w.flush(end_ms - 700).unwrap();
        w.write_fixed(1, 1024, &packets[0]);
        println!("crashed without finish");
        return;
    }
    let (_, fin) = w.finish(end_ms).unwrap();
    println!("{fin:?}");
}
