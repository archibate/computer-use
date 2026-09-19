//! Shared timing and release handling for pointer input.

use std::{
    thread,
    time::{Duration, Instant},
};

use cu_protocol::{CuError, Point};

const STEP: Duration = Duration::from_millis(8);

/// Preserve an input failure and report a release failure as well.
///
/// # Errors
/// Returns either failure, combining diagnostics when both operations failed.
pub fn finish_input<T>(
    result: Result<T, CuError>,
    release: Result<(), CuError>,
) -> Result<T, CuError> {
    match (result, release) {
        (Err(mut error), Err(release)) => {
            error.message = format!("{}; input release failed: {release}", error.message);
            Err(error)
        }
        (result, Ok(())) => result,
        (Ok(_), Err(error)) => Err(error),
    }
}

/// Execute an operation while holding a button, then attempt release even on failure.
/// A failed press can have reached the display, so it also requires a release attempt.
///
/// # Errors
/// Returns a press, operation, or release failure.
pub fn with_held_button<T>(
    input: &mut T,
    mut button: impl FnMut(&mut T, bool) -> Result<(), CuError>,
    operation: impl FnOnce(&mut T) -> Result<(), CuError>,
) -> Result<(), CuError> {
    let result = button(input, true).and_then(|()| operation(input));
    finish_input(result, button(input, false))
}

/// Move along a validated path after the caller has pressed at its first point.
/// Omitted movement timing retains the historical move-then-sleep pacing.
///
/// # Errors
/// Returns the first pointer injection failure; the caller must release the button.
pub fn drag_path(
    path: &[Point],
    hold_ms: u64,
    duration_ms: Option<u64>,
    mut move_pointer: impl FnMut(Point) -> Result<(), CuError>,
) -> Result<(), CuError> {
    let Some(duration_ms) = duration_ms else {
        thread::sleep(Duration::from_millis(hold_ms));
        for &point in path.iter().skip(1) {
            move_pointer(point)?;
            thread::sleep(STEP);
        }
        return Ok(());
    };

    let started = Instant::now();
    play_steps(
        timed_steps(path, hold_ms, duration_ms),
        |at| thread::sleep(at.saturating_sub(started.elapsed())),
        move_pointer,
    )
}

#[derive(Debug, PartialEq, Eq)]
struct TimedPoint {
    at: Duration,
    point: Point,
}

fn play_steps(
    steps: Vec<TimedPoint>,
    mut wait_until: impl FnMut(Duration),
    mut move_pointer: impl FnMut(Point) -> Result<(), CuError>,
) -> Result<(), CuError> {
    for step in steps {
        wait_until(step.at);
        move_pointer(step.point)?;
    }
    Ok(())
}

fn timed_steps(path: &[Point], hold_ms: u64, duration_ms: u64) -> Vec<TimedPoint> {
    let hold = Duration::from_millis(hold_ms);
    let duration = Duration::from_millis(duration_ms);
    let lengths: Vec<_> = path
        .windows(2)
        .map(|pair| {
            let dx = f64::from(pair[1].x) - f64::from(pair[0].x);
            let dy = f64::from(pair[1].y) - f64::from(pair[0].y);
            dx.hypot(dy)
        })
        .collect();
    let total: f64 = lengths.iter().sum();
    if duration.is_zero() || total == 0.0 {
        return path
            .iter()
            .skip(1)
            .map(|&point| TimedPoint {
                at: hold + duration,
                point,
            })
            .collect();
    }

    let mut steps = Vec::new();
    let mut traversed = 0.0;
    let mut tick = STEP;
    // Publishing the starting position at the end of the hold phase also makes
    // its boundary explicit when the first segment is shorter than one tick.
    steps.push(TimedPoint {
        at: hold,
        point: path[0],
    });
    for (pair, length) in path.windows(2).zip(lengths) {
        if length == 0.0 {
            continue;
        }
        let start = duration.mul_f64(traversed / total);
        traversed += length;
        let end = duration.mul_f64((traversed / total).min(1.0));
        while tick < end {
            let fraction =
                tick.saturating_sub(start).as_secs_f64() / end.saturating_sub(start).as_secs_f64();
            steps.push(TimedPoint {
                at: hold + tick,
                point: interpolate(pair[0], pair[1], fraction),
            });
            tick += STEP;
        }
        steps.push(TimedPoint {
            at: hold + end,
            point: pair[1],
        });
        if tick == end {
            tick += STEP;
        }
    }
    steps
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "rounded interpolation stays between validated i32 coordinates"
)]
fn interpolate(from: Point, to: Point, fraction: f64) -> Point {
    let coordinate =
        |a: i32, b: i32| (f64::from(a) + (f64::from(b) - f64::from(a)) * fraction).round() as i32;
    Point {
        x: coordinate(from.x, to.x),
        y: coordinate(from.y, to.y),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cu_protocol::ErrorCode;
    use std::cell::{Cell, RefCell};

    fn point(x: i32, y: i32) -> Point {
        Point { x, y }
    }

    #[test]
    fn movement_is_interpolated_after_hold_and_preserves_corners() {
        let path = [point(0, 0), point(80, 0), point(80, 160)];
        let steps = timed_steps(&path, 50, 240);
        assert_eq!(steps.first().unwrap().at, Duration::from_millis(50));
        assert!(steps.contains(&TimedPoint {
            at: Duration::from_millis(90),
            point: point(40, 0)
        }));
        assert!(steps.contains(&TimedPoint {
            at: Duration::from_millis(130),
            point: point(80, 0)
        }));
        assert_eq!(
            steps.last().unwrap(),
            &TimedPoint {
                at: Duration::from_millis(290),
                point: point(80, 160)
            }
        );
        assert!(steps.windows(2).all(|p| p[0].at <= p[1].at));
    }

    #[test]
    fn stationary_duplicate_and_zero_duration_paths() {
        let a = point(10, 10);
        let b = point(50, 10);
        assert_eq!(
            timed_steps(&[a, a], 30, 70),
            vec![TimedPoint {
                at: Duration::from_millis(100),
                point: a
            }]
        );
        assert_eq!(timed_steps(&[a, a, b], 0, 40), timed_steps(&[a, b], 0, 40));
        assert_eq!(
            timed_steps(&[a, b, a], 20, 0),
            vec![
                TimedPoint {
                    at: Duration::from_millis(20),
                    point: b
                },
                TimedPoint {
                    at: Duration::from_millis(20),
                    point: a
                },
            ]
        );
    }

    #[test]
    fn clock_deadlines_do_not_accumulate_injection_cost() {
        let now = Cell::new(Duration::ZERO);
        let events = RefCell::new(Vec::new());
        play_steps(
            timed_steps(&[point(0, 0), point(24, 0)], 10, 24),
            |deadline| now.set(now.get().max(deadline)),
            |p| {
                events.borrow_mut().push((now.get(), p));
                now.set(now.get() + Duration::from_millis(3));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            events.borrow().last().unwrap(),
            &(Duration::from_millis(34), point(24, 0))
        );
    }

    #[test]
    fn short_moves_keep_all_corners_even_with_extreme_segment_lengths() {
        let path = [
            point(0, 0),
            point(1, 0),
            point(i32::MAX, i32::MAX),
            point(i32::MAX, i32::MAX - 1),
        ];
        for duration in [1, 7, 8, 9, 10_000] {
            let steps = timed_steps(&path, 0, duration);
            assert_eq!(steps.last().unwrap().at, Duration::from_millis(duration));
            assert!(steps.windows(2).all(|pair| pair[0].at <= pair[1].at));
            for corner in path {
                assert!(steps.iter().any(|step| step.point == corner));
            }
        }
    }

    #[test]
    fn playback_failure_stops_motion_and_releases_button() {
        let mut events = Vec::new();
        let result = with_held_button(
            &mut events,
            |events, down| {
                events.push(if down { "down" } else { "up" });
                Ok(())
            },
            |events| {
                play_steps(
                    timed_steps(&[point(0, 0), point(80, 0)], 0, 80),
                    |_| {},
                    |_| {
                        events.push("move");
                        Err(CuError::new(ErrorCode::InputFailed, "motion failed"))
                    },
                )
            },
        );
        assert_eq!(result.unwrap_err().message, "motion failed");
        assert_eq!(events, ["down", "move", "up"]);
        let error = with_held_button(
            &mut (),
            |(), down| {
                if down {
                    Ok(())
                } else {
                    Err(CuError::new(ErrorCode::InputFailed, "release failed"))
                }
            },
            |()| Ok(()),
        )
        .unwrap_err();
        assert_eq!(error.message, "release failed");
    }

    #[test]
    fn failures_still_release_and_preserve_both_diagnostics() {
        for fail_press in [false, true] {
            let mut events = Vec::new();
            let error = with_held_button(
                &mut events,
                |events, down| {
                    events.push(if down { "down" } else { "up" });
                    if down && !fail_press {
                        Ok(())
                    } else {
                        Err(CuError::new(
                            ErrorCode::InputFailed,
                            if down {
                                "press failed"
                            } else {
                                "release failed"
                            },
                        ))
                    }
                },
                |events| {
                    events.push("move");
                    Err(CuError::new(ErrorCode::InputFailed, "motion failed"))
                },
            )
            .unwrap_err();
            assert_eq!(
                events,
                if fail_press {
                    vec!["down", "up"]
                } else {
                    vec!["down", "move", "up"]
                }
            );
            assert!(error.message.contains(if fail_press {
                "press failed"
            } else {
                "motion failed"
            }));
            assert!(error.message.contains("release failed"));
        }
    }
}
