//! MP3 帧头时长解析（纯 Rust，零依赖）
//!
//! 解析 MP3 字节流，读取真实采样率 / 比特率计算音频时长（毫秒），
//! 用于插件端精确计算"同步音频触发冷却时间"（cooldown = duration + 600）。
//!
//! 支持：
//! - 跳过 ID3v2 标签（含 v2.4 Footer）
//! - MPEG1 / MPEG2 / MPEG2.5，Layer III
//! - CBR（按帧长推算总帧数）与 VBR（读 Xing / Info 头帧数）
//!
//! 任何解析失败或非 MP3 输入，均回退到 128kbps 字节估算，
//! 保证返回值非 0 且位于 [300, 600_000] 毫秒。

/// 解析 MP3 字节，返回音频时长（毫秒）。
/// 失败 / 非 MP3 时回退 128kbps 估算，永不返回 0。
pub fn parse_duration_ms(bytes: &[u8]) -> u32 {
    match try_parse_duration_ms(bytes) {
        Some(ms) if ms > 0 => ms.clamp(300, 600_000),
        _ => fallback_duration_ms(bytes),
    }
}

/// 128kbps 字节估算兜底（与旧逻辑一致）
fn fallback_duration_ms(bytes: &[u8]) -> u32 {
    if bytes.is_empty() {
        return 1500;
    }
    // 128 bit/ms → 总比特数 / 128 = 毫秒
    let est = (bytes.len() as u64 * 8) / 128;
    est.clamp(300, 600_000) as u32
}

/// 解码后的帧头信息
struct Header {
    sample_rate: u32,       // Hz
    frame_len: u32,         // 每帧字节数（含 padding）
    samples_per_frame: u32, // 每帧采样数
}

fn try_parse_duration_ms(bytes: &[u8]) -> Option<u32> {
    let start = skip_id3v2(bytes)?;
    let data = &bytes[start..];
    let (pos, h) = find_frame_header(data)?;
    let abs_pos = start + pos;
    let header = decode_header(h)?;

    // VBR / 带头部信息的 CBR：优先读 Xing / Info 头的总帧数
    if let Some(frames) = read_xing_frames(data, pos, h) {
        return duration_from_frames(frames, &header);
    }

    // CBR：有效音频字节 / 帧长 × 每帧时长
    let audio_bytes = bytes.len().saturating_sub(abs_pos);
    if audio_bytes == 0 {
        return None;
    }
    let frame_count = audio_bytes / header.frame_len as usize;
    if frame_count == 0 {
        return None;
    }
    duration_from_frames(frame_count as u64, &header)
}

fn duration_from_frames(frames: u64, h: &Header) -> Option<u32> {
    let ms = frames
        .checked_mul(h.samples_per_frame as u64)?
        .checked_mul(1000)?
        .checked_div(h.sample_rate as u64)?;
    if ms == 0 {
        return None;
    }
    Some(ms as u32)
}

/// 跳过 ID3v2 标签，返回有效音频数据起始偏移；无 ID3 标签则返回 0。
fn skip_id3v2(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < 10 || &bytes[0..3] != b"ID3" {
        return Some(0);
    }
    let size = ((bytes[6] as usize & 0x7f) << 21)
        | ((bytes[7] as usize & 0x7f) << 14)
        | ((bytes[8] as usize & 0x7f) << 7)
        | (bytes[9] as usize & 0x7f);
    let mut total = 10usize.saturating_add(size);
    // ID3v2.4 可选 Footer（10 字节）
    if (bytes[5] & 0x10) != 0 {
        total = total.saturating_add(10);
    }
    Some(total)
}

/// 定位第一帧帧头（0xFFEx），返回 (偏移, 4 字节帧头)。
fn find_frame_header(data: &[u8]) -> Option<(usize, [u8; 4])> {
    let mut i = 0usize;
    while i + 4 <= data.len() {
        if data[i] == 0xFF && (data[i + 1] & 0xE0) == 0xE0 {
            let h = [data[i], data[i + 1], data[i + 2], data[i + 3]];
            // 过滤保留值：版本 != 1（0b01 reserved），层 == 1（Layer III）
            let version = (h[1] >> 3) & 3;
            let layer = (h[1] >> 1) & 3;
            if version != 1 && layer == 1 {
                return Some((i, h));
            }
        }
        i += 1;
    }
    None
}

/// 解码帧头，得到比特率 / 采样率 / 帧长 / 每帧采样数。
fn decode_header(h: [u8; 4]) -> Option<Header> {
    let version = (h[1] >> 3) & 3; // 3=MPEG1, 2=MPEG2, 0=MPEG2.5, 1=reserved
    let layer = (h[1] >> 1) & 3; // 1=Layer III, 2=Layer II, 3=Layer I, 0=reserved
    let bitrate_idx = (h[2] >> 4) as usize & 0x0F;
    let sr_idx = ((h[2] >> 2) & 3) as usize;
    let padding = ((h[2] >> 1) & 1) as u32;

    // 非 Layer III 或保留值：交给兜底估算
    if version == 1 || layer != 1 || bitrate_idx == 0 || bitrate_idx == 15 || sr_idx == 3 {
        return None;
    }

    let bitrate: u32 = if version == 3 {
        // MPEG1 Layer III 比特率表（kbps）
        [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320][bitrate_idx]
    } else {
        // MPEG2 / MPEG2.5 Layer III 比特率表（kbps）
        [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160][bitrate_idx]
    };
    let sample_rate: u32 = match (version, sr_idx) {
        (3, 0) => 44100,
        (3, 1) => 48000,
        (3, 2) => 32000,
        (2, 0) => 22050,
        (2, 1) => 24000,
        (2, 2) => 16000,
        (0, 0) => 11025,
        (0, 1) => 12000,
        (0, 2) => 8000,
        _ => return None,
    };
    let samples_per_frame = if version == 3 { 1152 } else { 576 };

    // Layer III 帧长（字节）= 144 * bitrate(kbps) * 1000 / sample_rate + padding
    let frame_len = 144u32
        .checked_mul(bitrate)?
        .checked_mul(1000)?
        .checked_div(sample_rate)?
        .checked_add(padding)?;
    if frame_len == 0 {
        return None;
    }

    Some(Header {
        sample_rate,
        frame_len,
        samples_per_frame,
    })
}

/// 读取 Xing / Info 头中的总帧数（VBR 或带头部信息的 CBR）。
fn read_xing_frames(data: &[u8], frame_pos: usize, h: [u8; 4]) -> Option<u64> {
    let version = (h[1] >> 3) & 3;
    let has_crc = (h[1] & 1) == 0; // protection bit 为 0 表示有 CRC
    // Xing 标记位于第一个帧头之后：帧头(4) + [CRC(2)] + side_info
    let side = if version == 3 { 32usize } else { 17usize };
    let base = frame_pos + 4 + if has_crc { 2 } else { 0 };
    let c = base + side;
    if c + 16 > data.len() {
        return None;
    }
    let tag = &data[c..c + 4];
    if tag != b"Xing" && tag != b"Info" {
        return None;
    }
    let flags = u32::from_be_bytes([data[c + 4], data[c + 5], data[c + 6], data[c + 7]]);
    if (flags & 0x1) == 0 || c + 12 > data.len() {
        return None;
    }
    let frames =
        u32::from_be_bytes([data[c + 8], data[c + 9], data[c + 10], data[c + 11]]);
    if frames == 0 {
        return None;
    }
    Some(frames as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_works_on_empty() {
        assert_eq!(parse_duration_ms(&[]), 1500);
    }

    #[test]
    fn fallback_on_garbage() {
        // 无帧头 → 回退估算
        let garbage = vec![0u8; 4096];
        let ms = parse_duration_ms(&garbage);
        assert!(ms >= 300);
        assert!(ms <= 600_000);
    }

    #[test]
    fn cbr_128kbps_44k() {
        // 手工构造 CBR 流：MPEG1 Layer III, 128kbps, 44100Hz, padding 0
        // 帧头：FF FB 90 00，每帧 417 字节
        let mut bytes = Vec::new();
        // ID3v2 头（10 字节，size=0）
        bytes.extend_from_slice(b"ID3\x03\x00\x00\x00\x00\x00\x00");
        // 100 帧数据：1 帧头 + 填充到 10 + 417*100 字节
        bytes.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
        bytes.resize(10 + 417 * 100, 0u8);
        // 100 帧 × 1152 samples / 44100Hz ≈ 2612ms
        let ms = parse_duration_ms(&bytes);
        assert!(ms >= 2400 && ms <= 2800, "ms={}", ms);
    }

    #[test]
    fn mp3_2_22050_cbr() {
        // MPEG2 Layer III, 32kbps, 22050Hz：
        // version=10(MPEG2), layer=01(Layer III), protection=1 → byte1 = 0xF3
        // bitrate_idx=4(32kbps), sr_idx=0(22050Hz) → byte2 = 0x40
        let h = [0xFFu8, 0xF3u8, 0x40u8, 0x00u8];
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&h);
        // 帧长 = 144*32*1000/22050 = 208.9 → 208 字节，100 帧
        bytes.resize(4 + 208 * 100, 0u8);
        // 100 帧 × 576 采样 / 22050Hz ≈ 2612ms
        let ms = parse_duration_ms(&bytes);
        assert!(ms >= 2400 && ms <= 2800, "ms={}", ms);
    }
}
