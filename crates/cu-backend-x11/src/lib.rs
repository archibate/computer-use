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
    COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT, CURRENT_TIME, NO_SYMBOL, NONE,
    connection::Connection,
    image::{Image as XImage, PixelLayout},
    protocol::{
        xinput::{self, DeviceUse},
        xkb::{self, ConnectionExt as _},
        xproto::{
            AtomEnum, BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, ConnectionExt as _,
            CreateWindowAux, KEY_PRESS_EVENT, KEY_RELEASE_EVENT, MOTION_NOTIFY_EVENT, ModMask,
            PropMode, Screen, Visualtype, WindowClass,
        },
        xtest::ConnectionExt as _,
    },
    rust_connection::RustConnection,
    wrapper::{ConnectionExt as _, GrabServer},
};
use xkeysym::{Keysym, key};

const KEY_DELAY: Duration = Duration::from_millis(12);
const DRAG_DELAY: Duration = Duration::from_millis(8);
const KEYMAP_OWNER_ATOM_NAME: &[u8] = b"_COMPUTER_USE_X11_KEYMAP_OWNER_V1";
const KEYMAP_JOURNAL_ATOM_NAME: &[u8] = b"_COMPUTER_USE_X11_KEYMAP_JOURNAL_V1";
const KEYMAP_JOURNAL_MAGIC: u32 = 0x4355_4b4d;
const KEYMAP_JOURNAL_VERSION: u32 = 1;
const KEYMAP_JOURNAL_HEADER_WORDS: usize = 4;
const MAX_KEYMAP_JOURNAL_WORDS: u32 = 256;

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
        let keymap = KeyboardMap::read(&connection, screen.root, &modifier_keycodes)?;

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
        let _ = self.keymap.restore(&self.connection);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct KeymapJournal {
    keysyms_per_keycode: u8,
    borrowed: Vec<u8>,
}

struct KeymapLease {
    root: u32,
    owner_atom: u32,
    owner_window: u32,
    journal_atom: u32,
}

impl KeymapLease {
    fn acquire(connection: &RustConnection, root: u32) -> Result<Self, CuError> {
        let owner_atom = connection
            .intern_atom(false, KEYMAP_OWNER_ATOM_NAME)
            .map_err(input_error)?
            .reply()
            .map_err(input_error)?
            .atom;
        let journal_atom = connection
            .intern_atom(false, KEYMAP_JOURNAL_ATOM_NAME)
            .map_err(input_error)?
            .reply()
            .map_err(input_error)?
            .atom;
        let owner_window = connection.generate_id().map_err(input_error)?;
        connection
            .create_window(
                COPY_DEPTH_FROM_PARENT,
                owner_window,
                root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_ONLY,
                COPY_FROM_PARENT,
                &CreateWindowAux::new(),
            )
            .map_err(input_error)?
            .check()
            .map_err(input_error)?;

        {
            let _server_grab = GrabServer::grab(connection).map_err(input_error)?;
            let current_owner = connection
                .get_selection_owner(owner_atom)
                .map_err(input_error)?
                .reply()
                .map_err(input_error)?
                .owner;
            if current_owner != NONE {
                return Err(CuError::new(
                    ErrorCode::LeaseConflict,
                    "another X11 backend owns the temporary keyboard map",
                ));
            }
            connection
                .set_selection_owner(owner_window, owner_atom, CURRENT_TIME)
                .map_err(input_error)?
                .check()
                .map_err(input_error)?;
            let claimed_owner = connection
                .get_selection_owner(owner_atom)
                .map_err(input_error)?
                .reply()
                .map_err(input_error)?
                .owner;
            if claimed_owner != owner_window {
                return Err(CuError::new(
                    ErrorCode::LeaseConflict,
                    "failed to claim the X11 temporary keyboard map",
                ));
            }
        }
        connection.sync().map_err(input_error)?;

        Ok(Self {
            root,
            owner_atom,
            owner_window,
            journal_atom,
        })
    }

    fn recover(
        &self,
        connection: &RustConnection,
        min_keycode: u8,
        max_keycode: u8,
        modifier_keycodes: &HashSet<u8>,
    ) -> Result<(), CuError> {
        let reply = connection
            .get_property(
                false,
                self.root,
                self.journal_atom,
                AtomEnum::CARDINAL,
                0,
                MAX_KEYMAP_JOURNAL_WORDS,
            )
            .map_err(input_error)?
            .reply()
            .map_err(input_error)?;
        if reply.type_ == NONE {
            return Ok(());
        }
        if reply.type_ != u32::from(AtomEnum::CARDINAL) || reply.bytes_after != 0 {
            return Err(invalid_keymap_journal(
                "property type or length does not match the journal format",
            ));
        }
        let words = reply
            .value32()
            .ok_or_else(|| invalid_keymap_journal("property is not 32-bit"))?
            .collect::<Vec<_>>();
        let journal = decode_keymap_journal(&words)?;
        let empty_mapping = vec![NO_SYMBOL; usize::from(journal.keysyms_per_keycode)];
        for &keycode in &journal.borrowed {
            if !(min_keycode..=max_keycode).contains(&keycode)
                || modifier_keycodes.contains(&keycode)
            {
                return Err(invalid_keymap_journal(
                    "journal contains a keycode outside the safe temporary range",
                ));
            }
            connection
                .change_keyboard_mapping(1, keycode, journal.keysyms_per_keycode, &empty_mapping)
                .map_err(input_error)?
                .check()
                .map_err(input_error)?;
        }
        connection
            .delete_property(self.root, self.journal_atom)
            .map_err(input_error)?
            .check()
            .map_err(input_error)?;
        connection.sync().map_err(input_error)
    }

    fn persist(
        &self,
        connection: &RustConnection,
        keysyms_per_keycode: u8,
        borrowed: &[u8],
    ) -> Result<(), CuError> {
        let words = encode_keymap_journal(keysyms_per_keycode, borrowed)?;
        connection
            .change_property32(
                PropMode::REPLACE,
                self.root,
                self.journal_atom,
                AtomEnum::CARDINAL,
                &words,
            )
            .map_err(input_error)?
            .check()
            .map_err(input_error)?;
        connection.sync().map_err(input_error)
    }

    fn release(&self, connection: &RustConnection, journal: &KeymapJournal) -> Result<(), CuError> {
        if !journal.borrowed.is_empty() {
            let empty_mapping = vec![NO_SYMBOL; usize::from(journal.keysyms_per_keycode)];
            for &keycode in &journal.borrowed {
                connection
                    .change_keyboard_mapping(
                        1,
                        keycode,
                        journal.keysyms_per_keycode,
                        &empty_mapping,
                    )
                    .map_err(input_error)?
                    .check()
                    .map_err(input_error)?;
            }
            connection
                .delete_property(self.root, self.journal_atom)
                .map_err(input_error)?
                .check()
                .map_err(input_error)?;
        }
        connection
            .set_selection_owner(NONE, self.owner_atom, CURRENT_TIME)
            .map_err(input_error)?
            .check()
            .map_err(input_error)?;
        connection
            .destroy_window(self.owner_window)
            .map_err(input_error)?
            .check()
            .map_err(input_error)?;
        connection.sync().map_err(input_error)
    }
}

struct KeyboardMap {
    min_keycode: u8,
    keysyms_per_keycode: u8,
    keysyms: Vec<u32>,
    unused: VecDeque<u8>,
    borrowed: Vec<u8>,
    mapped: HashMap<u32, u8>,
    mapped_order: VecDeque<u32>,
    held: HashSet<u8>,
    lease: KeymapLease,
}

impl KeyboardMap {
    fn read(
        connection: &RustConnection,
        root: u32,
        modifier_keycodes: &HashSet<u8>,
    ) -> Result<Self, CuError> {
        let setup = connection.setup();
        let min_keycode = setup.min_keycode;
        let max_keycode = setup.max_keycode;
        let lease = KeymapLease::acquire(connection, root)?;
        lease.recover(connection, min_keycode, max_keycode, modifier_keycodes)?;
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
            borrowed: Vec::new(),
            mapped: HashMap::new(),
            mapped_order: VecDeque::new(),
            held: HashSet::new(),
            lease,
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
            if let Err(error) = self.borrow_keycode(connection, keycode) {
                self.unused.push_front(keycode);
                return Err(error);
            }
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

    fn borrow_keycode(&mut self, connection: &RustConnection, keycode: u8) -> Result<(), CuError> {
        self.borrowed.push(keycode);
        if let Err(error) = self
            .lease
            .persist(connection, self.keysyms_per_keycode, &self.borrowed)
        {
            self.borrowed.pop();
            return Err(error);
        }
        Ok(())
    }

    fn base_keycode(&self, symbol: u32) -> Option<u8> {
        base_keycode(
            self.min_keycode,
            self.keysyms_per_keycode,
            &self.keysyms,
            symbol,
        )
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

    fn restore(&mut self, connection: &RustConnection) -> Result<(), CuError> {
        let journal = KeymapJournal {
            keysyms_per_keycode: self.keysyms_per_keycode,
            borrowed: self.borrowed.clone(),
        };
        self.lease.release(connection, &journal)?;
        self.borrowed.clear();
        self.mapped.clear();
        self.mapped_order.clear();
        self.held.clear();
        Ok(())
    }
}

fn encode_keymap_journal(keysyms_per_keycode: u8, borrowed: &[u8]) -> Result<Vec<u32>, CuError> {
    if keysyms_per_keycode == 0 {
        return Err(invalid_keymap_journal("keysyms-per-keycode is zero"));
    }
    let capacity = borrowed
        .len()
        .checked_add(KEYMAP_JOURNAL_HEADER_WORDS)
        .ok_or_else(|| invalid_keymap_journal("journal length overflows"))?;
    let mut words = Vec::with_capacity(capacity);
    words.extend([
        KEYMAP_JOURNAL_MAGIC,
        KEYMAP_JOURNAL_VERSION,
        u32::from(keysyms_per_keycode),
        u32::try_from(borrowed.len())
            .map_err(|_| invalid_keymap_journal("too many borrowed keycodes"))?,
    ]);
    words.extend(borrowed.iter().copied().map(u32::from));
    Ok(words)
}

fn decode_keymap_journal(words: &[u32]) -> Result<KeymapJournal, CuError> {
    let [
        magic,
        version,
        keysyms_per_keycode,
        borrowed_count,
        payload @ ..,
    ] = words
    else {
        return Err(invalid_keymap_journal("journal header is truncated"));
    };
    if *magic != KEYMAP_JOURNAL_MAGIC || *version != KEYMAP_JOURNAL_VERSION {
        return Err(invalid_keymap_journal(
            "journal magic or version is unsupported",
        ));
    }
    let keysyms_per_keycode = u8::try_from(*keysyms_per_keycode)
        .ok()
        .filter(|&count| count != 0)
        .ok_or_else(|| invalid_keymap_journal("invalid keysyms-per-keycode"))?;
    let borrowed_count = usize::try_from(*borrowed_count)
        .map_err(|_| invalid_keymap_journal("borrowed keycode count is too large"))?;
    if payload.len() != borrowed_count {
        return Err(invalid_keymap_journal("journal payload length is invalid"));
    }
    let mut seen = HashSet::new();
    let mut borrowed = Vec::with_capacity(borrowed_count);
    for &raw_keycode in payload {
        let keycode = u8::try_from(raw_keycode)
            .map_err(|_| invalid_keymap_journal("keycode is outside the X11 range"))?;
        if keycode == 8 || !seen.insert(keycode) {
            return Err(invalid_keymap_journal(
                "journal contains a reserved or duplicate keycode",
            ));
        }
        borrowed.push(keycode);
    }
    Ok(KeymapJournal {
        keysyms_per_keycode,
        borrowed,
    })
}

fn invalid_keymap_journal(message: &str) -> CuError {
    CuError::new(
        ErrorCode::InputFailed,
        format!("invalid cu X11 keymap recovery journal: {message}"),
    )
}

fn base_keycode(
    min_keycode: u8,
    keysyms_per_keycode: u8,
    keysyms: &[u32],
    symbol: u32,
) -> Option<u8> {
    keysyms
        .chunks(usize::from(keysyms_per_keycode))
        .position(|symbols| symbols.first() == Some(&symbol))
        .and_then(|offset| u8::try_from(offset).ok())
        .and_then(|offset| min_keycode.checked_add(offset))
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
        let keysyms = [key::a, key::A, key::b, key::B];

        assert_eq!(base_keycode(8, 2, &keysyms, key::a), Some(8));
        assert_eq!(base_keycode(8, 2, &keysyms, key::A), None);
        assert_eq!(base_keycode(8, 2, &keysyms, key::b), Some(9));
    }

    #[test]
    fn keymap_recovery_journal_round_trips_borrowed_keycodes() {
        let journal = KeymapJournal {
            keysyms_per_keycode: 4,
            borrowed: vec![97, 103],
        };

        let encoded =
            encode_keymap_journal(journal.keysyms_per_keycode, &journal.borrowed).unwrap();

        assert_eq!(decode_keymap_journal(&encoded).unwrap(), journal);
    }

    #[test]
    fn keymap_recovery_journal_rejects_duplicate_keycodes() {
        let words = [KEYMAP_JOURNAL_MAGIC, KEYMAP_JOURNAL_VERSION, 1, 2, 97, 97];

        assert!(decode_keymap_journal(&words).is_err());
    }

    #[test]
    #[ignore = "requires an isolated X11 server in CU_X11_TEST_DISPLAY"]
    fn recovers_journaled_mapping_after_previous_x11_client_disconnects() {
        let display = test_display();
        let stale = install_stale_journaled_mapping(&display);
        let backend = X11Backend::new(&display, CaptureLimits::default()).unwrap();
        assert_mapping_was_recovered(&display, stale);
        drop(backend);
    }

    #[test]
    #[ignore = "requires an isolated X11 server in CU_X11_TEST_DISPLAY"]
    fn backend_without_borrowed_keys_does_not_delete_a_later_journal() {
        let display = test_display();
        let backend = X11Backend::new(&display, CaptureLimits::default()).unwrap();
        let stale = install_stale_journaled_mapping(&display);
        drop(backend);

        let replacement = X11Backend::new(&display, CaptureLimits::default()).unwrap();
        assert_mapping_was_recovered(&display, stale);
        drop(replacement);
    }

    #[test]
    #[ignore = "requires an isolated X11 server in CU_X11_TEST_DISPLAY"]
    fn keymap_lease_rejects_a_second_live_backend_and_releases_on_drop() {
        let display = test_display();
        let first = X11Backend::new(&display, CaptureLimits::default()).unwrap();

        match X11Backend::new(&display, CaptureLimits::default()) {
            Err(error) => assert_eq!(error.code, ErrorCode::LeaseConflict),
            Ok(_) => panic!("a second backend unexpectedly acquired the keymap lease"),
        }

        drop(first);
        let replacement = X11Backend::new(&display, CaptureLimits::default()).unwrap();
        drop(replacement);
    }

    #[derive(Clone, Copy)]
    struct StaleMapping {
        keycode: u8,
        root: u32,
        journal_atom: u32,
    }

    fn test_display() -> String {
        std::env::var("CU_X11_TEST_DISPLAY")
            .expect("CU_X11_TEST_DISPLAY must name an isolated X11 server")
    }

    fn install_stale_journaled_mapping(display: &str) -> StaleMapping {
        let (connection, screen_index) = x11rb::connect(Some(display)).unwrap();
        let root = connection.setup().roots[screen_index].root;
        let min_keycode = connection.setup().min_keycode;
        let max_keycode = connection.setup().max_keycode;
        let count = max_keycode - min_keycode + 1;
        let modifier_keycodes = read_modifier_keycodes(&connection).unwrap();
        let mapping = connection
            .get_keyboard_mapping(min_keycode, count)
            .unwrap()
            .reply()
            .unwrap();
        let keycode = mapping
            .keysyms
            .chunks(usize::from(mapping.keysyms_per_keycode))
            .zip(min_keycode..=max_keycode)
            .find(|(symbols, keycode)| {
                *keycode != 8
                    && !modifier_keycodes.contains(keycode)
                    && symbols.iter().all(|&symbol| symbol == NO_SYMBOL)
            })
            .map(|(_, keycode)| keycode)
            .expect("isolated X11 server must expose a spare keycode");
        let journal_atom = connection
            .intern_atom(false, KEYMAP_JOURNAL_ATOM_NAME)
            .unwrap()
            .reply()
            .unwrap()
            .atom;
        let journal_words = encode_keymap_journal(mapping.keysyms_per_keycode, &[keycode]).unwrap();
        connection
            .change_property32(
                PropMode::REPLACE,
                root,
                journal_atom,
                AtomEnum::CARDINAL,
                &journal_words,
            )
            .unwrap()
            .check()
            .unwrap();
        let stale_symbol = Keysym::from_char('你').raw();
        connection
            .change_keyboard_mapping(1, keycode, 2, &[stale_symbol, stale_symbol])
            .unwrap()
            .check()
            .unwrap();
        connection.sync().unwrap();

        StaleMapping {
            keycode,
            root,
            journal_atom,
        }
    }

    fn assert_mapping_was_recovered(display: &str, stale: StaleMapping) {
        let (connection, _) = x11rb::connect(Some(display)).unwrap();
        let restored = connection
            .get_keyboard_mapping(stale.keycode, 1)
            .unwrap()
            .reply()
            .unwrap();
        assert!(restored.keysyms.iter().all(|&symbol| symbol == NO_SYMBOL));
        let property = connection
            .get_property(
                false,
                stale.root,
                stale.journal_atom,
                AtomEnum::CARDINAL,
                0,
                MAX_KEYMAP_JOURNAL_WORDS,
            )
            .unwrap()
            .reply()
            .unwrap();
        assert_eq!(property.type_, NONE);
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
