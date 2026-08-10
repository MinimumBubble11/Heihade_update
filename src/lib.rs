//! 嘿哈嘚 自定义音频同步 — AstroBox v2 插件
//!
//! 通过 interconnect 接口向「嘿哈嘚」手表快应用（com.huashu.heihade）
//! 分块传输自定义音频。协议见 src/transfer.rs 与快应用端 src/common/audiosync.js。
use wit_bindgen::FutureReader;

use crate::exports::astrobox::psys_plugin::{event_v3 as event, event_v3::EventType, lifecycle};

pub mod logger;
pub mod mp3;
pub mod state;
pub mod transfer;
pub mod ui;

wit_bindgen::generate!({
    path: "wit",
    world: "psys-world-v3",
    generate_all,
});

struct MyPlugin;

impl event::Guest for MyPlugin {
    fn on_event(event_type: EventType, event_payload: _rt::String) -> FutureReader<String> {
        match event_type {
            EventType::Timer => {
                transfer::on_timer_tick(&event_payload);
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
        immediate_string(String::new())
    }

    fn on_ui_event_v3(
        event_id: _rt::String,
        event: event::Event,
        event_payload: _rt::String,
    ) -> FutureReader<_rt::String> {
        ui::ui_event_processor(event, &event_id, &event_payload);
        immediate_string(String::new())
    }

    fn on_ui_render(element_id: _rt::String) -> FutureReader<()> {
        ui::render_main_ui(&element_id);
        immediate_unit()
    }

    fn on_card_render(_card_id: _rt::String) -> FutureReader<()> {
        immediate_unit()
    }
}

fn immediate_string(value: String) -> FutureReader<String> {
    let (writer, reader) = wit_future::new(String::new);
    wit_bindgen::spawn(async move {
        let _ = writer.write(value).await;
    });
    reader
}

fn immediate_unit() -> FutureReader<()> {
    let (writer, reader) = wit_future::new::<()>(|| ());
    wit_bindgen::spawn(async move {
        let _ = writer.write(()).await;
    });
    reader
}

impl lifecycle::Guest for MyPlugin {
    fn on_load() -> () {
        logger::init();
        tracing::info!("嘿哈嘚 自定义音频同步插件已加载");
        state::refresh_devices();
        let registered = transfer::register_all();
        tracing::info!("interconnect-recv registered devices: {}", registered);
    }
}

export!(MyPlugin);
