//! H.264 ビットストリームの最小限の操作（Annex B → AVCC 変換、avcC 生成）。

/// Annex B（00 00 01 / 00 00 00 01 区切り）を NAL 単位に分ける。
pub fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0;
    let mut start: Option<usize> = None;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if let Some(s) = start {
                out.push(trim_zeros(&data[s..i]));
            }
            start = Some(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    if let Some(s) = start {
        out.push(trim_zeros(&data[s..]));
    }
    out.retain(|n| !n.is_empty());
    out
}

fn trim_zeros(mut nal: &[u8]) -> &[u8] {
    while let [rest @ .., 0] = nal {
        nal = rest;
    }
    nal
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AccessUnit {
    /// 4 バイト長プレフィックス形式（SPS/PPS/AUD/フィラーは除去済み）。
    pub avcc: Vec<u8>,
    pub key: bool,
    pub sps: Option<Vec<u8>>,
    pub pps: Option<Vec<u8>>,
}

/// エンコーダ出力（Annex B の 1 アクセスユニット）を MP4 用に変換する。
pub fn annexb_to_avcc(data: &[u8]) -> AccessUnit {
    let mut au = AccessUnit::default();
    for nal in nal_units(data) {
        match nal[0] & 0x1f {
            7 => au.sps = Some(nal.to_vec()),
            8 => au.pps = Some(nal.to_vec()),
            9 | 12 => {}
            t => {
                if t == 5 {
                    au.key = true;
                }
                au.avcc.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                au.avcc.extend_from_slice(nal);
            }
        }
    }
    au
}

/// AVCDecoderConfigurationRecord（avcC の中身）。
pub fn avcc_config(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let profile = sps.get(1).copied().unwrap_or(66);
    let mut v = vec![
        1,
        profile,
        sps.get(2).copied().unwrap_or(0),
        sps.get(3).copied().unwrap_or(31),
        0xff, // NAL 長 4 バイト
        0xe1, // SPS 1 個
    ];
    v.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    v.extend_from_slice(sps);
    v.push(1);
    v.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    v.extend_from_slice(pps);
    if matches!(profile, 100 | 110 | 122 | 144) {
        // High 系は chroma_format / bit depth を明記する（4:2:0 8bit 固定）。
        v.extend_from_slice(&[0xfc | 1, 0xf8, 0xf8, 0]);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_mixed_start_codes() {
        let data = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 9, 0, 0,
        ];
        let n = nal_units(&data);
        assert_eq!(
            n,
            vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 9, 9][..]]
        );
        assert!(nal_units(&[1, 2, 3]).is_empty());
        assert!(nal_units(&[]).is_empty());
    }

    #[test]
    fn converts_keyframe() {
        let data = [
            0, 0, 0, 1, 0x09, 0xf0, 0, 0, 0, 1, 0x67, 100, 0, 31, 0, 0, 1, 0x68, 1, 0, 0, 1, 0x65,
            7, 7,
        ];
        let au = annexb_to_avcc(&data);
        assert!(au.key);
        assert_eq!(au.sps.as_deref(), Some(&[0x67, 100, 0, 31][..]));
        assert_eq!(au.pps.as_deref(), Some(&[0x68, 1][..]));
        assert_eq!(au.avcc, vec![0, 0, 0, 3, 0x65, 7, 7]);
        let p = annexb_to_avcc(&[0, 0, 1, 0x41, 5]);
        assert!(!p.key && p.sps.is_none());
        assert_eq!(p.avcc, vec![0, 0, 0, 2, 0x41, 5]);
    }

    #[test]
    fn config_record() {
        let c = avcc_config(&[0x67, 100, 0, 31, 0xaa], &[0x68, 0xbb]);
        assert_eq!(&c[..6], &[1, 100, 0, 31, 0xff, 0xe1]);
        assert_eq!(&c[6..8], &[0, 5]);
        assert_eq!(c.len(), 6 + 2 + 5 + 1 + 2 + 2 + 4);
        let base = avcc_config(&[0x67, 66, 0, 30], &[0x68]);
        assert_eq!(base.len(), 6 + 2 + 4 + 1 + 2 + 1);
    }
}
