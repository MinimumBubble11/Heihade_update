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

pub struct State {
    pub root_element_id: Option<String>,
    /// (设备地址, 设备名)
    pub devices: Vec<(String, String)>,
    pub selected_device: Option<String>,
    /// 播放模式："single" | "sequence"
    pub mode: String,
    pub pending_files: Vec<PendingFile>,
    pub image: Option<SelectedImage>,
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
                pending_files: Vec::new(),
                image: None,
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
    pub pending_files: Vec<(String, u32, usize)>,
    pub image_name: Option<String>,
    pub image_size: usize,
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
    Snapshot {
        devices: st.devices.clone(),
        selected_device: st.selected_device.clone(),
        mode: st.mode.clone(),
        pending_files: st
            .pending_files
            .iter()
            .map(|f| (f.name.clone(), f.duration, f.bytes.len()))
            .collect(),
        image_name: st.image.as_ref().map(|i| i.name.clone()),
        image_size: st.image.as_ref().map(|i| i.bytes.len()).unwrap_or(0),
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
