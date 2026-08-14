//! 插件全局状态（通过 OnceLock<Mutex> 保持跨事件调用共享）
use std::future::IntoFuture;
use std::sync::{Mutex, OnceLock};

use crate::astrobox::psys_host::device;

/// 目标快应用包名（必须与 src/manifest.json 的 package 一致）
pub const PKG_NAME: &str = "com.huashu.heihade";

/// 待同步的音频文件
#[derive(Clone)]
pub struct PendingFile {
    pub name: String,
    pub duration: u32,
    pub bytes: Vec<u8>,
}

/// 待同步的封面图片
#[derive(Clone)]
pub struct SelectedImage {
    pub name: String,
    pub bytes: Vec<u8>,
}

/// 快应用上已同步的音效（由快应用清单上报）
#[derive(Clone, Default)]
pub struct SyncedSound {
    pub id: String,
    pub name: String,
    pub mode: String,
    /// 播放节奏："mode1" | "mode2" | "random"
    pub play_pattern: String,
    pub file: String,
    pub size: u64,
}

/// 传输单元（音频或图片文件）
#[derive(Clone)]
pub struct TransferUnit {
    pub kind: String, // "audio" | "image"
    pub file: String,
    pub duration: u32,
    pub cooldown: u32, // 触发冷却（毫秒）= duration + 600
    pub size: usize, // 原始文件字节数（供快应用端完整性校验）
    pub chunks: Vec<String>, // base64 分块
    pub sent: usize,
}

#[derive(Clone, Default)]
pub struct TransferInfo {
    pub active: bool,
    pub id: String,
    pub name: String,
    pub chunks_total: usize,
    pub chunks_sent: usize,
    pub message: String,
}

/// 文件处理任务状态（封面压缩/转换），用于进度显示与同步门禁
#[derive(Clone)]
pub struct Processing {
    /// 任务类型："image" | "audio"
    pub kind: String,
    /// 正在处理的文件名
    pub name: String,
    /// 进度百分比 0-100
    pub percent: u8,
    /// 进度描述
    pub message: String,
}

pub struct State {
    pub root_element_id: Option<String>,
    /// (设备地址, 设备名)
    pub devices: Vec<(String, String)>,
    pub selected_device: Option<String>,
    /// 播放模式："single" | "sequence"
    pub mode: String,
    /// 播放节奏："mode1" | "mode2"（多音频 2 文件时生效）
    pub play_pattern: String,
    pub pending_files: Vec<PendingFile>,
    pub image: Option<SelectedImage>,
    /// 进行中的文件处理任务（None 表示空闲）
    pub processing: Option<Processing>,
    /// 处理任务代次，用于忽略过期阶段回调
    pub process_gen: u64,
    pub transfer: TransferInfo,
    pub transfer_units: Vec<TransferUnit>,
    pub transfer_current_unit: usize,
    pub transfer_timer_id: Option<u64>,
    /// 待真正同步时使用的自定义名称（由定时器触发读取）
    pub pending_custom_name: Option<String>,
    pub synced_sounds: Vec<SyncedSound>,
    pub notice: String,
}

static STATE: OnceLock<Mutex<State>> = OnceLock::new();

pub fn lock() -> std::sync::MutexGuard<'static, State> {
    STATE
        .get_or_init(|| {
            Mutex::new(State {
                root_element_id: None,
                devices: Vec::new(),
                selected_device: None,
                mode: "single".to_string(),
                play_pattern: "mode1".to_string(),
                pending_files: Vec::new(),
                image: None,
                processing: None,
                process_gen: 0,
                transfer: TransferInfo::default(),
                transfer_units: Vec::new(),
                transfer_current_unit: 0,
                transfer_timer_id: None,
                pending_custom_name: None,
                synced_sounds: Vec::new(),
                notice: String::new(),
            })
        })
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 供 UI 读取的只读快照，避免在构建元素时长时间持有锁
#[derive(Clone, Default)]
pub struct Snapshot {
    pub devices: Vec<(String, String)>,
    pub selected_device: Option<String>,
    pub mode: String,
    /// 播放节奏："mode1" | "mode2"
    pub play_pattern: String,
    /// 节奏选项是否可选（恰好 2 音频）
    pub can_choose_pattern: bool,
    pub pending_files: Vec<(String, u32, usize)>,
    pub image_name: Option<String>,
    pub image_size: usize,
    /// 是否有文件正在处理（封面压缩/转换中）
    pub processing: bool,
    pub process_percent: u8,
    pub process_message: String,
    pub transfer_active: bool,
    pub transfer_name: String,
    pub transfer_message: String,
    pub chunks_total: usize,
    pub chunks_sent: usize,
    pub synced_sounds: Vec<SyncedSound>,
    pub notice: String,
}

pub fn snapshot() -> Snapshot {
    let st = lock();
    let transfer = st.transfer.clone();
    let audio_count = st.pending_files.len();
    Snapshot {
        devices: st.devices.clone(),
        selected_device: st.selected_device.clone(),
        mode: st.mode.clone(),
        play_pattern: st.play_pattern.clone(),
        can_choose_pattern: audio_count == 2,
        pending_files: st
            .pending_files
            .iter()
            .map(|f| (f.name.clone(), f.duration, f.bytes.len()))
            .collect(),
        image_name: st.image.as_ref().map(|i| i.name.clone()),
        image_size: st.image.as_ref().map(|i| i.bytes.len()).unwrap_or(0),
        processing: st.processing.is_some(),
        process_percent: st.processing.as_ref().map(|p| p.percent).unwrap_or(0),
        process_message: st
            .processing
            .as_ref()
            .map(|p| p.message.clone())
            .unwrap_or_default(),
        transfer_active: transfer.active,
        transfer_name: transfer.name,
        transfer_message: transfer.message,
        chunks_total: transfer.chunks_total,
        chunks_sent: transfer.chunks_sent,
        synced_sounds: st.synced_sounds.clone(),
        notice: st.notice.clone(),
    }
}

pub fn set_root(element_id: &str) {
    lock().root_element_id = Some(element_id.to_string());
}

pub fn root() -> Option<String> {
    lock().root_element_id.clone()
}

pub fn set_notice(msg: String) {
    lock().notice = msg;
}

/// 刷新已连接设备列表（block_on 宿主调用）
pub fn refresh_devices() {
    let list = wit_bindgen::block_on(device::get_connected_device_list().into_future());
    let devices: Vec<(String, String)> = list
        .into_iter()
        .map(|d| (d.addr, d.name))
        .collect();
    let mut st = lock();
    st.devices = devices;
    let keep = st
        .selected_device
        .as_ref()
        .filter(|sel| st.devices.iter().any(|(a, _)| a.as_str() == sel.as_str()))
        .cloned();
    st.selected_device = keep.or_else(|| st.devices.first().map(|(a, _)| a.clone()));
    if st.devices.is_empty() {
        st.notice = "未检测到已连接设备".to_string();
    }
}

pub fn set_selected_device(addr: String) {
    lock().selected_device = Some(addr);
}

pub fn selected_device() -> Option<String> {
    lock().selected_device.clone()
}

pub fn set_mode(mode: String) {
    let mut st = lock();
    st.mode = if mode == "sequence" {
        "sequence".to_string()
    } else {
        "single".to_string()
    };
    // 单音频模式仅保留第一个文件
    if st.mode == "single" && st.pending_files.len() > 1 {
        st.pending_files.truncate(1);
    }
}

/// 设置播放节奏："mode1" | "mode2"
pub fn set_play_pattern(pattern: String) {
    let mut st = lock();
    st.play_pattern = if pattern == "mode2" { "mode2" } else { "mode1" }.to_string();
}

pub fn add_pending_file(name: String, bytes: Vec<u8>) {
    let mut st = lock();
    // 真实时长：MP3 帧头解析（失败自动回退 128kbps 估算），用于冷却计算与播放兜底
    let duration = crate::mp3::parse_duration_ms(&bytes);
    if st.mode == "single" {
        st.pending_files.clear();
    }
    st.pending_files.push(PendingFile {
        name,
        duration,
        bytes,
    });
}

pub fn remove_pending_file(index: usize) {
    let mut st = lock();
    if index < st.pending_files.len() {
        st.pending_files.remove(index);
    }
}

pub fn clear_pending() {
    let mut st = lock();
    st.pending_files.clear();
    st.image = None;
}

pub fn set_image(name: String, bytes: Vec<u8>) {
    lock().image = Some(SelectedImage { name, bytes });
}

pub fn remove_image() {
    lock().image = None;
}

pub fn set_synced_sounds(sounds: Vec<SyncedSound>) {
    lock().synced_sounds = sounds;
}

/// 开始一个文件处理任务，返回代次 gen（用于忽略过期阶段回调）
pub fn start_processing(kind: &str, name: &str) -> u64 {
    let mut st = lock();
    st.process_gen += 1;
    st.processing = Some(Processing {
        kind: kind.to_string(),
        name: name.to_string(),
        percent: 0,
        message: String::new(),
    });
    st.process_gen
}

/// 更新处理进度；若 gen 不匹配（任务已过期）返回 false，调用方应中止
pub fn update_processing(gen: u64, percent: u8, message: &str) -> bool {
    let mut st = lock();
    if st.process_gen != gen {
        return false;
    }
    if let Some(p) = st.processing.as_mut() {
        p.percent = percent;
        p.message = message.to_string();
        true
    } else {
        false
    }
}

/// 结束处理任务；gen 不匹配时返回 false
pub fn finish_processing(gen: u64) -> bool {
    let mut st = lock();
    if st.process_gen != gen {
        return false;
    }
    st.processing = None;
    true
}

/// 是否有文件正在处理（用于同步门禁）
pub fn is_processing() -> bool {
    lock().processing.is_some()
}
