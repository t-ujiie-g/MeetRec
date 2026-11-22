//! 最小限の fragmented MP4（ISO BMFF）muxer と、異常終了ファイルの修復。
//!
//! 構造：`ftyp` `moov`(mvex 付き・サンプル表は空) の後に `moof`+`mdat` を約 2 秒ごとに
//! 追記していく。どの時点で書き込みが止まっても、最後の完全な `mdat` までは
//! そのまま再生できる。正常終了時は `mfra`（シーク用索引）を付け、`mehd` に総時間を書く。
//!
//! 映像は「変化があったときだけフレームが来る」前提なので、各サンプルの長さは
//! 次のサンプルが来るまで確定しない。フラグメントを閉じる時点で未確定の最後の
//! サンプルは暫定の長さで書き、次のサンプル到着時にその 4 バイトだけを上書きする。

use std::fs::OpenOptions;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::h264;

pub const VIDEO_TIMESCALE: u32 = 90_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackKind {
    Video {
        width: u16,
        height: u16,
        sps: Vec<u8>,
        pps: Vec<u8>,
    },
    Audio {
        sample_rate: u32,
        channels: u16,
        asc: Vec<u8>,
        bitrate: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackSpec {
    pub kind: TrackKind,
    pub timescale: u32,
    /// false のトラックは既定では再生されない（別トラック保存用）。
    pub enabled: bool,
}

struct Sample {
    duration: u32,
    size: u32,
    key: bool,
}

struct Pending {
    pts: u64,
    key: bool,
    data: Vec<u8>,
}

struct Track {
    spec: TrackSpec,
    samples: Vec<Sample>,
    data: Vec<u8>,
    /// 現フラグメント先頭サンプルの DTS。
    frag_dts: u64,
    /// 次に書くサンプルの DTS（= これまでの終端）。
    next_dts: u64,
    pending: Option<Pending>,
    /// 直前フラグメントに暫定長で書いた最後のサンプル（ファイル上の duration 位置, pts）。
    patch: Option<(u64, u64)>,
    last_pts: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Finished {
    pub bytes: u64,
    pub duration_ms: u64,
    pub fragments: u32,
}

pub struct FragWriter<W: Write + Seek> {
    w: W,
    pos: u64,
    tracks: Vec<Track>,
    seq: u32,
    mehd_pos: u64,
    /// mfra 用：(時刻, moof 位置, traf 番号, サンプル番号)。先頭トラックのキーフレームのみ。
    index: Vec<(u64, u64, u8, u8)>,
}

fn bx(buf: &mut Vec<u8>, typ: &[u8; 4], f: impl FnOnce(&mut Vec<u8>)) {
    let start = buf.len();
    buf.extend_from_slice(&[0; 4]);
    buf.extend_from_slice(typ);
    f(buf);
    let size = (buf.len() - start) as u32;
    buf[start..start + 4].copy_from_slice(&size.to_be_bytes());
}

fn full(buf: &mut Vec<u8>, typ: &[u8; 4], version: u8, flags: u32, f: impl FnOnce(&mut Vec<u8>)) {
    bx(buf, typ, |b| {
        b.extend_from_slice(&((version as u32) << 24 | flags).to_be_bytes());
        f(b)
    })
}

fn u16be(b: &mut Vec<u8>, v: u16) {
    b.extend_from_slice(&v.to_be_bytes());
}
fn u32be(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_be_bytes());
}
fn u64be(b: &mut Vec<u8>, v: u64) {
    b.extend_from_slice(&v.to_be_bytes());
}
fn zeros(b: &mut Vec<u8>, n: usize) {
    b.resize(b.len() + n, 0);
}

const MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

fn write_trak(b: &mut Vec<u8>, id: u32, t: &TrackSpec) {
    let is_video = matches!(t.kind, TrackKind::Video { .. });
    bx(b, b"trak", |b| {
        // enabled | in_movie | in_preview
        let flags = if t.enabled { 7 } else { 6 };
        full(b, b"tkhd", 0, flags, |b| {
            zeros(b, 8);
            u32be(b, id);
            zeros(b, 4 + 4 + 8);
            u16be(b, 0); // layer
            u16be(b, if is_video { 0 } else { 1 }); // 音声は同じ alternate group
            u16be(b, if is_video { 0 } else { 0x0100 });
            u16be(b, 0);
            for m in MATRIX {
                u32be(b, m);
            }
            let (w, h) = match t.kind {
                TrackKind::Video { width, height, .. } => (width as u32, height as u32),
                _ => (0, 0),
            };
            u32be(b, w << 16);
            u32be(b, h << 16);
        });
        bx(b, b"mdia", |b| {
            full(b, b"mdhd", 0, 0, |b| {
                zeros(b, 8);
                u32be(b, t.timescale);
                u32be(b, 0);
                u16be(b, 0x55c4); // und
                u16be(b, 0);
            });
            full(b, b"hdlr", 0, 0, |b| {
                u32be(b, 0);
                b.extend_from_slice(if is_video { b"vide" } else { b"soun" });
                zeros(b, 12);
                b.extend_from_slice(if is_video { b"Video\0" } else { b"Audio\0" });
            });
            bx(b, b"minf", |b| {
                if is_video {
                    full(b, b"vmhd", 0, 1, |b| zeros(b, 8));
                } else {
                    full(b, b"smhd", 0, 0, |b| zeros(b, 4));
                }
                bx(b, b"dinf", |b| {
                    full(b, b"dref", 0, 0, |b| {
                        u32be(b, 1);
                        full(b, b"url ", 0, 1, |_| {});
                    })
                });
                bx(b, b"stbl", |b| {
                    full(b, b"stsd", 0, 0, |b| {
                        u32be(b, 1);
                        write_sample_entry(b, t);
                    });
                    full(b, b"stts", 0, 0, |b| u32be(b, 0));
                    full(b, b"stsc", 0, 0, |b| u32be(b, 0));
                    full(b, b"stsz", 0, 0, |b| zeros(b, 8));
                    full(b, b"stco", 0, 0, |b| u32be(b, 0));
                });
            });
        });
    });
}

fn write_sample_entry(b: &mut Vec<u8>, t: &TrackSpec) {
    match &t.kind {
        TrackKind::Video {
            width,
            height,
            sps,
            pps,
        } => bx(b, b"avc1", |b| {
            zeros(b, 6);
            u16be(b, 1);
            zeros(b, 16);
            u16be(b, *width);
            u16be(b, *height);
            u32be(b, 0x0048_0000);
            u32be(b, 0x0048_0000);
            u32be(b, 0);
            u16be(b, 1);
            zeros(b, 32);
            u16be(b, 0x0018);
            u16be(b, 0xffff);
            bx(b, b"avcC", |b| {
                b.extend_from_slice(&h264::avcc_config(sps, pps))
            });
        }),
        TrackKind::Audio {
            sample_rate,
            channels,
            asc,
            bitrate,
        } => bx(b, b"mp4a", |b| {
            zeros(b, 6);
            u16be(b, 1);
            zeros(b, 8);
            u16be(b, *channels);
            u16be(b, 16);
            zeros(b, 4);
            u32be(b, sample_rate << 16);
            full(b, b"esds", 0, 0, |b| {
                let dsi = 2 + asc.len();
                let dcd = 2 + 13 + dsi;
                let es = 3 + dcd + 3;
                b.extend_from_slice(&[0x03, es as u8, 0, 0, 0]);
                b.extend_from_slice(&[0x04, (13 + dsi) as u8, 0x40, 0x15, 0, 0, 0]);
                u32be(b, *bitrate);
                u32be(b, *bitrate);
                b.extend_from_slice(&[0x05, asc.len() as u8]);
                b.extend_from_slice(asc);
                b.extend_from_slice(&[0x06, 1, 2]);
            });
        }),
    }
}

impl<W: Write + Seek> FragWriter<W> {
    /// ヘッダ（ftyp + moov）を書いて開始する。先頭トラックが mfra の索引対象になる。
    pub fn new(mut w: W, specs: Vec<TrackSpec>) -> io::Result<Self> {
        let mut b = Vec::with_capacity(2048);
        bx(&mut b, b"ftyp", |b| {
            b.extend_from_slice(b"isom");
            u32be(b, 0x200);
            b.extend_from_slice(b"isomiso5iso6avc1mp41");
        });
        let mut mehd_pos = 0;
        bx(&mut b, b"moov", |b| {
            full(b, b"mvhd", 0, 0, |b| {
                zeros(b, 8);
                u32be(b, 1000);
                u32be(b, 0);
                u32be(b, 0x0001_0000);
                u16be(b, 0x0100);
                zeros(b, 10);
                for m in MATRIX {
                    u32be(b, m);
                }
                zeros(b, 24);
                u32be(b, specs.len() as u32 + 1);
            });
            for (i, t) in specs.iter().enumerate() {
                write_trak(b, i as u32 + 1, t);
            }
            bx(b, b"mvex", |b| {
                full(b, b"mehd", 1, 0, |b| {
                    mehd_pos = b.len() as u64;
                    u64be(b, 0);
                });
                for i in 0..specs.len() {
                    full(b, b"trex", 0, 0, |b| {
                        u32be(b, i as u32 + 1);
                        u32be(b, 1);
                        zeros(b, 12);
                    });
                }
            });
        });
        w.write_all(&b)?;
        let tracks = specs
            .into_iter()
            .map(|spec| Track {
                spec,
                samples: Vec::new(),
                data: Vec::new(),
                frag_dts: 0,
                next_dts: 0,
                pending: None,
                patch: None,
                last_pts: None,
            })
            .collect();
        Ok(FragWriter {
            w,
            pos: b.len() as u64,
            tracks,
            seq: 0,
            mehd_pos,
            index: Vec::new(),
        })
    }

    pub fn bytes_written(&self) -> u64 {
        self.pos
    }

    fn patch_u32(&mut self, at: u64, value: u32) -> io::Result<()> {
        self.w.seek(SeekFrom::Start(at))?;
        self.w.write_all(&value.to_be_bytes())?;
        self.w.seek(SeekFrom::Start(self.pos))?;
        Ok(())
    }

    /// 長さが「次のサンプルまで」で決まるサンプル（映像）を追加する。`pts` はトラックの timescale。
    pub fn write_timed(
        &mut self,
        track: usize,
        pts: u64,
        key: bool,
        data: &[u8],
    ) -> io::Result<()> {
        // 単調増加を保証する（同時刻・逆行は 1 tick 先へ）。
        let pts = match self.tracks[track].last_pts {
            Some(last) if pts <= last => last + 1,
            _ => pts,
        };
        if let Some((at, prev_pts)) = self.tracks[track].patch.take() {
            // 直前フラグメント末尾の暫定長を確定値に直す。
            let d = (pts - prev_pts).min(u32::MAX as u64) as u32;
            self.patch_u32(at, d)?;
        }
        let t = &mut self.tracks[track];
        if let Some(p) = t.pending.take() {
            let duration = (pts - p.pts).min(u32::MAX as u64) as u32;
            if t.samples.is_empty() {
                t.frag_dts = p.pts;
            }
            t.samples.push(Sample {
                duration,
                size: p.data.len() as u32,
                key: p.key,
            });
            t.data.extend_from_slice(&p.data);
        }
        t.pending = Some(Pending {
            pts,
            key,
            data: data.to_vec(),
        });
        t.last_pts = Some(pts);
        t.next_dts = pts;
        Ok(())
    }

    /// 固定長サンプル（音声）を追加する。
    pub fn write_fixed(&mut self, track: usize, duration: u32, data: &[u8]) {
        let t = &mut self.tracks[track];
        if t.samples.is_empty() {
            t.frag_dts = t.next_dts;
        }
        t.samples.push(Sample {
            duration,
            size: data.len() as u32,
            key: true,
        });
        t.data.extend_from_slice(data);
        t.next_dts += duration as u64;
    }

    /// 溜まったサンプルを 1 フラグメントとして書き出す。`now_ms` は録画開始からの経過時間で、
    /// 未確定の映像サンプルの暫定長に使う。
    pub fn flush(&mut self, now_ms: u64) -> io::Result<()> {
        // 未確定サンプルを暫定長で現フラグメントに入れる。
        let mut provisional: Vec<Option<u64>> = vec![None; self.tracks.len()];
        for (i, t) in self.tracks.iter_mut().enumerate() {
            if let Some(p) = t.pending.take() {
                let now = now_ms * t.spec.timescale as u64 / 1000;
                let duration = now.saturating_sub(p.pts).clamp(1, u32::MAX as u64) as u32;
                if t.samples.is_empty() {
                    t.frag_dts = p.pts;
                }
                t.samples.push(Sample {
                    duration,
                    size: p.data.len() as u32,
                    key: p.key,
                });
                t.data.extend_from_slice(&p.data);
                t.next_dts = p.pts + duration as u64;
                provisional[i] = Some(p.pts);
            }
        }
        if self.tracks.iter().all(|t| t.samples.is_empty()) {
            return Ok(());
        }
        self.seq += 1;
        let seq = self.seq;
        let mut b = Vec::with_capacity(1024);
        let mut data_offset_at: Vec<(usize, usize)> = Vec::new(); // (track, moof 内位置)
        let mut last_dur_at: Vec<(usize, usize)> = Vec::new();
        let mut index_entry: Option<(u64, u8, u8)> = None;
        let mut traf_no = 0u8;
        let tracks = &self.tracks;
        bx(&mut b, b"moof", |b| {
            full(b, b"mfhd", 0, 0, |b| u32be(b, seq));
            for (i, t) in tracks.iter().enumerate() {
                if t.samples.is_empty() {
                    continue;
                }
                traf_no += 1;
                bx(b, b"traf", |b| {
                    full(b, b"tfhd", 0, 0x02_0000, |b| u32be(b, i as u32 + 1));
                    full(b, b"tfdt", 1, 0, |b| u64be(b, t.frag_dts));
                    full(b, b"trun", 0, 0x000701, |b| {
                        u32be(b, t.samples.len() as u32);
                        data_offset_at.push((i, b.len()));
                        u32be(b, 0);
                        let mut dts = t.frag_dts;
                        for (n, s) in t.samples.iter().enumerate() {
                            if n + 1 == t.samples.len() {
                                last_dur_at.push((i, b.len()));
                            }
                            if i == 0 && s.key && index_entry.is_none() && n < 255 {
                                index_entry = Some((dts, traf_no, n as u8 + 1));
                            }
                            u32be(b, s.duration);
                            u32be(b, s.size);
                            u32be(b, if s.key { 0x0200_0000 } else { 0x0101_0000 });
                            dts += s.duration as u64;
                        }
                    });
                });
            }
        });
        let moof_len = b.len();
        let mut offset = moof_len + 8;
        for (i, at) in data_offset_at {
            b[at..at + 4].copy_from_slice(&(offset as u32).to_be_bytes());
            offset += self.tracks[i].data.len();
        }
        let mdat_len: usize = self.tracks.iter().map(|t| t.data.len()).sum::<usize>() + 8;
        if mdat_len > u32::MAX as usize {
            return Err(io::Error::other("fragment too large"));
        }
        u32be(&mut b, mdat_len as u32);
        b.extend_from_slice(b"mdat");
        let moof_pos = self.pos;
        self.w.write_all(&b)?;
        for t in &self.tracks {
            self.w.write_all(&t.data)?;
        }
        self.w.flush()?;
        self.pos += (moof_len + mdat_len) as u64;
        if let Some((time, traf, sample)) = index_entry {
            self.index.push((time, moof_pos, traf, sample));
        }
        for (i, at) in last_dur_at {
            if let Some(pts) = provisional[i] {
                self.tracks[i].patch = Some((moof_pos + at as u64, pts));
            }
        }
        for t in &mut self.tracks {
            t.samples.clear();
            t.data.clear();
            // 大きなキーフレーム後に確保しすぎたバッファを返す。
            if t.data.capacity() > 1 << 20 {
                t.data.shrink_to(1 << 18);
            }
        }
        Ok(())
    }

    fn duration_ms(&self) -> u64 {
        self.tracks
            .iter()
            .map(|t| t.next_dts * 1000 / t.spec.timescale as u64)
            .max()
            .unwrap_or(0)
    }

    /// 最後のフラグメントと索引を書き、総時間を記録して確定する。
    pub fn finish(mut self, end_ms: u64) -> io::Result<(W, Finished)> {
        self.flush(end_ms)?;
        let duration_ms = self.duration_ms();
        if !self.index.is_empty() {
            let mut b = Vec::new();
            bx(&mut b, b"mfra", |b| {
                full(b, b"tfra", 1, 0, |b| {
                    u32be(b, 1);
                    u32be(b, 0); // traf/trun/sample 番号は各 1 バイト
                    u32be(b, self.index.len() as u32);
                    for &(time, moof, traf, sample) in &self.index {
                        u64be(b, time);
                        u64be(b, moof);
                        b.extend_from_slice(&[traf, 1, sample]);
                    }
                });
                let size = b.len() as u32 + 16;
                full(b, b"mfro", 0, 0, |b| u32be(b, size));
            });
            self.w.write_all(&b)?;
            self.pos += b.len() as u64;
        }
        self.w.seek(SeekFrom::Start(self.mehd_pos))?;
        self.w.write_all(&duration_ms.to_be_bytes())?;
        self.w.seek(SeekFrom::Start(self.pos))?;
        self.w.flush()?;
        let fin = Finished {
            bytes: self.pos,
            duration_ms,
            fragments: self.seq,
        };
        Ok((self.w, fin))
    }

    pub fn get_mut(&mut self) -> &mut W {
        &mut self.w
    }
}

// ---------------------------------------------------------------------------
// 修復
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RepairReport {
    pub fragments: u32,
    pub duration_ms: u64,
    pub truncated_bytes: u64,
    pub duration_patched: bool,
    /// 修復後のファイル長。
    pub valid_len: u64,
}

struct BoxHead {
    typ: [u8; 4],
    /// ヘッダを含む全長。
    size: u64,
    header: u64,
}

/// `pos` から `size` バイトのボックスが、長さ `len` のファイルに収まるか。
/// `size` はファイルから読んだ値なので、加算の桁あふれも「収まらない」として扱う。
fn fits(pos: u64, size: u64, len: u64) -> bool {
    pos.checked_add(size).is_some_and(|end| end <= len)
}

fn read_head<R: Read + Seek>(r: &mut R, pos: u64, len: u64) -> io::Result<Option<BoxHead>> {
    if !fits(pos, 8, len) {
        return Ok(None);
    }
    r.seek(SeekFrom::Start(pos))?;
    let mut h = [0u8; 8];
    r.read_exact(&mut h)?;
    let mut size = u32::from_be_bytes([h[0], h[1], h[2], h[3]]) as u64;
    let typ = [h[4], h[5], h[6], h[7]];
    let mut header = 8;
    if size == 1 {
        if !fits(pos, 16, len) {
            return Ok(None);
        }
        let mut l = [0u8; 8];
        r.read_exact(&mut l)?;
        size = u64::from_be_bytes(l);
        header = 16;
    } else if size == 0 {
        size = len - pos;
    }
    if size < header {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad box size"));
    }
    Ok(Some(BoxHead { typ, size, header }))
}

/// メモリ上のボックス列を走査する。
fn children(buf: &[u8]) -> impl Iterator<Item = (&[u8], &[u8])> {
    let mut pos = 0;
    std::iter::from_fn(move || {
        if pos + 8 > buf.len() {
            return None;
        }
        let size = u32::from_be_bytes(buf[pos..pos + 4].try_into().unwrap()) as usize;
        if size < 8 || pos + size > buf.len() {
            return None;
        }
        let item = (&buf[pos + 4..pos + 8], &buf[pos + 8..pos + size]);
        pos += size;
        Some(item)
    })
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4)
        .map(|s| u32::from_be_bytes(s.try_into().unwrap()))
}
fn be64(b: &[u8], at: usize) -> Option<u64> {
    b.get(at..at + 8)
        .map(|s| u64::from_be_bytes(s.try_into().unwrap()))
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

/// moof から (track_id, 終端時刻) を集める。
fn moof_track_ends(moof: &[u8], out: &mut Vec<(u32, u64)>) {
    for (typ, traf) in children(moof) {
        if typ != b"traf" {
            continue;
        }
        let (mut id, mut base, mut total) = (0u32, 0u64, 0u64);
        for (t, p) in children(traf) {
            match t {
                b"tfhd" => id = be32(p, 4).unwrap_or(0),
                b"tfdt" => {
                    base = if p.first() == Some(&1) {
                        be64(p, 4).unwrap_or(0)
                    } else {
                        be32(p, 4).unwrap_or(0) as u64
                    }
                }
                b"trun" => {
                    let flags = be32(p, 0).unwrap_or(0) & 0xff_ffff;
                    let count = be32(p, 4).unwrap_or(0) as usize;
                    let mut at = 8;
                    if flags & 0x1 != 0 {
                        at += 4;
                    }
                    if flags & 0x4 != 0 {
                        at += 4;
                    }
                    let per = [0x100, 0x200, 0x400, 0x800]
                        .iter()
                        .filter(|f| flags & **f != 0)
                        .count()
                        * 4;
                    if flags & 0x100 != 0 {
                        // サンプル数はファイルから読んだ値。実際に入っているぶんだけ読む。
                        for n in 0..count {
                            let Some(duration) = be32(p, at + n * per) else {
                                break;
                            };
                            total = total.saturating_add(duration as u64);
                        }
                    }
                }
                _ => {}
            }
        }
        out.push((id, base.saturating_add(total)));
    }
}

/// 異常終了した fMP4 を再生可能な状態にする：最後の完全なフラグメントの後ろを切り落とし、
/// 総時間を mehd に書く。正常なファイルには何もしない。
pub fn repair<F: Read + Write + Seek>(f: &mut F, len: u64) -> io::Result<RepairReport> {
    let mut pos = 0u64;
    let mut timescales: Vec<(u32, u32)> = Vec::new();
    let mut mehd: Option<(u64, u8, u64)> = None; // (位置, version, 現在値)
    let mut have_moov = false;

    // ヘッダ部（ftyp, moov）。
    while !have_moov {
        let head = read_head(f, pos, len)?.ok_or_else(|| invalid("no moov"))?;
        if !fits(pos, head.size, len) {
            return Err(invalid("truncated header"));
        }
        if &head.typ == b"moov" {
            if head.size > 16 << 20 {
                return Err(invalid("moov too large"));
            }
            let mut moov = vec![0u8; (head.size - head.header) as usize];
            f.seek(SeekFrom::Start(pos + head.header))?;
            f.read_exact(&mut moov)?;
            let base = pos + head.header;
            let off = |inner: &[u8]| base + (inner.as_ptr() as u64 - moov.as_ptr() as u64);
            for (typ, body) in children(&moov) {
                match typ {
                    b"trak" => {
                        let (mut id, mut ts) = (0, 0);
                        for (t, p) in children(body) {
                            if t == b"tkhd" {
                                id = be32(p, if p.first() == Some(&1) { 20 } else { 12 })
                                    .unwrap_or(0);
                            } else if t == b"mdia" {
                                for (t2, p2) in children(p) {
                                    if t2 == b"mdhd" {
                                        ts = be32(p2, if p2.first() == Some(&1) { 20 } else { 12 })
                                            .unwrap_or(0);
                                    }
                                }
                            }
                        }
                        if id != 0 && ts != 0 {
                            timescales.push((id, ts));
                        }
                    }
                    b"mvex" => {
                        for (t, p) in children(body) {
                            if t == b"mehd" {
                                let v = p.first().copied().unwrap_or(0);
                                let cur = if v == 1 {
                                    be64(p, 4).unwrap_or(0)
                                } else {
                                    be32(p, 4).unwrap_or(0) as u64
                                };
                                mehd = Some((off(p) + 4, v, cur));
                            }
                        }
                    }
                    _ => {}
                }
            }
            have_moov = true;
        } else if !matches!(&head.typ, b"ftyp" | b"free" | b"skip") {
            return Err(invalid("unexpected box before moov"));
        }
        pos += head.size;
    }

    let mut valid = pos;
    let mut fragments = 0u32;
    let mut ends: Vec<(u32, u64)> = Vec::new();
    while let Ok(Some(head)) = read_head(f, pos, len) {
        if !fits(pos, head.size, len) {
            break;
        }
        match &head.typ {
            b"moof" => {
                if head.size > 16 << 20 {
                    break;
                }
                let mut moof = vec![0u8; (head.size - head.header) as usize];
                if f.read_exact(&mut moof).is_err() {
                    break;
                }
                let mdat_pos = pos + head.size;
                match read_head(f, mdat_pos, len) {
                    Ok(Some(m)) if &m.typ == b"mdat" && fits(mdat_pos, m.size, len) => {
                        moof_track_ends(&moof, &mut ends);
                        fragments += 1;
                        pos = mdat_pos + m.size;
                        valid = pos;
                    }
                    _ => break,
                }
            }
            b"mfra" | b"free" | b"skip" => {
                pos += head.size;
                valid = pos;
            }
            _ => break,
        }
    }

    let mut duration_ms = 0u64;
    for (id, ts) in &timescales {
        let end = ends
            .iter()
            .filter(|(i, _)| i == id)
            .map(|(_, e)| *e)
            .max()
            .unwrap_or(0);
        let ms = end as u128 * 1000 / *ts as u128;
        duration_ms = duration_ms.max(u64::try_from(ms).unwrap_or(u64::MAX));
    }
    let mut duration_patched = false;
    if let Some((at, version, cur)) = mehd {
        if cur != duration_ms {
            f.seek(SeekFrom::Start(at))?;
            if version == 1 {
                f.write_all(&duration_ms.to_be_bytes())?;
            } else {
                f.write_all(&(duration_ms.min(u32::MAX as u64) as u32).to_be_bytes())?;
            }
            duration_patched = true;
        }
    }
    Ok(RepairReport {
        fragments,
        duration_ms,
        truncated_bytes: len - valid,
        duration_patched,
        valid_len: valid,
    })
}

pub fn repair_file(path: &Path) -> io::Result<RepairReport> {
    let mut f = OpenOptions::new().read(true).write(true).open(path)?;
    let len = f.metadata()?.len();
    let report = repair(&mut f, len)?;
    if report.truncated_bytes > 0 {
        f.set_len(report.valid_len)?;
    }
    f.sync_all()?;
    Ok(report)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Cursor;

    pub fn video_spec() -> TrackSpec {
        TrackSpec {
            kind: TrackKind::Video {
                width: 1280,
                height: 720,
                sps: vec![0x67, 100, 0, 31, 0xac],
                pps: vec![0x68, 0xee],
            },
            timescale: VIDEO_TIMESCALE,
            enabled: true,
        }
    }
    pub fn audio_spec(enabled: bool) -> TrackSpec {
        TrackSpec {
            kind: TrackKind::Audio {
                sample_rate: 48_000,
                channels: 2,
                asc: crate::audio::DEFAULT_ASC.to_vec(),
                bitrate: 96_000,
            },
            timescale: 48_000,
            enabled,
        }
    }

    /// トップレベルのボックス種別を列挙する。
    pub fn top_boxes(buf: &[u8]) -> Vec<String> {
        children(buf)
            .map(|(t, _)| String::from_utf8_lossy(t).into_owned())
            .collect()
    }

    /// 10 秒ぶん（映像 10fps・音声 48k）を 2 秒フラグメントで書く。
    fn build(seconds: u64, finish: bool) -> Vec<u8> {
        let mut w = FragWriter::new(
            Cursor::new(Vec::new()),
            vec![video_spec(), audio_spec(true)],
        )
        .unwrap();
        let mut audio_frames = 0u64;
        for ms in (0..seconds * 1000).step_by(100) {
            w.write_timed(
                0,
                ms * 90,
                ms % 4000 == 0,
                &[0, 0, 0, 2, 0x65, (ms / 100) as u8],
            )
            .unwrap();
            while audio_frames * 1024 * 1000 / 48_000 < ms + 100 {
                w.write_fixed(1, 1024, &[0xaa; 200]);
                audio_frames += 1;
            }
            if ms % 2000 == 1900 {
                w.flush(ms + 100).unwrap();
            }
        }
        if finish {
            let (c, fin) = w.finish(seconds * 1000).unwrap();
            assert_eq!(fin.fragments as u64, seconds / 2);
            c.into_inner()
        } else {
            std::mem::take(w.get_mut().get_mut())
        }
    }

    #[test]
    fn layout_of_finished_file() {
        let buf = build(10, true);
        let boxes = top_boxes(&buf);
        assert_eq!(&boxes[..2], &["ftyp", "moov"]);
        assert_eq!(boxes.iter().filter(|b| *b == "moof").count(), 5);
        assert_eq!(boxes.iter().filter(|b| *b == "mdat").count(), 5);
        assert_eq!(boxes.last().unwrap(), "mfra");
        // 全ボックスでファイル全体をちょうど覆う。
        let total: usize = children(&buf).map(|(_, p)| p.len() + 8).sum();
        assert_eq!(total, buf.len());
    }

    #[test]
    fn finished_file_is_intact_for_repair() {
        let mut buf = build(10, true);
        let len = buf.len() as u64;
        let r = repair(&mut Cursor::new(&mut buf), len).unwrap();
        assert_eq!(r.fragments, 5);
        assert_eq!(r.truncated_bytes, 0);
        assert!(!r.duration_patched);
        // 音声は 1024 サンプル単位なので 10 秒をわずかに超える。
        assert!(
            (10_000..10_030).contains(&r.duration_ms),
            "{}",
            r.duration_ms
        );
    }

    #[test]
    fn provisional_duration_is_patched() {
        // フラグメント境界をまたぐ映像サンプルの長さが、次のサンプル到着で確定値になる。
        let mut w = FragWriter::new(Cursor::new(Vec::new()), vec![video_spec()]).unwrap();
        w.write_timed(0, 0, true, &[1]).unwrap();
        w.flush(500).unwrap(); // 暫定 0.5 秒
        let mut snapshot = w.get_mut().get_ref().clone();
        let len = snapshot.len() as u64;
        assert_eq!(
            repair(&mut Cursor::new(&mut snapshot), len)
                .unwrap()
                .duration_ms,
            500
        );
        w.write_timed(0, 3 * 90_000, false, &[2]).unwrap(); // 実際は 3 秒後に次のフレーム
        let (c, fin) = w.finish(4000).unwrap();
        assert_eq!(fin.duration_ms, 4000);
        let mut buf = c.into_inner();
        let len = buf.len() as u64;
        let mut ends = Vec::new();
        for (t, p) in children(&buf) {
            if t == b"moof" {
                moof_track_ends(p, &mut ends);
            }
        }
        // 1 つ目のフラグメントは 0→3 秒、2 つ目は 3→4 秒。
        assert_eq!(ends, vec![(1, 3 * 90_000), (1, 4 * 90_000)]);
        assert!(
            !repair(&mut Cursor::new(&mut buf), len)
                .unwrap()
                .duration_patched
        );
    }

    #[test]
    fn non_monotonic_pts_is_fixed() {
        let mut w = FragWriter::new(Cursor::new(Vec::new()), vec![video_spec()]).unwrap();
        w.write_timed(0, 9000, true, &[1]).unwrap();
        w.write_timed(0, 9000, false, &[2]).unwrap();
        w.write_timed(0, 100, false, &[3]).unwrap();
        let (_, fin) = w.finish(1000).unwrap();
        assert_eq!(fin.duration_ms, 1000);
    }

    #[test]
    fn empty_flush_writes_nothing() {
        let mut w = FragWriter::new(Cursor::new(Vec::new()), vec![audio_spec(true)]).unwrap();
        let before = w.bytes_written();
        w.flush(1000).unwrap();
        assert_eq!(w.bytes_written(), before);
        let (c, fin) = w.finish(1000).unwrap();
        assert_eq!(fin.fragments, 0);
        let mut buf = c.into_inner();
        let len = buf.len() as u64;
        assert_eq!(
            repair(&mut Cursor::new(&mut buf), len).unwrap().fragments,
            0
        );
    }

    #[test]
    fn crash_at_any_point_is_repairable() {
        let full = build(10, false); // finish していない＝強制終了相当
        let header_len: usize = children(&full).take(2).map(|(_, p)| p.len() + 8).sum();
        let mut last_fragments = 0;
        // 1 バイト刻みは遅いので素数刻みで全域を走査する。
        for cut in (header_len..=full.len()).step_by(97).chain([full.len()]) {
            let mut buf = full[..cut].to_vec();
            let r = repair(&mut Cursor::new(&mut buf), cut as u64).unwrap();
            buf.truncate(r.valid_len as usize);
            assert_eq!(r.truncated_bytes, (cut as u64) - r.valid_len);
            let boxes = top_boxes(&buf);
            assert_eq!(
                boxes.iter().filter(|b| *b == "moof").count() as u32,
                r.fragments
            );
            assert_eq!(boxes.len() as u32, 2 + r.fragments * 2);
            assert!(r.fragments >= last_fragments);
            last_fragments = r.fragments;
            assert!(r.duration_ms >= r.fragments as u64 * 2000);
            if r.fragments > 0 {
                assert!(r.duration_patched);
                // 2 回目は何もしない（冪等）。
                let len = buf.len() as u64;
                let again = repair(&mut Cursor::new(&mut buf), len).unwrap();
                assert_eq!(again.truncated_bytes, 0);
                assert!(!again.duration_patched);
            }
        }
        assert_eq!(last_fragments, 5);
    }

    #[test]
    fn truncated_header_is_an_error() {
        let full = build(2, true);
        assert!(repair(&mut Cursor::new(full[..40].to_vec()), 40).is_err());
        assert!(repair(&mut Cursor::new(b"not an mp4 file at all".to_vec()), 22).is_err());
    }

    #[test]
    fn hostile_box_sizes_do_not_overflow() {
        let good = build(4, true);
        let good_len = good.len() as u64;
        // 末尾に「長さ u64::MAX」のボックス：位置との加算があふれても、切り落とすだけで済む。
        let mut buf = good.clone();
        buf.extend_from_slice(&1u32.to_be_bytes());
        buf.extend_from_slice(b"free");
        buf.extend_from_slice(&u64::MAX.to_be_bytes());
        let len = buf.len() as u64;
        let r = repair(&mut Cursor::new(&mut buf), len).unwrap();
        assert_eq!(r.valid_len, good_len);
        assert_eq!(r.fragments, 2);

        // trun のサンプル数を偽っても、実際に入っているぶんしか読まない（長時間回らない）。
        let mut buf = good;
        let trun = buf.windows(4).position(|w| w == b"trun").unwrap();
        buf[trun + 8..trun + 12].copy_from_slice(&u32::MAX.to_be_bytes());
        let r = repair(&mut Cursor::new(&mut buf), good_len).unwrap();
        assert_eq!(r.fragments, 2);
    }

    #[test]
    fn disabled_track_flag() {
        let w = FragWriter::new(
            Cursor::new(Vec::new()),
            vec![audio_spec(true), audio_spec(false)],
        )
        .unwrap();
        let (c, _) = w.finish(0).unwrap();
        let buf = c.into_inner();
        let moov = children(&buf).find(|(t, _)| *t == b"moov").unwrap().1;
        let flags: Vec<u32> = children(moov)
            .filter(|(t, _)| *t == b"trak")
            .map(|(_, trak)| {
                be32(children(trak).find(|(t, _)| *t == b"tkhd").unwrap().1, 0).unwrap()
            })
            .collect();
        assert_eq!(flags, vec![7, 6]);
    }
}
