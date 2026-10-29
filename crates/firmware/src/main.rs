mod config;
mod encoder;
mod screen;
mod state;
mod transport;

use embedded_hal::spi::MODE_0;
use esp_idf_hal::{
    delay::Ets,
    gpio::{Gpio19, PinDriver},
    peripherals::Peripherals,
    spi::{config as spi_config, SpiDeviceDriver, SpiDriverConfig},
    units::FromValueType,
};
use esp_idf_svc::{
    eventloop::EspSystemEventLoop,
    nvs::EspDefaultNvsPartition,
    wifi::{AuthMethod, ClientConfiguration, Configuration, EspWifi},
};
use mipidsi::{
    interface::SpiInterface,
    models::ST7735s,
    options::{Orientation, Rotation},
    Builder,
};
use quota_common::{MAX_RESPONSE_BYTES, POLL_SECONDS};
use state::MonitorState;
use std::{
    thread,
    time::{Duration, Instant},
};

fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();
    log::info!("Starting LLM Quota Monitor network terminal");

    let p = Peripherals::take().unwrap();
    let mut fault_led = PinDriver::output(p.pins.gpio22).unwrap();
    fault_led.set_high().unwrap();
    let dc = PinDriver::output(p.pins.gpio26).unwrap();
    let rst = PinDriver::output(p.pins.gpio25).unwrap();
    let spi_config = spi_config::Config::new()
        .baudrate(2.MHz().into())
        .data_mode(MODE_0);
    let spi = SpiDeviceDriver::new_single(
        p.spi2,
        p.pins.gpio18,
        p.pins.gpio23,
        None::<Gpio19>,
        Some(p.pins.gpio27),
        &SpiDriverConfig::new(),
        &spi_config,
    )
    .unwrap();
    let mut buffer = [0_u8; 512];
    let interface = SpiInterface::new(spi, dc, &mut buffer);
    let mut display = Builder::new(ST7735s, interface)
        .display_size(128, 160)
        .orientation(Orientation::new().rotate(Rotation::Deg270))
        .reset_pin(rst)
        .init(&mut Ets)
        .unwrap();
    let mut state = MonitorState::default();
    let mut selection = screen::Selection::default();
    let encoder_events = encoder::spawn(p.pins.gpio2, p.pins.gpio4, p.pins.gpio16);
    let network = match config::NetworkConfig::load() {
        Ok(config) => config,
        Err(message) => {
            log::error!("{message}");
            state.config_required = true;
            screen::draw(&mut display, &state, selection, Instant::now()).unwrap();
            loop {
                thread::sleep(Duration::from_secs(1));
            }
        }
    };
    screen::draw(&mut display, &state, selection, Instant::now()).unwrap();
    let sysloop = EspSystemEventLoop::take().unwrap();
    let nvs = EspDefaultNvsPartition::take().unwrap();
    let mut wifi = EspWifi::new(p.modem, sysloop, Some(nvs)).unwrap();
    wifi.set_configuration(&Configuration::Client(ClientConfiguration {
        ssid: network.ssid.try_into().unwrap(),
        password: network.password.try_into().unwrap(),
        auth_method: if network.password.is_empty() {
            AuthMethod::None
        } else {
            AuthMethod::WPA2Personal
        },
        ..Default::default()
    }))
    .unwrap();
    // Start is asynchronous; the loop checks both association and DHCP readiness.
    wifi.start().unwrap();
    let mut body = Vec::with_capacity(MAX_RESPONSE_BYTES);
    let started = Instant::now();
    let mut next_connect = started;
    let mut next_poll = started;
    let mut last_view = None;
    let mut last_led = true;
    let mut changed = true;
    loop {
        if selection.provider.is_some_and(|index| {
            state
                .snapshot
                .as_ref()
                .is_none_or(|snapshot| index >= snapshot.providers.len())
        }) {
            selection = screen::Selection::default();
            changed = true;
        }
        while let Ok(event) = encoder_events.try_recv() {
            encoder::apply(&state, &mut selection, event);
            changed = true;
        }
        let now = Instant::now();
        let up = wifi.is_up().unwrap_or(false);
        if up != state.wifi_up {
            state.set_wifi(up);
            next_poll = now;
            changed = true;
            log::info!("Wi-Fi IP ready: {up}");
        }
        if state.fault_led_on() != last_led {
            last_led = state.fault_led_on();
            if last_led {
                fault_led.set_high().unwrap();
            } else {
                fault_led.set_low().unwrap();
            }
        }
        if !up {
            if now >= next_connect {
                next_connect = now + Duration::from_secs(10);
                if !wifi.is_connected().unwrap_or(false) {
                    if let Err(error) = wifi.connect() {
                        log::warn!("Wi-Fi association not ready: {error}");
                    }
                }
            }
        } else if now >= next_poll {
            match transport::fetch(network.collector_url, &mut body) {
                Ok(snapshot) => {
                    if let Err(error) = state.accept(snapshot, Instant::now()) {
                        state.collector_up = false;
                        log::warn!("Rejected snapshot: {error}");
                    } else {
                        log::info!("Quota snapshot received");
                    }
                }
                Err(error) => {
                    state.collector_up = false;
                    log::warn!("{error}");
                }
            }
            state.set_wifi(wifi.is_up().unwrap_or(false));
            next_poll = Instant::now()
                + Duration::from_secs(if state.collector_up { POLL_SECONDS } else { 5 });
            changed = true;
        }
        let now = Instant::now();
        let view = (state.status(now), state.stale_providers(now));
        if changed || last_view != Some(view) {
            screen::draw(&mut display, &state, selection, now).unwrap();
            last_view = Some(view);
            changed = false;
        }
        if state.fault_led_on() != last_led {
            last_led = state.fault_led_on();
            if last_led {
                fault_led.set_high().unwrap();
            } else {
                fault_led.set_low().unwrap();
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
}
