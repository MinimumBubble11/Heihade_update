//! 自定义音频同步传输（插件 → 快应用）
//!
//! 协议与快应用端 src/common/audiosync.js 严格对应，均为 JSON 消息：
//!   start      { "type":"audiosync","action":"start","id":"..","name":"..",
//!                "mode":"single"|"sequence","display":"image"|"text",
//!                "imageName":"..","duration":N,"cooldown":N,"totalSteps":N,
//!                "bgText":"..","centerText":"..","chunks":N,"size":S,
//!                "units":[{"kind":"audio"|"image","file":"..","duration":N}, ...] }
//!   unit-start { "type":"audiosync","action":"unit-start","id":"..","unitIndex":i,
//!                "kind":"audio"|"image","file":"..","chunks":N,"size":S }
//!   chunk      { "type":"audiosync","action":"chunk","id":"..","unitIndex":i,
//!                "index":j,"data":"<base64>" }
//!   end        { "type":"audiosync","action":"end","id":"..","ok":true }
//!   delete     { "type":"audiosync","action":"delete","id":"<soundId>" }
//!   clear      { "type":"audiosync","action":"clear" }
//!
//! 节流：每次定时器 tick 最多发送 CHUNKS_PER_TICK 个分块，向宿主让出控制权，
//!       避免一次性灌满 QAIC/BLE 发送队列导致死锁（同官方示例的坑）。
use std::future::IntoFuture;
use std::sync::atomic::{AtomicU64, Ordering};

use base64::Engine as _;
use serde_json::{json, Value};

use crate::astrobox::psys_host::{interconnect, register, thirdpartyapp, timer};
use crate::state::{self, SyncedSound, TransferUnit};

/// 定时器 payload 标记，用于识别本插件的传输定时器
pub const TRANSFER_TIMER_PAYLOAD: &str = "heihade-audiosync-transfer";
/// 页面导航定时器 payload 前缀（"heihade-nav:<page>"）
const NAV_PAYLOAD_PREFIX: &str = "heihade-nav:";
/// 同步开始定时器 payload
const SYNC_START_PAYLOAD: &str = "heihade-start-sync";

/// 每个分块承载的原始字节数（base64 后约 4000 字符）
const CHUNK_BYTES: usize = 3000;
/// 每个定时器 tick 发送的分块数
const CHUNKS_PER_TICK: usize = 4;
/// 定时器间隔（毫秒）
const INTERVAL_MS: u64 = 100;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// 为每个已连接设备注册 interconnect 接收，返回成功数量
pub fn register_all() -> usize {
    let devices = state::lock().devices.clone();
    if devices.is_empty() {
        return register_one("", state::PKG_NAME) as usize;
    }
    let mut ok = 0usize;
    for (addr, _) in &devices {
        if register_one(addr, state::PKG_NAME) {
            ok += 1;
        }
    }
    ok
}

fn register_one(addr: &str, pkg: &str) -> bool {
    match wit_bindgen::block_on(register::register_interconnect_recv(addr, pkg).into_future()) {
        Ok(()) => {
            tracing::info!("register interconnect-recv ok addr={} pkg={}", addr, pkg);
            true
        }
        Err(()) => {
            tracing::error!(
                "register interconnect-recv failed addr={} pkg={} (权限/授权?)",
                addr,
                pkg
            );
            false
        }
    }
}

/// 发送一条 JSON 消息到快应用
fn send_json(addr: &str, pkg: &str, value: &Value) -> bool {
    let text = value.to_string();
    let result = wit_bindgen::block_on(
        interconnect::send_qaic_message(addr, pkg, &text).into_future(),
    );
    match result {
        Ok(()) => {
            tracing::info!("interconnect send ok len={}", text.len());
            true
        }
        Err(()) => {
            tracing::error!("interconnect send failed addr={} pkg={}", addr, pkg);
            false
        }
    }
}

fn new_id() -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("heihade_{}_{}", ts, n)
}

/// 将字节切分为 base64 分块
fn chunk_base64(data: &[u8], chunk_bytes: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < data.len() {
        let end = (i + chunk_bytes).min(data.len());
        out.push(base64::engine::general_purpose::STANDARD.encode(&data[i..end]));
        i = end;
    }
    out
}

/// 文件名去扩展名
fn file_stem(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(stem, _)| stem.to_string())
        .unwrap_or_else(|| name.to_string())
}

/// 由待同步的音频文件 + 封面图片构建传输单元，返回（单元, 总块数, 总字节数）
fn build_units() -> (Vec<TransferUnit>, usize, usize) {
    let (files, image) = {
        let st = state::lock();
        (st.pending_files.clone(), st.image.clone())
    };
    let mut units: Vec<TransferUnit> = Vec::new();
    let mut total_chunks = 0usize;
    let mut total_bytes = 0usize;
    for f in files {
        if f.bytes.is_empty() {
            continue;
        }
        let chunks = chunk_base64(&f.bytes, CHUNK_BYTES);
        total_chunks += chunks.len();
        total_bytes += f.bytes.len();
        units.push(TransferUnit {
            kind: "audio".to_string(),
            file: f.name.clone(),
            duration: f.duration,
            cooldown: f.duration + 600, // 单个音频冷却 = 时长(ms) + 600ms
            chunks,
            sent: 0,
        });
    }
    if let Some(img) = image {
        if !img.bytes.is_empty() {
            let chunks = chunk_base64(&img.bytes, CHUNK_BYTES);
            total_chunks += chunks.len();
            total_bytes += img.bytes.len();
            units.push(TransferUnit {
                kind: "image".to_string(),
                file: img.name.clone(),
                duration: 0,
                cooldown: 0,
                chunks,
                sent: 0,
            });
        }
    }
    (units, total_chunks, total_bytes)
}

/// 中止传输（停表 + 更新状态）
fn abort_transfer(msg: &str) {
    let tid = {
        let mut st = state::lock();
        st.transfer.active = false;
        st.transfer.message = msg.to_string();
        st.notice = msg.to_string();
        st.transfer_timer_id.take()
    };
    if let Some(tid) = tid {
        let _ = wit_bindgen::block_on(timer::clear_timer(tid).into_future());
    }
    crate::ui::rerender();
}

/// 开始一次同步：先自动打开手表应用（默认页）→ 100ms 后发导航指令跳转同步页 → 250ms 后真正开始同步
/// custom_name：用户在插件端输入的自定义名称；None 或空串时回退到文件名
pub fn start_sync(custom_name: Option<String>) {
    let addr = state::selected_device().unwrap_or_default();
    if addr.is_empty() {
        state::set_notice("请先选择设备".to_string());
        return;
    }
    if state::lock().pending_files.is_empty() {
        state::set_notice("请先添加音频文件".to_string());
        return;
    }
    // 保存名称，供定时器触发真正同步时读取
    state::lock().pending_custom_name = custom_name;
    // 1. 自动打开手表应用（默认页）
    if !do_launch(&addr, "pages/start") {
        return;
    }
    // 2. 100ms 后发送页面跳转指令（任意页面可接收，跳到同步页）
    let _ = wit_bindgen::block_on(
        timer::set_timeout(100, &format!("{NAV_PAYLOAD_PREFIX}pages/custom")).into_future(),
    );
    // 3. 250ms 后真正开始同步（等待应用启动并跳转完成）
    let _ = wit_bindgen::block_on(timer::set_timeout(250, SYNC_START_PAYLOAD).into_future());
    state::set_notice("正在打开手表应用并准备同步…".to_string());
    crate::ui::rerender();
}

/// 真正执行同步：发送 start（含模式/展示/单元列表）、装配单元、启动节流定时器
fn do_start_sync() {
    let custom_name = state::lock().pending_custom_name.take();
    let (addr, mode, name, image_name, duration, cooldown) = {
        let mut st = state::lock();
        let Some(addr) = st.selected_device.clone() else {
            st.notice = "请先选择设备".to_string();
            return;
        };
        if st.pending_files.is_empty() {
            st.notice = "请先添加音频文件".to_string();
            return;
        }
        let mode = st.mode.clone();
        let first_file = st.pending_files[0].name.clone();
        // 自定义名称优先；留空或未输入则回退到文件名
        let name = match custom_name {
            Some(n) if !n.trim().is_empty() => n.trim().to_string(),
            _ => file_stem(&first_file),
        };
        let image_name = st.image.as_ref().map(|i| i.name.clone()).unwrap_or_default();
        let duration = st.pending_files[0].duration;
        let cooldown = duration + 600; // 单个音频冷却 = 时长(ms) + 600ms
        (addr, mode, name, image_name, duration, cooldown)
    };

    // 构建单元（纯计算，锁外进行）
    let (units, chunks_total, total_bytes) = build_units();
    if units.is_empty() {
        state::set_notice("没有可同步的文件".to_string());
        return;
    }

    let id = new_id();
    let total_steps = units.iter().filter(|u| u.kind == "audio").count();
    let display = if image_name.is_empty() { "text" } else { "image" };
    let units_json: Vec<Value> = units
        .iter()
        .map(|u| json!({ "kind": u.kind, "file": u.file, "duration": u.duration, "cooldown": u.cooldown }))
        .collect();

    let start_msg = json!({
        "type": "audiosync",
        "action": "start",
        "id": id,
        "name": name,
        "mode": mode,
        "display": display,
        "imageName": image_name,
        "duration": duration,
        "cooldown": cooldown,
        "totalSteps": total_steps,
        "bgText": "",
        "centerText": name,
        "chunks": chunks_total,
        "size": total_bytes,
        "units": units_json
    });
    if !send_json(&addr, state::PKG_NAME, &start_msg) {
        state::set_notice("发送 start 失败，请检查权限/连接".to_string());
        return;
    }

    {
        let mut st = state::lock();
        st.transfer = state::TransferInfo {
            active: true,
            id: id.clone(),
            name: name.clone(),
            chunks_total,
            chunks_sent: 0,
            message: format!("准备同步「{}」（共 {} 块）", name, chunks_total),
        };
        st.transfer_units = units;
        st.transfer_current_unit = 0;
        st.transfer_timer_id = None;
    }

    // 启动节流定时器（锁外）。set-interval 返回 future<u64>，直接取 timer id。
    let tid =
        wit_bindgen::block_on(timer::set_interval(INTERVAL_MS, TRANSFER_TIMER_PAYLOAD).into_future());
    {
        let mut st = state::lock();
        st.transfer_timer_id = Some(tid);
    }
    tracing::info!("transfer timer armed id={} chunks={}", tid, chunks_total);
    crate::ui::rerender();
}

/// 定时器 tick：发送下一批分块，全部完成后发送 end 并停表
pub fn on_timer_tick(payload: &str) {
    let parsed: Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(_) => return,
    };
    let inner = parsed
        .get("payload")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    // 页面导航：打开应用后 100ms 发送跳转指令（如跳到同步页）
    if let Some(page) = inner.strip_prefix(NAV_PAYLOAD_PREFIX) {
        send_nav(page);
        return;
    }
    // 同步开始：前置流程（打开应用 + 跳转）完成后真正开始同步
    if inner == SYNC_START_PAYLOAD {
        do_start_sync();
        return;
    }
    if inner != TRANSFER_TIMER_PAYLOAD {
        return;
    }

    // 1. 锁内取出当前单元与下一批数据
    let plan = {
        let st = state::lock();
        if !st.transfer.active {
            return;
        }
        let addr = st.selected_device.clone().unwrap_or_default();
        let id = st.transfer.id.clone();
        let ui = st.transfer_current_unit;
        if ui >= st.transfer_units.len() {
            return;
        }
        let unit = &st.transfer_units[ui];
        let kind = unit.kind.clone();
        let file = unit.file.clone();
        let unit_chunks_total = unit.chunks.len();
        let need_unit_start = unit.sent == 0;
        let start = unit.sent;
        let end = (start + CHUNKS_PER_TICK).min(unit_chunks_total);
        if start >= end || start >= unit.chunks.len() {
            return;
        }
        let batch = unit.chunks[start..end].to_vec();
        let unit_done = end >= unit_chunks_total;
        Some((
            addr, id, ui, kind, file, unit_chunks_total, need_unit_start, batch, start, unit_done,
        ))
    };
    let Some((
        addr,
        id,
        unit_index,
        kind,
        file,
        unit_chunks_total,
        need_unit_start,
        batch,
        base,
        unit_done,
    )) = plan
    else {
        return;
    };
    if addr.is_empty() {
        return;
    }

    // 2. 锁外发送：先 unit-start，再发送本批分块
    if need_unit_start {
        let start_msg = json!({
            "type": "audiosync",
            "action": "unit-start",
            "id": id,
            "unitIndex": unit_index,
            "kind": kind,
            "file": file,
            "chunks": unit_chunks_total
        });
        if !send_json(&addr, state::PKG_NAME, &start_msg) {
            abort_transfer("发送 unit-start 失败");
            return;
        }
    }
    let mut ok_all = true;
    for (offset, chunk) in batch.iter().enumerate() {
        let msg = json!({
            "type": "audiosync",
            "action": "chunk",
            "id": id,
            "unitIndex": unit_index,
            "index": base + offset,
            "data": chunk
        });
        if !send_json(&addr, state::PKG_NAME, &msg) {
            ok_all = false;
        }
    }

    // 3. 锁内更新进度
    let mut clear_timer: Option<u64> = None;
    let mut send_end = false;
    {
        let mut st = state::lock();
        st.transfer.chunks_sent += batch.len();
        if !ok_all {
            st.transfer.active = false;
            st.transfer.message = "同步中断（发送失败）".to_string();
            clear_timer = st.transfer_timer_id.take();
        } else {
            st.transfer_units[unit_index].sent = base + batch.len();
            if unit_done {
                st.transfer_current_unit += 1;
                if st.transfer_current_unit >= st.transfer_units.len() {
                    st.transfer.active = false;
                    st.transfer.message = "同步完成".to_string();
                    st.notice = format!("已完成同步「{}」", st.transfer.name);
                    clear_timer = st.transfer_timer_id.take();
                    send_end = true;
                } else {
                    st.transfer.message = format!(
                        "同步中 {}/{}",
                        st.transfer.chunks_sent, st.transfer.chunks_total
                    );
                }
            } else {
                st.transfer.message = format!(
                    "同步中 {}/{}",
                    st.transfer.chunks_sent, st.transfer.chunks_total
                );
            }
        }
    }

    // 4. 锁外停表 + 发送 end
    if let Some(tid) = clear_timer {
        let _ = wit_bindgen::block_on(timer::clear_timer(tid).into_future());
        tracing::info!("transfer timer cleared id={}", tid);
    }
    if send_end {
        let end_msg = json!({ "type": "audiosync", "action": "end", "id": id, "ok": true });
        send_json(&addr, state::PKG_NAME, &end_msg);
    }
    crate::ui::rerender();
}

/// 取消进行中的同步
pub fn cancel_sync() {
    let tid = {
        let mut st = state::lock();
        let tid = st.transfer_timer_id.take();
        st.transfer.active = false;
        st.transfer.message = "已取消".to_string();
        st.notice = "已取消同步".to_string();
        tid
    };
    if let Some(tid) = tid {
        let _ = wit_bindgen::block_on(timer::clear_timer(tid).into_future());
    }
    crate::ui::rerender();
}

/// 向快应用发送「删除指定音频」命令（按同步 id）
pub fn send_delete(sound_id: &str) {
    let addr = state::selected_device().unwrap_or_default();
    if addr.is_empty() {
        state::set_notice("请先选择设备".to_string());
        return;
    }
    let msg = json!({ "type": "audiosync", "action": "delete", "id": sound_id });
    if send_json(&addr, state::PKG_NAME, &msg) {
        state::set_notice(format!("已发送删除命令：{}", sound_id));
    } else {
        state::set_notice("删除命令发送失败".to_string());
    }
    crate::ui::rerender();
}

/// 向快应用发送「清空全部自定义音频」命令
pub fn send_clear() {
    let addr = state::selected_device().unwrap_or_default();
    if addr.is_empty() {
        state::set_notice("请先选择设备".to_string());
        return;
    }
    let msg = json!({ "type": "audiosync", "action": "clear" });
    if send_json(&addr, state::PKG_NAME, &msg) {
        state::set_notice("已发送清空命令".to_string());
    } else {
        state::set_notice("清空命令发送失败".to_string());
    }
    crate::ui::rerender();
}

/// 请求快应用重新上报清单（手动刷新列表）
pub fn request_manifest() {
    let addr = state::selected_device().unwrap_or_default();
    if addr.is_empty() {
        state::set_notice("请先选择设备".to_string());
        return;
    }
    let msg = json!({ "type": "audiosync", "action": "request-manifest" });
    if send_json(&addr, state::PKG_NAME, &msg) {
        state::set_notice("已请求刷新列表".to_string());
    } else {
        state::set_notice("刷新请求发送失败".to_string());
    }
    crate::ui::rerender();
}

/// 启动手表端快应用；open_sync_page=true 时打开应用后 100ms 发导航指令跳到同步页
pub fn launch_app(open_sync_page: bool) {
    let addr = state::selected_device().unwrap_or_default();
    if addr.is_empty() {
        state::set_notice("请先选择设备".to_string());
        return;
    }
    // 一律先打开应用默认页
    if !do_launch(&addr, "pages/start") {
        return;
    }
    if open_sync_page {
        // 100ms 后通过通信指令跳转同步页（任意页面可接收）
        let _ = wit_bindgen::block_on(
            timer::set_timeout(100, &format!("{NAV_PAYLOAD_PREFIX}pages/custom")).into_future(),
        );
        state::set_notice("已打开应用，正在跳转同步页…".to_string());
    } else {
        state::set_notice("已打开应用".to_string());
    }
    crate::ui::rerender();
}

/// 通过 interconnect 发送页面跳转指令（通用导航协议，非 audiosync 私有协议）
fn send_nav(page: &str) {
    let addr = state::selected_device().unwrap_or_default();
    if addr.is_empty() {
        return;
    }
    let msg = json!({ "type": "heihade-nav", "page": page });
    let _ = send_json(&addr, state::PKG_NAME, &msg);
}

/// 启动手表端应用到指定页面（返回是否成功）
fn do_launch(addr: &str, page: &str) -> bool {
    let app = thirdpartyapp::AppInfo {
        package_name: state::PKG_NAME.to_string(),
        fingerprint: vec![],
        version_code: 0,
        can_remove: false,
        app_name: "嘿哈嘚".to_string(),
    };
    match wit_bindgen::block_on(thirdpartyapp::launch_qa(addr, &app, page).into_future()) {
        Ok(()) => true,
        Err(()) => {
            state::set_notice("启动应用失败（未安装 / 权限？）".to_string());
            false
        }
    }
}

/// 处理快应用发来的消息（清单上报等）
pub fn on_incoming_message(payload: &str) {
    // 宿主用信封包装：{ addr, pkgName, payloadHex, payloadText }，payloadText 为原始消息
    let raw: String = serde_json::from_str::<Value>(payload)
        .ok()
        .and_then(|v| {
            v.get("payloadText")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
                .or_else(|| v.get("payload").map(|x| x.to_string()))
        })
        .unwrap_or_else(|| payload.to_string());

    let value: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => {
            tracing::info!("interconnect-message (non-json): {}", raw);
            return;
        }
    };
    if value.get("type").and_then(|x| x.as_str()) != Some("audiosync") {
        return;
    }
    match value.get("action").and_then(|x| x.as_str()) {
        Some("manifest") => {
            // 快应用上报同步清单 → 刷新插件侧已同步列表
            let mut sounds = Vec::new();
            if let Some(arr) = value.get("sounds").and_then(|x| x.as_array()) {
                for item in arr {
                    sounds.push(SyncedSound {
                        id: item.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                        name: item
                            .get("name")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string(),
                        mode: item
                            .get("mode")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string(),
                        file: item
                            .get("file")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string(),
                        size: item.get("size").and_then(|x| x.as_u64()).unwrap_or(0),
                    });
                }
            }
            state::set_synced_sounds(sounds);
            state::set_notice("已刷新手表端自定义音频列表".to_string());
            crate::ui::rerender();
        }
        other => {
            tracing::info!("interconnect-message action={:?}", other);
        }
    }
}
