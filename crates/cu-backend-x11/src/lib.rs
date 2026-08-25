use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::Cursor,
    thread,
    time::Duration,
};

use cu_core::{CaptureLimits, CapturedFrame, Desktop};
use cu_protocol::{Action, CuError, ErrorCode, MouseButton, Point, Viewport};
use image::{DynamicImage, ImageFormat, Rgb, RgbImage, imageops::FilterType};
use x11rb::{
    CURRENT_TIME, NO_SYMBOL, NONE,
    connection::Connection,
    image::{Image as XImage, PixelLayout},
    protocol::{
        xinput::{self, DeviceUse},
        xkb::{self, ConnectionExt as _},
        xproto::{
            BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, ConnectionExt as _, KEY_PRESS_EVENT,
            KEY_RELEASE_EVENT, MOTION_NOTIFY_EVENT, ModMask, Screen, Visualtype,
        },
        xtest::ConnectionExt as _,
    },
    rust_connection::RustConnection,
};
use xkeysym::{Keysym, key};

const KEY_DELAY: Duration = Duration::from_millis(12);
const DRAG_DELAY: Duration = Duration::from_millis(8);

pub struct X11Backend {
    connection: RustConnection,
    screen: Screen,
    screen_index: usize,
    display: String,
    capture_limits: CaptureLimits,
    transform: Option<CoordinateTransform>,
    keymap: KeyboardMap,
    keyboard_device: u8,
    pointer_device: u8,
    modifier_keycodes: HashSet<u8>,
}

#[derive(Debug, Clone, Copy)]
struct CoordinateTransform {
    frame: Viewport,
    screen: Viewport,
}

impl X11Backend {
    /// Connect to one explicit X11 display for root-window capture and XTEST input.
    ///
    /// # Errors
    ///
    /// Returns [`CuError`] if the display, XTEST/XInput/XKB extensions, capture
    /// format, keyboard map, or capture limits cannot be initialized safely.
    pub fn new(display: &str, capture_limits: CaptureLimits) -> Result<Self, CuError> {
        if display.trim().is_empty() {
            return Err(CuError::new(
                ErrorCode::InvalidAction,
                "X11 display must not be empty",
            ));
        }
        capture_limits.validate()?;

        let (connection, screen_index) = x11rb::connect(Some(display)).map_err(|error| {
            CuError::new(
                ErrorCode::TargetGone,
                format!("failed to connect to X11 display {display}: {error}"),
            )
        })?;
        let setup = connection.setup();
        let screen = setup.roots.get(screen_index).cloned().ok_or_else(|| {
            CuError::new(
                ErrorCode::TargetGone,
                format!("X11 display {display} has no screen {screen_index}"),
            )
        })?;
        connection
            .xtest_get_version(2, 2)
            .map_err(input_error)?
            .reply()
            .map_err(input_error)?;
        let xkb_version = connection
            .xkb_use_extension(1, 0)
            .map_err(input_error)?
            .reply()
            .map_err(input_error)?;
        if !xkb_version.supported {
            return Err(CuError::new(
                ErrorCode::UnsupportedInput,
                format!(
                    "X11 server does not support XKB 1.0 (server reports {}.{})",
                    xkb_version.server_major, xkb_version.server_minor
                ),
            ));
        }

        let devices = xinput::list_input_devices(&connection)
            .map_err(input_error)?
            .reply()
            .map_err(input_error)?
            .devices;
        let keyboard_device = find_device(&devices, DeviceUse::IS_X_KEYBOARD, "keyboard")?;
        let pointer_device = find_device(&devices, DeviceUse::IS_X_POINTER, "pointer")?;
        let modifier_keycodes = read_modifier_keycodes(&connection)?;
        normalize_keyboard_state(
            &connection,
            screen.root,
            keyboard_device,
            &modifier_keycodes,
        )?;
        let keymap = KeyboardMap::read(&connection, &modifier_keycodes)?;

        Ok(Self {
            connection,
            screen,
            screen_index,
            display: display.to_owned(),
            capture_limits,
            transform: None,
            keymap,
            keyboard_device,
            pointer_device,
            modifier_keycodes,
        })
    }

    #[must_use]
    pub fn display(&self) -> &str {
        &self.display
    }

    #[must_use]
    pub const fn screen_index(&self) -> usize {
        self.screen_index
    }

    fn capture_root(&mut self) -> Result<CapturedFrame, CuError> {
        let native_width = self.screen.width_in_pixels;
        let native_height = self.screen.height_in_pixels;
        let (ximage, visual_id) = XImage::get(
            &self.connection,
            self.screen.root,
            0,
            0,
            native_width,
            native_height,
        )
        .map_err(capture_error)?;
        let visual_id = if visual_id == 0 {
            self.screen.root_visual
        } else {
            visual_id
        };
        let visual = find_visual(&self.screen, visual_id).ok_or_else(|| {
            CuError::new(
                ErrorCode::CaptureFailed,
                format!("X11 root visual {visual_id:#x} is unavailable"),
            )
        })?;
        let layout = PixelLayout::from_visual_type(visual).map_err(|error| {
            CuError::new(
                ErrorCode::CaptureFailed,
                format!("unsupported X11 root visual: {error}"),
            )
        })?;

        let mut rgb = RgbImage::new(u32::from(native_width), u32::from(native_height));
        for y in 0..native_height {
            for x in 0..native_width {
                let (red, green, blue) = layout.decode(ximage.get_pixel(x, y));
                rgb.put_pixel(
                    u32::from(x),
                    u32::from(y),
                    Rgb([(red >> 8) as u8, (green >> 8) as u8, (blue >> 8) as u8]),
                );
            }
        }

        let frame = self.capture_limits.fit(rgb.width(), rgb.height());
        let image = if frame.width == rgb.width() && frame.height == rgb.height() {
            DynamicImage::ImageRgb8(rgb)
        } else {
            DynamicImage::ImageRgb8(rgb).resize_exact(
                frame.width,
                frame.height,
                FilterType::Lanczos3,
            )
        };
        self.transform = Some(CoordinateTransform {
            frame,
            screen: Viewport {
                width: u32::from(native_width),
                height: u32::from(native_height),
            },
        });

        let mut png = Cursor::new(Vec::new());
        image
            .write_to(&mut png, ImageFormat::Png)
            .map_err(capture_error)?;
        Ok(CapturedFrame {
            png: png.into_inner(),
            width: frame.width,
            height: frame.height,
            target: format!("x11:{}:screen:{}", self.display, self.screen_index),
        })
    }

    fn map_point(&self, point: Point, viewport: Viewport) -> Result<Point, CuError> {
        let transform = self.transform.ok_or_else(|| {
            CuError::new(
                ErrorCode::StaleFrame,
                "capture a frame before injecting pointer input",
            )
        })?;
        if transform.frame != viewport {
            return Err(CuError::new(
                ErrorCode::StaleFrame,
                "the action viewport does not match the last captured frame",
            ));
        }
        let x = i64::from(point.x) * i64::from(transform.screen.width)
            / i64::from(transform.frame.width);
        let y = i64::from(point.y) * i64::from(transform.screen.height)
            / i64::from(transform.frame.height);
        Ok(Point {
            x: i32::try_from(x).map_err(|_| {
                CuError::new(ErrorCode::OutOfBounds, "mapped x coordinate overflowed")
            })?,
            y: i32::try_from(y).map_err(|_| {
                CuError::new(ErrorCode::OutOfBounds, "mapped y coordinate overflowed")
            })?,
        })
    }

    fn move_pointer(&self, point: Point, viewport: Viewport) -> Result<(), CuError> {
        let point = self.map_point(point, viewport)?;
        let x = i16::try_from(point.x).map_err(|_| {
            CuError::new(ErrorCode::OutOfBounds, "X11 x coordinate does not fit i16")
        })?;
        let y = i16::try_from(point.y).map_err(|_| {
            CuError::new(ErrorCode::OutOfBounds, "X11 y coordinate does not fit i16")
        })?;
        self.connection
            .xtest_fake_input(
                MOTION_NOTIFY_EVENT,
                0,
                CURRENT_TIME,
                NONE,
                x,
                y,
                self.pointer_device,
            )
            .map_err(input_error)?
            .check()
            .map_err(input_error)
    }

    fn button_event(&self, detail: u8, press: bool) -> Result<(), CuError> {
        self.connection
            .xtest_fake_input(
                if press {
                    BUTTON_PRESS_EVENT
                } else {
                    BUTTON_RELEASE_EVENT
                },
                detail,
                CURRENT_TIME,
                self.screen.root,
                0,
                0,
                self.pointer_device,
            )
            .map_err(input_error)?
            .check()
            .map_err(input_error)
    }

    fn click_button(&self, detail: u8) -> Result<(), CuError> {
        self.button_event(detail, true)?;
        self.button_event(detail, false)
    }

    fn click(&self, point: Point, button: MouseButton, viewport: Viewport) -> Result<(), CuError> {
        self.move_pointer(point, viewport)?;
        self.click_button(map_button(button))
    }

    fn key_event(&self, keycode: u8, press: bool) -> Result<(), CuError> {
        self.connection
            .xtest_fake_input(
                if press {
                    KEY_PRESS_EVENT
                } else {
                    KEY_RELEASE_EVENT
                },
                keycode,
                CURRENT_TIME,
                self.screen.root,
                0,
                0,
                self.keyboard_device,
            )
            .map_err(input_error)?
            .check()
            .map_err(input_error)
    }

    fn normalize_keyboard_state(&self) -> Result<(), CuError> {
        normalize_keyboard_state(
            &self.connection,
            self.screen.root,
            self.keyboard_device,
            &self.modifier_keycodes,
        )
    }

    fn press_keys(&mut self, names: &[String]) -> Result<Vec<u8>, CuError> {
        let symbols = names
            .iter()
            .map(|name| parse_key(name))
            .collect::<Result<Vec<_>, _>>()?;
        let mut pressed = Vec::with_capacity(symbols.len());
        for symbol in symbols {
            let keycode = match self.keymap.keycode_for(&self.connection, symbol) {
                Ok(keycode) => keycode,
                Err(error) => {
                    let _ = self.release_keys(&pressed);
                    return Err(error);
                }
            };
            if let Err(error) = self.key_event(keycode, true) {
                let _ = self.release_keys(&pressed);
                return Err(error);
            }
            self.keymap.held.insert(keycode);
            pressed.push(keycode);
        }
        Ok(pressed)
    }

    fn release_keys(&mut self, keys: &[u8]) -> Result<(), CuError> {
        let mut first_error = None;
        for &keycode in keys.iter().rev() {
            if let Err(error) = self.key_event(keycode, false)
                && first_error.is_none()
            {
                first_error = Some(error);
            }
            self.keymap.held.remove(&keycode);
        }
        first_error.map_or(Ok(()), Err)
    }

    fn execute_with_modifiers(
        &mut self,
        modifiers: &[String],
        operation: impl FnOnce(&mut Self) -> Result<(), CuError>,
    ) -> Result<(), CuError> {
        let pressed = self.press_keys(modifiers)?;
        let result = operation(self);
        let release = self.release_keys(&pressed);
        result.and(release)
    }

    fn keypress(&mut self, names: &[String]) -> Result<(), CuError> {
        let pressed = self.press_keys(names)?;
        self.release_keys(&pressed)
    }

    fn type_text(&mut self, text: &str) -> Result<(), CuError> {
        for character in text.chars() {
            let symbol = Keysym::from_char(character);
            if symbol.raw() == NO_SYMBOL {
                return Err(CuError::new(
                    ErrorCode::UnsupportedInput,
                    format!("X11 cannot map character {character:?} to a keysym"),
                ));
            }
            let keycode = self.keymap.keycode_for(&self.connection, symbol)?;
            self.key_event(keycode, true)?;
            self.keymap.held.insert(keycode);
            let release = self.key_event(keycode, false);
            self.keymap.held.remove(&keycode);
            release?;
            thread::sleep(KEY_DELAY);
        }
        Ok(())
    }

    fn drag(&self, path: &[Point], viewport: Viewport) -> Result<(), CuError> {
        self.move_pointer(path[0], viewport)?;
        self.button_event(1, true)?;
        let result = path[1..].iter().try_for_each(|point| {
            self.move_pointer(*point, viewport)?;
            thread::sleep(DRAG_DELAY);
            Ok(())
        });
        let release = self.button_event(1, false);
        result.and(release)
    }
}

impl Desktop for X11Backend {
    fn capture(&mut self) -> Result<CapturedFrame, CuError> {
        self.capture_root()
    }

    fn validate(&self, action: &Action) -> Result<(), CuError> {
        match action {
            Action::Move { keys, .. }
            | Action::Click { keys, .. }
            | Action::DoubleClick { keys, .. }
            | Action::Drag { keys, .. }
            | Action::Scroll { keys, .. }
            | Action::Keypress { keys } => {
                for key in keys {
                    parse_key(key)?;
                }
            }
            Action::Type { text } => {
                for character in text.chars() {
                    if Keysym::from_char(character).raw() == NO_SYMBOL {
                        return Err(CuError::new(
                            ErrorCode::UnsupportedInput,
                            format!("X11 cannot map character {character:?} to a keysym"),
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn execute(&mut self, action: &Action, viewport: Viewport) -> Result<(), CuError> {
        if action_uses_keyboard(action) {
            self.normalize_keyboard_state()?;
        }
        match action {
            Action::Move { x, y, keys } => self.execute_with_modifiers(keys, |this| {
                this.move_pointer(Point { x: *x, y: *y }, viewport)
            }),
            Action::Click { x, y, button, keys } => self.execute_with_modifiers(keys, |this| {
                this.click(Point { x: *x, y: *y }, *button, viewport)
            }),
            Action::DoubleClick { x, y, keys } => self.execute_with_modifiers(keys, |this| {
                let point = Point { x: *x, y: *y };
                this.click(point, MouseButton::Left, viewport)?;
                thread::sleep(Duration::from_millis(60));
                this.click(point, MouseButton::Left, viewport)
            }),
            Action::Drag { path, keys } => {
                self.execute_with_modifiers(keys, |this| this.drag(path, viewport))
            }
            Action::Scroll {
                x,
                y,
                scroll_x,
                scroll_y,
                keys,
            } => self.execute_with_modifiers(keys, |this| {
                this.move_pointer(Point { x: *x, y: *y }, viewport)?;
                let horizontal = wheel_ticks(*scroll_x);
                let vertical = wheel_ticks(*scroll_y);
                this.scroll(horizontal, 6, 7)?;
                this.scroll(vertical, 4, 5)
            }),
            Action::Type { text } => self.type_text(text),
            Action::Keypress { keys } => self.keypress(keys),
        }
    }
}

impl X11Backend {
    fn scroll(&self, ticks: i32, negative_button: u8, positive_button: u8) -> Result<(), CuError> {
        let button = if ticks.is_negative() {
            negative_button
        } else {
            positive_button
        };
        for _ in 0..ticks.unsigned_abs() {
            self.click_button(button)?;
        }
        Ok(())
    }
}

impl Drop for X11Backend {
    fn drop(&mut self) {
        for &keycode in self.keymap.mapped.values() {
            let _ = self
                .connection
                .change_keyboard_mapping(1, keycode, 2, &[NO_SYMBOL, NO_SYMBOL]);
        }
        let _ = self.connection.flush();
    }
}

struct KeyboardMap {
    min_keycode: u8,
    keysyms_per_keycode: u8,
    keysyms: Vec<u32>,
    unused: VecDeque<u8>,
    mapped: HashMap<u32, u8>,
    mapped_order: VecDeque<u32>,
    held: HashSet<u8>,
}

impl KeyboardMap {
    fn read(connection: &RustConnection, modifier_keycodes: &HashSet<u8>) -> Result<Self, CuError> {
        let setup = connection.setup();
        let min_keycode = setup.min_keycode;
        let max_keycode = setup.max_keycode;
        let count = max_keycode
            .checked_sub(min_keycode)
            .and_then(|difference| difference.checked_add(1))
            .ok_or_else(|| {
                CuError::new(ErrorCode::InputFailed, "invalid X11 keyboard keycode range")
            })?;
        let reply = connection
            .get_keyboard_mapping(min_keycode, count)
            .map_err(input_error)?
            .reply()
            .map_err(input_error)?;
        let mut unused = VecDeque::new();
        for (symbols, keycode) in reply
            .keysyms
            .chunks(usize::from(reply.keysyms_per_keycode))
            .zip(min_keycode..=max_keycode)
        {
            if keycode != 8
                && !modifier_keycodes.contains(&keycode)
                && symbols.iter().all(|&symbol| symbol == NO_SYMBOL)
            {
                unused.push_back(keycode);
            }
        }
        if unused.is_empty() {
            return Err(CuError::new(
                ErrorCode::UnsupportedInput,
                "X11 keyboard map has no unused keycode for text injection",
            ));
        }
        Ok(Self {
            min_keycode,
            keysyms_per_keycode: reply.keysyms_per_keycode,
            keysyms: reply.keysyms,
            unused,
            mapped: HashMap::new(),
            mapped_order: VecDeque::new(),
            held: HashSet::new(),
        })
    }

    fn keycode_for(&mut self, connection: &RustConnection, symbol: Keysym) -> Result<u8, CuError> {
        if let Some(keycode) = self.base_keycode(symbol.raw()) {
            return Ok(keycode);
        }
        if let Some(&keycode) = self.mapped.get(&symbol.raw()) {
            self.touch(symbol.raw());
            return Ok(keycode);
        }

        let keycode = if let Some(keycode) = self.unused.pop_front() {
            keycode
        } else {
            self.reusable_keycode()?
        };
        connection
            .change_keyboard_mapping(1, keycode, 2, &[symbol.raw(), symbol.raw()])
            .map_err(input_error)?
            .check()
            .map_err(input_error)?;
        self.mapped.insert(symbol.raw(), keycode);
        self.mapped_order.push_back(symbol.raw());
        Ok(keycode)
    }

    fn base_keycode(&self, symbol: u32) -> Option<u8> {
        self.keysyms
            .chunks(usize::from(self.keysyms_per_keycode))
            .position(|symbols| symbols.first() == Some(&symbol))
            .and_then(|offset| u8::try_from(offset).ok())
            .and_then(|offset| self.min_keycode.checked_add(offset))
    }

    fn reusable_keycode(&mut self) -> Result<u8, CuError> {
        let mapped_count = self.mapped_order.len();
        for _ in 0..mapped_count {
            let symbol = self
                .mapped_order
                .pop_front()
                .expect("mapped order length was checked");
            let keycode = self.mapped[&symbol];
            if self.held.contains(&keycode) {
                self.mapped_order.push_back(symbol);
            } else {
                self.mapped.remove(&symbol);
                return Ok(keycode);
            }
        }
        Err(CuError::new(
            ErrorCode::UnsupportedInput,
            "all temporary X11 keycodes are currently held",
        ))
    }

    fn touch(&mut self, symbol: u32) {
        self.mapped_order.retain(|&existing| existing != symbol);
        self.mapped_order.push_back(symbol);
    }
}

fn read_modifier_keycodes(connection: &RustConnection) -> Result<HashSet<u8>, CuError> {
    let mapping = connection
        .get_modifier_mapping()
        .map_err(input_error)?
        .reply()
        .map_err(input_error)?;
    Ok(mapping
        .keycodes
        .into_iter()
        .filter(|&keycode| keycode != 0)
        .collect())
}

fn normalize_keyboard_state(
    connection: &RustConnection,
    root: u32,
    keyboard_device: u8,
    modifier_keycodes: &HashSet<u8>,
) -> Result<(), CuError> {
    let pressed = connection
        .query_keymap()
        .map_err(input_error)?
        .reply()
        .map_err(input_error)?
        .keys;
    for &keycode in modifier_keycodes {
        let byte = usize::from(keycode / 8);
        let bit = keycode % 8;
        if pressed[byte] & (1 << bit) != 0 {
            connection
                .xtest_fake_input(
                    KEY_RELEASE_EVENT,
                    keycode,
                    CURRENT_TIME,
                    root,
                    0,
                    0,
                    keyboard_device,
                )
                .map_err(input_error)?
                .check()
                .map_err(input_error)?;
        }
    }

    let all_modifiers = ModMask::from(0xff_u16);
    connection
        .xkb_latch_lock_state(
            xkb::ID::USE_CORE_KBD.into(),
            all_modifiers,
            ModMask::default(),
            true,
            0.into(),
            all_modifiers,
            true,
            0,
        )
        .map_err(input_error)?
        .check()
        .map_err(input_error)
}

fn action_uses_keyboard(action: &Action) -> bool {
    match action {
        Action::Move { keys, .. }
        | Action::Click { keys, .. }
        | Action::DoubleClick { keys, .. }
        | Action::Drag { keys, .. }
        | Action::Scroll { keys, .. } => !keys.is_empty(),
        Action::Type { .. } | Action::Keypress { .. } => true,
    }
}

fn find_visual(screen: &Screen, visual_id: u32) -> Option<Visualtype> {
    screen
        .allowed_depths
        .iter()
        .flat_map(|depth| depth.visuals.iter())
        .find(|visual| visual.visual_id == visual_id)
        .copied()
}

fn find_device(
    devices: &[xinput::DeviceInfo],
    usage: DeviceUse,
    kind: &str,
) -> Result<u8, CuError> {
    devices
        .iter()
        .find(|device| device.device_use == usage)
        .map(|device| device.device_id)
        .ok_or_else(|| {
            CuError::new(
                ErrorCode::UnsupportedInput,
                format!("X11 exposes no core {kind} device"),
            )
        })
}

const fn map_button(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 1,
        MouseButton::Wheel => 2,
        MouseButton::Right => 3,
        MouseButton::Back => 8,
        MouseButton::Forward => 9,
    }
}

fn wheel_ticks(delta: i32) -> i32 {
    if delta == 0 {
        0
    } else {
        let magnitude = delta.unsigned_abs().div_ceil(120);
        i32::try_from(magnitude).unwrap_or(i32::MAX) * delta.signum()
    }
}

fn parse_key(name: &str) -> Result<Keysym, CuError> {
    let normalized = name.trim().to_ascii_uppercase();
    let raw = match normalized.as_str() {
        "ALT" | "OPTION" => key::Alt_L,
        "BACKSPACE" => key::BackSpace,
        "CTRL" | "CONTROL" => key::Control_L,
        "DELETE" | "DEL" => key::Delete,
        "DOWN" | "ARROWDOWN" => key::Down,
        "END" => key::End,
        "ENTER" | "RETURN" => key::Return,
        "ESC" | "ESCAPE" => key::Escape,
        "F1" => key::F1,
        "F2" => key::F2,
        "F3" => key::F3,
        "F4" => key::F4,
        "F5" => key::F5,
        "F6" => key::F6,
        "F7" => key::F7,
        "F8" => key::F8,
        "F9" => key::F9,
        "F10" => key::F10,
        "F11" => key::F11,
        "F12" => key::F12,
        "HOME" => key::Home,
        "LEFT" | "ARROWLEFT" => key::Left,
        "META" | "CMD" | "COMMAND" | "SUPER" | "WIN" | "WINDOWS" => key::Super_L,
        "PAGEDOWN" => key::Page_Down,
        "PAGEUP" => key::Page_Up,
        "RIGHT" | "ARROWRIGHT" => key::Right,
        "SHIFT" => key::Shift_L,
        "SPACE" => key::space,
        "TAB" => key::Tab,
        "UP" | "ARROWUP" => key::Up,
        _ => {
            let mut characters = normalized.chars();
            let Some(character) = characters.next() else {
                return Err(unsupported_key(name));
            };
            if characters.next().is_some() {
                return Err(unsupported_key(name));
            }
            return Ok(Keysym::from_char(character.to_ascii_lowercase()));
        }
    };
    Ok(Keysym::new(raw))
}

fn unsupported_key(name: &str) -> CuError {
    CuError::new(
        ErrorCode::UnsupportedInput,
        format!("unsupported key name {name:?}"),
    )
}

fn input_error(error: impl std::fmt::Display) -> CuError {
    CuError::new(ErrorCode::InputFailed, error.to_string())
}

fn capture_error(error: impl std::fmt::Display) -> CuError {
    CuError::new(ErrorCode::CaptureFailed, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scroll_pixels_are_converted_to_nonzero_wheel_ticks() {
        assert_eq!(wheel_ticks(0), 0);
        assert_eq!(wheel_ticks(1), 1);
        assert_eq!(wheel_ticks(120), 1);
        assert_eq!(wheel_ticks(121), 2);
        assert_eq!(wheel_ticks(-600), -5);
    }

    #[test]
    fn maps_named_and_unicode_keys() {
        assert_eq!(parse_key("Ctrl").unwrap().raw(), key::Control_L);
        assert_eq!(parse_key("C").unwrap().raw(), key::c);
        assert_eq!(parse_key("你").unwrap(), Keysym::from_char('你'));
        assert!(parse_key("not-a-key").is_err());
    }

    #[test]
    fn finds_base_keysyms_only_in_the_unmodified_column() {
        let keymap = KeyboardMap {
            min_keycode: 8,
            keysyms_per_keycode: 2,
            keysyms: vec![key::a, key::A, key::b, key::B],
            unused: VecDeque::new(),
            mapped: HashMap::new(),
            mapped_order: VecDeque::new(),
            held: HashSet::new(),
        };

        assert_eq!(keymap.base_keycode(key::a), Some(8));
        assert_eq!(keymap.base_keycode(key::A), None);
        assert_eq!(keymap.base_keycode(key::b), Some(9));
    }

    #[test]
    fn maps_downscaled_frame_coordinates_to_the_x11_screen() {
        let transform = CoordinateTransform {
            frame: Viewport {
                width: 640,
                height: 400,
            },
            screen: Viewport {
                width: 1280,
                height: 800,
            },
        };
        let point = Point { x: 320, y: 200 };
        let x = i64::from(point.x) * i64::from(transform.screen.width)
            / i64::from(transform.frame.width);
        let y = i64::from(point.y) * i64::from(transform.screen.height)
            / i64::from(transform.frame.height);

        assert_eq!((x, y), (640, 400));
    }
}
