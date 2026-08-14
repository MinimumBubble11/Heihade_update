//! 封面图片处理（JPG→PNG 转换、最长边缩放、体积压缩）
//!
//! 集成在「选择封面图片」的上传流程中：选择图片后自动处理，处理完成前
//! 「同步到手表」按钮禁用（见 ui.rs 的 can_sync 门禁）。
//!
//! WASM 环境限制：插件为单线程沙箱（128MiB），无法真正后台并行，因此用
//! 宿主定时器把处理拆成 prepare → decode → encode → finalize 四个阶段，
//! 每阶段之间让出事件循环并刷新 UI 进度（PROGRESS + 百分比），
//! 避免单次事件回调长时间阻塞，不影响其他操作。
//!
//! 处理规则：
//!   - 最长边等比缩放到 250px（大图的主要压缩手段）；
//!   - 输出**强制 PNG**（满足 JPG→PNG，封面统一 PNG）。
//!
//! ⚠️ 不再回退输出 JPEG：Vela `<image>` 对手表端封面最可靠的是 png（内置封面
//! 全部为 png），jpg 封面存在手表端解码花屏/损坏风险且 `onerror` 未必触发
//! （曾致“上传 jpg 封面损坏”）。250px 上限下 png 体积有限，可接受。

use std::future::IntoFuture;
use std::sync::Mutex;

use image::{imageops::FilterType, DynamicImage};

use crate::astrobox::psys_host::timer;
use crate::state;

/// 处理定时器 payload 前缀（lib.rs 据此把 Timer 事件分发到本模块）。
/// 注意：set_timeout 的 payload 必须带上此前缀，否则 lib.rs 的 contains 判断
/// 不成立，定时器事件会被误转给传输逻辑而丢弃（曾导致卡在“准备处理封面”）。
pub const PROCESS_IMG_PAYLOAD_PREFIX: &str = "heihade-process-img:";
/// 阶段名（定时器 payload 形如 "heihade-process-img:decode:<gen>"）
const STEP_DECODE: &str = "decode";
const STEP_ENCODE: &str = "encode";
const STEP_FINALIZE: &str = "finalize";

/// 最长边目标像素
const MAX_EDGE: u32 = 250;

/// 跨阶段暂存的处理任务
struct PendingWork {
    gen: u64,
    name: String,
    bytes: Vec<u8>,                  // decode 阶段消费后清空
    orig_size: usize,                // decode 阶段记录原体积
    image: Option<DynamicImage>,     // decode 阶段填入、encode 阶段消费
    result: Option<(String, Vec<u8>)>, // encode 阶段填入（格式, 字节）
}

static PENDING: Mutex<Option<PendingWork>> = Mutex::new(None);

/// 启动封面图片处理流程（选择图片后调用）
pub fn start_image_processing(name: String, bytes: Vec<u8>) {
    let gen = state::start_processing("image", &name);
    *PENDING.lock().unwrap() = Some(PendingWork {
        gen,
        name: name.clone(),
        bytes,
        orig_size: 0,
        image: None,
        result: None,
    });
    state::update_processing(gen, 10, "准备处理封面…");
    arm(STEP_DECODE, gen);
    crate::ui::rerender();
}

/// 处理定时器回调（由 lib.rs 分发；payload 为宿主包装的 JSON）
/// 定时器 payload 形如 "heihade-process-img:decode:<gen>"，gen 用于
/// 忽略过期定时器（用户在处理中重新选择封面时旧定时器自动失效）。
pub fn on_timer(payload: &str) {
    let parsed: serde_json::Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(_) => return,
    };
    let inner = parsed.get("payload").and_then(|v| v.as_str()).unwrap_or("");
    let body = inner.strip_prefix(PROCESS_IMG_PAYLOAD_PREFIX).unwrap_or("");
    let (step, gen) = match body.rsplit_once(':') {
        Some((s, g)) => (s, g.parse::<u64>().ok()),
        None => (body, None),
    };
    let Some(gen) = gen else { return };
    match step {
        STEP_DECODE => step_decode(gen),
        STEP_ENCODE => step_encode(gen),
        STEP_FINALIZE => step_finalize(gen),
        _ => {}
    }
}

/// 在宿主侧注册一个 20ms 的一次性定时器（让出事件循环，让 UI 进度先刷新）。
/// payload 形如 "heihade-process-img:decode:<gen>"，必须含前缀供 lib.rs 分发。
fn arm(step: &str, gen: u64) {
    let payload = format!("{}{}:{}", PROCESS_IMG_PAYLOAD_PREFIX, step, gen);
    let _ = wit_bindgen::block_on(timer::set_timeout(20, &payload).into_future());
}

/// 该定时器对应的任务是否仍为当前最新任务
fn is_current(gen: u64) -> bool {
    PENDING.lock().unwrap().as_ref().map(|w| w.gen) == Some(gen)
}

/// 阶段 1：解码 + 等比缩放
fn step_decode(gen: u64) {
    if !is_current(gen) {
        return; // 过期定时器，忽略
    }
    let work = PENDING.lock().unwrap().take();
    let Some(mut work) = work else { return };
    // 幂等：若已被解码（理论上不会发生，仅防御），直接跳到编码阶段
    if work.image.is_some() {
        *PENDING.lock().unwrap() = Some(work);
        arm(STEP_ENCODE, gen);
        return;
    }
    state::update_processing(work.gen, 45, "解码图片并缩放到最长边 250px…");
    let orig_size = work.bytes.len();
    match decode_and_scale(&work.bytes) {
        Ok(img) => {
            work.bytes.clear();
            work.orig_size = orig_size;
            work.image = Some(img);
            *PENDING.lock().unwrap() = Some(work);
            arm(STEP_ENCODE, gen);
        }
        Err(e) => {
            state::finish_processing(work.gen);
            state::set_notice(format!("封面处理失败：{e}"));
        }
    }
    crate::ui::rerender();
}

/// 阶段 2：编码 + 压缩决策
fn step_encode(gen: u64) {
    if !is_current(gen) {
        return;
    }
    let work = PENDING.lock().unwrap().take();
    let Some(mut work) = work else { return };
    let Some(img) = work.image.take() else {
        // 异常：无解码结果（不应发生），终止任务
        state::finish_processing(work.gen);
        crate::ui::rerender();
        return;
    };
    state::update_processing(work.gen, 80, "编码并压缩图片…");
    match encode_and_compress(work.orig_size, &img) {
        Ok(r) => {
            work.result = Some(r);
            *PENDING.lock().unwrap() = Some(work);
            arm(STEP_FINALIZE, gen);
        }
        Err(e) => {
            state::finish_processing(work.gen);
            state::set_notice(format!("封面处理失败：{e}"));
        }
    }
    crate::ui::rerender();
}

/// 阶段 3：写回状态、结束处理
fn step_finalize(gen: u64) {
    if !is_current(gen) {
        return;
    }
    let work = PENDING.lock().unwrap().take();
    let Some(work) = work else { return };
    let Some((ext, bytes)) = work.result else { return };
    let new_name = new_image_name(&work.name, &ext);
    let orig_size = work.orig_size;
    state::set_image(new_name.clone(), bytes.clone());
    state::finish_processing(work.gen);
    let reduced_pct = if orig_size > bytes.len() {
        ((orig_size - bytes.len()) as f64 / orig_size as f64 * 100.0).round() as i64
    } else {
        0
    };
    state::set_notice(format!(
        "封面已处理：{} → {}（{}KB，体积减少约 {}%）",
        work.name,
        new_name,
        (bytes.len() + 512) / 1024,
        reduced_pct
    ));
    crate::ui::rerender();
}

/// 解码图片并按最长边等比缩放
fn decode_and_scale(bytes: &[u8]) -> Result<DynamicImage, String> {
    let img = image::load_from_memory(bytes).map_err(|e| format!("无法解析图片：{e}"))?;
    let (w, h) = (img.width(), img.height());
    let longest = w.max(h);
    if longest <= MAX_EDGE {
        return Ok(img);
    }
    let scale = MAX_EDGE as f32 / longest as f32;
    let nw = ((w as f32) * scale).round().max(1.0) as u32;
    let nh = ((h as f32) * scale).round().max(1.0) as u32;
    Ok(img.resize(nw, nh, FilterType::Triangle))
}

/// 编码：封面统一输出 PNG（JPG→PNG，永不回退 JPEG）。
/// 原体积仅用于 finalize 阶段统计“体积减少约 xx%”，不参与格式决策。
fn encode_and_compress(_orig_size: usize, img: &DynamicImage) -> Result<(String, Vec<u8>), String> {
    let png = encode_png(img)?;
    Ok(("png".to_string(), png))
}

fn encode_png(img: &DynamicImage) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .map_err(|e| format!("PNG 编码失败：{e}"))?;
    Ok(out)
}

/// 处理后的文件名（保留原名 stem，替换扩展名）
fn new_image_name(orig: &str, ext: &str) -> String {
    let stem = orig.rsplit_once('.').map(|(s, _)| s).unwrap_or(orig);
    format!("{stem}.{ext}")
}
