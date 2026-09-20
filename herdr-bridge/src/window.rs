//! Focus the dedicated Herdr Micro terminal by stable app ID and exact address.
use anyhow::{Context, Result};
use serde_json::Value;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[cfg(not(test))]
pub fn activate() -> Result<()> {
    // Other desktop platforms retain their existing terminal behavior.
    if !cfg!(target_os = "linux") {
        return Ok(());
    }
    activate_with(&mut SystemDesktop, Duration::from_secs(1))
}

trait Desktop {
    fn display(&self) -> Option<String>;
    fn window_class(&self) -> String;
    fn run(&mut self, args: &[&str], signature: Option<&str>, deadline: Instant) -> Result<Value>;
}

#[cfg(not(test))]
struct SystemDesktop;

#[cfg(not(test))]
impl Desktop for SystemDesktop {
    fn display(&self) -> Option<String> {
        std::env::var("WAYLAND_DISPLAY").ok()
    }

    fn window_class(&self) -> String {
        std::env::var("HERDR_MICRO_WINDOW_CLASS")
            .unwrap_or_else(|_| "dev.planeshift.herdr-micro".into())
    }

    fn run(&mut self, args: &[&str], signature: Option<&str>, deadline: Instant) -> Result<Value> {
        let mut command = Command::new("hyprctl");
        command.args(args);
        if let Some(signature) = signature {
            command.env("HYPRLAND_INSTANCE_SIGNATURE", signature);
        } else {
            command.env_remove("HYPRLAND_INSTANCE_SIGNATURE");
        }
        let output = run_bounded(command, deadline)
            .with_context(|| format!("hyprctl {}", args.join(" ")))?;
        if !output.status.success() {
            anyhow::bail!(
                "Hyprland command failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        if args.first() == Some(&"dispatch") {
            let reply = String::from_utf8_lossy(&output.stdout);
            if reply.trim() != "ok" {
                anyhow::bail!("Hyprland rejected dispatch: {}", reply.trim());
            }
            Ok(Value::Null)
        } else {
            serde_json::from_slice(&output.stdout).context("parse Hyprland response")
        }
    }
}

fn run_bounded(mut command: Command, deadline: Instant) -> Result<Output> {
    if Instant::now() >= deadline {
        anyhow::bail!("Hyprland command exceeded activation deadline");
    }
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command.spawn().context("spawn Hyprland command")?;
    let mut stdout = child.stdout.take().context("capture Hyprland stdout")?;
    let mut stderr = child.stderr.take().context("capture Hyprland stderr")?;
    // Drain concurrently, but bound collection too: descendants can keep a
    // pipe open after the direct child exits. Never join an unbounded reader.
    let (out_tx, out_rx) = mpsc::channel();
    let (err_tx, err_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout.read_to_end(&mut bytes).map(|_| bytes);
        let _ = out_tx.send(result);
    });
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stderr.read_to_end(&mut bytes).map(|_| bytes);
        let _ = err_tx.send(result);
    });
    let result = (|| {
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                anyhow::bail!("Hyprland command exceeded activation deadline");
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let stdout = out_rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .context("Hyprland stdout exceeded activation deadline")??;
        let stderr = err_rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .context("Hyprland stderr exceeded activation deadline")??;
        Ok(Output {
            status,
            stdout,
            stderr,
        })
    })();
    if result.is_err() {
        // This group was created solely for this command; never signal the
        // bridge's or terminal's process group.
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

fn instance(desktop: &mut impl Desktop, deadline: Instant) -> Result<String> {
    let instances = desktop.run(&["instances", "-j"], None, deadline)?;
    let display = desktop.display();
    instances
        .as_array()
        .context("Hyprland instances is not an array")?
        .iter()
        .filter(|item| {
            display.as_deref().is_none_or(|display| {
                item.get("wl_socket").and_then(Value::as_str) == Some(display)
            })
        })
        .max_by_key(|item| item.get("time").and_then(Value::as_u64).unwrap_or(0))
        .and_then(|item| item.get("instance").and_then(Value::as_str))
        .map(str::to_string)
        .context("no matching Hyprland instance")
}

fn window_address(
    desktop: &mut impl Desktop,
    signature: &str,
    deadline: Instant,
) -> Result<String> {
    let clients = desktop.run(&["-j", "clients"], Some(signature), deadline)?;
    let class = desktop.window_class();
    if class.is_empty() {
        anyhow::bail!("HERDR_MICRO_WINDOW_CLASS must not be empty");
    }
    let matches: Vec<_> = clients
        .as_array()
        .context("Hyprland clients is not an array")?
        .iter()
        .filter(|client| client.get("class").and_then(Value::as_str) == Some(&class))
        .collect();
    if matches.len() != 1 {
        anyhow::bail!("expected one Herdr Micro window with class {class}, found {}; launch herdr-micro (use a unique HERDR_MICRO_WINDOW_CLASS for each session)", matches.len());
    }
    let address = matches[0]
        .get("address")
        .and_then(Value::as_str)
        .context("Herdr window has no address")?;
    if !address.starts_with("0x")
        || address.len() <= 2
        || !address[2..].bytes().all(|c| c.is_ascii_hexdigit())
    {
        anyhow::bail!("invalid Herdr window address");
    }
    Ok(address.to_owned())
}

fn activate_with(desktop: &mut impl Desktop, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let signature = instance(desktop, deadline)?;
    let address = window_address(desktop, &signature, deadline)?;
    let selector = format!("address:{address}");
    let expression = format!("hl.dsp.focus({{ window = \"{selector}\" }})");
    if desktop
        .run(&["dispatch", &expression], Some(&signature), deadline)
        .is_err()
    {
        desktop.run(
            &["dispatch", "focuswindow", &selector],
            Some(&signature),
            deadline,
        )?;
    }
    loop {
        let active = desktop.run(&["-j", "activewindow"], Some(&signature), deadline)?;
        if active.get("address").and_then(Value::as_str) == Some(&address) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!("Hyprland did not focus the Herdr window before the activation deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::VecDeque;

    struct Fake {
        replies: VecDeque<Result<Value>>,
        calls: Vec<Vec<String>>,
    }
    impl Fake {
        fn new(replies: Vec<Result<Value>>) -> Self {
            Self {
                replies: replies.into(),
                calls: vec![],
            }
        }
    }
    impl Desktop for Fake {
        fn display(&self) -> Option<String> {
            Some("wayland-1".into())
        }
        fn window_class(&self) -> String {
            "dev.planeshift.herdr-micro".into()
        }
        fn run(
            &mut self,
            args: &[&str],
            signature: Option<&str>,
            _deadline: Instant,
        ) -> Result<Value> {
            self.calls
                .push(args.iter().map(|s| s.to_string()).collect());
            if args != ["instances", "-j"] {
                assert_eq!(signature, Some("current"));
            }
            self.replies
                .pop_front()
                .unwrap_or_else(|| Ok(json!({"address":"0x999"})))
        }
    }
    fn instances() -> Result<Value> {
        Ok(json!([
            {"instance":"old","wl_socket":"wayland-1","time":1},
            {"instance":"wrong","wl_socket":"wayland-2","time":9},
            {"instance":"current","wl_socket":"wayland-1","time":2}
        ]))
    }
    fn clients() -> Result<Value> {
        Ok(json!([
            {"title":"herdr","class":"unrelated-terminal","address":"0x456"},
            {"title":"saber: coding","class":"dev.planeshift.herdr-micro","address":"0x123"}
        ]))
    }
    #[test]
    fn dynamic_title_focuses_exact_address_not_an_unrelated_terminal() {
        let mut fake = Fake::new(vec![
            instances(),
            clients(),
            Ok(Value::Null),
            Ok(json!({"address":"0x123","title":"saber: other workspace"})),
        ]);
        activate_with(&mut fake, Duration::from_millis(100)).unwrap();
        assert_eq!(
            fake.calls[2],
            ["dispatch", "hl.dsp.focus({ window = \"address:0x123\" })"]
        );
    }
    #[test]
    fn legacy_dispatch_and_delayed_focus_are_verified() {
        let mut fake = Fake::new(vec![
            instances(),
            clients(),
            Err(anyhow::anyhow!("old dispatcher")),
            Ok(Value::Null),
            Ok(json!({"address":"0x999"})),
            Ok(json!({"address":"0x123"})),
        ]);
        activate_with(&mut fake, Duration::from_millis(100)).unwrap();
        assert_eq!(fake.calls[3], ["dispatch", "focuswindow", "address:0x123"]);
    }
    #[test]
    fn missing_or_ambiguous_window_never_dispatches() {
        for response in [
            Ok(json!([])),
            Ok(json!([
                {"class":"dev.planeshift.herdr-micro","address":"0x123"},
                {"class":"dev.planeshift.herdr-micro","address":"0x456"}
            ])),
            Ok(json!([{"class":"dev.planeshift.herdr-micro","address":"0x123\"bad"}])),
        ] {
            let mut fake = Fake::new(vec![instances(), response]);
            assert!(activate_with(&mut fake, Duration::from_millis(25)).is_err());
            assert_eq!(fake.calls.len(), 2);
        }
    }
    #[test]
    fn dispatch_success_without_focus_is_failure() {
        let mut fake = Fake::new(vec![instances(), clients(), Ok(Value::Null)]);
        assert!(activate_with(&mut fake, Duration::from_millis(25))
            .unwrap_err()
            .to_string()
            .contains("did not focus"));
    }
    #[test]
    fn child_timeout_is_bounded() {
        let start = Instant::now();
        let mut command = Command::new("sleep");
        command.arg("1");
        assert!(run_bounded(command, start + Duration::from_millis(25)).is_err());
        assert!(start.elapsed() < Duration::from_millis(500));
    }
    #[test]
    fn descendant_holding_output_cannot_extend_the_deadline() {
        let start = Instant::now();
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 10 & exit 0"]);
        assert!(run_bounded(command, start + Duration::from_millis(50)).is_err());
        assert!(start.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn expired_deadline_does_not_spawn() {
        let mut command = Command::new("command-that-does-not-exist");
        command.arg("ignored");
        assert!(run_bounded(command, Instant::now())
            .unwrap_err()
            .to_string()
            .contains("deadline"));
    }

    #[test]
    fn large_output_does_not_deadlock_the_child() {
        let mut command = Command::new("sh");
        command.args(["-c", "head -c 262144 /dev/zero"]);
        assert_eq!(
            run_bounded(command, Instant::now() + Duration::from_secs(2))
                .unwrap()
                .stdout
                .len(),
            262144
        );
    }
}
