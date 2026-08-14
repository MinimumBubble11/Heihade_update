//! 插件 UI（ui-v3）与事件处理
use std::future::IntoFuture;

use crate::astrobox::psys_host::{dialog, ui_v3 as ui};
use crate::exports::astrobox::psys_plugin::event_v3 as event;
use crate::media;
use crate::state;
use crate::transfer;

const EVENT_REFRESH_DEVICES: &str = "action:refresh-devices";
const EVENT_ADD_FILE: &str = "action:add-file";
const EVENT_REMOVE_FILE_PREFIX: &str = "action:remove-file:";
const EVENT_PICK_IMAGE: &str = "action:pick-image";
const EVENT_REMOVE_IMAGE: &str = "action:remove-image";
const EVENT_SET_MODE_PREFIX: &str = "action:mode:";
const EVENT_SET_PATTERN_PREFIX: &str = "action:pattern:";
const EVENT_START_SYNC: &str = "action:start-sync";
const EVENT_CANCEL_SYNC: &str = "action:cancel-sync";
const EVENT_DELETE_SOUND_PREFIX: &str = "action:delete-sound:";
const EVENT_CLEAR_AUDIO: &str = "action:clear-audio";
const EVENT_PICK_DEVICE_PREFIX: &str = "action:pick-device:";
const EVENT_REFRESH_SYNCED: &str = "action:refresh-synced";
const EVENT_LAUNCH_APP: &str = "action:launch-app";
const EVENT_LAUNCH_SYNC_PAGE: &str = "action:launch-sync-page";
// 音频处理工具（第三方网站，与作者无关，仅供参考）
const EVENT_TOOL_VIDEO2AUDIO: &str = "action:tool-video2audio";
const EVENT_TOOL_AUDIO_COMPRESS: &str = "action:tool-audio-compress";
const EVENT_TOOL_AUDIO_TRIMMER: &str = "action:tool-audio-trimmer";

// 第三方在线工具（仅为建议，与插件作者无任何关系）
const TOOL_VIDEO2AUDIO_URL: &str = "https://toolshu.com/video-to-audio";
const TOOL_AUDIO_COMPRESS_URL: &str = "https://toolshu.com/audio-compressor";
const TOOL_AUDIO_TRIMMER_URL: &str = "https://toolshu.com/audio-trimmer";

/// 音频文件建议压缩的体积阈值（MB）；超过则提示（非硬性要求）
const AUDIO_BIG_THRESHOLD_MB: f64 = 2.0;

// 深色配色，与「嘿哈嘚」手表端风格一致
const COLOR_TEXT: &str = "#f4f4f5";
const COLOR_MUTED: &str = "#8a8a8a";
const COLOR_ACCENT: &str = "#4ccfff";
const COLOR_OK: &str = "#4ade80";
const COLOR_DANGER: &str = "#f87171";
const COLOR_BTN_BG: &str = "#2563eb";
const COLOR_BTN_DANGER_BG: &str = "#3f1d1d";
const COLOR_DIVIDER: &str = "#27272a";

const AUDIO_EXTENSIONS: &[&str] = &["mp3", "wav", "ogg", "m4a", "aac", "flac", "wma"];
// 视频扩展名：选中后引导用户先转成 MP3 再添加（本插件不做转码，仅为建议）
const VIDEO_EXTENSIONS: &[&str] = &["mp4", "mov", "avi", "mkv", "flv", "webm", "m4v", "wmv", "3gp", "ts"];
// Vela <image> 组件仅支持 png/jpg（jpeg 兼容），过滤掉 webp/gif 避免“图写对了却显示不出来”
const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg"];

pub fn render_main_ui(element_id: &str) {
    state::set_root(element_id);
    rerender();
}

pub fn rerender() {
    let Some(root) = state::root() else {
        return;
    };
    ui::render(&root, build_main_ui());
}

pub fn ui_event_processor(_evtype: event::Event, event_id: &str, _payload_raw: &str) {
    match event_id {
        EVENT_REFRESH_DEVICES => {
            state::refresh_devices();
            transfer::register_all();
            state::set_notice("设备列表已刷新".to_string());
        }
        EVENT_ADD_FILE => {
            pick_audio_file();
        }
        EVENT_PICK_IMAGE => {
            pick_image_file();
        }
        EVENT_REMOVE_IMAGE => {
            state::remove_image();
            state::set_notice("封面已移除".to_string());
        }
        EVENT_START_SYNC => {
            // 弹输入框为本次同步命名；确认后开始（留空回退文件名），取消则不开始
            if let Some(name) = prompt_sync_name() {
                transfer::start_sync(Some(name));
            }
        }
        EVENT_CANCEL_SYNC => {
            transfer::cancel_sync();
        }
        EVENT_CLEAR_AUDIO => {
            transfer::send_clear();
        }
        EVENT_REFRESH_SYNCED => {
            transfer::request_manifest();
        }
        EVENT_LAUNCH_APP => {
            transfer::launch_app(false);
        }
        EVENT_LAUNCH_SYNC_PAGE => {
            transfer::launch_app(true);
        }
        // 音频处理工具：打开第三方网站（与作者无关，仅供参考）
        EVENT_TOOL_VIDEO2AUDIO => {
            dialog::open_url(TOOL_VIDEO2AUDIO_URL);
            state::set_notice("已打开「视频转音频」工具（第三方，与作者无关）".to_string());
        }
        EVENT_TOOL_AUDIO_COMPRESS => {
            dialog::open_url(TOOL_AUDIO_COMPRESS_URL);
            state::set_notice("已打开「音频压缩」工具（第三方，与作者无关）".to_string());
        }
        EVENT_TOOL_AUDIO_TRIMMER => {
            dialog::open_url(TOOL_AUDIO_TRIMMER_URL);
            state::set_notice("已打开「音频剪辑」工具（第三方，与作者无关）".to_string());
        }
        _ => {
            if let Some(addr) = event_id.strip_prefix(EVENT_PICK_DEVICE_PREFIX) {
                state::set_selected_device(addr.to_string());
                state::set_notice("设备已切换".to_string());
            } else if let Some(mode) = event_id.strip_prefix(EVENT_SET_MODE_PREFIX) {
                state::set_mode(mode.to_string());
                let msg = if mode == "sequence" {
                    "已切换为多音频模式".to_string()
                } else {
                    "已切换为单音频模式".to_string()
                };
                state::set_notice(msg);
            } else if let Some(pattern) = event_id.strip_prefix(EVENT_SET_PATTERN_PREFIX) {
                state::set_play_pattern(pattern.to_string());
                let label = if pattern == "mode2" { "模式2 (1,1,1,2)" } else { "模式1 (1,2)" };
                state::set_notice(format!("播放节奏已更新：{}", label));
            } else if let Some(index) = event_id.strip_prefix(EVENT_REMOVE_FILE_PREFIX) {
                if let Ok(i) = index.parse::<usize>() {
                    state::remove_pending_file(i);
                }
            } else if let Some(sound_id) = event_id.strip_prefix(EVENT_DELETE_SOUND_PREFIX) {
                transfer::send_delete(sound_id);
            }
        }
    }
    rerender();
}

/// 弹出输入框让用户为本次同步命名；确认返回名称（可为空串→用文件名），取消返回 None
fn prompt_sync_name() -> Option<String> {
    let ret = wit_bindgen::block_on(
        dialog::show_dialog(
            dialog::DialogType::Input,
            dialog::DialogStyle::Website,
            &dialog::DialogInfo {
                title: "同步名称".to_string(),
                content: "输入音频名称（留空使用文件名）".to_string(),
                buttons: vec![
                    dialog::DialogButton {
                        id: "confirm".to_string(),
                        primary: true,
                        content: "确认".to_string(),
                    },
                    dialog::DialogButton {
                        id: "cancel".to_string(),
                        primary: false,
                        content: "取消".to_string(),
                    },
                ],
            },
        )
        .into_future(),
    );
    if ret.clicked_btn_id != "confirm" {
        return None;
    }
    Some(ret.input_result.trim().to_string())
}

/// 调用宿主文件选择对话框（按扩展名过滤），返回 (文件名, 字节)
fn pick_with_extensions(exts: &[&str]) -> Option<(String, Vec<u8>)> {
    let config = dialog::PickConfig {
        read: true,
        copy_to: None,
    };
    let filter = dialog::FilterConfig {
        multiple: false,
        extensions: exts.iter().map(|s| s.to_string()).collect(),
        default_directory: String::new(),
        default_file_name: String::new(),
    };
    let result = wit_bindgen::block_on(dialog::pick_file(&config, &filter).into_future());
    if result.name.is_empty() {
        state::set_notice("未选择文件".to_string());
        return None;
    }
    Some((result.name, result.data))
}

/// 取小写扩展名（无扩展名返回空串）
fn file_extension(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, ext)| ext.to_lowercase())
        .unwrap_or_default()
}

fn pick_audio_file() {
    // 允许选择视频格式，以便检测到视频时引导用户转成 MP3
    let mut exts: Vec<&str> = AUDIO_EXTENSIONS.to_vec();
    exts.extend_from_slice(VIDEO_EXTENSIONS);
    let Some((name, data)) = pick_with_extensions(&exts) else {
        return;
    };
    let ext = file_extension(&name);
    if VIDEO_EXTENSIONS.contains(&ext.as_str()) {
        tracing::info!("picked video (unsupported): {} bytes={}", name, data.len());
        prompt_video_to_audio();
        state::set_notice("视频未添加：请先转成 MP3 后再导入".to_string());
        return;
    }
    tracing::info!("picked audio: {} bytes={}", name, data.len());
    let size_mb = data.len() as f64 / (1024.0 * 1024.0);
    state::add_pending_file(name.clone(), data);
    if size_mb > AUDIO_BIG_THRESHOLD_MB {
        // 非硬性要求：仅提示建议压缩（第三方工具见「音频工具」区块）
        state::set_notice(format!(
            "已添加音频：{}（{:.1}MB）。文件较大，建议压缩后再同步以节省空间（工具见「音频工具」，与作者无任何关系，仅供参考）",
            name, size_mb
        ));
    } else {
        state::set_notice(format!("已添加音频：{}", name));
    }
}

/// 检测到视频文件时弹窗引导：跳转在线工具转 MP3（第三方，仅建议）
fn prompt_video_to_audio() {
    let ret = wit_bindgen::block_on(
        dialog::show_dialog(
            dialog::DialogType::Alert,
            dialog::DialogStyle::System,
            &dialog::DialogInfo {
                title: "检测到视频文件".to_string(),
                content: "请先用在线工具把视频转成 MP3 后再添加。\n（第三方网站，与插件作者无任何关系，仅供参考）".to_string(),
                buttons: vec![
                    dialog::DialogButton {
                        id: "open".to_string(),
                        primary: true,
                        content: "打开工具".to_string(),
                    },
                    dialog::DialogButton {
                        id: "cancel".to_string(),
                        primary: false,
                        content: "取消".to_string(),
                    },
                ],
            },
        )
        .into_future(),
    );
    if ret.clicked_btn_id == "open" {
        dialog::open_url(TOOL_VIDEO2AUDIO_URL);
    }
}

fn pick_image_file() {
    let Some((name, data)) = pick_with_extensions(IMAGE_EXTENSIONS) else {
        return;
    };
    tracing::info!("picked image: {} bytes={}", name, data.len());
    // 异步处理：JPG→PNG + 最长边 250px + 体积压缩（处理中显示进度，完成后才可同步）
    media::start_image_processing(name, data);
}

fn build_main_ui() -> ui::Element {
    let snap = state::snapshot();

    let mut root = ui::Element::new(ui::ElementType::Div, None)
        .flex()
        .flex_direction(ui::FlexDirection::Column)
        .width_full()
        .padding(24)
        .gap(12);

    root = root.child(
        ui::Element::new(ui::ElementType::P, Some("嘿哈嘚 · 音频同步"))
            .size(26)
            .text_color(COLOR_TEXT),
    );
    root = root.child(device_section(&snap));
    root = root.child(divider());
    root = root.child(app_section(&snap));
    root = root.child(divider());
    root = root.child(mode_section(&snap));
    root = root.child(divider());
    // 播放节奏 + 随机（仅多音频模式）
    if snap.mode == "sequence" {
        root = root.child(pattern_section(&snap));
        root = root.child(divider());
    }
    root = root.child(files_section(&snap));
    root = root.child(divider());
    root = root.child(tools_section());
    root = root.child(divider());
    root = root.child(image_section(&snap));
    root = root.child(divider());
    root = root.child(sync_section(&snap));
    root = root.child(divider());
    root = root.child(synced_section(&snap));
    if !snap.notice.is_empty() {
        root = root.child(notice_line(&snap.notice));
    }

    root
}

fn divider() -> ui::Element {
    ui::Element::new(ui::ElementType::Div, None)
        .width_full()
        .height(1)
        .bg(COLOR_DIVIDER)
}

fn section_title(text: &str) -> ui::Element {
    ui::Element::new(ui::ElementType::P, Some(text))
        .size(16)
        .text_color(COLOR_ACCENT)
}

fn device_section(snap: &state::Snapshot) -> ui::Element {
    let mut col = ui::Element::new(ui::ElementType::Div, None)
        .flex()
        .flex_direction(ui::FlexDirection::Column)
        .width_full()
        .gap(8);

    col = col.child(section_title("设备"));

    if snap.devices.is_empty() {
        col = col.child(
            ui::Element::new(ui::ElementType::P, Some("未连接任何设备"))
                .size(14)
                .text_color(COLOR_MUTED),
        );
    } else {
        for (addr, name) in &snap.devices {
            let selected = snap.selected_device.as_ref() == Some(addr);
            let label = if selected {
                format!("● {} （{addr}）", name)
            } else {
                format!("{} （{addr}）", name)
            };
            let color = if selected { COLOR_ACCENT } else { COLOR_TEXT };
            let btn = ui::Element::new(ui::ElementType::Button, Some(label.as_str()))
                .bg(COLOR_BTN_BG)
                .text_color(color)
                .on(ui::Event::Click, &format!("{EVENT_PICK_DEVICE_PREFIX}{addr}"));
            col = col.child(btn);
        }
    }

    col.child(
        ui::Element::new(ui::ElementType::Button, Some("刷新设备"))
            .bg(COLOR_BTN_BG)
            .text_color(COLOR_TEXT)
            .on(ui::Event::Click, EVENT_REFRESH_DEVICES),
    )
}

/// 手表应用入口：打开应用 / 直接跳转同步页（同行显示）
fn app_section(snap: &state::Snapshot) -> ui::Element {
    let mut col = ui::Element::new(ui::ElementType::Div, None)
        .flex()
        .flex_direction(ui::FlexDirection::Column)
        .width_full()
        .gap(8);

    col = col.child(section_title("手表应用"));

    let mut row = ui::Element::new(ui::ElementType::Div, None)
        .flex()
        .flex_direction(ui::FlexDirection::Row)
        .width_full()
        .gap(8);

    let can_launch = snap.selected_device.is_some();
    let mut open_btn = ui::Element::new(ui::ElementType::Button, Some("打开应用"))
        .bg(COLOR_BTN_BG)
        .text_color(COLOR_TEXT)
        .on(ui::Event::Click, EVENT_LAUNCH_APP);
    let mut sync_btn = ui::Element::new(ui::ElementType::Button, Some("同步页"))
        .bg(COLOR_BTN_BG)
        .text_color(COLOR_TEXT)
        .on(ui::Event::Click, EVENT_LAUNCH_SYNC_PAGE);
    if !can_launch {
        open_btn = open_btn.disabled();
        sync_btn = sync_btn.disabled();
    }
    row = row.child(open_btn);
    row = row.child(sync_btn);
    col.child(row)
}

fn mode_section(snap: &state::Snapshot) -> ui::Element {
    let mut col = ui::Element::new(ui::ElementType::Div, None)
        .flex()
        .flex_direction(ui::FlexDirection::Column)
        .width_full()
        .gap(8);

    col = col.child(section_title("播放模式"));

    let mut row = ui::Element::new(ui::ElementType::Div, None)
        .flex()
        .flex_direction(ui::FlexDirection::Row)
        .width_full()
        .gap(8);

    let single_sel = snap.mode != "sequence";
    let single_label = if single_sel { "● 单音频" } else { "单音频" };
    let single_color = if single_sel { COLOR_ACCENT } else { COLOR_TEXT };
    row = row.child(
        ui::Element::new(ui::ElementType::Button, Some(single_label))
            .bg(COLOR_BTN_BG)
            .text_color(single_color)
            .on(ui::Event::Click, &format!("{EVENT_SET_MODE_PREFIX}single")),
    );

    let multi_sel = snap.mode == "sequence";
    let multi_label = if multi_sel { "● 多音频" } else { "多音频" };
    let multi_color = if multi_sel { COLOR_ACCENT } else { COLOR_TEXT };
    row = row.child(
        ui::Element::new(ui::ElementType::Button, Some(multi_label))
            .bg(COLOR_BTN_BG)
            .text_color(multi_color)
            .on(ui::Event::Click, &format!("{EVENT_SET_MODE_PREFIX}sequence")),
    );

    col = col.child(row);
    let hint = if snap.mode == "sequence" {
        "多音频：多段音效循环（带进度条）"
    } else {
        "单音频：单段音效（冷却触发）"
    };
    col.child(
        ui::Element::new(ui::ElementType::P, Some(hint))
            .size(13)
            .text_color(COLOR_MUTED),
    )
}

/// 播放节奏（模式1/模式2，仅 2 音频可选）+ 随机播放开关（≥2 音频）
fn pattern_section(snap: &state::Snapshot) -> ui::Element {
    let mut col = ui::Element::new(ui::ElementType::Div, None)
        .flex()
        .flex_direction(ui::FlexDirection::Column)
        .width_full()
        .gap(8);

    col = col.child(section_title("播放节奏"));

    // 模式1 / 模式2（不足 2 音频时置灰）。随机播放由手表端节奏页设置，插件端不提供。
    let mut row = ui::Element::new(ui::ElementType::Div, None)
        .flex()
        .flex_direction(ui::FlexDirection::Row)
        .width_full()
        .gap(8);

    let mode1_sel = snap.play_pattern != "mode2";
    let mode2_sel = snap.play_pattern == "mode2";
    let pattern_enabled = snap.can_choose_pattern;

    let m1_label = if mode1_sel { "● 模式1 (1,2)" } else { "模式1 (1,2)" };
    let m1_color = if mode1_sel { COLOR_ACCENT } else { COLOR_TEXT };
    let mut m1 = ui::Element::new(ui::ElementType::Button, Some(m1_label))
        .bg(COLOR_BTN_BG)
        .text_color(m1_color)
        .on(ui::Event::Click, &format!("{EVENT_SET_PATTERN_PREFIX}mode1"));

    let m2_label = if mode2_sel { "● 模式2 (1,1,1,2)" } else { "模式2 (1,1,1,2)" };
    let m2_color = if mode2_sel { COLOR_ACCENT } else { COLOR_TEXT };
    let mut m2 = ui::Element::new(ui::ElementType::Button, Some(m2_label))
        .bg(COLOR_BTN_BG)
        .text_color(m2_color)
        .on(ui::Event::Click, &format!("{EVENT_SET_PATTERN_PREFIX}mode2"));

    if !pattern_enabled {
        m1 = m1.disabled();
        m2 = m2.disabled();
    }
    row = row.child(m1);
    row = row.child(m2);
    col = col.child(row);

    // 提示
    let hint = if !snap.can_choose_pattern {
        "播放节奏需恰好 2 个音频"
    } else {
        "模式1：依次播放 (1,2)；模式2：Combo (1,1,1,2)"
    };
    col.child(
        ui::Element::new(ui::ElementType::P, Some(hint))
            .size(13)
            .text_color(COLOR_MUTED),
    )
}

fn files_section(snap: &state::Snapshot) -> ui::Element {
    let mut col = ui::Element::new(ui::ElementType::Div, None)
        .flex()
        .flex_direction(ui::FlexDirection::Column)
        .width_full()
        .gap(8);

    col = col.child(section_title("音频文件"));

    if snap.pending_files.is_empty() {
        col = col.child(
            ui::Element::new(ui::ElementType::P, Some("尚未添加音频文件"))
                .size(14)
                .text_color(COLOR_MUTED),
        );
    } else {
        for (i, (name, _dur, size)) in snap.pending_files.iter().enumerate() {
            let line = format!("✕ {}（{}B）", name, size);
            let btn = ui::Element::new(ui::ElementType::Button, Some(line.as_str()))
                .bg(COLOR_BTN_DANGER_BG)
                .text_color(COLOR_DANGER)
                .on(ui::Event::Click, &format!("{EVENT_REMOVE_FILE_PREFIX}{}", i));
            col = col.child(btn);
        }
    }

    col.child(
        ui::Element::new(ui::ElementType::Button, Some("添加音频文件"))
            .bg(COLOR_BTN_BG)
            .text_color(COLOR_TEXT)
            .on(ui::Event::Click, EVENT_ADD_FILE),
    )
}

/// 音频处理工具（第三方在线工具，与作者无任何关系，仅作建议）
fn tools_section() -> ui::Element {
    let mut col = ui::Element::new(ui::ElementType::Div, None)
        .flex()
        .flex_direction(ui::FlexDirection::Column)
        .width_full()
        .gap(8);

    col = col.child(section_title("音频工具（可选）"));
    col = col.child(
        ui::Element::new(
            ui::ElementType::P,
            Some("以下为第三方在线工具，与插件作者无任何关系，仅供参考："),
        )
        .size(12)
        .text_color(COLOR_MUTED),
    );

    let mut row = ui::Element::new(ui::ElementType::Div, None)
        .flex()
        .flex_direction(ui::FlexDirection::Row)
        .width_full()
        .gap(8);

    row = row.child(
        ui::Element::new(ui::ElementType::Button, Some("视频转音频"))
            .bg(COLOR_BTN_BG)
            .text_color(COLOR_TEXT)
            .on(ui::Event::Click, EVENT_TOOL_VIDEO2AUDIO),
    );
    row = row.child(
        ui::Element::new(ui::ElementType::Button, Some("音频压缩"))
            .bg(COLOR_BTN_BG)
            .text_color(COLOR_TEXT)
            .on(ui::Event::Click, EVENT_TOOL_AUDIO_COMPRESS),
    );
    row = row.child(
        ui::Element::new(ui::ElementType::Button, Some("音频剪辑"))
            .bg(COLOR_BTN_BG)
            .text_color(COLOR_TEXT)
            .on(ui::Event::Click, EVENT_TOOL_AUDIO_TRIMMER),
    );

    col.child(row)
}

fn image_section(snap: &state::Snapshot) -> ui::Element {
    let mut col = ui::Element::new(ui::ElementType::Div, None)
        .flex()
        .flex_direction(ui::FlexDirection::Column)
        .width_full()
        .gap(8);

    col = col.child(section_title("封面图片（可选）"));

    // 封面处理中：显示进度（PROGRESS + 百分比），并提供重新选择（覆盖旧任务）。
    // 处理期间隐藏「已选择/移除」区域，避免与处理结果冲突。
    if snap.processing {
        col = col.child(
            ui::Element::new(
                ui::ElementType::P,
                Some(
                    format!("处理封面：{} {}%", snap.process_message, snap.process_percent)
                        .as_str(),
                ),
            )
            .size(14)
            .text_color(COLOR_ACCENT),
        );
        col = col.child(
            ui::Element::new(
                ui::ElementType::Progress,
                Some(format!("{}%", snap.process_percent).as_str()),
            )
            .prop("value", &snap.process_percent.to_string())
            .width_full(),
        );
        col = col.child(
            ui::Element::new(ui::ElementType::Button, Some("重新选择封面"))
                .bg(COLOR_BTN_BG)
                .text_color(COLOR_TEXT)
                .on(ui::Event::Click, EVENT_PICK_IMAGE),
        );
        return col;
    }

    match &snap.image_name {
        Some(name) => {
            col = col.child(
                ui::Element::new(
                    ui::ElementType::P,
                    Some(format!("已选择：{}（{}B）", name, snap.image_size).as_str()),
                )
                .size(14)
                .text_color(COLOR_OK),
            );
            col = col.child(
                ui::Element::new(ui::ElementType::Button, Some("移除封面"))
                    .bg(COLOR_BTN_DANGER_BG)
                    .text_color(COLOR_DANGER)
                    .on(ui::Event::Click, EVENT_REMOVE_IMAGE),
            );
        }
        None => {
            col = col.child(
                ui::Element::new(ui::ElementType::P, Some("未选择封面（将使用文字展示）"))
                    .size(14)
                    .text_color(COLOR_MUTED),
            );
            col = col.child(
                ui::Element::new(ui::ElementType::Button, Some("选择封面图片"))
                    .bg(COLOR_BTN_BG)
                    .text_color(COLOR_TEXT)
                    .on(ui::Event::Click, EVENT_PICK_IMAGE),
            );
        }
    }
    col
}

fn sync_section(snap: &state::Snapshot) -> ui::Element {
    let mut col = ui::Element::new(ui::ElementType::Div, None)
        .flex()
        .flex_direction(ui::FlexDirection::Column)
        .width_full()
        .gap(8);

    let status = if snap.transfer_active {
        format!(
            "{}（{}/{}）",
            snap.transfer_message, snap.chunks_sent, snap.chunks_total
        )
    } else {
        snap.transfer_message.clone()
    };
    col = col.child(
        ui::Element::new(
            ui::ElementType::P,
            if snap.transfer_active {
                Some(status.as_str())
            } else if status.is_empty() {
                Some("等待同步")
            } else {
                Some(status.as_str())
            },
        )
        .size(14)
        .text_color(if snap.transfer_active {
            COLOR_ACCENT
        } else {
            COLOR_MUTED
        }),
    );

    // 门禁：必须选择设备、有待同步音频、无传输进行中，且无文件正在处理（完成后才可同步）
    let can_sync = snap.selected_device.is_some()
        && !snap.pending_files.is_empty()
        && !snap.transfer_active
        && !snap.processing;
    let sync_label = if snap.processing {
        "正在处理文件…"
    } else if can_sync {
        "同步到手表"
    } else {
        "选择设备与音频后同步"
    };
    let mut sync_btn = ui::Element::new(ui::ElementType::Button, Some(sync_label))
        .bg(COLOR_BTN_BG)
        .text_color(COLOR_TEXT);
    if !can_sync {
        sync_btn = sync_btn.disabled();
    }
    sync_btn = sync_btn.on(ui::Event::Click, EVENT_START_SYNC);
    col = col.child(sync_btn);
    if snap.transfer_active {
        col = col.child(
            ui::Element::new(ui::ElementType::Button, Some("取消同步"))
                .bg(COLOR_BTN_DANGER_BG)
                .text_color(COLOR_DANGER)
                .on(ui::Event::Click, EVENT_CANCEL_SYNC),
        );
    }
    col
}

fn synced_section(snap: &state::Snapshot) -> ui::Element {
    let mut col = ui::Element::new(ui::ElementType::Div, None)
        .flex()
        .flex_direction(ui::FlexDirection::Column)
        .width_full()
        .gap(8);

    col = col.child(section_title("手表端已同步"));

    if snap.synced_sounds.is_empty() {
        col = col.child(
            ui::Element::new(ui::ElementType::P, Some("暂无（在手表端打开「自定义音频」页会自动同步此列表）"))
                .size(13)
                .text_color(COLOR_MUTED),
        );
    } else {
        for s in &snap.synced_sounds {
            let mode = if s.mode == "sequence" { "多" } else { "单" };
            let pattern = match s.play_pattern.as_str() {
                "mode2" => "模式2",
                "random" => "随机",
                _ => "模式1",
            };
            let line = format!("{} [{}|{}] {}", s.name, mode, pattern, s.file);
            let row = ui::Element::new(ui::ElementType::Div, None)
                .flex()
                .flex_direction(ui::FlexDirection::Row)
                .width_full()
                .gap(8)
                .child(
                    ui::Element::new(ui::ElementType::P, Some(line.as_str()))
                        .size(13)
                        .text_color(COLOR_TEXT),
                )
                .child(
                    ui::Element::new(ui::ElementType::Button, Some("删除"))
                        .bg(COLOR_BTN_DANGER_BG)
                        .text_color(COLOR_DANGER)
                        .on(ui::Event::Click, &format!("{EVENT_DELETE_SOUND_PREFIX}{}", s.id)),
                );
            col = col.child(row);
        }
    }

    // 手动刷新列表 + 清空，同行显示
    let mut row = ui::Element::new(ui::ElementType::Div, None)
        .flex()
        .flex_direction(ui::FlexDirection::Row)
        .width_full()
        .gap(8);
    let mut refresh_btn = ui::Element::new(ui::ElementType::Button, Some("刷新列表"))
        .bg(COLOR_BTN_BG)
        .text_color(COLOR_TEXT)
        .on(ui::Event::Click, EVENT_REFRESH_SYNCED);
    let mut clear_btn = ui::Element::new(ui::ElementType::Button, Some("清空自定义音频"))
        .bg(COLOR_BTN_DANGER_BG)
        .text_color(COLOR_DANGER)
        .on(ui::Event::Click, EVENT_CLEAR_AUDIO);
    if snap.selected_device.is_none() {
        refresh_btn = refresh_btn.disabled();
        clear_btn = clear_btn.disabled();
    }
    row = row.child(refresh_btn);
    row = row.child(clear_btn);
    col.child(row)
}

fn notice_line(text: &str) -> ui::Element {
    ui::Element::new(ui::ElementType::P, Some(text))
        .size(13)
        .text_color(COLOR_MUTED)
}
