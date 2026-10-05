//! 土薯（toolshu.com）在线工具集成 —— 应用内浏览器 + `eval` 自动化
//!
//! 背景：插件是 WASM 沙箱，自身无法转码（无 ffmpeg）。而土薯的这些工具是
//! **纯前端 ffmpeg.wasm**，转换完全在浏览器内完成，且结果以 `a[download]` 的
//! blob URL 暴露，可用同步 XHR 读回 → 因此可以用「应用内浏览器 + eval」自动化。
//!
//! 阶段（每阶段一次 eval，由宿主定时器驱动，**不依赖 eval 的异步能力**）：
//!   1. inject : 注入本地文件(base64→File→DataTransfer) + 设格式/码率 + 点开始
//!   2. poll   : 轮询处理状态与结果链接
//!   3. fetch  : 读回结果（blob → base64）→ 写入 state
//!
//! 音频压缩码率档位（实测滑轨百分比映射）：
//!   5%→320k  25%→192k  50%→128k  **75%→64k(语音清晰)**  95%→32k
//!
//! 说明：在线工具为第三方服务，与插件作者无任何关系，仅供参考。

use std::future::IntoFuture;
use std::sync::Mutex;

use astrobox_ng_wit::astrobox::psys_host_v4::browser::{self, OpenOptions};
use astrobox_ng_wit::astrobox::psys_host_v4::timer;
use base64::Engine as _;

use crate::state;

/// 工具定时器 payload 前缀（lib.rs 据此分发）
pub const TOOL_TIMER_PREFIX: &str = "heihade-tool:";
/// 单次轮询间隔
const POLL_INTERVAL_MS: u64 = 2500;
/// 最大轮询次数（约 2.5s × 80 ≈ 200s 上限）
const MAX_POLLS: u32 = 80;
/// 手动模式下的轮询上限（约 2.5s × 480 ≈ 20 分钟，给用户在网页里操作的时间）
const MAX_POLLS_MANUAL: u32 = 480;
/// 本地文件大小上限（base64 膨胀 1.33×，且 eval 的 script 字符串不宜过大）
const MAX_INPUT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    /// 视频转音频（输出 MP3）
    VideoToAudio,
    /// 音频体积压缩（目标码率 64k）
    AudioCompress,
    /// 音频去除静音
    AudioSilence,
}

impl ToolKind {
    pub fn url(self) -> &'static str {
        match self {
            ToolKind::VideoToAudio => "https://toolshu.com/video-to-audio",
            ToolKind::AudioCompress => "https://toolshu.com/audio-compressor",
            ToolKind::AudioSilence => "https://toolshu.com/audio-silence-remover",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            ToolKind::VideoToAudio => "视频转音频",
            ToolKind::AudioCompress => "音频压缩(64k)",
            ToolKind::AudioSilence => "去除静音",
        }
    }
}

/// 任务暂存
struct Job {
    gen: u64,
    kind: ToolKind,
    browser_id: u32,
    /// 输入文件名（注入时用）
    input_name: String,
    /// 输出文件名前缀
    out_name: String,
    /// 输入文件的 base64（分块传给页面，避免单次 eval 脚本过大）
    input_b64: String,
    /// 已传偏移（也充当进度）
    in_offset: usize,
    /// poll 已重试次数
    polls: u32,
    /// 等待页面就绪的已重试次数
    waits: u32,
    /// 等待结果编码完成的已重试次数
    grab_waits: u32,
    /// 结果 base64 总长度
    b64_total: usize,
    /// 已读取的 base64 片段（也充当读取偏移）
    b64_buf: String,
    /// 工具给出的结果文件名
    result_name: String,
    /// 手动模式：不自动注入/点击（移动端 WebView 不支持），只保持网页打开，
    /// 由用户自己在网页里选文件并点开始，插件只负责轮询并自动取回结果。
    manual: bool,
}

static JOB: Mutex<Option<Job>> = Mutex::new(None);
static GEN: Mutex<u64> = Mutex::new(0);

fn next_gen() -> u64 {
    let mut g = GEN.lock().unwrap();
    *g += 1;
    *g
}

fn arm(stage: &str, gen: u64) {
    let payload = format!("{}{}:{}", TOOL_TIMER_PREFIX, stage, gen);
    let _ = timer::set_timeout(300, &payload);
}

/// 启动一个工具任务
pub fn start(kind: ToolKind, name: String, bytes: Vec<u8>) {
    if bytes.is_empty() {
        state::set_notice("文件为空".to_string());
        return;
    }
    if bytes.len() > MAX_INPUT_BYTES {
        state::set_notice(format!(
            "文件过大（{:.1}MB）：在线工具自动化上限 8MB，请点「打开工具」手动处理",
            bytes.len() as f64 / 1024.0 / 1024.0
        ));
        return;
    }
    // 打开应用内浏览器（第三方工具，需用户授权 browser 权限）
    let opts = OpenOptions {
        url: kind.url().to_string(),
        title: Some(format!("{} · 土薯工具（第三方）", kind.label())),
        user_agent: None,
        intercept_prefixes: vec![],
        close_on_intercept: false,
        ephemeral: true,
        width: None,
        height: None,
    };
    let id = match astrobox_ng_wit::block_on(browser::open(opts).into_future()) {
        Ok(id) => id,
        Err(e) => {
            state::set_notice(format!("打开应用内浏览器失败：{e}"));
            return;
        }
    };
    let gen = next_gen();
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let out_name = format!("{}.{}", file_stem(&name), "mp3");
    *JOB.lock().unwrap() = Some(Job {
        gen,
        kind,
        browser_id: id,
        input_name: name.clone(),
        out_name,
        input_b64: b64,
        in_offset: 0,
        polls: 0,
        waits: 0,
        grab_waits: 0,
        b64_total: 0,
        b64_buf: String::new(),
        result_name: String::new(),
        manual: false,
    });
    state::begin_tool_job(kind.label(), &name);
    state::update_tool_job(5, "等待页面加载…");
    arm("wait", gen);
    crate::ui::rerender();
}

/// 定时器分发（由 lib.rs 调用）
pub fn on_timer(payload: &str) {
    let parsed: serde_json::Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(_) => return,
    };
    let inner = parsed.get("payload").and_then(|v| v.as_str()).unwrap_or("");
    let body = inner.strip_prefix(TOOL_TIMER_PREFIX).unwrap_or("");
    let (stage, gen) = match body.rsplit_once(':') {
        Some((s, g)) => (s, g.parse::<u64>().ok()),
        None => (body, None),
    };
    let Some(gen) = gen else { return };
    match stage {
        "wait" => step_wait(gen),
        "detect" => step_detect(gen),
        "inject_chunk" => step_inject_chunk(gen),
        "inject_finish" => step_inject_finish(gen),
        "bitrate" => step_bitrate(gen),
        "confirm" => step_confirm(gen),
        "click" => step_click(gen),
        "poll" => step_poll(gen),
        "grab_start" => step_grab_start(gen),
        "grab_wait" => step_grab_wait(gen),
        "grab_chunk" => step_grab_chunk(gen),
        _ => {}
    }
}

/// 执行脚本，并把返回值当 JSON 字符串取值（用于分块读取等原始字符串）
fn eval_raw(id: u32, script: &str) -> Result<String, String> {
    let text = eval(id, script)?;
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(serde_json::Value::String(s)) => Ok(s),
        _ => Ok(text),
    }
}

/// 兼容 eval 返回值可能被二次 JSON 序列化（JSON 字符串里再包一层 JSON）
fn parse_eval_json(text: &str) -> serde_json::Value {
    let v: serde_json::Value = serde_json::from_str(text).unwrap_or(serde_json::Value::Null);
    if let serde_json::Value::String(s) = &v {
        if let Ok(inner) = serde_json::from_str::<serde_json::Value>(s) {
            return inner;
        }
    }
    v
}

/// 阶段 0：等待页面就绪（文件输入框 + 「开始」按钮可用 → 说明 ffmpeg 已加载完）
fn step_wait(gen: u64) {
    if !is_current(gen) {
        return;
    }
    let (id, waits) = {
        let j = JOB.lock().unwrap();
        let Some(j) = j.as_ref() else { return };
        (j.browser_id, j.waits)
    };
    if waits >= 120 {
        abort("等待工具页面就绪超时（若页面一直显示「加载处理库」，请检查网络后重试）");
        return;
    }
    {
        if let Some(j) = JOB.lock().unwrap().as_mut() {
            j.waits += 1;
        }
    }
    match eval(id, READY_SCRIPT) {
        Ok(text) => {
            let v = parse_eval_json(&text);
            let ready = v.get("ready").and_then(|x| x.as_bool()).unwrap_or(false);
            let has_input = v.get("hasInput").and_then(|x| x.as_bool()).unwrap_or(false);
            let lib = v.get("lib").and_then(|x| x.as_bool()).unwrap_or(false);
            let btn_disabled = v.get("btnDisabled").and_then(|x| x.as_bool()).unwrap_or(true);
            let title = v.get("title").and_then(|x| x.as_str()).unwrap_or("");
            if ready {
                tracing::info!("tool 页面就绪: {title}");
                state::update_tool_job(10, "检测浏览器环境…");
                crate::ui::rerender();
                arm("detect", gen);
            } else {
                let msg = if !has_input {
                    "等待工具页面加载…"
                } else if !lib {
                    "工具正在加载处理库（ffmpeg）…"
                } else if btn_disabled {
                    "等待工具就绪…"
                } else {
                    "等待工具页面…"
                };
                tracing::info!("tool 等待就绪 #{waits}: {msg} | {}", truncate(&text, 160));
                state::update_tool_job(5, msg);
                crate::ui::rerender();
                let payload = format!("{}{}:{}", TOOL_TIMER_PREFIX, "wait", gen);
                let _ = timer::set_timeout(800, &payload);
            }
        }
        Err(e) => abort(&format!("无法访问工具页面：{e}（应用内浏览器是否已关闭？）")),
    }
}

/// 阶段 0.5：检测宿主环境，决定「自动模式」还是「手动模式」
fn step_detect(gen: u64) {
    if !is_current(gen) {
        return;
    }
    let id = {
        let j = JOB.lock().unwrap();
        let Some(j) = j.as_ref() else { return };
        j.browser_id
    };
    match eval(id, DETECT_SCRIPT) {
        Ok(text) => {
            tracing::info!("tool 环境检测: {}", truncate(&text, 300));
            let v = parse_eval_json(&text);
            let mobile = v.get("mobile").and_then(|x| x.as_bool()).unwrap_or(false);
            let can_build = v.get("canBuild").and_then(|x| x.as_bool()).unwrap_or(false);
            if mobile || !can_build {
                let why = if mobile { "移动端 WebView" } else { "不支持自动填充文件" };
                tracing::info!("tool 转为手动模式：{why}");
                if let Some(j) = JOB.lock().unwrap().as_mut() {
                    j.manual = true;
                    j.polls = 0;
                }
                state::set_notice(format!(
                    "已改为手动模式（{why}）：请在打开的网页里自己选文件并点开始，完成后结果会自动回到插件"
                ));
                state::update_tool_job(15, "手动模式：请在网页中选文件并点开始");
                crate::ui::rerender();
                arm("poll", gen);
            } else {
                state::update_tool_job(10, "页面就绪，开始传输文件…");
                crate::ui::rerender();
                arm("inject_chunk", gen);
            }
        }
        Err(e) => abort(&format!("无法访问工具页面：{e}（应用内浏览器是否已关闭？）")),
    }
}

/// 任意时刻终止任务（关浏览器 + 清状态）
pub fn abort(msg: &str) {
    let job = JOB.lock().unwrap().take();
    if let Some(j) = job {
        let _ = astrobox_ng_wit::block_on(browser::close(j.browser_id).into_future());
    }
    state::finish_tool_job();
    state::set_notice(msg.to_string());
    crate::ui::rerender();
}

/// 校验 gen 是否为当前任务
fn is_current(gen: u64) -> bool {
    JOB.lock().unwrap().as_ref().map(|j| j.gen) == Some(gen)
}

/// 阶段 1a：分块把文件 base64 传到页面（每次 ≈64KB，避免 eval 脚本过大挂起）
fn step_inject_chunk(gen: u64) {
    if !is_current(gen) {
        return;
    }
    const CHUNK: usize = 65536;
    let (id, offset, total) = {
        let j = JOB.lock().unwrap();
        let Some(j) = j.as_ref() else { return };
        (j.browser_id, j.in_offset, j.input_b64.len())
    };
    if offset >= total {
        arm("inject_finish", gen);
        return;
    }
    let end = (offset + CHUNK).min(total);
    let chunk = {
        JOB.lock()
            .unwrap()
            .as_ref()
            .map(|j| j.input_b64[offset..end].to_string())
            .unwrap_or_default()
    };
    // base64 片段仅含 A-Za-z0-9+/=，在 JS 单引号字符串中安全
    let script = format!(
        "(function(){{try{{window.__hs_in=(window.__hs_in||'')+'{}';return JSON.stringify({{ok:true,len:window.__hs_in.length}});}}catch(e){{return JSON.stringify({{ok:false,err:String(e)}});}}}})()",
        chunk
    );
    match eval(id, &script) {
        Ok(text) => {
            let v = parse_eval_json(&text);
            if !v.get("ok").and_then(|x| x.as_bool()).unwrap_or(false) {
                let err = v.get("err").and_then(|x| x.as_str()).unwrap_or("未知错误");
                abort(&format!("传输文件失败：{err}"));
                return;
            }
            if let Some(j) = JOB.lock().unwrap().as_mut() {
                j.in_offset = end;
            }
            let pct = 10u8 + (end * 8 / total.max(1)) as u8;
            state::update_tool_job(pct.min(18), "传输文件到工具页面…");
            crate::ui::rerender();
            arm("inject_chunk", gen);
        }
        Err(e) => abort(&format!("传输文件失败：{e}")),
    }
}

/// 阶段 1b：用页面内已累积的 base64 构造 File → 注入 → 设格式/码率 → 点开始
fn step_inject_finish(gen: u64) {
    if !is_current(gen) {
        return;
    }
    let (id, name_lit) = {
        let j = JOB.lock().unwrap();
        let Some(j) = j.as_ref() else { return };
        (
            j.browser_id,
            serde_json::to_string(&j.input_name).unwrap_or_else(|_| "\"input\"".to_string()),
        )
    };
    state::update_tool_job(20, "注入文件…");
    crate::ui::rerender();
    let script = build_finish_script(&name_lit);
    match eval(id, &script) {
        Ok(text) => {
            tracing::info!("tool inject 结果: {}", truncate(&text, 300));
            let v = parse_eval_json(&text);
            if !v.get("ok").and_then(|x| x.as_bool()).unwrap_or(false) {
                let err = v.get("err").and_then(|x| x.as_str()).unwrap_or("未知错误");
                abort(&format!("注入文件失败：{err}"));
                return;
            }
            if let Some(s) = v.get("slider").and_then(|x| x.as_str()) {
                tracing::info!("工具压缩档位: {s}");
            }
            // 重置计数供 confirm 阶段使用
            if let Some(j) = JOB.lock().unwrap().as_mut() {
                j.waits = 0;
            }
            state::update_tool_job(22, "设置压缩强度（64k）…");
            crate::ui::rerender();
            arm("bitrate", gen);
        }
        Err(e) => abort(&format!("页面脚本执行失败：{e}")),
    }
}

/// 阶段 1b-2：把压缩强度调到「语音清晰 (64k)」
/// 页面默认是「标准 (128k)」（手柄在 50%），需点到 75% 位置。
fn step_bitrate(gen: u64) {
    if !is_current(gen) {
        return;
    }
    let (id, tries) = {
        let j = JOB.lock().unwrap();
        let Some(j) = j.as_ref() else { return };
        (j.browser_id, j.waits)
    };
    if tries >= 5 {
        // 不终止流程：设置失败时仍可用页面默认档位完成转换
        tracing::warn!("未能设置压缩强度，改用页面默认档位");
        state::set_notice("未能设置 64k，已使用工具默认档位".to_string());
        if let Some(j) = JOB.lock().unwrap().as_mut() {
            j.waits = 0;
        }
        arm("confirm", gen);
        return;
    }
    match eval(id, SET_BITRATE_SCRIPT) {
        Ok(text) => {
            tracing::info!("tool 压缩强度: {}", truncate(&text, 200));
            let v = parse_eval_json(&text);
            if v.get("ok").and_then(|x| x.as_bool()).unwrap_or(false) {
                if let Some(t) = v.get("text").and_then(|x| x.as_str()) {
                    tracing::info!("工具当前压缩档位: {t}");
                }
                if let Some(j) = JOB.lock().unwrap().as_mut() {
                    j.waits = 0;
                }
                state::update_tool_job(25, "等待文件出现在列表…");
                crate::ui::rerender();
                arm("confirm", gen);
            } else {
                if let Some(j) = JOB.lock().unwrap().as_mut() {
                    j.waits += 1;
                }
                arm("bitrate", gen);
            }
        }
        Err(e) => abort(&format!("无法访问工具页面：{e}")),
    }
}

/// 阶段 1c：等文件真正出现在待处理列表后，才点击「开始处理」
fn step_confirm(gen: u64) {
    if !is_current(gen) {
        return;
    }
    let (id, tries) = {
        let j = JOB.lock().unwrap();
        let Some(j) = j.as_ref() else { return };
        (j.browser_id, j.waits)
    };
    if tries >= 40 {
        // 自动注入未被页面接受（常见于移动端 WebView）→ 回退手动模式，保持浏览器打开
        let manual = JOB.lock().unwrap().as_ref().map(|j| j.manual).unwrap_or(false);
        if manual {
            abort("等待你在网页中操作超时");
        } else {
            tracing::info!("tool 自动注入未生效，切换手动模式");
            if let Some(j) = JOB.lock().unwrap().as_mut() {
                j.manual = true;
                j.waits = 0;
                j.polls = 0;
            }
            state::set_notice(
                "自动注入未生效，已改为手动模式：请在打开的网页里自己选文件并点开始，完成后结果会自动回到插件"
                    .to_string(),
            );
            state::update_tool_job(20, "手动模式：请在网页中选文件并点开始");
            crate::ui::rerender();
            arm("poll", gen);
        }
        return;
    }
    match eval(id, CONFIRM_SCRIPT) {
        Ok(text) => {
            let v = parse_eval_json(&text);
            if v.get("ready").and_then(|x| x.as_bool()).unwrap_or(false) {
                let name = v.get("name").and_then(|x| x.as_str()).unwrap_or("");
                tracing::info!("tool 文件已进入待处理列表: {name}");
                arm("click", gen);
            } else {
                if let Some(j) = JOB.lock().unwrap().as_mut() {
                    j.waits += 1;
                }
                arm("confirm", gen);
            }
        }
        Err(e) => abort(&format!("无法访问工具页面：{e}")),
    }
}

/// 阶段 1d：点击「开始处理」
fn step_click(gen: u64) {
    if !is_current(gen) {
        return;
    }
    let id = {
        let j = JOB.lock().unwrap();
        let Some(j) = j.as_ref() else { return };
        j.browser_id
    };
    state::update_tool_job(30, "已提交处理，等待转换…");
    crate::ui::rerender();
    match eval(id, CLICK_SCRIPT) {
        Ok(text) => {
            tracing::info!("tool 点击开始: {}", truncate(&text, 200));
            let v = parse_eval_json(&text);
            if !v.get("ok").and_then(|x| x.as_bool()).unwrap_or(false) {
                let err = v.get("err").and_then(|x| x.as_str()).unwrap_or("未知错误");
                abort(&format!("无法开始处理：{err}"));
                return;
            }
            state::update_tool_job(40, "处理中（浏览器内 ffmpeg）…");
            crate::ui::rerender();
            arm("poll", gen);
        }
        Err(e) => abort(&format!("页面脚本执行失败：{e}")),
    }
}

/// 阶段 2：轮询状态
fn step_poll(gen: u64) {
    if !is_current(gen) {
        return;
    }
    let (id, polls, manual) = {
        let j = JOB.lock().unwrap();
        let Some(j) = j.as_ref() else { return };
        (j.browser_id, j.polls, j.manual)
    };
    let max_polls = if manual { MAX_POLLS_MANUAL } else { MAX_POLLS };
    if polls >= max_polls {
        abort(if manual {
            "等待你在网页中操作超时"
        } else {
            "工具处理超时（可手动在浏览器窗口内操作）"
        });
        return;
    }
    {
        if let Some(j) = JOB.lock().unwrap().as_mut() {
            j.polls += 1;
        }
    }
    match eval(id, POLL_SCRIPT) {
        Ok(text) => {
            let v = parse_eval_json(&text);
            let busy = v.get("busy").and_then(|x| x.as_bool()).unwrap_or(false);
            let results = v.get("results").and_then(|x| x.as_u64()).unwrap_or(0);
            tracing::info!("tool poll: busy={} results={}", busy, truncate(&text, 200));
            // 只要页面出现结果链接就进入取回（不依赖 busy 判断，避免页面残留文本导致卡住）
            if results > 0 {
                // 处理完成，进入取回：先让页面把结果转成 base64 存到 window（避免同步 XHR 兼容性问题）
                state::update_tool_job(75, "读取转换结果…");
                crate::ui::rerender();
                arm("grab_start", gen);
            } else {
                // 45% → 70% 之间小幅推进，让用户看到活动
                let pct = 45u8 + ((polls.min(10)) as u8 * 2);
                let msg = if manual {
                    format!("等待你在网页中操作…（第 {} 次检查）", polls + 1)
                } else {
                    format!("处理中（浏览器内 ffmpeg）… 第 {} 次检查", polls + 1)
                };
                state::update_tool_job(pct.min(70), &msg);
                crate::ui::rerender();
                let payload = format!("{}{}:{}", TOOL_TIMER_PREFIX, "poll", gen);
                let _ = timer::set_timeout(POLL_INTERVAL_MS, &payload);
            }
        }
        Err(e) => {
            // 浏览器可能被用户关闭 → 结束任务
            abort(&format!("无法访问工具页面（浏览器已关闭？）：{e}"));
        }
    }
}

/// 阶段 3a：让页面把结果转成 base64 存到 window（页面内 fetch，避免同步 XHR 兼容问题）
fn step_grab_start(gen: u64) {
    if !is_current(gen) {
        return;
    }
    let id = {
        let j = JOB.lock().unwrap();
        let Some(j) = j.as_ref() else { return };
        j.browser_id
    };
    match eval(id, GRAB_START_SCRIPT) {
        Ok(text) => {
            let v = parse_eval_json(&text);
            if !v.get("ok").and_then(|x| x.as_bool()).unwrap_or(false) {
                let err = v.get("err").and_then(|x| x.as_str()).unwrap_or("未知错误");
                abort(&format!("读取结果失败：{err}"));
                return;
            }
            tracing::info!("tool grab_start 已启动页面内编码");
            state::update_tool_job(78, "页面内编码结果…");
            crate::ui::rerender();
            arm("grab_wait", gen);
        }
        Err(e) => abort(&format!("读取结果失败：{e}")),
    }
}

/// 阶段 3b：等待页面把结果编码为 base64
fn step_grab_wait(gen: u64) {
    if !is_current(gen) {
        return;
    }
    let (id, waits) = {
        let j = JOB.lock().unwrap();
        let Some(j) = j.as_ref() else { return };
        (j.browser_id, j.grab_waits)
    };
    if waits >= 60 {
        abort("读取结果超时（结果过大或页面被关闭）");
        return;
    }
    {
        if let Some(j) = JOB.lock().unwrap().as_mut() {
            j.grab_waits += 1;
        }
    }
    match eval(id, GRAB_WAIT_SCRIPT) {
        Ok(text) => {
            let v = parse_eval_json(&text);
            if let Some(err) = v.get("err").and_then(|x| x.as_str()) {
                if !err.is_empty() {
                    abort(&format!("读取结果失败：{err}"));
                    return;
                }
            }
            let ready = v.get("ready").and_then(|x| x.as_bool()).unwrap_or(false);
            let len = v.get("len").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
            let name = v.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();
            if ready && len > 0 {
                if let Some(j) = JOB.lock().unwrap().as_mut() {
                    j.b64_total = len;
                    j.b64_buf = String::with_capacity(len);
                    j.result_name = name;
                }
                tracing::info!("tool 结果 base64 长度={}", len);
                state::update_tool_job(85, "分块读取结果…");
                crate::ui::rerender();
                arm("grab_chunk", gen);
            } else {
                state::update_tool_job(80, "页面内编码结果…");
                crate::ui::rerender();
                let payload = format!("{}{}:{}", TOOL_TIMER_PREFIX, "grab_wait", gen);
                let _ = timer::set_timeout(500, &payload);
            }
        }
        Err(e) => abort(&format!("读取结果失败：{e}")),
    }
}

/// 阶段 3c：分块读取 base64（规避单次 eval 返回过大被截断）
fn step_grab_chunk(gen: u64) {
    if !is_current(gen) {
        return;
    }
    const CHUNK: usize = 65536;
    let (id, offset, total, kind, out_name, result_name) = {
        let j = JOB.lock().unwrap();
        let Some(j) = j.as_ref() else { return };
        (
            j.browser_id,
            j.b64_buf.len(),
            j.b64_total,
            j.kind,
            j.out_name.clone(),
            j.result_name.clone(),
        )
    };
    if offset >= total {
        // 读取完成 → 解码并写入待同步列表
        let b64 = {
            JOB.lock()
                .unwrap()
                .as_ref()
                .map(|j| j.b64_buf.clone())
                .unwrap_or_default()
        };
        let bytes = match base64::engine::general_purpose::STANDARD.decode(&b64) {
            Ok(b) => b,
            Err(e) => {
                abort(&format!("结果解码失败：{e}"));
                return;
            }
        };
        if bytes.is_empty() {
            abort("工具返回了空文件");
            return;
        }
        let final_name = if result_name.is_empty() {
            out_name.clone()
        } else {
            format!("{}_{}", file_stem(&out_name), result_name)
        };
        let _ = astrobox_ng_wit::block_on(browser::close(id).into_future());
        *JOB.lock().unwrap() = None;
        let size_kb = bytes.len() / 1024;
        state::add_pending_file(final_name.clone(), bytes);
        state::finish_tool_job();
        state::set_notice(format!(
            "「{}」完成：{}（{}KB），已加入待同步列表",
            kind.label(),
            final_name,
            size_kb
        ));
        crate::ui::rerender();
        return;
    }
    let end = (offset + CHUNK).min(total);
    let script = format!(
        "(function(){{try{{var b=window.__hs_b64||'';return b.substr({0},{1});}}catch(e){{return '';}}}})()",
        offset,
        end - offset
    );
    match eval_raw(id, &script) {
        Ok(part) => {
            if part.is_empty() {
                abort("读取结果片段为空（页面结果可能已被清理）");
                return;
            }
            let got = part.len();
            if let Some(j) = JOB.lock().unwrap().as_mut() {
                j.b64_buf.push_str(&part);
            }
            let pct = 85u8 + ((offset + got) * 10 / total.max(1)) as u8;
            state::update_tool_job(pct.min(95), "读取结果…");
            crate::ui::rerender();
            arm("grab_chunk", gen);
        }
        Err(e) => abort(&format!("读取结果片段失败：{e}")),
    }
}

fn eval(id: u32, script: &str) -> Result<String, String> {
    astrobox_ng_wit::block_on(browser::eval(id, script.to_string()).into_future())
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// 取文件名主干（去掉扩展名），供工具输出与插件内音频输出共用
pub fn file_stem(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(s, _)| s.to_string())
        .unwrap_or_else(|| name.to_string())
}

/// 注入脚本：base64 → File → 文件输入框；设格式与码率；点击「开始」
/// 收尾脚本：把页面内已累积的 base64 还原为 File 并注入，
/// 设输出格式/码率后点击「开始」。脚本体积很小（不包含文件数据）。
/// 注入脚本：把页面内已累积的 base64 还原为 File 并注入，并将输出格式设为 mp3。
/// **不点击**「开始处理」—— 点击由后续 confirm 阶段在确认文件已出现在列表后进行。
/// 压缩强度实测默认即为「语音清晰 (64k)」，此处只读取回报，不做滑块操作。
fn build_finish_script(name_lit: &str) -> String {
    format!(
        r#"(function(){{
  try {{
    var fname = {name_lit};
    var b64 = window.__hs_in || '';
    if (!b64) return JSON.stringify({{ ok: false, err: "no-input-data" }});
    function mimeOf(n) {{
      var e = (String(n).split('.').pop() || '').toLowerCase();
      var m = {{
        mp4:'video/mp4', m4v:'video/x-m4v', mov:'video/quicktime', mkv:'video/x-matroska',
        webm:'video/webm', avi:'video/x-msvideo', flv:'video/x-flv', wmv:'video/x-ms-wmv',
        ts:'video/mp2t', m2ts:'video/mp2t', '3gp':'video/3gpp', rmvb:'video/vnd.rn-realvideo',
        vob:'video/dvd', ogv:'video/ogg', mpeg:'video/mpeg', mpg:'video/mpeg',
        mp3:'audio/mpeg', wav:'audio/wav', m4a:'audio/mp4', aac:'audio/aac',
        flac:'audio/flac', ogg:'audio/ogg', opus:'audio/opus', wma:'audio/x-ms-wma',
        amr:'audio/amr', ape:'audio/x-ape', aiff:'audio/aiff', wv:'audio/x-wavpack'
      }};
      return m[e] || 'application/octet-stream';
    }}
    var mt = mimeOf(fname);
    var bin = atob(b64), n = bin.length, u8 = new Uint8Array(n);
    for (var i = 0; i < n; i++) u8[i] = bin.charCodeAt(i);
    var f = new File([u8], fname, {{ type: mt }});
    var inp = document.querySelector('input[type=file]');
    if (!inp) return JSON.stringify({{ ok: false, err: "no-file-input" }});
    // 放宽 accept：部分 WebView 会因 MIME 与 accept（如 video/*）不匹配而拒收文件，
    // 表现为工具页提示「不支持…」且待处理列表里不出现该项。
    try {{ inp.accept = ''; }} catch (e) {{}}
    var dt = new DataTransfer(); dt.items.add(f);
    inp.files = dt.files;
    inp.dispatchEvent(new Event('change', {{ bubbles: true }}));
    var sel = document.getElementById('targetFormat');
    if (sel && sel.value !== 'mp3') {{ sel.value = "mp3"; sel.dispatchEvent(new Event('change', {{ bubbles: true }})); }}
    var sv = document.querySelector('.slider-val');
    window.__hs_in = null;
    return JSON.stringify({{
      ok: true, format: sel ? sel.value : null, mime: mt,
      slider: sv ? (sv.textContent || '').trim() : null,
      fileCount: inp.files ? inp.files.length : -1,
      alerts: (window.__hs_alerts || []).slice(-3)
    }});
  }} catch (e) {{ return JSON.stringify({{ ok: false, err: String(e) }}); }}
}})()"#,
        name_lit = name_lit
    )
}

/// 就绪检测脚本（文件输入框 + 「开始」按钮可用 + FFmpeg 库已就绪）
/// ⚠ 不要检测整页 innerText：页面底部用户评论里出现过「处理库加载失败」字样，会误判为未就绪。
const READY_SCRIPT: &str = r#"(function(){
  try {
    // 劫持弹窗：部分 WebView 下工具页会弹「浏览器不支持现代 JavaScript 语法」等提示，
    // 原生 alert 会阻塞 JS 线程，导致后续 eval 永不返回（表现为「没有后续反应」）。
    if (!window.__hs_patched) {
      window.__hs_patched = true;
      window.__hs_alerts = [];
      try { window.alert = function(m) { window.__hs_alerts.push('alert: ' + String(m).slice(0, 200)); }; } catch (e) {}
      try { window.confirm = function() { return true; }; } catch (e) {}
      try { window.prompt = function() { return null; }; } catch (e) {}
    }
    var inp = document.querySelector('input[type=file]');
    var scope = document.getElementById('tool-body') || document;
    var btn = null, bs = scope.querySelectorAll('button');
    for (var i = 0; i < bs.length; i++) { if (/开始/.test(bs[i].textContent || '')) { btn = bs[i]; break; } }
    var btnDisabled = btn ? !!btn.disabled : true;
    var lib = (typeof window.FFmpegWASM !== 'undefined') || (typeof window.FFmpegUtil !== 'undefined');
    return JSON.stringify({
      ready: !!inp && !!btn && !btnDisabled && lib,
      hasInput: !!inp,
      btnText: btn ? (btn.textContent || '').trim() : '',
      btnDisabled: btnDisabled,
      lib: lib,
      title: document.title || ''
    });
  } catch (e) { return JSON.stringify({ ready: false, err: String(e) }); }
})()"#;

/// 环境检测脚本：判断当前宿主 WebView 能否可靠完成自动注入
/// （移动端 WebView 常因安全策略拒绝为 input[type=file] 程序化赋值）
const DETECT_SCRIPT: &str = r#"(function(){
  try {
    var ua = navigator.userAgent || '';
    var mobile = /Android|iPhone|iPad|iPod|Mobile|HarmonyOS|MIUI|Phone|MicroMessenger/i.test(ua);
    var canBuild = false, reason = '';
    try {
      if (typeof DataTransfer === 'undefined') { reason = 'no-DataTransfer'; }
      else if (typeof File === 'undefined') { reason = 'no-File'; }
      else {
        var dt = new DataTransfer();
        dt.items.add(new File([new Uint8Array(8)], '__hs_probe__.bin', { type: 'application/octet-stream' }));
        canBuild = !!(dt.files && dt.files.length === 1);
        if (!canBuild) { reason = 'dt-items-rejected'; }
      }
    } catch (e) { reason = String(e).slice(0, 90); }
    var inp = document.querySelector('input[type=file]');
    if (!inp) { reason = reason || 'no-file-input'; }
    return JSON.stringify({
      ua: ua.slice(0, 180), mobile: mobile, canBuild: canBuild,
      accept: inp ? (inp.accept || '') : null, reason: reason
    });
  } catch (e) { return JSON.stringify({ err: String(e) }); }
})()"#;

/// 确认脚本：文件是否已出现在待处理列表
/// ⚠ 音频工具页的项是 `.audio-item`，**视频工具页是 `.video-item`**，故以 `.file-name` 为准。
const CONFIRM_SCRIPT: &str = r#"(function(){
  try {
    var fn = document.querySelector('#fileList .file-name');
    var name = fn ? (fn.textContent || '').trim() : '';
    var items = document.querySelectorAll('#fileList .audio-item, #fileList .video-item');
    return JSON.stringify({ ready: !!name, name: name, count: items.length, alerts: (window.__hs_alerts || []).slice(-3) });
  } catch (e) { return JSON.stringify({ ready: false, err: String(e) }); }
})()"#;

/// 点击「开始处理」脚本（限定在工具主体内查找，避免误点其它按钮）
const CLICK_SCRIPT: &str = r#"(function(){
  try {
    var scope = document.getElementById('tool-body') || document;
    var btn = null, bs = scope.querySelectorAll('button');
    for (var i = 0; i < bs.length; i++) { if (/开始/.test(bs[i].textContent || '')) { btn = bs[i]; break; } }
    if (!btn) return JSON.stringify({ ok: false, err: "no-button" });
    if (btn.disabled) return JSON.stringify({ ok: false, err: "button-disabled" });
    btn.click();
    return JSON.stringify({ ok: true, btnText: (btn.textContent || '').trim() });
  } catch (e) { return JSON.stringify({ ok: false, err: String(e) }); }
})()"#;

/// 压缩强度脚本：页面默认「标准 (128k)」（手柄 50%），点到 75% 位置 → 「语音清晰 (64k)」
const SET_BITRATE_SCRIPT: &str = r#"(function(){
  try {
    var sv = document.querySelector('.slider-val');
    var cur = sv ? (sv.textContent || '').trim() : null;
    if (!cur) return JSON.stringify({ ok: true, noSlider: true, text: null });
    if (/64k/.test(cur)) return JSON.stringify({ ok: true, text: cur, changed: false });
    var sl = document.querySelector('.layui-slider');
    if (!sl) return JSON.stringify({ ok: true, noSlider: true, text: cur });
    var r = sl.getBoundingClientRect();
    var x = r.left + r.width * 0.75, y = r.top + r.height / 2;
    var fire = function(t, buttons) {
      sl.dispatchEvent(new MouseEvent(t, {
        bubbles: true, cancelable: true, view: window,
        clientX: x, clientY: y, button: 0, buttons: buttons, which: 1
      }));
    };
    fire('mousedown', 1); fire('mouseup', 0); fire('click', 0);
    var sv2 = document.querySelector('.slider-val');
    var after = sv2 ? (sv2.textContent || '').trim() : null;
    return JSON.stringify({ ok: !!(after && /64k/.test(after)), text: after, changed: true });
  } catch (e) { return JSON.stringify({ ok: false, err: String(e) }); }
})()"#;

/// 轮询脚本
const POLL_SCRIPT: &str = r#"(function(){
  try {
    var txt = document.body ? (document.body.innerText || '') : '';
    var busy = /处理中|转换中|加载处理库|正在处理/.test(txt);
    var as = document.querySelectorAll('a[download]');
    var names = [];
    for (var i = 0; i < as.length; i++) names.push(as[i].getAttribute('download') || '');
    return JSON.stringify({ busy: busy, results: names.length, names: names.slice(0, 5) });
  } catch (e) { return JSON.stringify({ busy: false, results: 0, err: String(e) }); }
})()"#;

/// 取回 3a：页面内 fetch 结果 blob → base64 存入 window（异步，由 3b 轮询）
const GRAB_START_SCRIPT: &str = r#"(function(){
  try {
    var a = document.querySelector('a[download]');
    if (!a) return JSON.stringify({ ok: false, err: 'no-result-link' });
    window.__hs_b64 = null; window.__hs_err = null; window.__hs_name = '';
    fetch(a.href).then(function(r){ return r.arrayBuffer(); }).then(function(buf){
      var u8 = new Uint8Array(buf), n = u8.length, parts = [], CH = 0x8000;
      for (var i = 0; i < n; i += CH) parts.push(String.fromCharCode.apply(null, u8.subarray(i, Math.min(n, i + CH))));
      window.__hs_b64 = btoa(parts.join(''));
      window.__hs_name = a.getAttribute('download') || '';
    }).catch(function(e){ window.__hs_err = String(e); });
    return JSON.stringify({ ok: true });
  } catch (e) { return JSON.stringify({ ok: false, err: String(e) }); }
})()"#;

/// 取回 3b：查询页面内编码是否完成
const GRAB_WAIT_SCRIPT: &str = r#"(function(){
  try {
    var b = window.__hs_b64;
    return JSON.stringify({
      err: window.__hs_err || '',
      ready: (typeof b === 'string' && b.length > 0),
      len: (typeof b === 'string' ? b.length : 0),
      name: window.__hs_name || ''
    });
  } catch (e) { return JSON.stringify({ err: String(e), ready: false, len: 0, name: '' }); }
})()"#;
