//! 嘿哈嚈 自定义音频同步 — AstroBox 插件（API Level 4）
//!
//! 通过 interconnect 接口向「嘿哈嚈」手表快应用（com.huashu.heihade）
//! 分块传输自定义音频。协议见 src/transfer.rs 与快应用端 src/common/audiosync.js。
//!
//! API Level 4：运行在 WASI Preview 3 + 新版 wasmtime 上，宿主接口为 `async func`，
//! 导出直接返回结果（不再需要 future<T> / FutureReader / wit_future::spawn 样板）。
use astrobox_ng_wit::astrobox::psys_host_v4::ui::Event;
use astrobox_ng_wit::export;
use astrobox_ng_wit::exports::astrobox::psys_plugin_v4::{event, event::EventType, lifecycle};

pub mod audio;
pub mod logger;
pub mod media;
pub mod mp3;
pub mod state;
pub mod transfer;
pub mod ui;

struct MyPlugin;

impl event::Guest for MyPlugin {
    async fn on_event(event_type: EventType, event_payload: String) -> String {
        match event_type {
            EventType::Timer => {
                if event_payload.contains(audio::AUDIO_TIMER_PREFIX) {
                    // 同步前音频优化（next→decode→encode→finish）
                    audio::on_timer(&event_payload);
                } else if event_payload.contains(media::PROCESS_IMG_PAYLOAD_PREFIX) {
                    // 封面图片处理定时器（prepare→decode→encode→finalize）
                    media::on_timer(&event_payload);
                } else {
                    transfer::on_timer_tick(&event_payload);
                }
            }
            EventType::InterconnectMessage => {
                transfer::on_incoming_message(&event_payload);
            }
            EventType::DeviceAction => {
                state::refresh_devices();
                transfer::register_all();
                ui::rerender();
            }
            EventType::PluginMessage => {
                tracing::info!("plugin-message: {}", event_payload);
            }
            EventType::ProviderAction => {}
            EventType::DeeplinkAction => {}
            EventType::TransportPacket => {}
        }
        String::new()
    }

    async fn on_ui_event(event_id: String, event: Event, event_payload: String) -> String {
        ui::ui_event_processor(event, &event_id, &event_payload);
        String::new()
    }

    async fn on_ui_render(element_id: String) {
        ui::render_main_ui(&element_id);
    }

    async fn on_card_render(_card_id: String) {}
}

impl lifecycle::Guest for MyPlugin {
    async fn on_load() {
        logger::init();
        tracing::info!("嘿哈嚈 自定义音频同步插件已加载（API Level 4）");
        state::refresh_devices();
        let registered = transfer::register_all();
        tracing::info!("interconnect-recv registered devices: {}", registered);
    }
}

export!(MyPlugin);
