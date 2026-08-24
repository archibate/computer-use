use std::{io::Cursor, thread, time::Duration};

use cu_core::{CapturedFrame, Desktop};
use cu_protocol::{Action, CuError, ErrorCode, MouseButton, Point, Viewport};
use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use image::{DynamicImage, GenericImageView, ImageFormat, imageops::FilterType};
use libwayshot::{OutputInfo, WayshotConnection};

pub struct NiriBackend {
    capture: WayshotConnection,
    input: Enigo,
    output_name: String,
    capture_limits: CaptureLimits,
    transform: Option<CoordinateTransform>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CaptureLimits {
    pub max_width: Option<u32>,
    pub max_height: Option<u32>,
}

#[derive(Debug, Clone, Copy)]
struct CoordinateTransform {
    frame: Viewport,
    logical_origin: Point,
    logical_size: Viewport,
}

impl NiriBackend {
    /// Connect to niri's Wayland screencopy and virtual-input protocols.
    ///
    /// # Errors
    ///
    /// Returns [`CuError`] when output discovery, capture limits, or either
    /// Wayland capability cannot be initialized safely.
    pub fn new(output_name: Option<&str>, capture_limits: CaptureLimits) -> Result<Self, CuError> {
        validate_capture_limits(capture_limits)?;
        let capture = WayshotConnection::new().map_err(|error| {
            CuError::new(
                ErrorCode::CaptureFailed,
                format!("failed to connect to Wayland screencopy: {error}"),
            )
        })?;
        let available_outputs = capture
            .get_all_outputs()
            .iter()
            .map(|output| output.name.clone())
            .collect::<Vec<_>>();
        let output_name = select_output(&available_outputs, output_name)?;
        let input = Enigo::new(&Settings::default()).map_err(|error| {
            CuError::new(
                ErrorCode::InputFailed,
                format!("failed to connect to Wayland input protocols: {error}"),
            )
        })?;
        Ok(Self {
            capture,
            input,
            output_name,
            capture_limits,
            transform: None,
        })
    }

    #[must_use]
    pub fn output_name(&self) -> &str {
        &self.output_name
    }

    fn output(&self) -> Result<OutputInfo, CuError> {
        self.capture
            .get_all_outputs()
            .iter()
            .find(|output| output.name == self.output_name)
            .cloned()
            .ok_or_else(|| {
                CuError::new(
                    ErrorCode::TargetGone,
                    format!("Wayland output {} is unavailable", self.output_name),
                )
            })
    }

    fn encode_frame(
        &mut self,
        image: DynamicImage,
        output: &OutputInfo,
    ) -> Result<CapturedFrame, CuError> {
        let image = fit_within(image, self.capture_limits);
        let (width, height) = image.dimensions();
        let logical_size = output.logical_size();
        let logical_position = output.logical_position();
        self.transform = Some(CoordinateTransform {
            frame: Viewport { width, height },
            logical_origin: Point {
                x: logical_position.x,
                y: logical_position.y,
            },
            logical_size: Viewport {
                width: logical_size.width,
                height: logical_size.height,
            },
        });

        let mut png = Cursor::new(Vec::new());
        image
            .write_to(&mut png, ImageFormat::Png)
            .map_err(|error| {
                CuError::new(
                    ErrorCode::CaptureFailed,
                    format!("failed to encode screenshot as PNG: {error}"),
                )
            })?;
        Ok(CapturedFrame {
            png: png.into_inner(),
            width,
            height,
            target: format!("output:{}", self.output_name),
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

        let x = i64::from(transform.logical_origin.x)
            + i64::from(point.x) * i64::from(transform.logical_size.width)
                / i64::from(transform.frame.width);
        let y = i64::from(transform.logical_origin.y)
            + i64::from(point.y) * i64::from(transform.logical_size.height)
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

    fn move_pointer(&mut self, point: Point, viewport: Viewport) -> Result<(), CuError> {
        let point = self.map_point(point, viewport)?;
        self.input
            .move_mouse(point.x, point.y, Coordinate::Abs)
            .map_err(input_error)
    }

    fn press_keys(&mut self, names: &[String]) -> Result<Vec<Key>, CuError> {
        let keys = names
            .iter()
            .map(|name| parse_key(name))
            .collect::<Result<Vec<_>, _>>()?;
        let mut pressed = Vec::with_capacity(keys.len());
        for key in keys {
            if let Err(error) = self.input.key(key, Direction::Press) {
                self.release_keys(&pressed);
                return Err(input_error(error));
            }
            pressed.push(key);
        }
        Ok(pressed)
    }

    fn release_keys(&mut self, keys: &[Key]) {
        for key in keys.iter().rev() {
            let _ = self.input.key(*key, Direction::Release);
        }
    }

    fn execute_with_modifiers(
        &mut self,
        modifiers: &[String],
        operation: impl FnOnce(&mut Self) -> Result<(), CuError>,
    ) -> Result<(), CuError> {
        let pressed = self.press_keys(modifiers)?;
        let result = operation(self);
        self.release_keys(&pressed);
        result
    }

    fn click(
        &mut self,
        point: Point,
        button: MouseButton,
        viewport: Viewport,
    ) -> Result<(), CuError> {
        self.move_pointer(point, viewport)?;
        self.input
            .button(map_button(button), Direction::Click)
            .map_err(input_error)
    }

    fn keypress(&mut self, names: &[String]) -> Result<(), CuError> {
        let pressed = self.press_keys(names)?;
        self.release_keys(&pressed);
        Ok(())
    }

    fn drag(&mut self, path: &[Point], viewport: Viewport) -> Result<(), CuError> {
        self.move_pointer(path[0], viewport)?;
        self.input
            .button(Button::Left, Direction::Press)
            .map_err(input_error)?;
        let result = path[1..].iter().try_for_each(|point| {
            self.move_pointer(*point, viewport)?;
            thread::sleep(Duration::from_millis(8));
            Ok(())
        });
        let release = self
            .input
            .button(Button::Left, Direction::Release)
            .map_err(input_error);
        result.and(release)
    }
}

impl Desktop for NiriBackend {
    fn capture(&mut self) -> Result<CapturedFrame, CuError> {
        let output = self.output()?;
        let image = self
            .capture
            .screenshot_single_output(&output, false)
            .map_err(|error| {
                CuError::new(
                    ErrorCode::CaptureFailed,
                    format!("failed to capture {}: {error}", self.output_name),
                )
            })?;
        self.encode_frame(image, &output)
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
            Action::Type { .. } => {}
        }
        Ok(())
    }

    fn execute(&mut self, action: &Action, viewport: Viewport) -> Result<(), CuError> {
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
                if horizontal != 0 {
                    this.input
                        .scroll(horizontal, Axis::Horizontal)
                        .map_err(input_error)?;
                }
                if vertical != 0 {
                    this.input
                        .scroll(vertical, Axis::Vertical)
                        .map_err(input_error)?;
                }
                Ok(())
            }),
            Action::Type { text } => self.input.text(text).map_err(input_error),
            Action::Keypress { keys } => self.keypress(keys),
        }
    }
}

fn select_output(outputs: &[String], requested: Option<&str>) -> Result<String, CuError> {
    if outputs.is_empty() {
        return Err(CuError::new(
            ErrorCode::TargetGone,
            "niri exposes no active Wayland outputs",
        ));
    }
    if outputs.len() != 1 {
        return Err(CuError::new(
            ErrorCode::UnsupportedInput,
            format!(
                "safe absolute input currently requires exactly one active output; found {} ({})",
                outputs.len(),
                outputs.join(", ")
            ),
        ));
    }
    let detected = &outputs[0];
    if let Some(requested) = requested
        && requested != detected
    {
        return Err(CuError::new(
            ErrorCode::TargetGone,
            format!(
                "requested Wayland output {requested} is unavailable; active output is {detected}"
            ),
        ));
    }
    Ok(detected.clone())
}

fn validate_capture_limits(limits: CaptureLimits) -> Result<(), CuError> {
    if limits.max_width == Some(0) || limits.max_height == Some(0) {
        return Err(CuError::new(
            ErrorCode::InvalidAction,
            "capture limits must be greater than zero",
        ));
    }
    Ok(())
}

fn fit_within(image: DynamicImage, limits: CaptureLimits) -> DynamicImage {
    let (width, height) = image.dimensions();
    let width_ratio = limits
        .max_width
        .map_or(1.0, |maximum| f64::from(maximum) / f64::from(width));
    let height_ratio = limits
        .max_height
        .map_or(1.0, |maximum| f64::from(maximum) / f64::from(height));
    let ratio = width_ratio.min(height_ratio).min(1.0);
    if ratio >= 1.0 {
        return image;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let resized_width = (f64::from(width) * ratio).round() as u32;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let resized_height = (f64::from(height) * ratio).round() as u32;
    image.resize_exact(resized_width, resized_height, FilterType::Lanczos3)
}

const fn map_button(button: MouseButton) -> Button {
    match button {
        MouseButton::Left => Button::Left,
        MouseButton::Right => Button::Right,
        MouseButton::Wheel => Button::Middle,
        MouseButton::Back => Button::Back,
        MouseButton::Forward => Button::Forward,
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

fn parse_key(name: &str) -> Result<Key, CuError> {
    let normalized = name.trim().to_ascii_uppercase();
    let key = match normalized.as_str() {
        "ALT" | "OPTION" => Key::Alt,
        "BACKSPACE" => Key::Backspace,
        "CTRL" | "CONTROL" => Key::Control,
        "DELETE" | "DEL" => Key::Delete,
        "DOWN" | "ARROWDOWN" => Key::DownArrow,
        "END" => Key::End,
        "ENTER" | "RETURN" => Key::Return,
        "ESC" | "ESCAPE" => Key::Escape,
        "F1" => Key::F1,
        "F2" => Key::F2,
        "F3" => Key::F3,
        "F4" => Key::F4,
        "F5" => Key::F5,
        "F6" => Key::F6,
        "F7" => Key::F7,
        "F8" => Key::F8,
        "F9" => Key::F9,
        "F10" => Key::F10,
        "F11" => Key::F11,
        "F12" => Key::F12,
        "HOME" => Key::Home,
        "LEFT" | "ARROWLEFT" => Key::LeftArrow,
        "META" | "CMD" | "COMMAND" | "SUPER" | "WIN" | "WINDOWS" => Key::Meta,
        "PAGEDOWN" => Key::PageDown,
        "PAGEUP" => Key::PageUp,
        "RIGHT" | "ARROWRIGHT" => Key::RightArrow,
        "SHIFT" => Key::Shift,
        "SPACE" => Key::Space,
        "TAB" => Key::Tab,
        "UP" | "ARROWUP" => Key::UpArrow,
        _ => {
            let mut characters = normalized.chars();
            let Some(character) = characters.next() else {
                return Err(unsupported_key(name));
            };
            if characters.next().is_some() {
                return Err(unsupported_key(name));
            }
            Key::Unicode(character.to_ascii_lowercase())
        }
    };
    Ok(key)
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
    fn maps_frame_coordinates_to_output_logical_coordinates() {
        let transform = CoordinateTransform {
            frame: Viewport {
                width: 1600,
                height: 900,
            },
            logical_origin: Point { x: 100, y: 50 },
            logical_size: Viewport {
                width: 2560,
                height: 1440,
            },
        };

        let point = Point { x: 800, y: 450 };
        let x = i64::from(transform.logical_origin.x)
            + i64::from(point.x) * i64::from(transform.logical_size.width)
                / i64::from(transform.frame.width);
        let y = i64::from(transform.logical_origin.y)
            + i64::from(point.y) * i64::from(transform.logical_size.height)
                / i64::from(transform.frame.height);

        assert_eq!((x, y), (1380, 770));
    }

    #[test]
    fn refuses_ambiguous_multi_output_input() {
        let error = select_output(
            &["HDMI-A-1".to_owned(), "DP-1".to_owned()],
            Some("HDMI-A-1"),
        )
        .unwrap_err();

        assert_eq!(error.code, ErrorCode::UnsupportedInput);
    }

    #[test]
    fn auto_selects_the_only_active_output() {
        assert_eq!(
            select_output(&["HDMI-A-1".to_owned()], None).unwrap(),
            "HDMI-A-1"
        );
    }

    #[test]
    fn native_capture_is_not_resized_without_limits() {
        let image = DynamicImage::new_rgba8(256, 144);

        assert_eq!(
            fit_within(image, CaptureLimits::default()).dimensions(),
            (256, 144)
        );
    }

    #[test]
    fn one_capture_limit_preserves_aspect_ratio() {
        let image = DynamicImage::new_rgba8(256, 144);

        assert_eq!(
            fit_within(
                image,
                CaptureLimits {
                    max_width: Some(160),
                    max_height: None,
                },
            )
            .dimensions(),
            (160, 90)
        );
    }
}
