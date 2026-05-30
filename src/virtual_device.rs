use std::io::Error;
use std::time::Instant;
use std::{collections::HashMap, u16};

use evdev::{
    uinput::{VirtualDevice, VirtualDeviceBuilder},
    AbsInfo, AbsoluteAxisType, AttributeSet, EventType, InputEvent, Key, Synchronization,
    UinputAbsSetup,
};

#[derive(Default)]
pub struct RawDataReader {
    pub data: Vec<u8>,
}

impl RawDataReader {
    const X_AXIS_HIGH: usize = 1;
    const X_AXIS_LOW: usize = 2;
    const Y_AXIS_HIGH: usize = 3;
    const Y_AXIS_LOW: usize = 4;
    const PRESSURE_HIGH: usize = 5;
    const PRESSURE_LOW: usize = 6;
    const PEN_BUTTONS: usize = 9;
    const TABLET_BUTTONS_HIGH: usize = 12;
    const TABLET_BUTTONS_LOW: usize = 11;

    pub fn new() -> Self {
        RawDataReader {
            data: vec![0u8; 64],
        }
    }

    fn u16_from_2_u8(&self, high: u8, low: u8) -> u16 {
        (high as u16) << 8 | low as u16
    }

    fn x_axis(&self) -> i32 {
        self.u16_from_2_u8(self.data[Self::X_AXIS_HIGH], self.data[Self::X_AXIS_LOW]) as i32
    }

    fn y_axis(&self) -> i32 {
        self.u16_from_2_u8(self.data[Self::Y_AXIS_HIGH], self.data[Self::Y_AXIS_LOW]) as i32
    }

    fn pressure(&self) -> i32 {
        self.u16_from_2_u8(
            self.data[Self::PRESSURE_HIGH],
            self.data[Self::PRESSURE_LOW],
        ) as i32
    }

    fn tablet_buttons_as_binary_flags(&self) -> u16 {
        self.u16_from_2_u8(
            self.data[Self::TABLET_BUTTONS_HIGH],
            self.data[Self::TABLET_BUTTONS_LOW],
        ) | (0xcc << 8)
    }

    fn pen_buttons(&self) -> u8 {
        self.data[Self::PEN_BUTTONS]
    }

}

pub struct DeviceDispatcher {
    tablet_last_raw_pressed_buttons: u16,
    pen_last_raw_pressed_button: u8,
    tablet_button_id_to_key_code_map: HashMap<u8, Vec<Key>>,
    pen_button_id_to_key_code_map: HashMap<u8, Vec<Key>>,
    virtual_pen: VirtualDevice,
    virtual_keyboard: VirtualDevice,
    was_touching: bool,
    was_in_range: bool,
    last_pen_packet_at: Option<Instant>,
    last_x: i32,
    last_y: i32,
    last_pressure: i32,
    have_last_xy: bool,
    // Alternates each packet to apply an imperceptible +/-1 nudge on X while
    // the pen hovers stationary, so libinput keeps the cursor visible instead
    // of culling a perfectly static hover position.
    hover_jitter_toggle: bool,
}


impl Default for DeviceDispatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl DeviceDispatcher {
    const PRESSED: i32 = 1;
    const RELEASED: i32 = 0;
    const HOLD: i32 = 2;

    const PRESS_TOUCH_THRESHOLD: i32 = 1600;
    const PRESS_RELEASE_THRESHOLD: i32 = 1620;
    const PRESS_RAW_MIN: i32 = 933;
    const PRESS_DEADBAND: i32 = 8;
    const PRESS_OUT_MAX: i32 = 5000;

    pub fn new() -> Self {
        let default_tablet_button_id_to_key_code_map: HashMap<u8, Vec<Key>> = [
            (0, vec![Key::KEY_TAB]),
            (1, vec![Key::KEY_SPACE]),
            (2, vec![Key::KEY_LEFTALT]),
            (3, vec![Key::KEY_LEFTCTRL]),
            (4, vec![Key::KEY_SCROLLDOWN]),
            (5, vec![Key::KEY_SCROLLUP]),
            (6, vec![Key::KEY_LEFTBRACE]),
            (7, vec![Key::KEY_LEFTCTRL, Key::KEY_KPMINUS]),
            (8, vec![Key::KEY_LEFTCTRL, Key::KEY_KPPLUS]),
            (9, vec![Key::KEY_E]),
            (12, vec![Key::KEY_B]),
            (13, vec![Key::KEY_RIGHTBRACE]),
        ]
        .iter()
        .cloned()
        .collect();

        let default_pen_button_id_to_key_code_map: HashMap<u8, Vec<Key>> =
        [(4, vec![Key::BTN_STYLUS]), (6, vec![Key::BTN_STYLUS2])]
        .iter()
        .cloned()
        .collect();

        DeviceDispatcher {
            tablet_last_raw_pressed_buttons: 0xFFFF,
            pen_last_raw_pressed_button: 0,
            tablet_button_id_to_key_code_map: default_tablet_button_id_to_key_code_map.clone(),
            pen_button_id_to_key_code_map: default_pen_button_id_to_key_code_map.clone(),
            virtual_pen: Self::virtual_pen_builder(
                &default_pen_button_id_to_key_code_map
                .values()
                .flatten()
                .cloned()
                .collect::<Vec<Key>>(),
            )
            .expect("Error building virtual pen"),
            virtual_keyboard: Self::virtual_keyboard_builder(
                &default_tablet_button_id_to_key_code_map
                .values()
                .flatten()
                .cloned()
                .collect::<Vec<Key>>(),
            )
            .expect("Error building virtual keyborad"),
            was_touching: false,
            was_in_range: false,
            last_pen_packet_at: None,
            last_x: 0,
            last_y: 0,
            last_pressure: -1,
            have_last_xy: false,
            hover_jitter_toggle: false,
        }
    }

    pub fn syn(&mut self) -> Result<(), Error> {
        self.virtual_keyboard.emit(&[InputEvent::new(
            EventType::SYNCHRONIZATION,
            Synchronization::SYN_REPORT.0,
            0,
        )])?;
        self.virtual_pen.emit(&[InputEvent::new(
            EventType::SYNCHRONIZATION,
            Synchronization::SYN_REPORT.0,
            0,
        )])?;
        Ok(())
    }

    pub fn dispatch(&mut self, raw_data: &RawDataReader) {
        self.emit_pen_events(raw_data);
        self.emit_tablet_events(raw_data);
    }

    fn emit_tablet_events(&mut self, raw_data: &RawDataReader) {
        let raw_button_as_binary_flags = raw_data.tablet_buttons_as_binary_flags();
        self.binary_flags_to_tablet_key_events(raw_button_as_binary_flags);
        self.tablet_last_raw_pressed_buttons = raw_button_as_binary_flags;
    }

    fn virtual_keyboard_builder(tablet_emitted_keys: &[Key]) -> Result<VirtualDevice, Error> {
        let mut key_set = AttributeSet::<Key>::new();
        for key in tablet_emitted_keys {
            key_set.insert(*key);
        }

        VirtualDeviceBuilder::new()?
        .name("virtual_tablet")
        .with_keys(&key_set)?
        .build()
    }

    fn binary_flags_to_tablet_key_events(&mut self, raw_button_as_flags: u16) {
        (0..14)
        .filter(|i| ![10, 11].contains(i))
        .for_each(|i| self.emit_tablet_key_event(i, raw_button_as_flags));
    }

    pub fn emit_tablet_key_event(&mut self, i: u8, raw_button_as_flags: u16) {
        let id_as_binary_mask = 1 << i;
        let is_pressed = (raw_button_as_flags & id_as_binary_mask) == 0;
        let was_pressed = (self.tablet_last_raw_pressed_buttons & id_as_binary_mask) == 0;

        if let Some(state) = match (was_pressed, is_pressed) {
            (false, true) => Some(Self::PRESSED),
            (true, false) => Some(Self::RELEASED),
            (true, true) => Some(Self::HOLD),
            _ => None,
        } {
            if let Some(keys) = self.tablet_button_id_to_key_code_map.get(&i) {
                for &key in keys {
                    self.virtual_keyboard
                    .emit(&[InputEvent::new(EventType::KEY, key.code(), state)])
                    .expect("Error emitting vitual keyboard key.");
                }

                self.virtual_keyboard
                .emit(&[InputEvent::new(
                    EventType::SYNCHRONIZATION,
                    Synchronization::SYN_REPORT.0,
                    0,
                )])
                .expect("Error emitting SYN.");
            }
        };
    }

    fn virtual_pen_builder(pen_emitted_keys: &[Key]) -> Result<VirtualDevice, Error> {
        let abs_x_setup =
        UinputAbsSetup::new(AbsoluteAxisType::ABS_X, AbsInfo::new(0, 0, 4096, 0, 0, 1));
        let abs_y_setup =
        UinputAbsSetup::new(AbsoluteAxisType::ABS_Y, AbsInfo::new(0, 0, 4096, 0, 0, 1));
        let abs_pressure_setup = UinputAbsSetup::new(
            AbsoluteAxisType::ABS_PRESSURE,
            AbsInfo::new(0, 0, 5000, 0, 0, 1),
        );

        let mut key_set = AttributeSet::<Key>::new();
        for key in pen_emitted_keys {
            key_set.insert(*key);
        }

        for key in &[Key::BTN_TOOL_PEN, Key::BTN_TOUCH, Key::BTN_LEFT, Key::BTN_RIGHT] {
            key_set.insert(*key);
        }

        VirtualDeviceBuilder::new()?
        .name("virtual_tablet")
        .with_absolute_axis(&abs_x_setup)?
        .with_absolute_axis(&abs_y_setup)?
        .with_absolute_axis(&abs_pressure_setup)?
        .with_keys(&key_set)?
        .build()
    }

    fn emit_pen_events(&mut self, raw_data: &RawDataReader) {
        let raw_pen_buttons = raw_data.pen_buttons();
        self.raw_pen_buttons_to_pen_key_events(raw_pen_buttons);
        self.pen_last_raw_pressed_button = raw_pen_buttons;

        self.pen_emit_proximity(raw_data);

        let normalized_pressure = Self::normalize_pressure(raw_data.pressure());
        self.raw_pen_abs_to_pen_abs_events(
            raw_data.x_axis(),
                                           raw_data.y_axis(),
                                           normalized_pressure,
        );

        self.pen_emit_touch(raw_data);
    }


    // How long the pen is kept "in range" after the last valid packet.
    // The tablet stops sending fresh packets (or sends pressure==0 packets)
    // when the pen is hovering but stationary. Without this grace period the
    // tool would drop out the instant a stale/zero packet arrives, causing
    // KDE/libinput to hide the cursor until you move again.
    const PROXIMITY_TIMEOUT_MS: u128 = 600;

    fn pen_emit_proximity(&mut self, raw_data: &RawDataReader) {
        let raw = raw_data.pressure();

        // Any nonzero pressure reading means the pen is physically present.
        // The raw field is high while hovering (~1600 and up), lower while
        // pressing, and 0 only when the pen is genuinely gone / no packet.
        // We must NOT cap this at an upper bound: high hover values are valid
        // and were previously being rejected, which dropped proximity (and
        // hid the cursor) whenever you stopped moving while hovering.
        // Touch never had this problem because BTN_TOUCH keeps the tool alive.
        let has_valid_pen_data = raw > 0;
        if has_valid_pen_data {
            self.last_pen_packet_at = Some(Instant::now());
        }

        // Stay in range as long as we've seen a valid packet recently.
        let is_in_range = match self.last_pen_packet_at {
            Some(t) => t.elapsed().as_millis() < Self::PROXIMITY_TIMEOUT_MS,
            None => false,
        };

        if let Some(state) = match (self.was_in_range, is_in_range) {
            (false, true) => Some(Self::PRESSED),
            (true, false) => Some(Self::RELEASED),
            _ => None,
        } {
            self.virtual_pen.emit(&[InputEvent::new(
                EventType::KEY,
                Key::BTN_TOOL_PEN.code(),
                                                    state,
            )]).expect("Error emitting BTN_TOOL_PEN");
        }

        self.was_in_range = is_in_range;
    }



    fn normalize_pressure(raw_pressure: i32) -> i32 {
        let range = Self::PRESS_TOUCH_THRESHOLD - Self::PRESS_RAW_MIN;
        let force = Self::PRESS_TOUCH_THRESHOLD - raw_pressure;

        if force <= Self::PRESS_DEADBAND {
            return 0;
        }

        let clamped = force.min(range);
        (clamped * Self::PRESS_OUT_MAX) / range
    }

    fn raw_pen_abs_to_pen_abs_events(&mut self, x_axis: i32, y_axis: i32, pressure: i32) {
        const AXIS_MAX: i32 = 4096;

        // Opposite/inverted rotation: 180°
        let x_axis = AXIS_MAX - x_axis;
        let y_axis = AXIS_MAX - y_axis;

        // Anti-cull jitter: when the pen hovers without touching and the
        // position is identical to the last packet, libinput/KDE treats the
        // tool as "stale" and hides the cursor until movement resumes. The
        // tablet keeps streaming packets here, but a frozen coordinate isn't
        // enough. So when we detect a stationary hover, alternate a 1-unit
        // offset on X each packet. This is visually imperceptible but keeps
        // libinput seeing fresh motion. We only do this while NOT touching, so
        // drawing precision is never affected.
        let stationary = self.have_last_xy
            && x_axis == self.last_x
            && y_axis == self.last_y;
        // emit_x carries the optional jitter; x_axis stays the TRUE position
        // so stationary detection on the next packet compares like-for-like.
        let emit_x = if stationary && !self.was_touching {
            self.hover_jitter_toggle = !self.hover_jitter_toggle;
            if self.hover_jitter_toggle { x_axis + 1 } else { x_axis }
        } else {
            x_axis
        };

        self.virtual_pen.emit(&[InputEvent::new(
            EventType::ABSOLUTE,
            AbsoluteAxisType::ABS_X.0,
            emit_x,
        )]).expect("Error emitting ABS_X.");

        self.virtual_pen.emit(&[InputEvent::new(
            EventType::ABSOLUTE,
            AbsoluteAxisType::ABS_Y.0,
            y_axis,
        )]).expect("Error emitting ABS_Y.");

        self.virtual_pen.emit(&[InputEvent::new(
            EventType::ABSOLUTE,
            AbsoluteAxisType::ABS_PRESSURE.0,
            pressure,
        )]).expect("Error emitting Pressure.");

        self.last_x = x_axis;
        self.last_y = y_axis;
        self.last_pressure = pressure;
        self.have_last_xy = true;
    }

    fn pen_emit_touch(&mut self, raw_data: &RawDataReader) {
        let raw = raw_data.pressure();

        let is_touching = if self.was_touching {
            raw < Self::PRESS_RELEASE_THRESHOLD
        } else {
            raw < Self::PRESS_TOUCH_THRESHOLD
        };

        if let Some(state) = match (self.was_touching, is_touching) {
            (false, true) => Some(Self::PRESSED),
            (true, false) => Some(Self::RELEASED),
            _ => None,
        } {
            self.virtual_pen.emit(&[InputEvent::new(
                EventType::KEY,
                Key::BTN_TOUCH.code(),
                                                    state,
            )]).expect("Error emitting Touch");
        }

        self.was_touching = is_touching;
    }

    fn raw_pen_buttons_to_pen_key_events(&mut self, pen_button: u8) {
        if let Some((state, id)) = match (self.pen_last_raw_pressed_button, pen_button) {
            (2, x) if x == 6 || x == 4 => Some((Self::PRESSED, x)),
            (x, 2) if x == 6 || x == 4 => Some((Self::RELEASED, x)),
            (x, y) if x != 2 && x == y => Some((Self::HOLD, x)),
            _ => None,
        } {
            let keys = self
            .pen_button_id_to_key_code_map
            .get(&id)
            .expect("Error mapping pen keys.");

            for key in keys {
                self.virtual_pen
                .emit(&[InputEvent::new(EventType::KEY, key.code(), state)])
                .expect("Error emitting pen keys.")
            }
        }
    }
}
