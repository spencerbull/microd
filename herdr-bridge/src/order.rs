//! Ordering adapter for the installed Herdr release. The 0.8 API owns sorting;
//! 0.9.1 moved it into the local client shell. Its preference file is an optional,
//! version-specific adapter, not a public API for arbitrary client views.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::ValueEnum;
use serde_json::{json, Value};

use super::{one_shot_response, parse_agent_order, AgentOrderMode, HerdrApiError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OrderSelection {
    Auto,
    Grouped,
    Priority,
}

pub struct OrderSettings {
    selection: OrderSelection,
    config_path: PathBuf,
    preferences_path: PathBuf,
}

#[derive(Debug)]
pub struct InvalidOrderSettings;

impl std::fmt::Display for InvalidOrderSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "invalid Herdr ordering settings; correct the settings or use --order grouped|priority",
        )
    }
}

impl std::error::Error for InvalidOrderSettings {}

impl OrderSettings {
    pub fn from_env(selection: OrderSelection, api_socket: &Path, api_override: bool) -> Self {
        Self::from_paths_env(selection, api_socket, api_override, |key| {
            std::env::var(key).ok().map(PathBuf::from)
        })
    }

    fn from_paths_env(
        selection: OrderSelection,
        api_socket: &Path,
        api_override: bool,
        env: impl Fn(&str) -> Option<PathBuf>,
    ) -> Self {
        // Match Herdr's release-build Unix config/state rules, independent of
        // whether this bridge was compiled in debug mode. A config-file override
        // does not relocate the state directory.
        let config_dir = env("XDG_CONFIG_HOME")
            .map(|path| path.join("herdr"))
            .or_else(|| env("HOME").map(|path| path.join(".config/herdr")))
            .unwrap_or_else(|| std::env::temp_dir().join("herdr"));
        let state_dir = env("XDG_STATE_HOME")
            .map(|path| path.join("herdr"))
            .or_else(|| env("HOME").map(|path| path.join(".local/state/herdr")))
            .unwrap_or_else(|| std::env::temp_dir().join("herdr-state"));
        let config_path =
            env("HERDR_CONFIG_PATH").unwrap_or_else(|| config_dir.join("config.toml"));
        // Herdr hashes the *client protocol* socket, not the JSON API socket.
        // An explicit bridge API target also takes precedence over the legacy
        // client-only environment override.
        let client_socket = if api_override {
            client_socket_from_api(api_socket)
        } else {
            env("HERDR_CLIENT_SOCKET_PATH").unwrap_or_else(|| client_socket_from_api(api_socket))
        };
        Self {
            selection,
            config_path,
            preferences_path: preference_path(&state_dir, &client_socket),
        }
    }

    pub fn resolve(&self, socket: &PathBuf) -> Result<AgentOrderMode> {
        match self.selection {
            OrderSelection::Grouped => return Ok(AgentOrderMode::Grouped),
            OrderSelection::Priority => return Ok(AgentOrderMode::Priority),
            OrderSelection::Auto => {}
        }
        match one_shot_response(
            socket,
            &json!({"id":"order", "method":"agent.order.get", "params":{}}),
        ) {
            Ok(response) => parse_agent_order(&response).context(InvalidOrderSettings),
            Err(error) if removed_order_method(&error) => {
                self.local_order().context(InvalidOrderSettings)
            }
            Err(error) => Err(error).context("read Herdr agent order"),
        }
    }

    fn local_order(&self) -> Result<AgentOrderMode> {
        // Read both files on each reconciliation so persisted live toggles and
        // config edits take effect without restarting the bridge. A malformed
        // present setting must not silently select a different agent mapping.
        let configured = match read_optional(&self.config_path)? {
            Some(text) => parse_config_order(&text).with_context(|| {
                format!("read ui.agent_panel_sort in {}", self.config_path.display())
            })?,
            None => AgentOrderMode::Grouped,
        };
        let preferred = match read_optional(&self.preferences_path)? {
            Some(text) => parse_preference_order(&text).with_context(|| {
                format!(
                    "read agent_panel_sort in {}",
                    self.preferences_path.display()
                )
            })?,
            None => None,
        };
        Ok(preferred.unwrap_or(configured))
    }
}

fn removed_order_method(error: &anyhow::Error) -> bool {
    let Some(error) = error.downcast_ref::<HerdrApiError>() else {
        return false;
    };
    error.method == "agent.order.get"
        && (matches!(error.code.as_str(), "unknown_method" | "method_not_found")
            // Herdr 0.9.1 rejects the removed enum variant during request
            // decoding, with invalid_request rather than method_not_found.
            || (error.code == "invalid_request"
                && error.message.starts_with(
                    "invalid request: unknown variant `agent.order.get`, expected ",
                )))
}

fn client_socket_from_api(path: &Path) -> PathBuf {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("herdr");
    path.parent()
        .unwrap_or_else(|| Path::new(""))
        .join(format!("{stem}-client.sock"))
}

fn preference_path(state_dir: &Path, client_socket: &Path) -> PathBuf {
    // Herdr 0.9.1 src/client/shell/preferences.rs::path_for_local_endpoint.
    // Do not canonicalize: Herdr hashes the supplied path's lossy UTF-8 bytes.
    let hash = client_socket
        .to_string_lossy()
        .as_bytes()
        .iter()
        .fold(0xcbf29ce484222325u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        });
    state_dir
        .join("client-shell")
        .join(format!("local-{hash:016x}.json"))
}

fn read_optional(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

fn parse_configured_mode(mode: &str) -> Result<AgentOrderMode> {
    match mode {
        "spaces" | "workspaces" => Ok(AgentOrderMode::Grouped),
        "priority" => Ok(AgentOrderMode::Priority),
        _ => anyhow::bail!(
            "unsupported agent_panel_sort {mode:?}; expected spaces, workspaces, or priority"
        ),
    }
}

fn parse_config_order(text: &str) -> Result<AgentOrderMode> {
    let config: toml::Table = toml::from_str(text).context("parse Herdr TOML config")?;
    let Some(ui) = config.get("ui") else {
        return Ok(AgentOrderMode::Grouped);
    };
    let ui = ui.as_table().context("ui must be a table")?;
    let Some(mode) = ui.get("agent_panel_sort") else {
        return Ok(AgentOrderMode::Grouped);
    };
    parse_configured_mode(
        mode.as_str()
            .context("ui.agent_panel_sort must be a string")?,
    )
}

fn parse_preference_order(text: &str) -> Result<Option<AgentOrderMode>> {
    let preferences: Value =
        serde_json::from_str(text).context("parse Herdr client preferences")?;
    let preferences = preferences
        .as_object()
        .context("client preferences must be an object")?;
    match preferences.get("agent_panel_sort") {
        None | Some(Value::Null) => Ok(None),
        Some(mode) => {
            parse_configured_mode(mode.as_str().context("agent_panel_sort must be a string")?)
                .map(Some)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::{fs, thread};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        dir: PathBuf,
        socket: PathBuf,
        settings: OrderSettings,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "micro-order-{}-{}",
                std::process::id(),
                NEXT_DIR.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&dir).unwrap();
            let socket = dir.join("api.sock");
            let settings = OrderSettings {
                selection: OrderSelection::Auto,
                config_path: dir.join("config.toml"),
                preferences_path: dir.join("preferences.json"),
            };
            Self {
                dir,
                socket,
                settings,
            }
        }

        fn serve(&self, responses: Vec<(&'static str, Value)>) -> thread::JoinHandle<()> {
            let listener = UnixListener::bind(&self.socket).unwrap();
            thread::spawn(move || {
                for (method, response) in responses {
                    let (mut stream, _) = listener.accept().unwrap();
                    let mut request = String::new();
                    BufReader::new(stream.try_clone().unwrap())
                        .read_line(&mut request)
                        .unwrap();
                    let request: Value = serde_json::from_str(&request).unwrap();
                    assert_eq!(request["method"], method);
                    crate::send_line(&mut stream, &response).unwrap();
                }
            })
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.dir).unwrap();
        }
    }

    fn removed_response() -> Value {
        json!({"id":"", "error":{
            "code":"invalid_request",
            "message":"invalid request: unknown variant `agent.order.get`, expected one of `ping`, `agent.list` at line 1 column 39"
        }})
    }

    #[test]
    fn legacy_auto_reads_the_api_each_time_and_ignores_local_settings() {
        let fixture = Fixture::new();
        fs::write(&fixture.settings.config_path, "malformed TOML").unwrap();
        fs::write(&fixture.settings.preferences_path, "malformed JSON").unwrap();
        let server = fixture.serve(vec![
            (
                "agent.order.get",
                json!({"result":{"type":"agent_order","order":"grouped"}}),
            ),
            (
                "agent.order.get",
                json!({"result":{"type":"agent_order","order":"priority"}}),
            ),
        ]);
        assert_eq!(
            fixture.settings.resolve(&fixture.socket).unwrap(),
            AgentOrderMode::Grouped
        );
        assert_eq!(
            fixture.settings.resolve(&fixture.socket).unwrap(),
            AgentOrderMode::Priority
        );
        server.join().unwrap();
    }

    #[test]
    fn modern_reconcile_tracks_preference_and_config_changes() {
        let fixture = Fixture::new();
        let agents = json!([
            {"pane_id":"idle","agent":"codex","agent_status":"idle"},
            {"pane_id":"blocked","agent":"codex","agent_status":"blocked"}
        ]);
        let mut responses = Vec::new();
        for _ in 0..5 {
            responses.extend([
                ("agent.order.get", removed_response()),
                ("agent.list", json!({"result":{"agents":agents}})),
                ("pane.list", json!({"result":{"panes":agents}})),
            ]);
        }
        let server = fixture.serve(responses);
        let mut slots = Default::default();
        for (config, preference, expected) in [
            (None, None, "idle"),
            (Some("priority"), None, "blocked"),
            (Some("priority"), Some("spaces"), "idle"),
            (Some("spaces"), Some("priority"), "blocked"),
            (Some("spaces"), None, "idle"),
        ] {
            if let Some(mode) = config {
                fs::write(
                    &fixture.settings.config_path,
                    format!("[ui]\nagent_panel_sort = {mode:?}\n"),
                )
                .unwrap();
            }
            if let Some(mode) = preference {
                fs::write(
                    &fixture.settings.preferences_path,
                    json!({"agent_panel_sort":mode}).to_string(),
                )
                .unwrap();
            } else if fixture.settings.preferences_path.exists() {
                fs::remove_file(&fixture.settings.preferences_path).unwrap();
            }
            crate::reconcile(&fixture.socket, &mut slots, &fixture.settings).unwrap();
            assert_eq!(slots[0].as_ref().unwrap().pane_id, expected);
        }
        server.join().unwrap();
    }

    #[test]
    fn malformed_modern_settings_fail_startup_with_the_path() {
        let fixture = Fixture::new();
        fs::write(
            &fixture.settings.config_path,
            "[ui]\nagent_panel_sort = 'future'\n",
        )
        .unwrap();
        let server = fixture.serve(vec![("agent.order.get", removed_response())]);
        let error =
            crate::wait_reconcile(&fixture.socket, &mut Default::default(), &fixture.settings)
                .err()
                .expect("startup should fail instead of retrying");
        assert!(error.is::<InvalidOrderSettings>());
        let message = format!("{error:#}");
        assert!(message.contains(fixture.settings.config_path.to_str().unwrap()));
        assert!(message.contains("future"));
        server.join().unwrap();
    }

    #[test]
    fn malformed_live_preference_keeps_the_last_mapping_and_reports_its_path() {
        let fixture = Fixture::new();
        fs::write(
            &fixture.settings.preferences_path,
            r#"{"agent_panel_sort":"future"}"#,
        )
        .unwrap();
        let server = fixture.serve(vec![("agent.order.get", removed_response())]);
        let previous = crate::Slot {
            pane_id: "previous".into(),
            label: "codex".into(),
            status: "idle".into(),
            review_ready: true,
        };
        let mut slots = [Some(previous.clone()), None, None, None, None, None];
        let error = crate::reconcile(&fixture.socket, &mut slots, &fixture.settings)
            .err()
            .expect("invalid preference must reject the refresh");
        assert!(error.is::<InvalidOrderSettings>());
        assert!(format!("{error:#}").contains(fixture.settings.preferences_path.to_str().unwrap()));
        assert!(slots[0].as_ref() == Some(&previous));
        server.join().unwrap();
    }

    #[test]
    fn errors_remain_typed_and_do_not_fall_back_to_local_defaults() {
        for (code, message) in [
            ("unauthorized", "authentication required"),
            ("timeout", "request timed out"),
            ("invalid_request", "invalid request: unknown field `order`"),
            (
                "invalid_request",
                "invalid request: unknown variant `priority`, expected one of `grouped`",
            ),
            (
                "invalid_request",
                "invalid request: unknown variant `agent.list`, expected one of `ping`",
            ),
        ] {
            let fixture = Fixture::new();
            let server = fixture.serve(vec![(
                "agent.order.get",
                json!({"error":{"code":code,"message":message}}),
            )]);
            let error = fixture.settings.resolve(&fixture.socket).unwrap_err();
            let api_error = error.downcast_ref::<HerdrApiError>().unwrap();
            assert_eq!(api_error.code, code);
            assert_eq!(api_error.message, message);
            assert!(!removed_order_method(&error));
            server.join().unwrap();
        }
        for method in ["pane.list", "agent.list"] {
            let error = HerdrApiError {
                method: method.into(),
                code: "unknown_method".into(),
                message: "unknown method".into(),
            }
            .into();
            assert!(!removed_order_method(&error));
        }
    }

    #[test]
    fn unsupported_method_codes_allow_fallback_but_bad_success_does_not() {
        for code in ["unknown_method", "method_not_found"] {
            let fixture = Fixture::new();
            let server = fixture.serve(vec![(
                "agent.order.get",
                json!({"error":{"code":code,"message":"unknown method"}}),
            )]);
            assert_eq!(
                fixture.settings.resolve(&fixture.socket).unwrap(),
                AgentOrderMode::Grouped
            );
            server.join().unwrap();
        }
        for response in [
            json!({"result":{"type":"agent_order","order":"future"}}),
            json!({"result":{"type":"agents","order":"priority"}}),
            json!({"error":{"code":null,"message":"unknown method"}}),
            json!({"error":{"code":"unknown_method"}}),
        ] {
            let fixture = Fixture::new();
            let server = fixture.serve(vec![("agent.order.get", response)]);
            assert!(fixture.settings.resolve(&fixture.socket).is_err());
            server.join().unwrap();
        }
    }

    #[test]
    fn socket_timeout_does_not_select_a_default_order() {
        let fixture = Fixture::new();
        let listener = UnixListener::bind(&fixture.socket).unwrap();
        let server = thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            thread::sleep(std::time::Duration::from_millis(2200));
        });
        let error = fixture.settings.resolve(&fixture.socket).unwrap_err();
        let io_error = error.downcast_ref::<std::io::Error>().unwrap();
        assert!(matches!(
            io_error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ));
        assert!(!removed_order_method(&error));
        server.join().unwrap();
    }

    #[test]
    fn explicit_order_ignores_the_api_and_malformed_files() {
        let mut fixture = Fixture::new();
        fs::write(&fixture.settings.config_path, "not valid").unwrap();
        fs::write(&fixture.settings.preferences_path, "not valid").unwrap();
        for (selection, expected) in [
            (OrderSelection::Grouped, AgentOrderMode::Grouped),
            (OrderSelection::Priority, AgentOrderMode::Priority),
        ] {
            fixture.settings.selection = selection;
            assert_eq!(fixture.settings.resolve(&fixture.socket).unwrap(), expected);
        }
    }

    #[test]
    fn config_and_preference_parsers_reject_present_invalid_settings() {
        for mode in ["spaces", "workspaces", "priority"] {
            let expected = if mode == "priority" {
                AgentOrderMode::Priority
            } else {
                AgentOrderMode::Grouped
            };
            assert_eq!(
                parse_config_order(&format!("[ui]\nagent_panel_sort = {mode:?}")).unwrap(),
                expected
            );
            assert_eq!(
                parse_preference_order(&json!({"agent_panel_sort":mode}).to_string()).unwrap(),
                Some(expected)
            );
        }
        for text in ["", "[ui]", "[theme]\nname = 'catppuccin'"] {
            assert_eq!(parse_config_order(text).unwrap(), AgentOrderMode::Grouped);
        }
        for text in [
            "{}",
            "{\"agent_panel_sort\":null}",
            "{\"sidebar_width\":30}",
        ] {
            assert_eq!(parse_preference_order(text).unwrap(), None);
        }
        for text in [
            "ui = 1",
            "[ui]\nagent_panel_sort = 1",
            "[ui]\nagent_panel_sort = 'grouped'",
            "[ui]\nagent_panel_sort = 'future'",
            "[broken",
        ] {
            assert!(parse_config_order(text).is_err(), "{text}");
        }
        for text in [
            "[]",
            "null",
            "{",
            "{\"agent_panel_sort\":1}",
            "{\"agent_panel_sort\":\"future\"}",
        ] {
            assert!(parse_preference_order(text).is_err(), "{text}");
        }
        let fixture = Fixture::new();
        fs::create_dir(&fixture.settings.config_path).unwrap();
        assert!(
            fixture.settings.local_order().is_err(),
            "unreadable present config must not be treated as missing"
        );
    }

    #[test]
    fn paths_match_herdr_release_env_precedence_and_client_socket_hash() {
        let make = |values: &[(&str, &str)], api_override| {
            OrderSettings::from_paths_env(
                OrderSelection::Auto,
                Path::new("/run/herdr/herdr.sock"),
                api_override,
                |key| {
                    values
                        .iter()
                        .find(|(name, _)| *name == key)
                        .map(|(_, value)| PathBuf::from(value))
                },
            )
        };
        let settings = make(&[("HOME", "/home/alice")], false);
        assert_eq!(
            settings.config_path,
            PathBuf::from("/home/alice/.config/herdr/config.toml")
        );
        assert_eq!(
            settings.preferences_path,
            PathBuf::from(
                "/home/alice/.local/state/herdr/client-shell/local-4ceb3d6c21443f21.json"
            )
        );
        let env = [
            ("HOME", "/home/alice"),
            ("XDG_CONFIG_HOME", "/config"),
            ("XDG_STATE_HOME", "/state"),
            ("HERDR_CONFIG_PATH", "/custom/settings.toml"),
            ("HERDR_CLIENT_SOCKET_PATH", "/tmp/custom-api-client.sock"),
        ];
        let settings = make(&env, false);
        assert_eq!(settings.config_path, PathBuf::from("/custom/settings.toml"));
        assert_eq!(
            settings.preferences_path,
            PathBuf::from("/state/herdr/client-shell/local-00c549280c3876f8.json")
        );
        assert_eq!(
            make(&env, true).preferences_path,
            PathBuf::from("/state/herdr/client-shell/local-4ceb3d6c21443f21.json")
        );
        assert_eq!(
            make(&env[..3], false).config_path,
            PathBuf::from("/config/herdr/config.toml")
        );
        let settings = make(&[], false);
        assert_eq!(
            settings.config_path,
            std::env::temp_dir().join("herdr/config.toml")
        );
        assert_eq!(
            settings.preferences_path,
            std::env::temp_dir().join("herdr-state/client-shell/local-4ceb3d6c21443f21.json")
        );
        assert_eq!(
            client_socket_from_api(Path::new("/tmp/custom-api")),
            PathBuf::from("/tmp/custom-api-client.sock")
        );
        assert_eq!(
            client_socket_from_api(Path::new("/config/herdr/sessions/work/herdr.sock")),
            PathBuf::from("/config/herdr/sessions/work/herdr-client.sock")
        );
    }

    #[test]
    fn cli_defaults_to_auto_and_accepts_only_supported_overrides() {
        use clap::Parser;
        assert_eq!(
            crate::Cli::try_parse_from(["herdr-bridge"]).unwrap().order,
            OrderSelection::Auto
        );
        for (text, selection) in [
            ("auto", OrderSelection::Auto),
            ("grouped", OrderSelection::Grouped),
            ("priority", OrderSelection::Priority),
        ] {
            assert_eq!(
                crate::Cli::try_parse_from(["herdr-bridge", "--order", text])
                    .unwrap()
                    .order,
                selection
            );
        }
        assert!(crate::Cli::try_parse_from(["herdr-bridge", "--order", "future"]).is_err());
    }
}
