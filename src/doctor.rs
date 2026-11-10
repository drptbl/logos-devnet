//! `doctor`: checks what logos-devnet needs, one line each, with the fix for what's missing.

use std::process::{Command, Stdio};

use serde::Serialize;

use crate::devnet::Devnet;
use crate::home::RuntimeKind;
use crate::report::Report;
use crate::session;

#[derive(Debug, Serialize)]
pub struct Doctor {
    /// False when any check failed.
    pub ok: bool,
    pub checks: Vec<Check>,
}

#[derive(Debug, Serialize)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
    /// What to do about it, when it failed.
    pub fix: Option<String>,
}

fn pass(name: &str, detail: impl Into<String>) -> Check {
    Check {
        name: name.into(),
        ok: true,
        detail: detail.into(),
        fix: None,
    }
}

fn fail(name: &str, detail: impl Into<String>, fix: impl Into<String>) -> Check {
    Check {
        name: name.into(),
        ok: false,
        detail: detail.into(),
        fix: Some(fix.into()),
    }
}

/// Runs every check. `project_error` is why the project file didn't load, when it didn't.
pub fn run(devnet: &Devnet, project_error: Option<&anyhow::Error>) -> Doctor {
    let settings = devnet.settings();
    let mut checks = Vec::new();

    checks.push(match (project_error, devnet.project()) {
        (Some(e), _) => fail(
            "project file",
            format!("{e:#}"),
            "fix the file, or run from a directory without one",
        ),
        (None, Some(project)) => pass("project file", project.path.display().to_string()),
        (None, None) => pass("project file", "none here (optional)"),
    });

    match settings.runtime {
        RuntimeKind::Docker => {
            checks.push(
                if command_ok("docker", &["info", "--format", "{{.ServerVersion}}"]) {
                    pass("docker", "the daemon answers")
                } else {
                    fail(
                        "docker",
                        "no Docker daemon answers",
                        "start Docker (OrbStack, Docker Desktop or Colima), or use `up --native`",
                    )
                },
            );
            let image = settings.image();
            checks.push(
                if command_ok(
                    "docker",
                    &["image", "inspect", "--format", "{{.Id}}", &image],
                ) {
                    pass("image", format!("{image} is here"))
                } else if command_ok("docker", &["manifest", "inspect", &image]) {
                    pass("image", format!("{image} will be pulled on `up`"))
                } else {
                    fail(
                        "image",
                        format!("{image} is neither here nor pullable"),
                        "`up` builds it locally (this takes a while), or check your network",
                    )
                },
            );
        }
        RuntimeKind::Native => {
            for tool in ["git", "cargo"] {
                checks.push(if command_ok(tool, &["--version"]) {
                    pass(tool, "on PATH")
                } else {
                    fail(
                        tool,
                        "not on PATH",
                        "the native runtime builds LEZ: install git and a Rust toolchain",
                    )
                });
            }
        }
    }

    let running = devnet.running();
    checks.push(if running {
        pass(
            "port",
            format!("{} serves the devnet", settings.sequencer_url()),
        )
    } else if std::net::TcpListener::bind(("127.0.0.1", settings.port)).is_ok() {
        pass("port", format!("{} is free", settings.port))
    } else {
        fail(
            "port",
            format!("{} is taken by something else", settings.port),
            "stop what holds it, or `logos-devnet up --port <another>`",
        )
    });

    if running {
        checks.push(match devnet.faucet_balance() {
            Some(b) if b > 0 => pass("faucet", format!("{b} lepta")),
            _ => fail(
                "faucet",
                "the faucet holds nothing on this chain",
                "`logos-devnet reset` starts a chain that funds it",
            ),
        });
    } else {
        checks.push(pass("devnet", "not running; `logos-devnet up` starts it"));
    }

    if devnet.needs_logosctl() {
        checks.push(if session::logosctl_on_path() {
            pass("logosctl", "on PATH, for sessions")
        } else {
            fail(
                "logosctl",
                "sessions need logosctl, the Logos Core CLI, and it isn't on PATH",
                "install the Logos Core CLI (logosctl)",
            )
        });
    }

    Doctor {
        ok: checks.iter().all(|c| c.ok),
        checks,
    }
}

fn command_ok(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

impl Report for Doctor {
    fn human(&self) -> String {
        let mut out = String::new();
        for c in &self.checks {
            let mark = if c.ok { "ok  " } else { "FAIL" };
            out += &format!("{mark} {:<13} {}\n", c.name, c.detail);
            if let Some(fix) = &c.fix {
                out += &format!("     {:<13} fix: {fix}\n", "");
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn says_how_to_fix_a_failed_check() {
        let report = Doctor {
            ok: false,
            checks: vec![
                pass("docker", "the daemon answers"),
                fail("port", "3040 is taken", "`up --port <another>`"),
            ],
        };
        let text = report.human();
        assert!(text.contains("ok   docker"));
        assert!(text.contains("FAIL port"));
        assert!(text.contains("fix: `up --port <another>`"));
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["checks"][1]["fix"], "`up --port <another>`");
    }

    #[test]
    fn a_missing_program_is_a_failed_check() {
        assert!(!command_ok("/nonexistent/tool", &[]));
    }
}
