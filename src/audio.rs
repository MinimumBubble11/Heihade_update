//! 同步前音频优化（纯 Rust，无外部依赖、无网络）
//!
//! 点「同步到手表」时自动对每个待同步音频执行「去静音 → 64kbps 压缩」，并保证不劣化：
//!   - **原文件码率已 ≤ 64kbps → 完全跳过**（实测有 30% 的音效原本就是 32kbps，
//!     硬压到 64k 体积会翻倍，且去静音收益也是 0%）
//!   - **处理后体积 ≥ 原体积 → 保留原文件**
//!   - 原文件超过体积上限 → 跳过（解码后 PCM 约为输入的 10~40 倍，受 128MiB 内存约束）
//!
//! 为什么可行：`symphonia`（纯 Rust 解码，支持 mp3/aac/m4a/flac/wav/ogg-vorbis）
//! 与 `shine-rs`（纯 Rust MP3 编码，固定码率 8–320kbps）都能编译到 wasm32-wasip2。
//! 视频转音频没有可用的纯 Rust 视频解码器，仍走第三方在线工具。
//!
//! 执行方式：宿主 timer 分阶段驱动，逐个文件推进，避免长时间阻塞 WASM 单线程：
//!   next → decode → encode* → finish → next …（全部处理完）→ on_done()

use std::cell::RefCell;
use std::io::{Read, Seek, SeekFrom};
use std::sync::Mutex;

use astrobox_ng_wit::astrobox::psys_host_v4::timer;
use shine_rs::{Mp3Encoder, Mp3EncoderConfig, StereoMode};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::state;

/// timer payload 前缀（lib.rs 据此分发）
pub const AUDIO_TIMER_PREFIX: &str = "heihade-audio:";
/// 目标码率；已是 MP3 且不高于此值就完全不动
const TARGET_KBPS: f64 = 64.0;
/// 非 MP3 转码时的码率下限（低于它会向 32k 抬升，避免音质过度损失）
const MIN_KBPS: f64 = 32.0;
/// 每次 timer 编码的最大交错样本数（约 0.19s @44.1k 立体声），控制单次阻塞时长
const SAMPLES_PER_TICK: usize = 16_384;
/// 单个文件体积上限（解码后 PCM 约为输入的 10~40 倍）
const MAX_INPUT_BYTES: usize = 16 * 1024 * 1024;
/// 静音判定阈值（i16 振幅绝对值），约 -45 dBFS
const SILENCE_THRESHOLD: i32 = 1000;
/// 静音段最短时长（毫秒）：短于此值的静音不裁
const SILENCE_MIN_MS: u32 = 300;
/// 裁掉静音时两端各保留的余量（毫秒）
const SILENCE_KEEP_MS: u32 = 80;
/// 静音分析窗口（毫秒）
const WIN_MS: u32 = 10;

/// 内存数据源（WASM 沙箱没有文件系统，symphonia 需要自己提供 MediaSource）
struct MemSource {
    data: Vec<u8>,
    pos: u64,
}

impl Read for MemSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let p = self.pos as usize;
        if p >= self.data.len() {
            return Ok(0);
        }
        let n = buf.len().min(self.data.len() - p);
        buf[..n].copy_from_slice(&self.data[p..p + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for MemSource {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let np = match pos {
            SeekFrom::Start(n) => n as i64,
            SeekFrom::Current(n) => self.pos as i64 + n,
            SeekFrom::End(n) => self.data.len() as i64 + n,
        };
        self.pos = np.max(0) as u64;
        Ok(self.pos)
    }
}

impl MediaSource for MemSource {
    fn is_seekable(&self) -> bool {
        true
    }
    fn byte_len(&self) -> Option<u64> {
        Some(self.data.len() as u64)
    }
}

struct Job {
    /// 与 state.process_gen 一致，用于忽略过期阶段回调
    gen: u64,
    /// 全部处理完后调用的回调（继续同步流程）
    on_done: fn(),
    /// 当前处理的 pending_files 下标
    index: usize,
    /// 待处理文件总数（仅用于进度显示）
    total_files: usize,
    /// 已完成压缩的文件数
    done: usize,
    /// 因处理后更大而保留原文件的个数
    reverted: usize,
    /// 因超限/异常而跳过但未中断流程的个数
    skipped: usize,
    /// 当前文件名
    name: String,
    /// 当前文件原始体积
    src_len: usize,
    /// 当前文件的编码目标码率（kbps）
    target_kbps: u32,
    /// 当前文件非 MP3（必须转码，即使体积变大也要统一格式）
    force_convert: bool,
    /// 原始输入字节（decode 阶段取出后置空）
    data: Vec<u8>,
    /// 解码后的交错 PCM
    pcm: Vec<i16>,
    sample_rate: u32,
    channels: u16,
    /// 已产出的 MP3 字节
    out: Vec<u8>,
    /// 编码游标（交错样本下标）
    cursor: usize,
}

static JOB: Mutex<Option<Job>> = Mutex::new(None);

thread_local! {
    /// shine 的编码器内部持有裸指针（`*mut i16` / `*mut i32`），不是 `Send`，
    /// 无法放入 static Mutex。WASM 是单线程且编码器只在当前线程使用，
    /// 放线程局部即可，比 `unsafe impl Send` 更诚实。
    static ENC: RefCell<Option<Mp3Encoder>> = const { RefCell::new(None) };
}

fn arm(stage: &str, gen: u64) {
    let payload = format!("{}{}:{}", AUDIO_TIMER_PREFIX, stage, gen);
    let _ = timer::set_timeout(40, &payload);
}

fn is_current(gen: u64) -> bool {
    JOB.lock().unwrap().as_ref().map(|j| j.gen) == Some(gen)
}

/// 取消同步前优化（不继续同步）
pub fn cancel() {
    let gen = {
        let mut slot = JOB.lock().unwrap();
        match slot.take() {
            Some(j) => j.gen,
            None => return,
        }
    };
    ENC.with(|c| *c.borrow_mut() = None);
    state::finish_processing(gen);
    state::set_notice("已取消音频优化".to_string());
    crate::ui::rerender();
}

/// 码率（kbps）。时长未知时按体积粗略判断。
fn bitrate_kbps(bytes: usize, duration_ms: u32) -> f64 {
    if duration_ms == 0 {
        return if bytes > 512 * 1024 { 999.0 } else { 0.0 };
    }
    bytes as f64 * 8.0 / duration_ms as f64
}

/// 取文件名主干（去掉扩展名）
fn file_stem(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(s, _)| s.to_string())
        .unwrap_or_else(|| name.to_string())
}

/// 文件名是否为 mp3
fn is_mp3_name(name: &str) -> bool {
    name.rsplit('.')
        .next()
        .map(|e| e.eq_ignore_ascii_case("mp3"))
        .unwrap_or(false)
}

/// 该文件是否需要处理：
///   - 非 MP3 → **一律转码**（手表端 MP3 兼容性最好，格式必须统一）
///   - 已是 MP3 → 仅当码率高于目标值才压缩
///   - 体积超限 → 跳过（解码后 PCM 会撑爆 128MiB 内存）
fn needs_process(name: &str, bytes: usize, duration_ms: u32) -> bool {
    if bytes > MAX_INPUT_BYTES {
        return false;
    }
    if !is_mp3_name(name) {
        return true;
    }
    bitrate_kbps(bytes, duration_ms) > TARGET_KBPS
}

/// 目标码率：不超过 64k；非 MP3 时不低于原码率（避免把 32k 的拉到 64k 而体积翻倍）
fn target_kbps_for(name: &str, bytes: usize, duration_ms: u32) -> u32 {
    if is_mp3_name(name) {
        TARGET_KBPS as u32
    } else {
        let orig = bitrate_kbps(bytes, duration_ms);
        TARGET_KBPS.min(orig.max(MIN_KBPS)).round().clamp(8.0, 320.0) as u32
    }
}

/// 是否还有值得处理的音频
pub fn has_optimizable() -> bool {
    let st = state::lock();
    st.pending_files
        .iter()
        .any(|f| needs_process(&f.name, f.bytes.len(), f.duration))
}

/// 开始同步前优化；全部处理完后调用 `on_done`（继续同步流程）
pub fn start(on_done: fn()) {
    if state::is_processing() {
        state::set_notice("已有任务正在进行，请等待完成或取消".to_string());
        return;
    }
    let total_files = {
        let st = state::lock();
        st.pending_files.len()
    };
    let gen = state::start_processing("audio", "同步前优化");
    *JOB.lock().unwrap() = Some(Job {
        gen,
        on_done,
        index: 0,
        total_files,
        done: 0,
        reverted: 0,
        skipped: 0,
        name: String::new(),
        src_len: 0,
        target_kbps: TARGET_KBPS as u32,
        force_convert: false,
        data: Vec::new(),
        pcm: Vec::new(),
        sample_rate: 44_100,
        channels: 2,
        out: Vec::new(),
        cursor: 0,
    });
    state::update_processing(gen, 2, "正在优化音频…");
    crate::ui::rerender();
    arm("next", gen);
}

/// 定时器分发（由 lib.rs 调用）
pub fn on_timer(payload: &str) {
    let parsed: serde_json::Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(_) => return,
    };
    let inner = parsed.get("payload").and_then(|v| v.as_str()).unwrap_or("");
    let body = inner.strip_prefix(AUDIO_TIMER_PREFIX).unwrap_or("");
    let (stage, gen) = match body.rsplit_once(':') {
        Some((s, g)) => (s, g.parse::<u64>().ok()),
        None => (body, None),
    };
    let Some(gen) = gen else { return };
    if !is_current(gen) {
        return;
    }
    match stage {
        "next" => step_next(gen),
        "decode" => step_decode(gen),
        "encode" => step_encode(gen),
        "finish" => step_finish(gen),
        _ => {}
    }
}

/// 阶段 1：寻找下一个需要处理的文件（跳过码率已足够低的、跳过超限的）
fn step_next(gen: u64) {
    let start_idx = {
        let slot = JOB.lock().unwrap();
        let Some(j) = slot.as_ref() else { return };
        j.index
    };

    let mut idx = start_idx;
    let (name, bytes, duration, total_files) = {
        let st = state::lock();
        let total = st.pending_files.len();
        while idx < total {
            let f = &st.pending_files[idx];
            if needs_process(&f.name, f.bytes.len(), f.duration) {
                break;
            }
            idx += 1;
        }
        if idx >= total {
            (String::new(), Vec::new(), 0u32, total)
        } else {
            let f = &st.pending_files[idx];
            (f.name.clone(), f.bytes.clone(), f.duration, total)
        }
    };

    if bytes.is_empty() {
        finish_all(gen);
        return;
    }

    let force_convert = !is_mp3_name(&name);
    let target_kbps = target_kbps_for(&name, bytes.len(), duration);
    let (done, skipped) = {
        let mut slot = JOB.lock().unwrap();
        let Some(j) = slot.as_mut() else { return };
        // 记录被跳过的中间文件数（idx 从 start_idx 跳到这里）
        j.skipped += idx.saturating_sub(j.index);
        j.index = idx;
        j.total_files = total_files.max(1);
        j.name = name.clone();
        j.src_len = bytes.len();
        j.target_kbps = target_kbps;
        j.force_convert = force_convert;
        j.data = bytes;
        j.pcm.clear();
        j.out.clear();
        j.cursor = 0;
        (j.done, j.skipped)
    };

    let pct = ((done + skipped) as f64 / total_files.max(1) as f64 * 100.0) as u8;
    let action = if force_convert { "转码" } else { "压缩" };
    state::update_processing(
        gen,
        pct.min(95),
        &format!(
            "{}音频 {}/{}：{}",
            action,
            idx + 1,
            total_files,
            truncate_name(&name)
        ),
    );
    crate::ui::rerender();
    arm("decode", gen);
}

fn truncate_name(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    if chars.len() <= 18 {
        return name.to_string();
    }
    format!("{}…", chars[..18].iter().collect::<String>())
}

/// 阶段 2：整段解码为交错 PCM
/// （symphonia 的 packet 解码需连续持有解码器状态，故一次性完成；
///   实测 180 秒音频解码约 700ms，WASM 下也在可接受范围。）
fn step_decode(gen: u64) {
    let (name, data) = {
        let mut slot = JOB.lock().unwrap();
        let Some(j) = slot.as_mut() else { return };
        (j.name.clone(), std::mem::take(&mut j.data))
    };
    if data.is_empty() {
        // 数据异常：跳过该文件，继续下一个
        bump_and_next(gen, SkipKind::Skipped);
        return;
    }

    let mss = MediaSourceStream::new(Box::new(MemSource { data, pos: 0 }), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = std::path::Path::new(&name).extension() {
        hint.with_extension(&ext.to_string_lossy().to_lowercase());
    }

    let probed = match symphonia::default::get_probe().format(
        &hint,
        mss,
        &FormatOptions::default(),
        &MetadataOptions::default(),
    ) {
        Ok(p) => p,
        Err(_) => {
            tracing::info!("audio 跳过（无法识别格式）: {name}");
            bump_and_next(gen, SkipKind::Skipped);
            return;
        }
    };
    let mut format = probed.format;
    let Some(track) = format.default_track().cloned() else {
        bump_and_next(gen, SkipKind::Skipped);
        return;
    };
    let params = track.codec_params.clone();
    let sample_rate = params.sample_rate.unwrap_or(44_100);
    let channels = params.channels.map(|c| c.count()).unwrap_or(2).clamp(1, 2) as u16;

    let mut decoder = match symphonia::default::get_codecs()
        .make(&params, &DecoderOptions::default())
    {
        Ok(d) => d,
        Err(_) => {
            tracing::info!("audio 跳过（不支持的编码）: {name}");
            bump_and_next(gen, SkipKind::Skipped);
            return;
        }
    };

    let mut pcm: Vec<i16> = Vec::new();
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(_) => break,
        };
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let mut sb = SampleBuffer::<i16>::new(decoded.capacity() as u64, *decoded.spec());
        sb.copy_interleaved_ref(decoded);
        pcm.extend_from_slice(sb.samples());
    }

    if pcm.is_empty() {
        bump_and_next(gen, SkipKind::Skipped);
        return;
    }

    let orig_ms = (pcm.len() as u64 / (sample_rate as u64 * channels as u64)) * 1000;

    // 去静音
    let before = pcm.len();
    pcm = trim_silence(&pcm, sample_rate, channels);
    let trimmed = before.saturating_sub(pcm.len());
    if pcm.is_empty() {
        bump_and_next(gen, SkipKind::Skipped);
        return;
    }

    // 建立编码器（码率按文件而定：MP3 用 64k，非 MP3 不高抬原码率）
    let target_kbps = {
        let slot = JOB.lock().unwrap();
        slot.as_ref().map(|j| j.target_kbps).unwrap_or(TARGET_KBPS as u32)
    };
    let stereo = if channels >= 2 {
        StereoMode::Stereo
    } else {
        StereoMode::Mono
    };
    let config = Mp3EncoderConfig::new()
        .sample_rate(sample_rate)
        .bitrate(target_kbps)
        .channels(channels as u8)
        .stereo_mode(stereo);
    let enc = match Mp3Encoder::new(config) {
        Ok(e) => e,
        Err(_) => {
            tracing::info!("audio 跳过（不支持采样率 {sample_rate}Hz）: {name}");
            bump_and_next(gen, SkipKind::Skipped);
            return;
        }
    };
    ENC.with(|c| *c.borrow_mut() = Some(enc));

    tracing::info!(
        "audio 解码 {}: {}Hz {}ch {}ms -> 去静音后 {}ms（裁 {}ms）",
        name,
        sample_rate,
        channels,
        orig_ms,
        (pcm.len() as u64 / (sample_rate as u64 * channels as u64)) * 1000,
        (trimmed as u64 / (sample_rate as u64 * channels as u64)) * 1000
    );

    {
        let mut slot = JOB.lock().unwrap();
        let Some(j) = slot.as_mut() else { return };
        j.pcm = pcm;
        j.sample_rate = sample_rate;
        j.channels = channels;
        j.cursor = 0;
    }
    arm("encode", gen);
}

/// 跳过当前文件并前进到下一个（不打断整个流程）
enum SkipKind {
    Skipped,
}

fn bump_and_next(gen: u64, _kind: SkipKind) {
    let (done, skipped, total_files) = {
        let mut slot = JOB.lock().unwrap();
        let Some(j) = slot.as_mut() else { return };
        j.skipped += 1;
        j.index += 1;
        j.pcm.clear();
        j.out.clear();
        j.data.clear();
        (j.done, j.skipped, j.total_files)
    };
    ENC.with(|c| *c.borrow_mut() = None);
    let pct = ((done + skipped) as f64 / total_files.max(1) as f64 * 100.0) as u8;
    state::update_processing(gen, pct.min(95), "正在优化音频…");
    crate::ui::rerender();
    arm("next", gen);
}

/// 阶段 3：分块编码（每 tick 一批，让出事件循环以便刷新进度）
fn step_encode(gen: u64) {
    // 1) 取出本 tick 要编码的片段（不跨锁调用编码器）
    let (chunk, next_cursor, total, done, skipped, total_files) = {
        let mut slot = JOB.lock().unwrap();
        let Some(j) = slot.as_mut() else { return };
        let total = j.pcm.len();
        let end = (j.cursor + SAMPLES_PER_TICK).min(total);
        let chunk = if end > j.cursor {
            j.pcm[j.cursor..end].to_vec()
        } else {
            Vec::new()
        };
        (chunk, end, total, j.done, j.skipped, j.total_files)
    };

    // 2) 编码（编码器在 thread_local，非 Send）
    let frames = ENC.with(|c| {
        let mut b = c.borrow_mut();
        match b.as_mut() {
            Some(enc) => enc.encode_interleaved(&chunk).ok(),
            None => None,
        }
    });

    // 3) 写回结果与进度
    let finished: bool;
    {
        let mut slot = JOB.lock().unwrap();
        let Some(j) = slot.as_mut() else { return };
        match frames {
            Some(fs) => {
                for f in fs {
                    j.out.extend_from_slice(&f);
                }
            }
            None => {
                drop(slot);
                bump_and_next(gen, SkipKind::Skipped);
                return;
            }
        }
        j.cursor = next_cursor;
        finished = j.cursor >= total;
    }

    let inner = if total == 0 {
        1.0
    } else {
        next_cursor as f64 / total as f64
    };
    let pct = ((done + skipped) as f64 + inner) / total_files.max(1) as f64 * 100.0;
    state::update_processing(gen, (pct as u8).min(98), "正在压缩为 64kbps…");
    crate::ui::rerender();

    if finished {
        arm("finish", gen);
    } else {
        arm("encode", gen);
    }
}

/// 阶段 4：收尾、与原始体积比较、回写 pending_files
fn step_finish(gen: u64) {
    // 编码器在 thread_local（非 Send），take 出来 flush 并释放
    let tail = ENC.with(|c| {
        let mut b = c.borrow_mut();
        match b.take() {
            Some(mut e) => e.finish().unwrap_or_default(),
            None => Vec::new(),
        }
    });

    let (idx, name, mut out, src_len, sr, ch, pcm_len, done, reverted, skipped, total_files, force_convert, target_kbps) = {
        let mut slot = JOB.lock().unwrap();
        let Some(j) = slot.as_mut() else { return };
        j.out.extend_from_slice(&tail);
        (
            j.index,
            j.name.clone(),
            std::mem::take(&mut j.out),
            j.src_len,
            j.sample_rate,
            j.channels,
            j.pcm.len(),
            j.done,
            j.reverted,
            j.skipped,
            j.total_files,
            j.force_convert,
            j.target_kbps,
        )
    };

    // 体积保护：
    //   - 已是 MP3 且处理后没变小 → 保留原文件
    //   - 非 MP3 → 必须统一成 MP3，即使略大也采用（格式统一优先）
    let use_new = if force_convert {
        !out.is_empty()
    } else {
        !out.is_empty() && out.len() < src_len
    };
    if force_convert {
        tracing::info!(
            "audio 转码 {name} -> MP3 {target_kbps}k: {} -> {} bytes",
            src_len,
            out.len()
        );
    }
    if use_new {
        let new_name = format!("{}.mp3", file_stem(&name));
        let dur = (pcm_len as u64 / (sr as u64 * ch as u64)) * 1000;
        let mut st = state::lock();
        if let Some(f) = st.pending_files.get_mut(idx) {
            tracing::info!(
                "audio 替换 {} -> {}: {} -> {} bytes",
                f.name,
                new_name,
                f.bytes.len(),
                out.len()
            );
            f.name = new_name;
            f.bytes = std::mem::take(&mut out);
            f.duration = dur.min(u32::MAX as u64) as u32;
        } else {
            out.clear();
        }
    } else {
        tracing::info!(
            "audio 保留原文件 {name}: 处理后 {} bytes >= 原始 {src_len} bytes",
            out.len()
        );
    }

    {
        let mut slot = JOB.lock().unwrap();
        let Some(j) = slot.as_mut() else { return };
        if use_new {
            j.done = done + 1;
        } else {
            j.reverted = reverted + 1;
        }
        j.index = idx + 1;
        j.pcm.clear();
        j.data.clear();
    }
    let progress = ((done + reverted + skipped + 1) as f64 / total_files.max(1) as f64 * 100.0) as u8;
    state::update_processing(gen, progress.min(99), "正在优化音频…");
    crate::ui::rerender();
    arm("next", gen);
}

/// 全部处理完成：结束进度并回调继续同步
fn finish_all(gen: u64) {
    let (on_done, done, reverted, skipped) = {
        let mut slot = JOB.lock().unwrap();
        match slot.take() {
            Some(j) => (j.on_done, j.done, j.reverted, j.skipped),
            None => return,
        }
    };
    ENC.with(|c| *c.borrow_mut() = None);
    state::finish_processing(gen);

    let mut parts: Vec<String> = Vec::new();
    if done > 0 {
        parts.push(format!("压缩 {done} 个"));
    }
    if reverted > 0 {
        parts.push(format!("保留原文件 {reverted} 个"));
    }
    if skipped > 0 {
        parts.push(format!("跳过 {skipped} 个"));
    }
    let summary = if parts.is_empty() {
        "无需优化".to_string()
    } else {
        parts.join("，")
    };
    tracing::info!("audio 优化完成: {summary}");
    state::set_notice(format!("音频优化完成（{summary}）"));
    crate::ui::rerender();
    on_done();
}

/// 裁掉足够长的静音段，两端各保留 SILENCE_KEEP_MS 余量，避免听感突兀
fn trim_silence(pcm: &[i16], sample_rate: u32, channels: u16) -> Vec<i16> {
    let ch = channels as usize;
    let per_ms = (sample_rate as usize * ch) / 1000;
    let win = per_ms * (WIN_MS as usize);
    if per_ms == 0 || win == 0 || pcm.len() < win * 50 {
        return pcm.to_vec();
    }
    let n_win = pcm.len() / win;
    let min_win = ((SILENCE_MIN_MS / WIN_MS) as usize).max(1);
    let keep_win = ((SILENCE_KEEP_MS / WIN_MS) as usize).max(1);

    // 逐窗口判定是否静音（取窗口峰值）
    let mut silent = vec![false; n_win];
    for w in 0..n_win {
        let seg = &pcm[w * win..(w + 1) * win];
        let peak = seg.iter().map(|v| (*v as i32).abs()).max().unwrap_or(0);
        silent[w] = peak < SILENCE_THRESHOLD;
    }

    // 标记要跳过的窗口（仅处理足够长的静音段）
    let mut skip = vec![false; n_win];
    let mut i = 0usize;
    while i < n_win {
        if !silent[i] {
            i += 1;
            continue;
        }
        let start = i;
        while i < n_win && silent[i] {
            i += 1;
        }
        if i - start >= min_win {
            let a = (start + keep_win).min(i);
            let b = i.saturating_sub(keep_win);
            for k in a..b.max(a) {
                skip[k] = true;
            }
        }
    }

    let mut out: Vec<i16> = Vec::with_capacity(pcm.len());
    for w in 0..n_win {
        if !skip[w] {
            out.extend_from_slice(&pcm[w * win..(w + 1) * win]);
        }
    }
    // 末尾不足一个窗口的残余
    if n_win * win < pcm.len() {
        out.extend_from_slice(&pcm[n_win * win..]);
    }
    out
}
