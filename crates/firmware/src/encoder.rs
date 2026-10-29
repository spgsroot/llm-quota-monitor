use crate::{screen::Selection, state::MonitorState};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Clockwise,
    CounterClockwise,
    Press,
}

pub struct Decoder {
    previous: u8,
    position: i8,
}
impl Decoder {
    pub fn new(clk_high: bool, dt_high: bool) -> Self {
        Self {
            previous: u8::from(clk_high) << 1 | u8::from(dt_high),
            position: 0,
        }
    }
    pub fn transition(&mut self, clk_high: bool, dt_high: bool) -> Option<Event> {
        const DELTAS: [i8; 16] = [0, -1, 1, 0, 1, 0, 0, -1, -1, 0, 0, 1, 0, 1, -1, 0];
        let current = u8::from(clk_high) << 1 | u8::from(dt_high);
        if current == self.previous {
            return None;
        }
        let delta = DELTAS[usize::from(self.previous << 2 | current)];
        self.previous = current;
        if delta == 0 {
            self.position = 0;
            return None;
        }
        self.position += delta;
        if self.position >= 4 {
            self.position = 0;
            Some(Event::Clockwise)
        } else if self.position <= -4 {
            self.position = 0;
            Some(Event::CounterClockwise)
        } else {
            None
        }
    }
}

pub fn apply(state: &MonitorState, selection: &mut Selection, event: Event) {
    let Some(snapshot) = state.snapshot.as_ref() else {
        return;
    };
    let providers = snapshot.providers.len();
    if providers == 0 {
        *selection = Selection::default();
        return;
    }
    match event {
        Event::Press => {
            *selection = if selection.provider.is_some() {
                Selection::default()
            } else {
                Selection {
                    provider: Some(0),
                    window_page: 0,
                }
            };
        }
        Event::Clockwise | Event::CounterClockwise => {
            let clockwise = matches!(event, Event::Clockwise);
            match selection.provider {
                None => {
                    selection.provider = Some(if clockwise { 0 } else { providers - 1 });
                    selection.window_page = 0;
                }
                Some(index) => {
                    let Some((_, provider)) = snapshot.providers.iter().nth(index) else {
                        *selection = Selection::default();
                        return;
                    };
                    let pages = provider.windows.len().max(1).div_ceil(2);
                    if pages > 1 {
                        selection.window_page = if clockwise {
                            (selection.window_page + 1) % pages
                        } else {
                            (selection.window_page + pages - 1) % pages
                        };
                    } else {
                        selection.provider = Some(if clockwise {
                            (index + 1) % providers
                        } else {
                            (index + providers - 1) % providers
                        });
                        selection.window_page = 0;
                    }
                }
            }
        }
    }
}

#[cfg(target_os = "espidf")]
pub fn spawn(
    clk: esp_idf_hal::gpio::Gpio2<'static>,
    dt: esp_idf_hal::gpio::Gpio4<'static>,
    sw: esp_idf_hal::gpio::Gpio16<'static>,
) -> std::sync::mpsc::Receiver<Event> {
    use esp_idf_hal::{
        cpu::Core,
        delay::FreeRtos,
        gpio::{PinDriver, Pull},
        task::thread::ThreadSpawnConfiguration,
    };
    use std::{
        thread,
        time::{Duration, Instant},
    };

    let (sender, receiver) = std::sync::mpsc::channel();

    ThreadSpawnConfiguration {
        name: Some(c"ky040"),
        stack_size: 4096,
        priority: 5,
        pin_to_core: Some(Core::Core1),
        ..Default::default()
    }
    .set()
    .unwrap();

    thread::Builder::new()
        .spawn(move || {
            let clk = PinDriver::input(clk, Pull::Up).unwrap();
            let dt = PinDriver::input(dt, Pull::Up).unwrap();
            let sw = PinDriver::input(sw, Pull::Up).unwrap();
            let mut decoder = Decoder::new(clk.is_high(), dt.is_high());
            let mut stable_pressed = sw.is_low();
            let mut candidate_pressed = stable_pressed;
            let mut candidate_since = Instant::now();
            loop {
                if let Some(event) = decoder.transition(clk.is_high(), dt.is_high()) {
                    if sender.send(event).is_err() {
                        return;
                    }
                }
                let pressed = sw.is_low();
                let now = Instant::now();
                if pressed != candidate_pressed {
                    candidate_pressed = pressed;
                    candidate_since = now;
                }
                if candidate_pressed != stable_pressed
                    && now.saturating_duration_since(candidate_since) >= Duration::from_millis(20)
                {
                    stable_pressed = candidate_pressed;
                    if stable_pressed && sender.send(Event::Press).is_err() {
                        return;
                    }
                }

                // Спим минимум 1 тик FreeRTOS (10 мс при стандартных 100 Гц),
                // чтобы задача перешла в состояние Blocked и дала поработать IDLE.
                FreeRtos::delay_ms(10);
            }
        })
        .unwrap();

    ThreadSpawnConfiguration::default().set().unwrap();

    receiver
}
