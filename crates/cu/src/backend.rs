use std::env;

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use cu_backend_wayland::WaylandBackend;
use cu_backend_x11::X11Backend;
use cu_core::{CaptureLimits, Desktop};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum BackendChoice {
    #[default]
    Auto,
    Wayland,
    X11,
}

pub struct BackendOptions {
    pub choice: BackendChoice,
    pub display: Option<String>,
    pub output: Option<String>,
    pub capture_limits: CaptureLimits,
}

pub struct StartedBackend {
    pub desktop: Box<dyn Desktop>,
    pub target: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct SessionEnvironment {
    session_type: Option<String>,
    wayland_display: Option<String>,
    x11_display: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum ResolvedBackend {
    Wayland,
    X11 { display: String },
}

pub fn start(options: &BackendOptions) -> Result<StartedBackend> {
    let environment = SessionEnvironment::from_process();
    let resolved = resolve_backend(options, &environment)?;

    match resolved {
        ResolvedBackend::Wayland => {
            let backend = WaylandBackend::new(options.output.as_deref(), options.capture_limits)
                .map_err(anyhow::Error::new)
                .context(
                    "direct Wayland backend unavailable; this compositor may require the not-yet-implemented portal backend",
                )?;
            let target = format!("direct Wayland output {}", backend.output_name());
            Ok(StartedBackend {
                desktop: Box::new(backend),
                target,
            })
        }
        ResolvedBackend::X11 { display } => {
            let backend = X11Backend::new(&display, options.capture_limits)
                .map_err(anyhow::Error::new)
                .context("X11 backend unavailable")?;
            let target = format!(
                "X11 display {} screen {}",
                backend.display(),
                backend.screen_index()
            );
            Ok(StartedBackend {
                desktop: Box::new(backend),
                target,
            })
        }
    }
}

impl SessionEnvironment {
    fn from_process() -> Self {
        Self {
            session_type: environment_value("XDG_SESSION_TYPE")
                .map(|value| value.to_ascii_lowercase()),
            wayland_display: environment_value("WAYLAND_DISPLAY"),
            x11_display: environment_value("DISPLAY"),
        }
    }
}

fn environment_value(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

fn resolve_backend(
    options: &BackendOptions,
    environment: &SessionEnvironment,
) -> Result<ResolvedBackend> {
    if options.display.is_some() && options.output.is_some() {
        bail!("--display and --output target different display systems");
    }

    match options.choice {
        BackendChoice::Wayland => {
            if options.display.is_some() {
                bail!("--display requires --backend x11");
            }
            require_wayland(environment)?;
            Ok(ResolvedBackend::Wayland)
        }
        BackendChoice::X11 => {
            if options.output.is_some() {
                bail!("--output requires --backend wayland");
            }
            resolve_x11_display(options, environment)
        }
        BackendChoice::Auto => resolve_auto(options, environment),
    }
}

fn resolve_auto(
    options: &BackendOptions,
    environment: &SessionEnvironment,
) -> Result<ResolvedBackend> {
    if options.display.is_some() {
        return resolve_x11_display(options, environment);
    }
    if options.output.is_some() {
        require_wayland(environment)?;
        return Ok(ResolvedBackend::Wayland);
    }

    match environment.session_type.as_deref() {
        Some("wayland") => {
            require_wayland(environment)?;
            return Ok(ResolvedBackend::Wayland);
        }
        Some("x11") => return resolve_x11_display(options, environment),
        _ => {}
    }

    if environment.wayland_display.is_some() {
        return Ok(ResolvedBackend::Wayland);
    }
    if let Some(display) = &environment.x11_display {
        return Ok(ResolvedBackend::X11 {
            display: display.clone(),
        });
    }

    bail!("unable to detect a desktop session; set WAYLAND_DISPLAY or DISPLAY, or pass --backend")
}

fn require_wayland(environment: &SessionEnvironment) -> Result<()> {
    if environment.wayland_display.is_none() {
        bail!("Wayland backend requires WAYLAND_DISPLAY");
    }
    Ok(())
}

fn resolve_x11_display(
    options: &BackendOptions,
    environment: &SessionEnvironment,
) -> Result<ResolvedBackend> {
    let display = options
        .display
        .clone()
        .or_else(|| environment.x11_display.clone())
        .context("X11 backend requires --display or DISPLAY")?;
    Ok(ResolvedBackend::X11 { display })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(choice: BackendChoice) -> BackendOptions {
        BackendOptions {
            choice,
            display: None,
            output: None,
            capture_limits: CaptureLimits::default(),
        }
    }

    #[test]
    fn auto_prefers_a_wayland_session_over_xwayland() {
        let environment = SessionEnvironment {
            session_type: Some("wayland".to_owned()),
            wayland_display: Some("wayland-1".to_owned()),
            x11_display: Some(":1".to_owned()),
        };

        assert_eq!(
            resolve_backend(&options(BackendChoice::Auto), &environment).unwrap(),
            ResolvedBackend::Wayland
        );
    }

    #[test]
    fn auto_selects_x11_for_an_x11_session() {
        let environment = SessionEnvironment {
            session_type: Some("x11".to_owned()),
            wayland_display: None,
            x11_display: Some(":0".to_owned()),
        };

        assert_eq!(
            resolve_backend(&options(BackendChoice::Auto), &environment).unwrap(),
            ResolvedBackend::X11 {
                display: ":0".to_owned()
            }
        );
    }

    #[test]
    fn explicit_display_selects_x11_in_auto_mode() {
        let mut options = options(BackendChoice::Auto);
        options.display = Some(":99".to_owned());
        let environment = SessionEnvironment {
            session_type: Some("wayland".to_owned()),
            wayland_display: Some("wayland-1".to_owned()),
            x11_display: Some(":1".to_owned()),
        };

        assert_eq!(
            resolve_backend(&options, &environment).unwrap(),
            ResolvedBackend::X11 {
                display: ":99".to_owned()
            }
        );
    }

    #[test]
    fn auto_does_not_fall_back_to_xwayland_when_wayland_is_broken() {
        let environment = SessionEnvironment {
            session_type: Some("wayland".to_owned()),
            wayland_display: None,
            x11_display: Some(":1".to_owned()),
        };

        let error = resolve_backend(&options(BackendChoice::Auto), &environment).unwrap_err();

        assert!(error.to_string().contains("WAYLAND_DISPLAY"));
    }

    #[test]
    fn explicit_backend_rejects_foreign_target_options() {
        let mut options = options(BackendChoice::Wayland);
        options.display = Some(":99".to_owned());
        let environment = SessionEnvironment {
            wayland_display: Some("wayland-1".to_owned()),
            ..SessionEnvironment::default()
        };

        assert!(resolve_backend(&options, &environment).is_err());
    }
}
