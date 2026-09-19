use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
};

use cu_backend_x11::X11Backend;
use cu_core::{CaptureLimits, Desktop};
use cu_protocol::{Action, MouseButton, Point, Viewport};
use x11rb::{
    COPY_DEPTH_FROM_PARENT,
    connection::Connection,
    protocol::{
        Event,
        xproto::{ConnectionExt, CreateWindowAux, EventMask, KeyButMask, WindowClass},
    },
    rust_connection::RustConnection,
};

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[derive(Debug)]
enum Input {
    Down(u32),
    Up(u32, Point),
    Move(u32, Point),
}

fn events(connection: &RustConnection) -> Vec<Input> {
    connection.get_input_focus().unwrap().reply().unwrap();
    let mut events = Vec::new();
    while let Some(event) = connection.poll_for_event().unwrap() {
        match event {
            Event::ButtonPress(event) => events.push(Input::Down(event.time)),
            Event::ButtonRelease(event) => events.push(Input::Up(
                event.time,
                Point {
                    x: i32::from(event.root_x),
                    y: i32::from(event.root_y),
                },
            )),
            Event::MotionNotify(event) => events.push(Input::Move(
                event.time,
                Point {
                    x: i32::from(event.root_x),
                    y: i32::from(event.root_y),
                },
            )),
            _ => {}
        }
    }
    events
}

fn press_release(events: &[Input]) -> (u32, u32) {
    let downs: Vec<_> = events
        .iter()
        .filter_map(|event| {
            if let Input::Down(time) = event {
                Some(*time)
            } else {
                None
            }
        })
        .collect();
    let ups: Vec<_> = events
        .iter()
        .filter_map(|event| {
            if let Input::Up(time, _) = event {
                Some(*time)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(downs.len(), 1, "{events:?}");
    assert_eq!(ups.len(), 1, "{events:?}");
    (downs[0], ups[0])
}

fn desktop() -> (Server, RustConnection, X11Backend) {
    let mut server = Server(
        Command::new("Xvfb")
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                "640x480x24",
                "-nolisten",
                "tcp",
                "-ac",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let mut display = String::new();
    BufReader::new(server.0.stdout.take().unwrap())
        .read_line(&mut display)
        .unwrap();
    let display = format!(":{}", display.trim().parse::<u32>().unwrap());
    let (observer, screen_index) = x11rb::connect(Some(&display)).unwrap();
    let screen = &observer.setup().roots[screen_index];
    let root = screen.root;
    let window = observer.generate_id().unwrap();
    observer
        .create_window(
            COPY_DEPTH_FROM_PARENT,
            window,
            root,
            0,
            0,
            640,
            480,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &CreateWindowAux::new().override_redirect(1).event_mask(
                EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION,
            ),
        )
        .unwrap()
        .check()
        .unwrap();
    observer.map_window(window).unwrap().check().unwrap();
    let mut backend = X11Backend::new(&display, CaptureLimits::default()).unwrap();
    backend.capture().unwrap();
    events(&observer);
    (server, observer, backend)
}

#[test]
#[ignore = "starts a disposable Xvfb server and requires local X11 sockets"]
fn timed_pointer_actions_generate_real_events_and_release_inputs() {
    let (_server, observer, mut backend) = desktop();
    let root = observer.setup().roots[0].root;
    let viewport = Viewport {
        width: 640,
        height: 480,
    };
    backend
        .execute(
            &Action::Click {
                x: 80,
                y: 80,
                button: MouseButton::Left,
                keys: Vec::new(),
                duration_ms: 120,
            },
            viewport,
        )
        .unwrap();
    let recorded = events(&observer);
    let (down, up) = press_release(&recorded);
    assert!((100..1500).contains(&up.wrapping_sub(down)), "{recorded:?}");

    backend
        .execute(
            &Action::Drag {
                path: vec![
                    Point { x: 80, y: 80 },
                    Point { x: 200, y: 80 },
                    Point { x: 200, y: 160 },
                ],
                hold_ms: 80,
                duration_ms: Some(160),
                keys: vec!["SHIFT".to_owned()],
            },
            viewport,
        )
        .unwrap();
    let recorded = events(&observer);
    let (down, up) = press_release(&recorded);
    assert!((220..1500).contains(&up.wrapping_sub(down)), "{recorded:?}");
    let moves: Vec<_> = recorded
        .iter()
        .filter_map(|event| match event {
            Input::Move(time, point) if *point != (Point { x: 80, y: 80 }) => Some((*time, *point)),
            _ => None,
        })
        .collect();
    assert!(moves.len() >= 8, "{recorded:?}");
    assert!(moves[0].0.wrapping_sub(down) >= 65, "{recorded:?}");
    assert!(
        moves
            .iter()
            .any(|(_, point)| *point == Point { x: 200, y: 80 })
    );
    assert!(matches!(
        recorded.last(),
        Some(Input::Up(_, Point { x: 200, y: 160 }))
    ));
    let mask = observer.query_pointer(root).unwrap().reply().unwrap().mask;
    assert_eq!(
        u16::from(mask) & u16::from(KeyButMask::BUTTON1 | KeyButMask::SHIFT),
        0
    );

    check_stationary_drag(&observer, &mut backend, viewport);

    backend
        .execute(
            &Action::DoubleClick {
                x: 80,
                y: 80,
                keys: Vec::new(),
            },
            viewport,
        )
        .unwrap();
    let recorded = events(&observer);
    let buttons: Vec<_> = recorded
        .iter()
        .filter(|event| !matches!(event, Input::Move(..)))
        .collect();
    assert!(
        matches!(
            buttons.as_slice(),
            [Input::Down(_), Input::Up(..), Input::Down(_), Input::Up(..)]
        ),
        "{recorded:?}"
    );
    assert_eq!(
        u16::from(observer.query_pointer(root).unwrap().reply().unwrap().mask)
            & u16::from(KeyButMask::BUTTON1),
        0
    );
}

fn check_stationary_drag(observer: &RustConnection, backend: &mut X11Backend, viewport: Viewport) {
    // A stationary timed path must remain held, even without any movement.
    backend
        .execute(
            &Action::Drag {
                path: vec![Point { x: 200, y: 160 }; 2],
                hold_ms: 30,
                duration_ms: Some(90),
                keys: Vec::new(),
            },
            viewport,
        )
        .unwrap();
    let recorded = events(observer);
    let (down, up) = press_release(&recorded);
    assert!((100..1500).contains(&up.wrapping_sub(down)), "{recorded:?}");
}
