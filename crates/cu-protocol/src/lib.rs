use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const MAX_ACTIONS: usize = 16;
pub const MAX_TEXT_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Right,
    Wheel,
    Back,
    Forward,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    Move {
        x: i32,
        y: i32,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        keys: Vec<String>,
    },
    Click {
        x: i32,
        y: i32,
        #[serde(default = "default_mouse_button")]
        button: MouseButton,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        keys: Vec<String>,
    },
    DoubleClick {
        x: i32,
        y: i32,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        keys: Vec<String>,
    },
    Drag {
        path: Vec<Point>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        keys: Vec<String>,
    },
    Scroll {
        x: i32,
        y: i32,
        scroll_x: i32,
        scroll_y: i32,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        keys: Vec<String>,
    },
    Type {
        text: String,
    },
    Keypress {
        keys: Vec<String>,
    },
}

const fn default_mouse_button() -> MouseButton {
    MouseButton::Left
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SettlePolicy {
    #[serde(default = "default_quiet_ms")]
    pub quiet_ms: u64,
    #[serde(default = "default_timeout_ms")]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
pub struct ObserveRequest {
    #[serde(default)]
    pub settle: SettlePolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ActRequest {
    pub expected_frame_id: String,
    pub actions: Vec<Action>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Observation {
    pub frame_id: String,
    pub target: String,
    pub width: u32,
    pub height: u32,
    pub coordinate_space: String,
    pub settled: bool,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ActStatus {
    Ok,
    Partial,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ActOutcome {
    pub status: ActStatus,
    pub executed: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action_error: Option<CuError>,
    pub observation: Observation,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Error)]
#[error("{code}: {message}")]
pub struct CuError {
    pub code: ErrorCode,
    pub message: String,
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
    Ok(())
}

fn validate_modifier_keys(keys: &[String]) -> Result<(), CuError> {
    if keys.iter().any(String::is_empty) {
        return Err(CuError::new(
            ErrorCode::InvalidAction,
            "keys must not contain empty names",
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
}
