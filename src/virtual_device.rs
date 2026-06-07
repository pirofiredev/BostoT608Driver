use std::io::Error;
use std::time::Instant;
use std::collections::HashMap;

use evdev::{
    uinput::{VirtualDevice, VirtualDeviceBuilder},
    AbsInfo, AbsoluteAxisType, AttributeSet, EventType, InputEvent, Key, RelativeAxisType,
    Synchronization, UinputAbsSetup,
};

/// What a tablet express key does when triggered.
///
/// Express keys can now produce either a keyboard chord (one or more `Key`s
/// pressed/held/released together) OR a mouse-wheel motion (optionally with a
/// modifier held, e.g. Ctrl+Wheel for zoom). Wheel actions are emitted on the
/// dedicated virtual pointer device so the compositor treats them as genuine
/// scroll events.
#[derive(Clone)]
pub enum Action {
    /// Keyboard chord: keys are pressed in order, held, then released in order.
    Keys(Vec<Key>),
    /// One wheel notch. `modifiers` are held around a single REL_WHEEL step of
    /// `delta` (+1 = up/away, -1 = down/toward the user).
    Wheel { modifiers: Vec<Key>, delta: i32 },
}

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
    tablet_button_id_to_action_map: HashMap<u8, Action>,
    pen_button_id_to_key_code_map: HashMap<u8, Vec<Key>>,
    virtual_pen: VirtualDevice,
    virtual_keyboard: VirtualDevice,
    virtual_pointer: VirtualDevice,
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
        // `new` is fallible now; if device creation fails there is nothing
        // sensible to default to, so surface the error loudly. Callers should
        // prefer `DeviceDispatcher::new()` and handle the `Result`.
        Self::new().expect("failed to create virtual input devices")
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

    // ------------------------------------------------------------------
    //  KEY MAPPING  (Bosto T608 "G30")  --  MINIMAL build
    // ------------------------------------------------------------------
    // Per the user's request this driver is stripped to ONLY:
    //   * scroll wheel  -> plain mouse wheel (scroll)
    //   * stylus +      -> Ctrl + V  (paste)
    //   * stylus -      -> Ctrl + Z  (undo)
    // All d-pad zoom/undo/redo bindings have been removed.
    //
    // Raw button IDs are bit positions inside the 16-bit tablet button word
    // built by `tablet_buttons_as_binary_flags`:
    //
    //     word = (byte12 << 8) | byte11 | (0xcc << 8)        (active-LOW)
    //
    // At rest the word reads 0xffff; pressing a control CLEARS exactly one bit.
    // `binary_flags_to_tablet_key_events` scans bits 0..14, skipping 10 & 11.
    //
    // On this unit, bits 8 and 5 are the only tablet controls that emit any
    // signal at all (verified by per-control testing). They are the scroll
    // wheel's two rotation directions and are mapped to plain wheel up/down.
    // The rotary detent itself sends nothing distinct beyond these bits, and
    // the d-pad center is a full dropout (unmappable). Nothing else is bound.
    //
    // Scroll uses the dedicated `virtual_pointer` device: REL_WHEEL is a
    // relative pointer axis and cannot be emitted from a keyboard device.
    //
    // Pen stylus barrel buttons (byte 9, handled separately) emit KEYBOARD
    // chords, routed to `virtual_keyboard`:
    //   pen id 4 (stylus +) -> Ctrl + V   (paste)
    //   pen id 6 (stylus -) -> Ctrl + Z   (undo)
    // ------------------------------------------------------------------

    fn default_tablet_map() -> HashMap<u8, Action> {
        [
            // SCROLL WHEEL -------------------------------------------
            // Bits 8 and 5 are the only tablet controls that actually emit a
            // signal on this unit (verified by per-control testing); they are
            // the scroll wheel's two rotation directions. Bound to plain mouse
            // wheel so the wheel scrolls the page/canvas, one notch per detent.
            (8, Action::Wheel { modifiers: vec![], delta: 1 }),  // wheel one way -> scroll up
            (5, Action::Wheel { modifiers: vec![], delta: -1 }), // wheel other way -> scroll down
            // Everything else (d-pad zoom/undo bindings) intentionally removed.
        ]
        .into_iter()
        .collect()
    }

    fn default_pen_map() -> HashMap<u8, Vec<Key>> {
        [
            (4, vec![Key::KEY_LEFTCTRL, Key::KEY_V]), // stylus + -> Ctrl+V (paste)
            (6, vec![Key::KEY_LEFTCTRL, Key::KEY_Z]), // stylus - -> Ctrl+Z (undo)
        ]
        .into_iter()
        .collect()
    }

    /// Build the dispatcher. Fallible: returns an error instead of panicking
    /// if either virtual device cannot be created.
    pub fn new() -> Result<Self, Error> {
        let tablet_map = Self::default_tablet_map();
        let pen_map = Self::default_pen_map();

        // The keyboard device must advertise EVERY key it might emit:
        //  - all `Action::Keys` chords from the tablet map, AND
        //  - all pen barrel-button chords (Ctrl+V / Ctrl+Z), which are now
        //    keyboard chords routed to the keyboard device, not the pen.
        // `Action::Wheel` modifiers ALSO go on the pointer device, but the
        // keyboard never emits them, so they don't need to be on the keyboard.
        let mut keyboard_keys: Vec<Key> = Vec::new();
        for action in tablet_map.values() {
            if let Action::Keys(keys) = action {
                keyboard_keys.extend(keys.iter().copied());
            }
        }
        for keys in pen_map.values() {
            keyboard_keys.extend(keys.iter().copied());
        }

        // The pointer device needs every modifier used by any Wheel action so
        // it can hold Ctrl around the wheel step itself (keeping the chord on a
        // single device avoids cross-device timing races).
        let mut pointer_modifier_keys: Vec<Key> = Vec::new();
        for action in tablet_map.values() {
            if let Action::Wheel { modifiers, .. } = action {
                pointer_modifier_keys.extend(modifiers.iter().copied());
            }
        }

        // The pen device no longer emits any chord keys (barrel buttons became
        // keyboard chords); it only needs its tool/touch buttons, added inside
        // `virtual_pen_builder`.
        let virtual_pen = Self::virtual_pen_builder(&[])?;
        let virtual_keyboard = Self::virtual_keyboard_builder(&keyboard_keys)?;
        let virtual_pointer = Self::virtual_pointer_builder(&pointer_modifier_keys)?;

        Ok(DeviceDispatcher {
            tablet_last_raw_pressed_buttons: 0xFFFF,
            pen_last_raw_pressed_button: 0,
            tablet_button_id_to_action_map: tablet_map,
            pen_button_id_to_key_code_map: pen_map,
            virtual_pen,
            virtual_keyboard,
            virtual_pointer,
            was_touching: false,
            was_in_range: false,
            last_pen_packet_at: None,
            last_x: 0,
            last_y: 0,
            last_pressure: -1,
            have_last_xy: false,
            hover_jitter_toggle: false,
        })
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
        self.virtual_pointer.emit(&[InputEvent::new(
            EventType::SYNCHRONIZATION,
            Synchronization::SYN_REPORT.0,
            0,
        )])?;
        Ok(())
    }

    /// Dispatch one raw packet. Errors are propagated so the main loop can log
    /// and continue (or restart) rather than the driver core-dumping.
    pub fn dispatch(&mut self, raw_data: &RawDataReader) -> Result<(), Error> {
        self.emit_pen_events(raw_data)?;
        self.emit_tablet_events(raw_data)?;
        Ok(())
    }

    fn emit_tablet_events(&mut self, raw_data: &RawDataReader) -> Result<(), Error> {
        let raw_button_as_binary_flags = raw_data.tablet_buttons_as_binary_flags();
        self.binary_flags_to_tablet_key_events(raw_button_as_binary_flags)?;
        self.tablet_last_raw_pressed_buttons = raw_button_as_binary_flags;
        Ok(())
    }

    fn virtual_keyboard_builder(tablet_emitted_keys: &[Key]) -> Result<VirtualDevice, Error> {
        let mut key_set = AttributeSet::<Key>::new();
        for key in tablet_emitted_keys {
            key_set.insert(*key);
        }

        VirtualDeviceBuilder::new()?
            .name("virtual_tablet_keys")
            .with_keys(&key_set)?
            .build()
    }

    fn binary_flags_to_tablet_key_events(&mut self, raw_button_as_flags: u16) -> Result<(), Error> {
        for i in (0..14).filter(|i| ![10, 11].contains(i)) {
            self.emit_tablet_key_event(i, raw_button_as_flags)?;
        }
        Ok(())
    }

    pub fn emit_tablet_key_event(
        &mut self,
        i: u8,
        raw_button_as_flags: u16,
    ) -> Result<(), Error> {
        let id_as_binary_mask = 1 << i;
        let is_pressed = (raw_button_as_flags & id_as_binary_mask) == 0;
        let was_pressed = (self.tablet_last_raw_pressed_buttons & id_as_binary_mask) == 0;

        let Some(state) = (match (was_pressed, is_pressed) {
            (false, true) => Some(Self::PRESSED),
            (true, false) => Some(Self::RELEASED),
            (true, true) => Some(Self::HOLD),
            _ => None,
        }) else {
            return Ok(());
        };

        // Unmapped ids are silently ignored instead of panicking.
        let Some(action) = self.tablet_button_id_to_action_map.get(&i).cloned() else {
            return Ok(());
        };

        match action {
            // Keyboard chord: full press / hold(autorepeat) / release lifecycle
            // so held keys (e.g. nothing here today, but future bindings) and
            // the OS autorepeat both behave naturally.
            Action::Keys(keys) => {
                for key in &keys {
                    self.virtual_keyboard
                        .emit(&[InputEvent::new(EventType::KEY, key.code(), state)])?;
                }
                self.virtual_keyboard.emit(&[InputEvent::new(
                    EventType::SYNCHRONIZATION,
                    Synchronization::SYN_REPORT.0,
                    0,
                )])?;
            }
            // Wheel notch: fire exactly ONE notch on the PRESSED edge. We do
            // NOT repeat on HOLD -- the user asked for one wheel step per
            // physical detent, and a held d-pad should not machine-gun zoom.
            Action::Wheel { modifiers, delta } => {
                if state == Self::PRESSED {
                    self.emit_wheel(&modifiers, delta)?;
                }
            }
        }
        Ok(())
    }

    /// Emit one mouse-wheel notch, optionally with modifier keys held around it
    /// (e.g. Ctrl+Wheel = zoom-at-pointer in Excalidraw).
    ///
    /// CRITICAL: the modifier(s) AND the wheel step must come from the SAME
    /// device. An earlier version held Ctrl on the keyboard device and emitted
    /// the wheel on the pointer device; KDE/libinput did NOT fuse those into a
    /// single Ctrl+Wheel gesture, so zoom turned into a plain scroll. Here the
    /// modifier is pressed on the POINTER device, then the wheel, then the
    /// modifier is released -- the modifier is provably held at the instant the
    /// REL_WHEEL event is processed. The pointer device advertises these
    /// modifier keys (see `virtual_pointer_builder`).
    ///
    /// Each phase is its own SYN frame so the down -> motion -> up ordering is
    /// preserved by the compositor.
    fn emit_wheel(&mut self, modifiers: &[Key], delta: i32) -> Result<(), Error> {
        // 1. Press modifiers on the POINTER device + SYN.
        if !modifiers.is_empty() {
            for key in modifiers {
                self.virtual_pointer
                    .emit(&[InputEvent::new(EventType::KEY, key.code(), Self::PRESSED)])?;
            }
            self.virtual_pointer.emit(&[InputEvent::new(
                EventType::SYNCHRONIZATION,
                Synchronization::SYN_REPORT.0,
                0,
            )])?;
        }

        // 2. One wheel step + SYN, on the same device, with the modifier held.
        self.virtual_pointer.emit(&[InputEvent::new(
            EventType::RELATIVE,
            RelativeAxisType::REL_WHEEL.0,
            delta,
        )])?;
        self.virtual_pointer.emit(&[InputEvent::new(
            EventType::SYNCHRONIZATION,
            Synchronization::SYN_REPORT.0,
            0,
        )])?;

        // 3. Release modifiers (reverse order) + SYN.
        if !modifiers.is_empty() {
            for key in modifiers.iter().rev() {
                self.virtual_pointer
                    .emit(&[InputEvent::new(EventType::KEY, key.code(), Self::RELEASED)])?;
            }
            self.virtual_pointer.emit(&[InputEvent::new(
                EventType::SYNCHRONIZATION,
                Synchronization::SYN_REPORT.0,
                0,
            )])?;
        }
        Ok(())
    }

    /// Build the virtual pointer device that carries the scroll wheel. It must
    /// advertise REL_WHEEL plus at least one mouse button (BTN_LEFT) so the
    /// compositor classifies it as a pointer and accepts its relative axes.
    /// Any modifier keys used by wheel chords are also added so they can be
    /// held coherently if ever emitted from this device.
    fn virtual_pointer_builder(modifier_keys: &[Key]) -> Result<VirtualDevice, Error> {
        let mut rel_set = AttributeSet::<RelativeAxisType>::new();
        rel_set.insert(RelativeAxisType::REL_WHEEL);
        // REL_HWHEEL is harmless to advertise and lets the same device carry
        // horizontal scroll later if desired.
        rel_set.insert(RelativeAxisType::REL_HWHEEL);

        let mut key_set = AttributeSet::<Key>::new();
        // A pointer must expose at least one button to be recognized as such.
        key_set.insert(Key::BTN_LEFT);
        for key in modifier_keys {
            key_set.insert(*key);
        }

        VirtualDeviceBuilder::new()?
            .name("virtual_tablet_pointer")
            .with_relative_axes(&rel_set)?
            .with_keys(&key_set)?
            .build()
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
            .name("virtual_tablet_pen")
            .with_absolute_axis(&abs_x_setup)?
            .with_absolute_axis(&abs_y_setup)?
            .with_absolute_axis(&abs_pressure_setup)?
            .with_keys(&key_set)?
            .build()
    }

    fn emit_pen_events(&mut self, raw_data: &RawDataReader) -> Result<(), Error> {
        let raw_pen_buttons = raw_data.pen_buttons();
        self.raw_pen_buttons_to_pen_key_events(raw_pen_buttons)?;
        self.pen_last_raw_pressed_button = raw_pen_buttons;

        self.pen_emit_proximity(raw_data)?;

        let normalized_pressure = Self::normalize_pressure(raw_data.pressure());
        self.raw_pen_abs_to_pen_abs_events(
            raw_data.x_axis(),
            raw_data.y_axis(),
            normalized_pressure,
        )?;

        self.pen_emit_touch(raw_data)?;
        Ok(())
    }

    // How long the pen is kept "in range" after the last valid packet.
    // The tablet stops sending fresh packets (or sends pressure==0 packets)
    // when the pen is hovering but stationary. Without this grace period the
    // tool would drop out the instant a stale/zero packet arrives, causing
    // KDE/libinput to hide the cursor until you move again.
    const PROXIMITY_TIMEOUT_MS: u128 = 600;

    fn pen_emit_proximity(&mut self, raw_data: &RawDataReader) -> Result<(), Error> {
        let raw = raw_data.pressure();

        // Any nonzero pressure reading means the pen is physically present.
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
            )])?;
        }

        self.was_in_range = is_in_range;
        Ok(())
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

    fn raw_pen_abs_to_pen_abs_events(
        &mut self,
        x_axis: i32,
        y_axis: i32,
        pressure: i32,
    ) -> Result<(), Error> {
        const AXIS_MAX: i32 = 4096;

        // Opposite/inverted rotation: 180°
        let x_axis = AXIS_MAX - x_axis;
        let y_axis = AXIS_MAX - y_axis;

        // Anti-cull jitter: when the pen hovers without touching and the
        // position is identical to the last packet, libinput/KDE treats the
        // tool as "stale" and hides the cursor. Alternate a 1-unit offset on X
        // each packet while hovering stationary to keep motion fresh.
        let stationary = self.have_last_xy && x_axis == self.last_x && y_axis == self.last_y;
        let emit_x = if stationary && !self.was_touching {
            self.hover_jitter_toggle = !self.hover_jitter_toggle;
            if self.hover_jitter_toggle {
                x_axis + 1
            } else {
                x_axis
            }
        } else {
            x_axis
        };

        self.virtual_pen.emit(&[InputEvent::new(
            EventType::ABSOLUTE,
            AbsoluteAxisType::ABS_X.0,
            emit_x,
        )])?;

        self.virtual_pen.emit(&[InputEvent::new(
            EventType::ABSOLUTE,
            AbsoluteAxisType::ABS_Y.0,
            y_axis,
        )])?;

        self.virtual_pen.emit(&[InputEvent::new(
            EventType::ABSOLUTE,
            AbsoluteAxisType::ABS_PRESSURE.0,
            pressure,
        )])?;

        self.last_x = x_axis;
        self.last_y = y_axis;
        self.last_pressure = pressure;
        self.have_last_xy = true;
        Ok(())
    }

    fn pen_emit_touch(&mut self, raw_data: &RawDataReader) -> Result<(), Error> {
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
            )])?;
        }

        self.was_touching = is_touching;
        Ok(())
    }

    /// Translate raw pen barrel-button bytes into key events.
    ///
    /// FIX: the original `(x, y) if x != 2 && x == y` HOLD arm fired for *any*
    /// repeated byte, including `0` (no button), then called
    /// `.get(&id).expect("Error mapping pen keys.")` — which panicked because
    /// `0` is not in the pen map. This was the `virtual_device.rs:420`
    /// core-dump. We now restrict to the real button ids and only act on the
    /// PRESSED rising edge.
    ///
    /// The barrel buttons now emit KEYBOARD chords (Ctrl+V / Ctrl+Z), not pen
    /// BTN_STYLUS events, so we route them to `virtual_keyboard`. Each press
    /// emits a complete press->release PULSE of the chord exactly once, so a
    /// held barrel button pastes/undoes a single time instead of autorepeating.
    fn raw_pen_buttons_to_pen_key_events(&mut self, pen_button: u8) -> Result<(), Error> {
        // Only fire on the transition INTO a pressed barrel state (prev == 2,
        // the neutral "pen present, no barrel" code; now == 4 or 6). Releases
        // and holds are intentionally ignored: the chord already self-released.
        let Some(id) = (match (self.pen_last_raw_pressed_button, pen_button) {
            (prev, x) if prev != x && (x == 4 || x == 6) => Some(x),
            _ => None,
        }) else {
            return Ok(());
        };

        if let Some(keys) = self.pen_button_id_to_key_code_map.get(&id).cloned() {
            // Press the whole chord, SYN, then release it in reverse, SYN.
            for &key in &keys {
                self.virtual_keyboard.emit(&[InputEvent::new(
                    EventType::KEY,
                    key.code(),
                    Self::PRESSED,
                )])?;
            }
            self.virtual_keyboard.emit(&[InputEvent::new(
                EventType::SYNCHRONIZATION,
                Synchronization::SYN_REPORT.0,
                0,
            )])?;
            for &key in keys.iter().rev() {
                self.virtual_keyboard.emit(&[InputEvent::new(
                    EventType::KEY,
                    key.code(),
                    Self::RELEASED,
                )])?;
            }
            self.virtual_keyboard.emit(&[InputEvent::new(
                EventType::SYNCHRONIZATION,
                Synchronization::SYN_REPORT.0,
                0,
            )])?;
        }
        Ok(())
    }
}
