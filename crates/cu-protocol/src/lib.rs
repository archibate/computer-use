use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const MAX_ACTIONS: usize = 16;
pub const MAX_KEYS: usize = 16;
pub const MAX_DRAG_POINTS: usize = 256;
pub const MAX_TEXT_BYTES: usize = 64 * 1024;

/// Mouse button used by a click action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Right,
    Wheel,
    Back,
    Forward,
}

/// One pixel coordinate in the returned screenshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Point {
    /// Horizontal pixel offset from the left edge; must be less than the frame width.
    #[schemars(range(min = 0))]
    pub x: i32,
    /// Vertical pixel offset from the top edge; must be less than the frame height.
    #[schemars(range(min = 0))]
    pub y: i32,
}

/// One input operation grounded in the latest returned screenshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    /// Move the pointer to a screenshot pixel.
    Move {
        /// Horizontal frame pixel; zero is the left edge.
        #[schemars(range(min = 0))]
        x: i32,
        /// Vertical frame pixel; zero is the top edge.
        #[schemars(range(min = 0))]
        y: i32,
        /// Keys held during the move, normally modifiers such as `CTRL` or `SHIFT`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        #[schemars(length(max = 16))]
        keys: Vec<String>,
    },
    /// Move to a screenshot pixel and click one mouse button.
    Click {
        /// Horizontal frame pixel; zero is the left edge.
        #[schemars(range(min = 0))]
        x: i32,
        /// Vertical frame pixel; zero is the top edge.
        #[schemars(range(min = 0))]
        y: i32,
        /// Button to click; defaults to `left`.
        #[serde(default = "default_mouse_button")]
        button: MouseButton,
        /// Keys held during the click, normally modifiers such as `CTRL` or `SHIFT`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        #[schemars(length(max = 16))]
        keys: Vec<String>,
    },
    /// Move to a screenshot pixel and double-click the left mouse button.
    DoubleClick {
        /// Horizontal frame pixel; zero is the left edge.
        #[schemars(range(min = 0))]
        x: i32,
        /// Vertical frame pixel; zero is the top edge.
        #[schemars(range(min = 0))]
        y: i32,
        /// Keys held during both clicks, normally modifiers such as `CTRL` or `SHIFT`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        #[schemars(length(max = 16))]
        keys: Vec<String>,
    },
    /// Hold the left mouse button while following a path of screenshot pixels.
    Drag {
        /// Ordered pointer path containing between 2 and 256 frame-pixel points.
        #[schemars(length(min = 2, max = 256))]
        path: Vec<Point>,
        /// Keys held for the entire drag, normally modifiers such as `CTRL` or `SHIFT`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        #[schemars(length(max = 16))]
        keys: Vec<String>,
    },
    /// Move to a screenshot pixel and scroll. Positive x scrolls right and positive y scrolls down.
    Scroll {
        /// Horizontal frame pixel at which to scroll.
        #[schemars(range(min = 0))]
        x: i32,
        /// Vertical frame pixel at which to scroll.
        #[schemars(range(min = 0))]
        y: i32,
        /// Signed horizontal scroll delta; positive is right and negative is left.
        scroll_x: i32,
        /// Signed vertical scroll delta; positive is down and negative is up.
        scroll_y: i32,
        /// Keys held during scrolling, normally modifiers such as `CTRL` or `SHIFT`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        #[schemars(length(max = 16))]
        keys: Vec<String>,
    },
    /// Type Unicode text using the backend's text-input path.
    Type {
        /// UTF-8 text of at most 65536 bytes; NUL bytes are rejected.
        text: String,
    },
    /// Press all listed keys in order as one chord, then release them in reverse order.
    Keypress {
        /// Between 1 and 16 case-insensitive names: ALT, BACKSPACE, CTRL, DELETE, arrows,
        /// END, ENTER, ESC, F1-F12, HOME, META/SUPER, PAGEDOWN, PAGEUP, SHIFT, SPACE,
        /// TAB, or one Unicode character. Examples: `["CTRL", "L"]` and `["ENTER"]`.
        #[schemars(length(min = 1, max = 16))]
        keys: Vec<String>,
    },
}

const fn default_mouse_button() -> MouseButton {
    MouseButton::Left
}

/// Visual settling policy applied before an observation is returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SettlePolicy {
    /// Required unchanged-screen interval in milliseconds, from 1 through 5000.
    #[serde(default = "default_quiet_ms")]
    #[schemars(range(min = 1, max = 5000))]
    pub quiet_ms: u64,
    /// Overall wait limit in milliseconds, at least `quiet_ms` and at most 30000.
    #[serde(default = "default_timeout_ms")]
    #[schemars(range(min = 1, max = 30000))]
    pub timeout_ms: u64,
}

const fn default_quiet_ms() -> u64 {
    150
}

const fn default_timeout_ms() -> u64 {
    1_500
}

impl Default for SettlePolicy {
    fn default() -> Self {
        Self {
            quiet_ms: default_quiet_ms(),
            timeout_ms: default_timeout_ms(),
        }
    }
}

/// Request a fresh screenshot after waiting for the target to settle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
pub struct ObserveRequest {
    /// Optional settling policy; defaults to 150 ms quiet within a 1500 ms timeout.
    #[serde(default)]
    pub settle: SettlePolicy,
}

/// Execute a bounded batch of input actions against one observed frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(title = "cu action input")]
pub struct ActRequest {
    /// Opaque `frame_id` from the latest observe or act result. Observe again after `stale_frame`.
    #[schemars(length(min = 1))]
    pub expected_frame_id: String,
    /// Between 1 and 16 sequential actions. Batch only when intermediate UI need not be inspected.
    #[schemars(length(min = 1, max = 16))]
    pub actions: Vec<Action>,
    /// Settling policy applied before capturing the post-action screenshot.
    #[serde(default)]
    pub settle: SettlePolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum DaemonRequest {
    Observe(ObserveRequest),
    Act(ActRequest),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestEnvelope {
    pub request_id: String,
    #[serde(flatten)]
    pub request: DaemonRequest,
}

/// Pixel dimensions of a returned screenshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Viewport {
    /// Screenshot width in pixels.
    pub width: u32,
    /// Screenshot height in pixels.
    pub height: u32,
}

/// Coordinate system used by action positions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CoordinateSpace {
    /// Integer pixels in the returned screenshot, with origin at its top-left corner.
    FramePixels,
}

/// A frame-grounded desktop observation returned as structured data alongside a PNG image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Observation {
    /// Opaque frame identifier required by the next `computer_act` call.
    pub frame_id: String,
    /// Backend-defined identifier for the captured desktop target.
    pub target: String,
    /// PNG width and exclusive upper bound for action x coordinates.
    pub width: u32,
    /// PNG height and exclusive upper bound for action y coordinates.
    pub height: u32,
    /// Coordinate system used by all action points.
    pub coordinate_space: CoordinateSpace,
    /// Whether the screen stayed unchanged for `quiet_ms`; an unsettled frame remains usable.
    pub settled: bool,
    /// Owner-only local path backing the returned PNG image content.
    pub image_path: String,
}

impl Observation {
    #[must_use]
    pub const fn viewport(&self) -> Viewport {
        Viewport {
            width: self.width,
            height: self.height,
        }
    }
}

/// Whether every requested action executed or execution stopped after a prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ActStatus {
    /// Every action executed and the returned observation is the resulting state.
    Ok,
    /// Only `executed` leading actions ran; inspect `action_error` and the returned observation.
    Partial,
}

/// Structured result returned by `computer_act` alongside the resulting PNG image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ActOutcome {
    /// Complete or partial execution status.
    pub status: ActStatus,
    /// Number of leading actions that executed successfully.
    #[schemars(range(max = 16))]
    pub executed: usize,
    /// Failure that stopped a partial batch; absent when status is `ok`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action_error: Option<CuError>,
    /// Whether a cached replay outlived its retained screenshot.
    #[serde(default, skip_serializing_if = "is_false")]
    pub image_expired: bool,
    /// Fresh post-action or post-failure frame, absent only after its cached image expires.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observation: Option<Observation>,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", content = "result", rename_all = "snake_case")]
pub enum DaemonResponse {
    Observe(Observation),
    Act(ActOutcome),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseResult {
    Ok(DaemonResponse),
    Error(CuError),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseEnvelope {
    pub request_id: String,
    pub result: ResponseResult,
}

/// Stable machine-readable category for a daemon or action failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    StaleFrame,
    TargetGone,
    LeaseConflict,
    InvalidAction,
    OutOfBounds,
    UnsupportedInput,
    CaptureFailed,
    InputFailed,
    PartialExecution,
    Indeterminate,
    Timeout,
    ProtocolError,
    Internal,
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = serde_variant_name(*self);
        f.write_str(value)
    }
}

const fn serde_variant_name(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::StaleFrame => "stale_frame",
        ErrorCode::TargetGone => "target_gone",
        ErrorCode::LeaseConflict => "lease_conflict",
        ErrorCode::InvalidAction => "invalid_action",
        ErrorCode::OutOfBounds => "out_of_bounds",
        ErrorCode::UnsupportedInput => "unsupported_input",
        ErrorCode::CaptureFailed => "capture_failed",
        ErrorCode::InputFailed => "input_failed",
        ErrorCode::PartialExecution => "partial_execution",
        ErrorCode::Indeterminate => "indeterminate",
        ErrorCode::Timeout => "timeout",
        ErrorCode::ProtocolError => "protocol_error",
        ErrorCode::Internal => "internal",
    }
}

/// Structured computer-use failure returned as an MCP tool error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Error)]
#[error("{code}: {message}")]
pub struct CuError {
    /// Stable error category. Observe again after `stale_frame`.
    pub code: ErrorCode,
    /// Human-readable diagnostic and recovery context.
    pub message: String,
    /// Number of leading actions known to have executed, when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executed: Option<usize>,
}

impl CuError {
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            executed: None,
        }
    }

    #[must_use]
    pub const fn with_executed(mut self, executed: usize) -> Self {
        self.executed = Some(executed);
        self
    }
}

/// Validate an action batch against the observed viewport and protocol limits.
///
/// # Errors
///
/// Returns [`CuError`] when the frame identifier, batch, settle policy, or an
/// individual action is invalid.
pub fn validate_act_request(request: &ActRequest, viewport: Viewport) -> Result<(), CuError> {
    if request.expected_frame_id.is_empty() {
        return Err(CuError::new(
            ErrorCode::InvalidAction,
            "expected_frame_id must not be empty",
        ));
    }
    if request.actions.is_empty() {
        return Err(CuError::new(
            ErrorCode::InvalidAction,
            "actions must not be empty",
        ));
    }
    if request.actions.len() > MAX_ACTIONS {
        return Err(CuError::new(
            ErrorCode::InvalidAction,
            format!("actions exceeds the limit of {MAX_ACTIONS}"),
        ));
    }
    validate_settle_policy(request.settle)?;
    for action in &request.actions {
        validate_action(action, viewport)?;
    }
    Ok(())
}

/// Validate the quiet period and overall timeout used for visual settling.
///
/// # Errors
///
/// Returns [`CuError`] when either duration is outside the protocol limits.
pub fn validate_settle_policy(policy: SettlePolicy) -> Result<(), CuError> {
    if policy.quiet_ms == 0 || policy.quiet_ms > 5_000 {
        return Err(CuError::new(
            ErrorCode::InvalidAction,
            "quiet_ms must be between 1 and 5000",
        ));
    }
    if policy.timeout_ms < policy.quiet_ms || policy.timeout_ms > 30_000 {
        return Err(CuError::new(
            ErrorCode::InvalidAction,
            "timeout_ms must be at least quiet_ms and no more than 30000",
        ));
    }
    Ok(())
}

/// Validate one action against the current frame coordinate space.
///
/// # Errors
///
/// Returns [`CuError`] for invalid coordinates, key names, paths, or text size.
pub fn validate_action(action: &Action, viewport: Viewport) -> Result<(), CuError> {
    match action {
        Action::Move { x, y, keys }
        | Action::Click { x, y, keys, .. }
        | Action::DoubleClick { x, y, keys }
        | Action::Scroll { x, y, keys, .. } => {
            validate_point(Point { x: *x, y: *y }, viewport)?;
            validate_modifier_keys(keys)?;
        }
        Action::Drag { path, keys } => {
            if path.len() < 2 {
                return Err(CuError::new(
                    ErrorCode::InvalidAction,
                    "drag path must contain at least two points",
                ));
            }
            if path.len() > MAX_DRAG_POINTS {
                return Err(CuError::new(
                    ErrorCode::InvalidAction,
                    format!("drag path exceeds the limit of {MAX_DRAG_POINTS} points"),
                ));
            }
            for point in path {
                validate_point(*point, viewport)?;
            }
            validate_modifier_keys(keys)?;
        }
        Action::Type { text } => {
            if text.as_bytes().contains(&0) {
                return Err(CuError::new(
                    ErrorCode::InvalidAction,
                    "text must not contain NUL bytes",
                ));
            }
            if text.len() > MAX_TEXT_BYTES {
                return Err(CuError::new(
                    ErrorCode::InvalidAction,
                    format!("text exceeds the {MAX_TEXT_BYTES}-byte limit"),
                ));
            }
        }
        Action::Keypress { keys } => validate_keys(keys)?,
    }
    Ok(())
}

fn validate_point(point: Point, viewport: Viewport) -> Result<(), CuError> {
    let valid = point.x >= 0
        && point.y >= 0
        && u32::try_from(point.x).is_ok_and(|x| x < viewport.width)
        && u32::try_from(point.y).is_ok_and(|y| y < viewport.height);
    if valid {
        Ok(())
    } else {
        Err(CuError::new(
            ErrorCode::OutOfBounds,
            format!(
                "point ({}, {}) is outside {}x{}",
                point.x, point.y, viewport.width, viewport.height
            ),
        ))
    }
}

fn validate_keys(keys: &[String]) -> Result<(), CuError> {
    if keys.is_empty() {
        return Err(CuError::new(
            ErrorCode::InvalidAction,
            "keys must not be empty",
        ));
    }
    if keys.iter().any(String::is_empty) {
        return Err(CuError::new(
            ErrorCode::InvalidAction,
            "keys must not contain empty names",
        ));
    }
    if keys.len() > MAX_KEYS {
        return Err(CuError::new(
            ErrorCode::InvalidAction,
            format!("keys exceeds the limit of {MAX_KEYS}"),
        ));
    }
    Ok(())
}

fn validate_modifier_keys(keys: &[String]) -> Result<(), CuError> {
    if keys.iter().any(String::is_empty) {
        return Err(CuError::new(
            ErrorCode::InvalidAction,
            "keys must not contain empty names",
        ));
    }
    if keys.len() > MAX_KEYS {
        return Err(CuError::new(
            ErrorCode::InvalidAction,
            format!("keys exceeds the limit of {MAX_KEYS}"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_out_of_bounds_click() {
        let error = validate_action(
            &Action::Click {
                x: 100,
                y: 50,
                button: MouseButton::Left,
                keys: Vec::new(),
            },
            Viewport {
                width: 100,
                height: 100,
            },
        )
        .unwrap_err();

        assert_eq!(error.code, ErrorCode::OutOfBounds);
    }

    #[test]
    fn serializes_actions_with_stable_discriminators() {
        let action = Action::Keypress {
            keys: vec!["CTRL".to_owned(), "L".to_owned()],
        };

        assert_eq!(
            serde_json::to_value(action).unwrap(),
            serde_json::json!({"type": "keypress", "keys": ["CTRL", "L"]})
        );
    }

    #[test]
    fn rejects_oversized_key_chords_and_drag_paths() {
        let keys = vec!["A".to_owned(); MAX_KEYS + 1];
        assert_eq!(
            validate_action(
                &Action::Keypress { keys },
                Viewport {
                    width: 100,
                    height: 100,
                },
            )
            .unwrap_err()
            .code,
            ErrorCode::InvalidAction
        );

        let path = vec![Point { x: 1, y: 1 }; MAX_DRAG_POINTS + 1];
        assert_eq!(
            validate_action(
                &Action::Drag {
                    path,
                    keys: Vec::new(),
                },
                Viewport {
                    width: 100,
                    height: 100,
                },
            )
            .unwrap_err()
            .code,
            ErrorCode::InvalidAction
        );
    }
}
