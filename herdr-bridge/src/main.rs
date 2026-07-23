//! herdr-bridge: maps Codex Micro pad events onto a running herdr server.
//!
//! Talks to two sockets:
//!   - microd's socket for pad events in and LED commands out
//!   - herdr's JSON API socket for agent/pane/workspace state and control
//!
//! The six Agent Keys mirror up to six herdr agents (working = breathing
//! blue, blocked = flashing amber, done = solid green, idle = dim white);
//! pad controls drive focus/approve/interrupt/navigation.

use anyhow::{Context, Result};
use clap::Parser;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SLOTS: usize = 6;
const HYPRLAND_HERDR_SELECTOR: &str = "title:^herdr$";

#[derive(Parser)]
#[command(name = "herdr-bridge", about = "Bridge microd pad events onto herdr")]
struct Cli {
    /// Path to the herdr API socket (default: $HERDR_SOCKET_PATH or ~/.config/herdr/herdr.sock)
    #[arg(long)]
    socket: Option<PathBuf>,
    /// Path to microd (default: $MICROD_SOCKET, then $XDG_RUNTIME_DIR/microd/microd.sock)
    #[arg(long)]
    hub: Option<PathBuf>,
    /// Stop a bridge-owned Voxtype recording and exit (used by systemd cleanup)
    #[arg(long, hide = true)]
    cleanup_dictation: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum PadEvent {
    /// Agent Key AG00-AG05 pressed
    AgentKey(usize),
    /// Command key ACT06-ACT12 pressed
    Act(u8),
    /// Physical microphone key ACT10: true on press, false on release
    Dictation(bool),
    /// Dial step: +1 clockwise, -1 counter-clockwise
    EncStep(i32),
    /// Dial click
    EncClick,
    Joy(JoyDir),
}

enum DictationCommand {
    Set(bool),
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum VoiceState {
    Idle,
    Recording,
    Processing,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum JoyDir {
    Right,
    Down,
    Left,
    Up,
}

#[derive(Clone, PartialEq)]
struct Slot {
    pane_id: String,
    label: String,
    status: String,
    /// Latched when a previously working agent becomes idle. Herdr's Codex
    /// detector reports that transition as `idle`, so the bridge retains the
    /// review signal until the user focuses the agent from the pad.
    review_ready: bool,
}

#[derive(Debug)]
struct HubAck {
    id: u64,
    ok: bool,
    error: Option<String>,
}

struct DictationSession {
    active: bool,
    marker: PathBuf,
}

impl Default for DictationSession {
    fn default() -> Self {
        Self {
            active: false,
            marker: dictation_marker_path(),
        }
    }
}

impl DictationSession {
    fn set(&mut self, active: bool) -> Result<()> {
        if self.active == active {
            return Ok(());
        }
        if active {
            // A stale marker means the prior bridge was killed before cleanup.
            cleanup_owned_dictation()?;
            let status = voxtype_status()?;
            if status != "idle" {
                anyhow::bail!("Voxtype is already {status}; refusing to take recording ownership");
            }
            if let Some(parent) = self.marker.parent() {
                std::fs::create_dir_all(parent)
                    .context("create bridge runtime directory for dictation ownership")?;
            }
            // Claim ownership before the side effect so ExecStopPost can
            // safely clean up even if the bridge is killed during start.
            std::fs::write(&self.marker, b"herdr-bridge\n")
                .context("record bridge dictation ownership")?;
            if let Err(error) = run_voxtype("start") {
                let _ = remove_dictation_marker(&self.marker);
                return Err(error);
            }
            self.active = true;
        } else {
            run_voxtype("stop")?;
            remove_dictation_marker(&self.marker)?;
            self.active = false;
        }
        println!(
            "ACT10 mic -> dictation {}",
            if active { "started" } else { "stopped" }
        );
        Ok(())
    }
}

impl Drop for DictationSession {
    fn drop(&mut self) {
        if self.active && run_voxtype("stop").is_ok() {
            let _ = remove_dictation_marker(&self.marker);
        }
    }
}

fn dictation_marker_path() -> PathBuf {
    if let Ok(path) = std::env::var("STATE_DIRECTORY") {
        if let Some(path) = path.split(':').next().filter(|path| !path.is_empty()) {
            return PathBuf::from(path).join("dictation-active");
        }
    }
    if let Ok(path) = std::env::var("XDG_STATE_HOME") {
        return PathBuf::from(path).join("herdr-bridge/dictation-active");
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".local/state/herdr-bridge/dictation-active");
    }
    std::env::temp_dir().join("herdr-bridge-dictation-active")
}

fn remove_dictation_marker(path: &PathBuf) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("remove bridge dictation ownership marker"),
    }
}

fn cleanup_owned_dictation() -> Result<()> {
    let marker = dictation_marker_path();
    if !marker.exists() {
        return Ok(());
    }
    run_voxtype("stop")?;
    remove_dictation_marker(&marker)
}

fn run_voxtype(action: &str) -> Result<()> {
    let output = Command::new("voxtype")
        .args(["record", action])
        .output()
        .with_context(|| format!("run voxtype record {action}"))?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("voxtype record {action} failed: {}", error.trim());
    }
    Ok(())
}

fn voxtype_status() -> Result<String> {
    let output = Command::new("voxtype")
        .arg("status")
        .output()
        .context("run voxtype status")?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("voxtype status failed: {}", error.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn dictation_loop(rx: mpsc::Receiver<DictationCommand>, voice_tx: mpsc::Sender<VoiceState>) {
    let mut session = DictationSession::default();
    while let Ok(command) = rx.recv() {
        match command {
            DictationCommand::Set(active) => match session.set(active) {
                Ok(()) if active => {
                    let _ = voice_tx.send(VoiceState::Recording);
                }
                Ok(()) => {
                    let _ = voice_tx.send(VoiceState::Processing);
                    let state = if wait_for_voxtype_idle() {
                        VoiceState::Idle
                    } else {
                        eprintln!(
                            "Voxtype did not return to idle after dictation; preserving attention"
                        );
                        VoiceState::Error
                    };
                    let _ = voice_tx.send(state);
                }
                Err(error) => {
                    eprintln!("ACT10 dictation failed: {error:#}");
                    let _ = voice_tx.send(VoiceState::Error);
                }
            },
            DictationCommand::Shutdown => break,
        }
    }
}

fn wait_for_voxtype_idle() -> bool {
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if matches!(voxtype_status().as_deref(), Ok("idle")) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    false
}

struct DictationThread {
    tx: mpsc::Sender<DictationCommand>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl DictationThread {
    fn start(voice_tx: mpsc::Sender<VoiceState>) -> Self {
        let (tx, rx) = mpsc::channel();
        let handle = std::thread::spawn(move || dictation_loop(rx, voice_tx));
        Self {
            tx,
            handle: Some(handle),
        }
    }

    fn sender(&self) -> mpsc::Sender<DictationCommand> {
        self.tx.clone()
    }
}

impl Drop for DictationThread {
    fn drop(&mut self) {
        let _ = self.tx.send(DictationCommand::Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.cleanup_dictation {
        return cleanup_owned_dictation();
    }
    let herdr_socket = cli.socket.unwrap_or_else(default_herdr_socket);
    let hub_socket = cli.hub.unwrap_or_else(default_hub_socket);

    // microd connection: reader thread decodes pad events; the write half
    // (shared) carries LED commands. Wait for both sockets instead of
    // failing so a service supervisor gets a self-healing pair.
    let hub = wait_connect(&hub_socket, "microd");
    let hub_writer = Arc::new(Mutex::new(hub.try_clone()?));
    let (pad_tx, pad_rx) = mpsc::channel::<PadEvent>();
    let (ack_tx, ack_rx) = mpsc::channel::<HubAck>();
    let (voice_tx, voice_rx) = mpsc::channel();
    let _dictation_thread = DictationThread::start(voice_tx);
    let dictation_tx = _dictation_thread.sender();
    let hub_thread = std::thread::spawn(move || hub_loop(hub, pad_tx, ack_tx, dictation_tx));

    let mut slots: [Option<Slot>; SLOTS] = Default::default();
    let mut rendered: Option<Value> = None;
    let mut voice_state = VoiceState::Idle;
    let mut next_hub_id = 1u64;
    let mut line = String::new();
    let mut last_reconcile = Instant::now();

    let mut agent_panes = wait_reconcile(&herdr_socket, &mut slots);
    let mut lines = subscribe(&herdr_socket, &agent_panes)?;
    render(
        &hub_writer,
        &ack_rx,
        &mut next_hub_id,
        &slots,
        voice_state,
        &mut rendered,
    )?;

    println!(
        "herdr-bridge running: {} <-> {}",
        herdr_socket.display(),
        hub_socket.display()
    );

    loop {
        if hub_thread.is_finished() {
            anyhow::bail!("microd connection lost");
        }

        // Drain pending pad events, coalescing dial steps into one net motion.
        let mut enc_delta = 0i32;
        let mut actions = Vec::new();
        while let Ok(ev) = pad_rx.try_recv() {
            match ev {
                PadEvent::EncStep(d) => enc_delta += d,
                other => actions.push(other),
            }
        }
        if enc_delta != 0 {
            actions.push(PadEvent::EncStep(enc_delta));
        }
        while let Ok(next_voice_state) = voice_rx.try_recv() {
            voice_state = next_voice_state;
        }
        let mut need_reconcile = false;
        for action in actions {
            let result = handle_pad_event(action, &herdr_socket, &mut slots);
            if let Err(e) = result {
                eprintln!("{action:?} failed: {e:#}");
                // A dead pane means our slot state is stale; rebuild it.
                need_reconcile |= format!("{e:#}").contains("pane_not_found");
            }
        }
        render(
            &hub_writer,
            &ack_rx,
            &mut next_hub_id,
            &slots,
            voice_state,
            &mut rendered,
        )?;
        if need_reconcile {
            if let Ok(current) = reconcile(&herdr_socket, &mut slots) {
                agent_panes = current;
                lines = subscribe(&herdr_socket, &agent_panes)?;
                last_reconcile = Instant::now();
            }
            render(
                &hub_writer,
                &ack_rx,
                &mut next_hub_id,
                &slots,
                voice_state,
                &mut rendered,
            )?;
        }

        line.clear();
        match lines.read_line(&mut line) {
            Ok(0) => anyhow::bail!("herdr closed the socket"),
            Ok(_) => {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if msg.get("event").is_some() {
                    match reconcile(&herdr_socket, &mut slots) {
                        Ok(current) => {
                            if current != agent_panes {
                                agent_panes = current;
                                lines = subscribe(&herdr_socket, &agent_panes)?;
                            }
                            last_reconcile = Instant::now();
                        }
                        Err(e) => eprintln!("reconcile failed: {e:#}"),
                    }
                }
                render(
                    &hub_writer,
                    &ack_rx,
                    &mut next_hub_id,
                    &slots,
                    voice_state,
                    &mut rendered,
                )?;
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                if last_reconcile.elapsed() >= Duration::from_secs(5) {
                    match reconcile(&herdr_socket, &mut slots) {
                        Ok(current) => {
                            if current != agent_panes {
                                agent_panes = current;
                                lines = subscribe(&herdr_socket, &agent_panes)?;
                            }
                            render(
                                &hub_writer,
                                &ack_rx,
                                &mut next_hub_id,
                                &slots,
                                voice_state,
                                &mut rendered,
                            )?;
                        }
                        Err(e) => eprintln!("periodic reconcile failed: {e:#}"),
                    }
                    last_reconcile = Instant::now();
                }
                continue;
            }
            Err(e) => return Err(e).context("read from herdr socket"),
        }
    }
}

fn wait_connect(path: &PathBuf, name: &str) -> UnixStream {
    let mut waiting = false;
    loop {
        match UnixStream::connect(path) {
            Ok(s) => return s,
            Err(e) => {
                if !waiting {
                    println!("waiting for {name} at {} ({e})", path.display());
                    waiting = true;
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    }
}

fn default_herdr_socket() -> PathBuf {
    if let Ok(p) = std::env::var("HERDR_SOCKET_PATH") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".config/herdr/herdr.sock")
}

fn default_hub_socket() -> PathBuf {
    if let Ok(p) = std::env::var("MICROD_SOCKET") {
        return PathBuf::from(p);
    }
    if let Ok(p) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(p).join("microd/microd.sock");
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".cache/microd/microd.sock")
}

fn subscription_request(agent_panes: &BTreeSet<String>) -> Value {
    let mut subscriptions = vec![
        json!({"type": "pane.created"}),
        json!({"type": "pane.updated"}),
        json!({"type": "pane.closed"}),
        json!({"type": "pane.exited"}),
        json!({"type": "pane.moved"}),
        json!({"type": "pane.agent_detected"}),
        json!({"type": "tab.closed"}),
        json!({"type": "workspace.closed"}),
    ];
    subscriptions.extend(
        agent_panes
            .iter()
            .map(|pane_id| json!({"type": "pane.agent_status_changed", "pane_id": pane_id})),
    );
    json!({
        "id": "sub",
        "method": "events.subscribe",
        "params": {"subscriptions": subscriptions},
    })
}

fn subscribe(socket: &PathBuf, agent_panes: &BTreeSet<String>) -> Result<BufReader<UnixStream>> {
    let stream = wait_connect(socket, "herdr");
    let mut writer = stream.try_clone()?;
    send_line(&mut writer, &subscription_request(agent_panes))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut lines = BufReader::new(stream);
    let mut ack = String::new();
    lines
        .read_line(&mut ack)
        .context("read herdr subscription acknowledgement")?;
    let ack: Value =
        serde_json::from_str(&ack).context("parse herdr subscription acknowledgement")?;
    if let Some(error) = ack.get("error") {
        anyhow::bail!("herdr rejected event subscription: {error}");
    }
    if ack.pointer("/result/type").and_then(Value::as_str) != Some("subscription_started") {
        anyhow::bail!("unexpected herdr subscription acknowledgement: {ack}");
    }
    lines
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(250)))?;
    Ok(lines)
}

fn wait_reconcile(socket: &PathBuf, slots: &mut [Option<Slot>; SLOTS]) -> BTreeSet<String> {
    let mut waiting = false;
    loop {
        match reconcile(socket, slots) {
            Ok(agent_panes) => return agent_panes,
            Err(e) => {
                if !waiting {
                    println!("waiting for herdr state at {} ({e:#})", socket.display());
                    waiting = true;
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    }
}

/// Read microd's event stream, decode into PadEvents.
fn hub_loop(
    hub: UnixStream,
    pad_tx: mpsc::Sender<PadEvent>,
    ack_tx: mpsc::Sender<HubAck>,
    dictation_tx: mpsc::Sender<DictationCommand>,
) {
    // Joystick gesture arming: fire once when deflection crosses 0.9, re-arm
    // once it falls back below 0.3.
    let mut joy_armed = true;
    for line in BufReader::new(hub).lines() {
        let Ok(line) = line else { return };
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let (Some(id), Some(ok)) = (
            msg.get("id").and_then(Value::as_u64),
            msg.get("ok").and_then(Value::as_bool),
        ) {
            if ack_tx
                .send(HubAck {
                    id,
                    ok,
                    error: msg.get("error").and_then(Value::as_str).map(str::to_string),
                })
                .is_err()
            {
                return;
            }
            continue;
        }
        let pad_event = match msg.get("event").and_then(Value::as_str) {
            Some("key") => decode_key(&msg),
            Some("joystick") => decode_joystick(&msg, &mut joy_armed),
            _ => None,
        };
        if let Some(pe) = pad_event {
            match pe {
                PadEvent::Dictation(active) => {
                    if dictation_tx.send(DictationCommand::Set(active)).is_err() {
                        return;
                    }
                }
                other => {
                    if pad_tx.send(other).is_err() {
                        return;
                    }
                }
            }
        }
    }
}

fn decode_key(msg: &Value) -> Option<PadEvent> {
    let key = msg.get("key").and_then(Value::as_str)?;
    let action = msg.get("action").and_then(Value::as_str)?;
    match (action, key) {
        ("step", "ENC_CW") => Some(PadEvent::EncStep(1)),
        ("step", "ENC_CC") => Some(PadEvent::EncStep(-1)),
        ("press", "ENC_CLK") => Some(PadEvent::EncClick),
        ("press", "ACT10") => Some(PadEvent::Dictation(true)),
        ("release", "ACT10") => Some(PadEvent::Dictation(false)),
        ("press", k) => {
            if let Some(n) = k.strip_prefix("AG").and_then(|n| n.parse::<usize>().ok()) {
                Some(PadEvent::AgentKey(n))
            } else {
                k.strip_prefix("ACT")
                    .and_then(|n| n.parse::<u8>().ok())
                    .map(PadEvent::Act)
            }
        }
        _ => None,
    }
}

fn decode_joystick(msg: &Value, armed: &mut bool) -> Option<PadEvent> {
    let d = msg.get("deflection").and_then(Value::as_f64)?;
    if d < 0.3 {
        *armed = true;
        return None;
    }
    if !*armed || d < 0.9 {
        return None;
    }
    *armed = false;
    // Angle is in normalized turns: 0 = right, 0.25 = down, 0.5 = left, 0.75 = up.
    let a = msg.get("angle").and_then(Value::as_f64)?;
    let dir = match a {
        a if !(0.125..0.875).contains(&a) => JoyDir::Right,
        a if a < 0.375 => JoyDir::Down,
        a if a < 0.625 => JoyDir::Left,
        _ => JoyDir::Up,
    };
    Some(PadEvent::Joy(dir))
}

// ---------------------------------------------------------------------------
// LED rendering (via microd)

fn render(
    hub: &Arc<Mutex<UnixStream>>,
    ack_rx: &mpsc::Receiver<HubAck>,
    next_id: &mut u64,
    slots: &[Option<Slot>; SLOTS],
    voice_state: VoiceState,
    rendered: &mut Option<Value>,
) -> Result<()> {
    let lights: Value = slots
        .iter()
        .enumerate()
        .map(|(i, slot)| {
            let (color, effect) = match slot.as_ref().map(effective_status) {
                Some("working") => (0x0066ff, "shallow_breath"),
                Some("blocked") => (0xffaa00, "breath"),
                Some("done") => (0x00ff00, "solid"),
                Some("idle") => (0x303030, "solid"),
                Some("error" | "failed") => (0xff2020, "breath"),
                Some(_) => (0x300840, "solid"),
                None => (0, "off"),
            };
            json!({ "slot": i, "color": color, "effect": effect, "speed": 50 })
        })
        .collect();
    let ambient = ambient_lighting(slots, voice_state);
    let lighting_config = json!({
        "ambient": ambient,
        // Per-agent lights are driven by v.oai.thstatus. Keep the separate
        // global key zone off so it cannot wash out those six indicators.
        "keys": lighting_side("off", 0, 0.0, 0.0),
    });
    let frame = json!({"lighting_config": lighting_config, "lights": lights});
    if rendered.as_ref() == Some(&frame) {
        return Ok(());
    }

    send_hub_command(
        hub,
        ack_rx,
        next_id,
        json!({
            "cmd": "lighting_config",
            "ambient": frame["lighting_config"]["ambient"],
            "keys": frame["lighting_config"]["keys"],
        }),
        "outer-ring lighting",
    )?;
    send_hub_command(
        hub,
        ack_rx,
        next_id,
        json!({ "cmd": "lights", "lights": frame["lights"] }),
        "agent-key lighting",
    )?;
    *rendered = Some(frame);
    Ok(())
}

fn lighting_side(effect: &str, color: u32, brightness: f64, speed: f64) -> Value {
    json!({
        "effect": effect,
        "brightness": brightness,
        "speed": speed,
        "magic": 0.0,
        "color": color,
    })
}

/// The outer ring advertises the highest-priority state anywhere on the pad:
/// error > approval/blocked > ready for review > working > off.
fn ambient_lighting(slots: &[Option<Slot>; SLOTS], voice_state: VoiceState) -> Value {
    match voice_state {
        VoiceState::Recording => return lighting_side("snake", 0x2e8b57, 1.0, 0.4),
        VoiceState::Processing => return lighting_side("snake", 0xffffff, 1.0, 0.4),
        VoiceState::Error => return lighting_side("snake", 0xff2020, 1.0, 0.55),
        VoiceState::Idle => {}
    }
    let statuses: Vec<&str> = slots.iter().flatten().map(effective_status).collect();
    if statuses
        .iter()
        .any(|status| matches!(*status, "error" | "failed"))
    {
        lighting_side("snake", 0xff2020, 1.0, 0.55)
    } else if statuses.contains(&"blocked") {
        lighting_side("snake", 0xffaa00, 1.0, 0.45)
    } else if statuses.contains(&"done") {
        lighting_side("solid", 0x00ff00, 1.0, 0.0)
    } else if statuses.contains(&"working") {
        lighting_side("snake", 0x0066ff, 1.0, 0.4)
    } else {
        lighting_side("off", 0, 0.0, 0.0)
    }
}

fn effective_status(slot: &Slot) -> &str {
    if slot.review_ready {
        "done"
    } else if slot.status == "done" {
        "idle"
    } else {
        slot.status.as_str()
    }
}

fn send_hub_command(
    hub: &Arc<Mutex<UnixStream>>,
    ack_rx: &mpsc::Receiver<HubAck>,
    next_id: &mut u64,
    mut msg: Value,
    description: &str,
) -> Result<()> {
    let id = *next_id;
    *next_id = next_id.wrapping_add(1);
    msg["id"] = json!(id);
    let mut stream = hub.lock().expect("hub lock");
    stream
        .write_all(format!("{msg}\n").as_bytes())
        .with_context(|| format!("send {description} to microd"))?;
    drop(stream);

    let deadline = Instant::now() + Duration::from_secs(7);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let ack = ack_rx
            .recv_timeout(remaining)
            .context("wait for microd lights acknowledgement")?;
        if ack.id != id {
            continue;
        }
        if !ack.ok {
            anyhow::bail!(
                "microd rejected {description}: {}",
                ack.error.as_deref().unwrap_or("unknown error")
            );
        }
        break;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// herdr state tracking

fn send_line(w: &mut impl Write, msg: &Value) -> Result<()> {
    let mut s = serde_json::to_string(msg)?;
    s.push('\n');
    w.write_all(s.as_bytes()).context("write to herdr socket")
}

fn one_shot(socket: &PathBuf, msg: &Value) -> Result<()> {
    one_shot_response(socket, msg).map(drop)
}

fn one_shot_response(socket: &PathBuf, msg: &Value) -> Result<Value> {
    let mut stream = UnixStream::connect(socket)?;
    send_line(&mut stream, msg)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut resp = String::new();
    BufReader::new(stream).read_line(&mut resp)?;
    let value: Value = serde_json::from_str(&resp).context("parse herdr response")?;
    if value.get("error").is_some() {
        anyhow::bail!("herdr error: {}", resp.trim());
    }
    Ok(value)
}

/// `info` is either an AgentInfo or a PaneInfo — both carry pane_id,
/// agent_status, and naming fields.
fn upsert(slots: &mut [Option<Slot>; SLOTS], info: &Value) {
    let Some(pane_id) = info.get("pane_id").and_then(Value::as_str) else {
        return;
    };
    let label = ["name", "display_agent", "agent", "label"]
        .iter()
        .find_map(|k| info.get(*k).and_then(Value::as_str))
        .unwrap_or(pane_id)
        .to_string();
    let status = info
        .get("agent_status")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let new = Slot {
        pane_id: pane_id.to_string(),
        label,
        review_ready: status == "done",
        status,
    };

    if let Some(existing) = slots
        .iter_mut()
        .flatten()
        .find(|s| s.pane_id == new.pane_id)
    {
        let review_ready = match new.status.as_str() {
            "working" | "blocked" => false,
            "done" if existing.status == "done" => existing.review_ready,
            "done" => true,
            "idle" if existing.status == "working" => true,
            "idle" => existing.review_ready,
            _ => false,
        };
        *existing = Slot {
            review_ready,
            ..new
        };
    } else if let Some(free) = slots.iter_mut().find(|s| s.is_none()) {
        println!("agent {} ({}) -> slot", new.label, new.pane_id);
        *free = Some(new);
    }
}

/// Rebuild slot state from the server. Live agents are the union of
/// registered agents (agent.list, validated against panes that still exist —
/// the registry can hold stale records for panes removed via tab/workspace
/// close) and panes with a detected agent, which don't appear in agent.list.
fn reconcile(socket: &PathBuf, slots: &mut [Option<Slot>; SLOTS]) -> Result<BTreeSet<String>> {
    let fetch = |method: &str| {
        one_shot_response(socket, &json!({"id": "r", "method": method, "params": {}}))
    };
    let agents = fetch("agent.list").context("list herdr agents")?;
    let panes = fetch("pane.list").context("list herdr panes")?;
    let pane_records: Vec<&Value> = panes
        .pointer("/result/panes")
        .and_then(Value::as_array)
        .map(|panes| panes.iter().collect())
        .unwrap_or_default();
    let existing_panes: Vec<&str> = pane_records
        .iter()
        .filter_map(|p| p.get("pane_id").and_then(Value::as_str))
        .collect();
    let mut live: Vec<&Value> = agents
        .pointer("/result/agents")
        .and_then(Value::as_array)
        .map(|agents| {
            agents
                .iter()
                .filter(|a| {
                    a.get("pane_id")
                        .and_then(Value::as_str)
                        .is_some_and(|id| existing_panes.contains(&id))
                })
                .collect()
        })
        .unwrap_or_default();
    for pane in &pane_records {
        let has_agent = pane.get("agent").and_then(Value::as_str).is_some();
        let pane_id = pane.get("pane_id").and_then(Value::as_str);
        let already = pane_id.is_some_and(|id| {
            live.iter()
                .any(|a| a.get("pane_id").and_then(Value::as_str) == Some(id))
        });
        if has_agent && !already {
            live.push(pane);
        }
    }

    for slot in slots.iter_mut() {
        if let Some(s) = slot {
            let still_live = live
                .iter()
                .any(|a| a.get("pane_id").and_then(Value::as_str) == Some(s.pane_id.as_str()));
            if !still_live {
                println!("agent gone: {} ({})", s.label, s.pane_id);
                *slot = None;
            }
        }
    }
    let agent_panes = live
        .iter()
        .filter_map(|agent| agent.get("pane_id").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    for agent in live {
        upsert(slots, agent);
    }
    Ok(agent_panes)
}

// ---------------------------------------------------------------------------
// pad actions -> herdr

fn handle_pad_event(
    ev: PadEvent,
    socket: &PathBuf,
    slots: &mut [Option<Slot>; SLOTS],
) -> Result<()> {
    match ev {
        PadEvent::AgentKey(idx) => {
            if let Some(slot) = slots.get(idx).and_then(Clone::clone) {
                println!("key AG{idx:02} -> focus {} ({})", slot.label, slot.pane_id);
                focus_pane(socket, &slot.pane_id)?;
                if let Some(slot) = slots.get_mut(idx).and_then(Option::as_mut) {
                    slot.review_ready = false;
                }
            } else {
                // Empty agent slot: fall back to focusing the Nth workspace.
                focus_workspace_by_index(socket, idx)?;
            }
        }
        // ACT06 = approve (enter), ACT07 = deny/interrupt (esc) on the focused pane
        PadEvent::Act(6) => send_keys_to_focused(socket, "enter")?,
        PadEvent::Act(7) => send_keys_to_focused(socket, "esc")?,
        // ACT08 = jump to the next agent that needs attention
        PadEvent::Act(8) => focus_next_with_status(socket, slots, "blocked")?,
        // The physical Enter key adjacent to the microphone is ACT11 on v0.4.1.
        PadEvent::Act(11) => send_keys_to_focused(socket, "enter")?,
        PadEvent::Act(n) => println!("ACT{n:02} pressed (unmapped)"),
        PadEvent::Dictation(_) => anyhow::bail!("dictation event reached Herdr action handler"),
        PadEvent::EncStep(dir) => cycle_agent_focus(socket, slots, dir)?,
        PadEvent::EncClick => {
            println!("dial click -> zoom toggle");
            one_shot(
                socket,
                &json!({"id": "zoom", "method": "pane.zoom", "params": {}}),
            )?;
        }
        PadEvent::Joy(JoyDir::Left) => cycle_tab(socket, -1)?,
        PadEvent::Joy(JoyDir::Right) => cycle_tab(socket, 1)?,
        PadEvent::Joy(JoyDir::Up) => cycle_workspace(socket, -1)?,
        PadEvent::Joy(JoyDir::Down) => cycle_workspace(socket, 1)?,
    }
    Ok(())
}

fn focus_pane(socket: &PathBuf, pane_id: &str) -> Result<()> {
    one_shot(
        socket,
        &json!({"id": "focus", "method": "pane.focus", "params": {"pane_id": pane_id}}),
    )?;
    activate_herdr_window_best_effort();
    Ok(())
}

fn activate_herdr_window_best_effort() {
    #[cfg(not(test))]
    if let Err(error) = activate_herdr_window() {
        eprintln!("could not activate Herdr window: {error:#}");
    }
}

fn hyprland_focus_expression(selector: &str) -> String {
    let selector = selector.replace('\\', "\\\\").replace('"', "\\\"");
    format!("hl.dsp.focus({{ window = \"{selector}\" }})")
}

#[cfg(not(test))]
fn activate_herdr_window() -> Result<()> {
    activate_herdr_window_with(&mut CommandHyprlandIpc, Duration::from_millis(200))
}

#[derive(Debug)]
struct HyprlandOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

trait HyprlandIpc {
    fn wayland_display(&self) -> Option<String>;
    fn run(
        &mut self,
        args: &[&str],
        signature: Option<&str>,
        deadline: Instant,
    ) -> Result<HyprlandOutput>;
}

#[cfg(not(test))]
struct CommandHyprlandIpc;

#[cfg(not(test))]
impl HyprlandIpc for CommandHyprlandIpc {
    fn wayland_display(&self) -> Option<String> {
        std::env::var("WAYLAND_DISPLAY").ok()
    }

    fn run(
        &mut self,
        args: &[&str],
        signature: Option<&str>,
        deadline: Instant,
    ) -> Result<HyprlandOutput> {
        let mut command = Command::new("hyprctl");
        command.args(args);
        match signature {
            Some(signature) => {
                command.env("HYPRLAND_INSTANCE_SIGNATURE", signature);
            }
            None => {
                // Instance discovery must also work when the bridge started
                // before Hyprland imported its environment into systemd.
                command.env_remove("HYPRLAND_INSTANCE_SIGNATURE");
            }
        }
        let output = run_bounded_command(command, deadline)?;
        Ok(HyprlandOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

fn run_bounded_command(mut command: Command, deadline: Instant) -> Result<Output> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().context("spawn Hyprland IPC command")?;
    loop {
        if child
            .try_wait()
            .context("poll Hyprland IPC command")?
            .is_some()
        {
            return child
                .wait_with_output()
                .context("collect Hyprland IPC output");
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("Hyprland IPC command exceeded activation deadline");
        }
        std::thread::sleep(remaining.min(Duration::from_millis(5)));
    }
}

fn hyprland_instance_signature(ipc: &mut impl HyprlandIpc, deadline: Instant) -> Result<String> {
    let output = ipc
        .run(&["instances", "-j"], None, deadline)
        .context("discover running Hyprland instances")?;
    if !output.success {
        anyhow::bail!(
            "Hyprland instance discovery failed: {}",
            output.stderr.trim()
        );
    }
    let instances: Value =
        serde_json::from_str(&output.stdout).context("parse Hyprland instance list")?;
    let instances = instances
        .as_array()
        .context("Hyprland instance list was not an array")?;
    let display = ipc.wayland_display();
    let candidates = instances.iter().filter(|instance| {
        display.as_deref().is_none_or(|display| {
            instance.get("wl_socket").and_then(Value::as_str) == Some(display)
        })
    });
    candidates
        .max_by_key(|instance| instance.get("time").and_then(Value::as_u64).unwrap_or(0))
        .and_then(|instance| instance.get("instance").and_then(Value::as_str))
        .map(str::to_string)
        .context("no running Hyprland instance matched this Wayland display")
}

fn activate_herdr_window_with(ipc: &mut impl HyprlandIpc, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let signature = hyprland_instance_signature(ipc, deadline)?;

    let expression = hyprland_focus_expression(HYPRLAND_HERDR_SELECTOR);
    let modern = ipc
        .run(&["dispatch", &expression], Some(&signature), deadline)
        .context("run Hyprland focus dispatcher")?;
    if !modern.success {
        // Hyprland before the Lua dispatcher API used the legacy
        // `focuswindow` command. Keep that fallback for non-Omarchy users.
        let legacy = ipc
            .run(
                &["dispatch", "focuswindow", HYPRLAND_HERDR_SELECTOR],
                Some(&signature),
                deadline,
            )
            .context("run legacy Hyprland focus dispatcher")?;
        if !legacy.success {
            anyhow::bail!(
                "Hyprland rejected window activation: modern={}, legacy={}",
                modern.stderr.trim(),
                legacy.stderr.trim()
            );
        }
    }

    // Compositor focus changes can settle just after the dispatcher returns.
    // Confirm the target before treating the button action as complete.
    while Instant::now() < deadline {
        let active = ipc
            .run(&["-j", "activewindow"], Some(&signature), deadline)
            .context("query active Hyprland window")?;
        if active.success
            && serde_json::from_str::<Value>(&active.stdout)
                .ok()
                .and_then(|window| {
                    window
                        .get("title")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .as_deref()
                == Some("herdr")
        {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if !remaining.is_zero() {
            std::thread::sleep(remaining.min(Duration::from_millis(10)));
        }
    }
    anyhow::bail!("Hyprland did not focus the Herdr window before the activation deadline")
}

fn focused_pane_id(socket: &PathBuf) -> Result<String> {
    let resp = one_shot_response(
        socket,
        &json!({"id": "cur", "method": "pane.current", "params": {}}),
    )?;
    resp.pointer("/result/pane/pane_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .context("no focused pane")
}

fn send_keys_to_focused(socket: &PathBuf, key: &str) -> Result<()> {
    let pane_id = focused_pane_id(socket)?;
    println!("send {key} -> {pane_id}");
    one_shot(
        socket,
        &json!({"id": "keys", "method": "pane.send_keys", "params": {"pane_id": pane_id, "keys": [key]}}),
    )
}

fn focus_workspace_by_index(socket: &PathBuf, idx: usize) -> Result<()> {
    let resp = one_shot_response(
        socket,
        &json!({"id": "ws", "method": "workspace.list", "params": {}}),
    )?;
    let Some(id) = resp
        .pointer("/result/workspaces")
        .and_then(Value::as_array)
        .and_then(|list| list.get(idx))
        .and_then(|w| w.get("workspace_id").and_then(Value::as_str))
    else {
        println!("key AG{idx:02}: no agent and no workspace {}", idx + 1);
        return Ok(());
    };
    println!("key AG{idx:02} -> focus workspace {id}");
    one_shot(
        socket,
        &json!({"id": "wsf", "method": "workspace.focus", "params": {"workspace_id": id}}),
    )?;
    activate_herdr_window_best_effort();
    Ok(())
}

/// Dial: move focus to the next/previous occupied agent slot.
fn cycle_agent_focus(socket: &PathBuf, slots: &[Option<Slot>; SLOTS], dir: i32) -> Result<()> {
    let occupied: Vec<(usize, &Slot)> = slots
        .iter()
        .enumerate()
        .filter_map(|(i, s)| s.as_ref().map(|s| (i, s)))
        .collect();
    if occupied.is_empty() {
        return Ok(());
    }
    let focused = focused_pane_id(socket).unwrap_or_default();
    let pos = occupied.iter().position(|(_, s)| s.pane_id == focused);
    let next = match pos {
        Some(p) => (p as i32 + dir).rem_euclid(occupied.len() as i32) as usize,
        None => 0,
    };
    let (idx, slot) = occupied[next];
    println!("dial -> focus slot {idx} {} ({})", slot.label, slot.pane_id);
    focus_pane(socket, &slot.pane_id)
}

fn focus_next_with_status(
    socket: &PathBuf,
    slots: &[Option<Slot>; SLOTS],
    status: &str,
) -> Result<()> {
    let focused = focused_pane_id(socket).unwrap_or_default();
    match next_slot_with_status(slots, status, &focused) {
        Some(slot) => {
            println!(
                "ACT08 -> focus {} agent {} ({})",
                status, slot.label, slot.pane_id
            );
            focus_pane(socket, &slot.pane_id)
        }
        None => {
            println!("ACT08: no {status} agent");
            Ok(())
        }
    }
}

fn next_slot_with_status<'a>(
    slots: &'a [Option<Slot>; SLOTS],
    status: &str,
    focused_pane_id: &str,
) -> Option<&'a Slot> {
    let start = slots
        .iter()
        .position(|slot| {
            slot.as_ref()
                .is_some_and(|slot| slot.pane_id == focused_pane_id)
        })
        .map_or(0, |index| index + 1);
    (0..SLOTS)
        .map(|offset| (start + offset) % SLOTS)
        .filter_map(|index| slots[index].as_ref())
        .find(|slot| slot.status == status)
}

/// Joystick up/down: focus the previous/next workspace in number order.
fn cycle_workspace(socket: &PathBuf, dir: i32) -> Result<()> {
    let resp = one_shot_response(
        socket,
        &json!({"id": "ws", "method": "workspace.list", "params": {}}),
    )?;
    let Some(workspaces) = resp.pointer("/result/workspaces").and_then(Value::as_array) else {
        return Ok(());
    };
    if workspaces.is_empty() {
        return Ok(());
    }
    let pos = workspaces
        .iter()
        .position(|w| w.get("focused").and_then(Value::as_bool) == Some(true))
        .unwrap_or(0);
    let next = (pos as i32 + dir).rem_euclid(workspaces.len() as i32) as usize;
    let Some(id) = workspaces[next].get("workspace_id").and_then(Value::as_str) else {
        return Ok(());
    };
    println!("joystick -> focus workspace {id}");
    one_shot(
        socket,
        &json!({"id": "wsf", "method": "workspace.focus", "params": {"workspace_id": id}}),
    )?;
    activate_herdr_window_best_effort();
    Ok(())
}

/// Joystick left/right: focus the previous/next tab in the focused workspace.
/// (tab.list without a workspace filter returns tabs across all workspaces.)
fn cycle_tab(socket: &PathBuf, dir: i32) -> Result<()> {
    let ws = one_shot_response(
        socket,
        &json!({"id": "ws", "method": "workspace.list", "params": {}}),
    )?;
    let focused_ws = ws
        .pointer("/result/workspaces")
        .and_then(Value::as_array)
        .and_then(|list| {
            list.iter()
                .find(|w| w.get("focused").and_then(Value::as_bool) == Some(true))
        })
        .and_then(|w| w.get("workspace_id").and_then(Value::as_str))
        .context("no focused workspace")?
        .to_string();
    let resp = one_shot_response(
        socket,
        &json!({"id": "tabs", "method": "tab.list", "params": {"workspace_id": focused_ws}}),
    )?;
    let Some(tabs) = resp.pointer("/result/tabs").and_then(Value::as_array) else {
        return Ok(());
    };
    if tabs.is_empty() {
        return Ok(());
    }
    let pos = tabs
        .iter()
        .position(|t| t.get("focused").and_then(Value::as_bool) == Some(true))
        .unwrap_or(0);
    let next = (pos as i32 + dir).rem_euclid(tabs.len() as i32) as usize;
    let Some(id) = tabs[next].get("tab_id").and_then(Value::as_str) else {
        return Ok(());
    };
    println!("joystick -> focus tab {id}");
    one_shot(
        socket,
        &json!({"id": "tabf", "method": "tab.focus", "params": {"tab_id": id}}),
    )?;
    activate_herdr_window_best_effort();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::fs;
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;

    static NEXT_SOCKET: AtomicU64 = AtomicU64::new(1);
    type ExpectedRequest<'a> = (&'a str, Value);
    type ActionCase<'a> = (PadEvent, usize, Vec<ExpectedRequest<'a>>);

    #[derive(Clone)]
    struct FakeHyprlandReply {
        success: bool,
        stdout: &'static str,
        stderr: &'static str,
    }

    struct FakeHyprlandIpc {
        display: Option<String>,
        replies: VecDeque<FakeHyprlandReply>,
        calls: Vec<(Vec<String>, Option<String>)>,
    }

    impl FakeHyprlandIpc {
        fn new(
            display: Option<&str>,
            replies: impl IntoIterator<Item = FakeHyprlandReply>,
        ) -> Self {
            Self {
                display: display.map(str::to_string),
                replies: replies.into_iter().collect(),
                calls: Vec::new(),
            }
        }
    }

    impl HyprlandIpc for FakeHyprlandIpc {
        fn wayland_display(&self) -> Option<String> {
            self.display.clone()
        }

        fn run(
            &mut self,
            args: &[&str],
            signature: Option<&str>,
            _deadline: Instant,
        ) -> Result<HyprlandOutput> {
            self.calls.push((
                args.iter().map(|arg| (*arg).to_string()).collect(),
                signature.map(str::to_string),
            ));
            let reply = self.replies.pop_front().unwrap_or(FakeHyprlandReply {
                success: true,
                stdout: r#"{"title":"not-herdr"}"#,
                stderr: "",
            });
            Ok(HyprlandOutput {
                success: reply.success,
                stdout: reply.stdout.to_string(),
                stderr: reply.stderr.to_string(),
            })
        }
    }

    fn successful_reply(stdout: &'static str) -> FakeHyprlandReply {
        FakeHyprlandReply {
            success: true,
            stdout,
            stderr: "",
        }
    }

    fn socket_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "herdr-bridge-{label}-{}-{}.sock",
            std::process::id(),
            NEXT_SOCKET.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn slot(pane_id: &str, status: &str) -> Slot {
        Slot {
            pane_id: pane_id.to_string(),
            label: pane_id.to_string(),
            status: status.to_string(),
            review_ready: status == "done",
        }
    }

    fn run_action(
        event: PadEvent,
        slots: &[Option<Slot>; SLOTS],
        request_count: usize,
    ) -> Vec<Value> {
        let path = socket_path("action");
        let listener = UnixListener::bind(&path).unwrap();
        let (request_tx, request_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            for _ in 0..request_count {
                let (mut stream, _) = listener.accept().unwrap();
                let mut line = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                let response = match request.get("method").and_then(Value::as_str) {
                    Some("pane.current") => {
                        json!({"id":request["id"],"result":{"pane":{"pane_id":"p0"}}})
                    }
                    Some("workspace.list") => json!({"id":request["id"],"result":{"workspaces":[
                        {"workspace_id":"w0","focused":true},
                        {"workspace_id":"w1","focused":false},
                        {"workspace_id":"w2","focused":false}
                    ]}}),
                    Some("tab.list") => json!({"id":request["id"],"result":{"tabs":[
                        {"tab_id":"t0","focused":true},
                        {"tab_id":"t1","focused":false}
                    ]}}),
                    _ => json!({"id":request["id"],"result":{}}),
                };
                request_tx.send(request).unwrap();
                send_line(&mut stream, &response).unwrap();
            }
        });
        let mut slots = slots.clone();
        handle_pad_event(event, &path, &mut slots).unwrap();
        server.join().unwrap();
        fs::remove_file(path).unwrap();
        request_rx.try_iter().collect()
    }

    #[test]
    fn subscription_covers_lifecycle_and_each_agent_status() {
        let panes = BTreeSet::from(["wA:p1".to_string(), "wB:p2".to_string()]);
        let request = subscription_request(&panes);
        let subscriptions = request
            .pointer("/params/subscriptions")
            .and_then(Value::as_array)
            .unwrap();
        for event in [
            "pane.created",
            "pane.updated",
            "pane.closed",
            "pane.exited",
            "pane.moved",
            "pane.agent_detected",
            "tab.closed",
            "workspace.closed",
        ] {
            assert!(subscriptions
                .iter()
                .any(
                    |subscription| subscription.get("type").and_then(Value::as_str) == Some(event)
                ));
        }
        let status_panes: BTreeSet<_> = subscriptions
            .iter()
            .filter(|subscription| {
                subscription.get("type").and_then(Value::as_str)
                    == Some("pane.agent_status_changed")
            })
            .filter_map(|subscription| subscription.get("pane_id").and_then(Value::as_str))
            .collect();
        assert_eq!(status_panes, BTreeSet::from(["wA:p1", "wB:p2"]));
    }

    #[test]
    fn subscription_requires_the_documented_acknowledgement() {
        for (ack, accepted) in [
            (
                json!({"id":"sub","result":{"type":"subscription_started"}}),
                true,
            ),
            (
                json!({"id":"sub","error":{"code":"invalid_subscription"}}),
                false,
            ),
        ] {
            let path = socket_path("subscribe");
            let listener = UnixListener::bind(&path).unwrap();
            let server = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut request)
                    .unwrap();
                assert_eq!(
                    serde_json::from_str::<Value>(&request).unwrap()["method"],
                    "events.subscribe"
                );
                send_line(&mut stream, &ack).unwrap();
            });
            let result = subscribe(&path, &BTreeSet::new());
            assert_eq!(result.is_ok(), accepted);
            server.join().unwrap();
            fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn key_and_joystick_decoding_preserves_all_controls() {
        for (key, action, expected) in [
            ("AG00", "press", Some(PadEvent::AgentKey(0))),
            ("AG05", "press", Some(PadEvent::AgentKey(5))),
            ("ACT06", "press", Some(PadEvent::Act(6))),
            ("ACT10", "press", Some(PadEvent::Dictation(true))),
            ("ACT10", "release", Some(PadEvent::Dictation(false))),
            ("ACT11", "press", Some(PadEvent::Act(11))),
            ("ACT12", "press", Some(PadEvent::Act(12))),
            ("ENC_CW", "step", Some(PadEvent::EncStep(1))),
            ("ENC_CC", "step", Some(PadEvent::EncStep(-1))),
            ("ENC_CLK", "press", Some(PadEvent::EncClick)),
            ("AG00", "release", None),
        ] {
            let decoded = decode_key(&json!({"key":key,"action":action}));
            assert_eq!(
                format!("{decoded:?}"),
                format!("{expected:?}"),
                "{key} {action}"
            );
        }

        for (angle, direction) in [
            (0.0, JoyDir::Right),
            (0.25, JoyDir::Down),
            (0.5, JoyDir::Left),
            (0.75, JoyDir::Up),
            (0.99, JoyDir::Right),
        ] {
            let mut armed = true;
            assert_eq!(
                decode_joystick(&json!({"angle":angle,"deflection":1.0}), &mut armed),
                Some(PadEvent::Joy(direction))
            );
            assert!(
                decode_joystick(&json!({"angle":angle,"deflection":1.0}), &mut armed).is_none()
            );
            assert!(
                decode_joystick(&json!({"angle":angle,"deflection":0.0}), &mut armed).is_none()
            );
            assert!(armed);
        }
    }

    #[test]
    fn hyprland_activation_uses_the_current_lua_dispatcher_shape() {
        assert_eq!(
            hyprland_focus_expression(HYPRLAND_HERDR_SELECTOR),
            "hl.dsp.focus({ window = \"title:^herdr$\" })"
        );
        assert_eq!(
            hyprland_focus_expression("title:\"herdr\"\\test"),
            "hl.dsp.focus({ window = \"title:\\\"herdr\\\"\\\\test\" })"
        );
    }

    #[test]
    fn hyprland_activation_discovers_the_current_instance_at_press_time() {
        let mut ipc = FakeHyprlandIpc::new(
            Some("wayland-1"),
            [
                successful_reply(
                    r#"[
                        {"instance":"older","time":1,"wl_socket":"wayland-1"},
                        {"instance":"other-display","time":3,"wl_socket":"wayland-9"},
                        {"instance":"current","time":2,"wl_socket":"wayland-1"}
                    ]"#,
                ),
                successful_reply("ok"),
                successful_reply(r#"{"title":"herdr"}"#),
            ],
        );

        activate_herdr_window_with(&mut ipc, Duration::from_millis(100)).unwrap();

        assert_eq!(
            ipc.calls,
            vec![
                (vec!["instances".to_string(), "-j".to_string()], None),
                (
                    vec![
                        "dispatch".to_string(),
                        "hl.dsp.focus({ window = \"title:^herdr$\" })".to_string()
                    ],
                    Some("current".to_string())
                ),
                (
                    vec!["-j".to_string(), "activewindow".to_string()],
                    Some("current".to_string())
                )
            ]
        );
    }

    #[test]
    fn hyprland_activation_falls_back_to_the_legacy_dispatcher() {
        let mut ipc = FakeHyprlandIpc::new(
            None,
            [
                successful_reply(r#"[{"instance":"current","time":2,"wl_socket":"wayland-1"}]"#),
                FakeHyprlandReply {
                    success: false,
                    stdout: "",
                    stderr: "unknown dispatcher",
                },
                successful_reply("ok"),
                successful_reply(r#"{"title":"herdr"}"#),
            ],
        );

        activate_herdr_window_with(&mut ipc, Duration::from_millis(100)).unwrap();

        assert_eq!(
            ipc.calls[2],
            (
                vec![
                    "dispatch".to_string(),
                    "focuswindow".to_string(),
                    HYPRLAND_HERDR_SELECTOR.to_string()
                ],
                Some("current".to_string())
            )
        );
    }

    #[test]
    fn hyprland_activation_reports_a_dispatch_that_did_not_take_effect() {
        let mut ipc = FakeHyprlandIpc::new(
            Some("wayland-1"),
            [
                successful_reply(r#"[{"instance":"current","time":2,"wl_socket":"wayland-1"}]"#),
                successful_reply("ok"),
            ],
        );

        let error = activate_herdr_window_with(&mut ipc, Duration::from_millis(25)).unwrap_err();

        assert!(
            error.to_string().contains("did not focus the Herdr window"),
            "{error:#}"
        );
        assert!(ipc.calls.len() >= 3);
    }

    #[test]
    fn hyprland_subprocess_is_killed_at_the_shared_deadline() {
        let started = Instant::now();
        let mut command = Command::new("/usr/bin/sleep");
        command.arg("1");

        let error = run_bounded_command(command, started + Duration::from_millis(25)).unwrap_err();

        assert!(
            error.to_string().contains("exceeded activation deadline"),
            "{error:#}"
        );
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "bounded command took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn blocked_focus_cycles_after_the_current_slot() {
        let slots = [
            Some(slot("p0", "blocked")),
            Some(slot("p1", "working")),
            Some(slot("p2", "blocked")),
            None,
            None,
            None,
        ];
        assert_eq!(
            next_slot_with_status(&slots, "blocked", "p0")
                .unwrap()
                .pane_id,
            "p2"
        );
        assert_eq!(
            next_slot_with_status(&slots, "blocked", "p2")
                .unwrap()
                .pane_id,
            "p0"
        );
    }

    #[test]
    fn every_documented_control_maps_to_the_expected_herdr_api() {
        let slots = [
            Some(slot("p0", "working")),
            Some(slot("p1", "blocked")),
            None,
            None,
            None,
            None,
        ];
        let cases: Vec<ActionCase<'_>> = vec![
            (
                PadEvent::AgentKey(1),
                1,
                vec![("pane.focus", json!({"pane_id":"p1"}))],
            ),
            (
                PadEvent::AgentKey(2),
                2,
                vec![
                    ("workspace.list", json!({})),
                    ("workspace.focus", json!({"workspace_id":"w2"})),
                ],
            ),
            (
                PadEvent::Act(6),
                2,
                vec![
                    ("pane.current", json!({})),
                    ("pane.send_keys", json!({"pane_id":"p0","keys":["enter"]})),
                ],
            ),
            (
                PadEvent::Act(7),
                2,
                vec![
                    ("pane.current", json!({})),
                    ("pane.send_keys", json!({"pane_id":"p0","keys":["esc"]})),
                ],
            ),
            (
                PadEvent::Act(8),
                2,
                vec![
                    ("pane.current", json!({})),
                    ("pane.focus", json!({"pane_id":"p1"})),
                ],
            ),
            (
                PadEvent::Act(11),
                2,
                vec![
                    ("pane.current", json!({})),
                    ("pane.send_keys", json!({"pane_id":"p0","keys":["enter"]})),
                ],
            ),
            (
                PadEvent::EncStep(1),
                2,
                vec![
                    ("pane.current", json!({})),
                    ("pane.focus", json!({"pane_id":"p1"})),
                ],
            ),
            (
                PadEvent::EncStep(-1),
                2,
                vec![
                    ("pane.current", json!({})),
                    ("pane.focus", json!({"pane_id":"p1"})),
                ],
            ),
            (PadEvent::EncClick, 1, vec![("pane.zoom", json!({}))]),
            (
                PadEvent::Joy(JoyDir::Left),
                3,
                vec![
                    ("workspace.list", json!({})),
                    ("tab.list", json!({"workspace_id":"w0"})),
                    ("tab.focus", json!({"tab_id":"t1"})),
                ],
            ),
            (
                PadEvent::Joy(JoyDir::Right),
                3,
                vec![
                    ("workspace.list", json!({})),
                    ("tab.list", json!({"workspace_id":"w0"})),
                    ("tab.focus", json!({"tab_id":"t1"})),
                ],
            ),
            (
                PadEvent::Joy(JoyDir::Up),
                2,
                vec![
                    ("workspace.list", json!({})),
                    ("workspace.focus", json!({"workspace_id":"w2"})),
                ],
            ),
            (
                PadEvent::Joy(JoyDir::Down),
                2,
                vec![
                    ("workspace.list", json!({})),
                    ("workspace.focus", json!({"workspace_id":"w1"})),
                ],
            ),
        ];
        for (event, request_count, expected) in cases {
            let requests = run_action(event, &slots, request_count);
            let actual: Vec<_> = requests
                .iter()
                .map(|request| {
                    (
                        request["method"].as_str().unwrap(),
                        request["params"].clone(),
                    )
                })
                .collect();
            assert_eq!(actual, expected, "{event:?}");
        }
    }

    #[test]
    fn reconcile_discards_stale_agents_and_keeps_detected_agents() {
        let path = socket_path("reconcile");
        let listener = UnixListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut request)
                    .unwrap();
                let request: Value = serde_json::from_str(&request).unwrap();
                let response = match request.get("method").and_then(Value::as_str) {
                    Some("agent.list") => json!({"id":"r","result":{"agents":[
                        {"pane_id":"live","name":"codex","agent_status":"working"},
                        {"pane_id":"stale","name":"codex","agent_status":"done"}
                    ]}}),
                    Some("pane.list") => json!({"id":"r","result":{"panes":[
                        {"pane_id":"live"},
                        {"pane_id":"detected","agent":"claude","agent_status":"idle"}
                    ]}}),
                    method => panic!("unexpected method: {method:?}"),
                };
                send_line(&mut stream, &response).unwrap();
            }
        });
        let mut slots = [Some(slot("old", "done")), None, None, None, None, None];
        let panes = reconcile(&path, &mut slots).unwrap();
        assert_eq!(
            panes,
            BTreeSet::from(["detected".to_string(), "live".to_string()])
        );
        let ids: BTreeSet<_> = slots
            .iter()
            .flatten()
            .map(|slot| slot.pane_id.as_str())
            .collect();
        assert_eq!(ids, BTreeSet::from(["detected", "live"]));
        server.join().unwrap();
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn render_waits_for_matching_microd_ack_before_caching() {
        let (client, server) = UnixStream::pair().unwrap();
        let hub = Arc::new(Mutex::new(client));
        let (ack_tx, ack_rx) = mpsc::channel();
        let responder = thread::spawn(move || {
            let mut reader = BufReader::new(server.try_clone().unwrap());
            for expected in ["lighting_config", "lights"] {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(request["cmd"], expected);
                ack_tx
                    .send(HubAck {
                        id: request["id"].as_u64().unwrap(),
                        ok: true,
                        error: None,
                    })
                    .unwrap();
            }
            server
                .set_read_timeout(Some(Duration::from_millis(50)))
                .unwrap();
            let mut unexpected = String::new();
            assert!(BufReader::new(server).read_line(&mut unexpected).is_err());
        });
        let slots = [
            Some(slot("p0", "working")),
            Some(slot("p1", "blocked")),
            Some(slot("p2", "done")),
            Some(slot("p3", "idle")),
            Some(slot("p4", "unknown")),
            None,
        ];
        let mut next_id = 10;
        let mut rendered = None;
        render(
            &hub,
            &ack_rx,
            &mut next_id,
            &slots,
            VoiceState::Idle,
            &mut rendered,
        )
        .unwrap();
        render(
            &hub,
            &ack_rx,
            &mut next_id,
            &slots,
            VoiceState::Idle,
            &mut rendered,
        )
        .unwrap();
        assert!(rendered.is_some());
        assert_eq!(next_id, 12);
        responder.join().unwrap();
    }

    #[test]
    fn ambient_ring_uses_attention_priority() {
        let mut slots: [Option<Slot>; SLOTS] = Default::default();
        assert_eq!(ambient_lighting(&slots, VoiceState::Idle)["effect"], "off");

        slots[0] = Some(slot("working", "working"));
        assert_eq!(
            ambient_lighting(&slots, VoiceState::Idle)["color"],
            0x0066ff
        );

        slots[1] = Some(slot("review", "done"));
        assert_eq!(
            ambient_lighting(&slots, VoiceState::Idle)["color"],
            0x00ff00
        );

        slots[2] = Some(slot("approval", "blocked"));
        assert_eq!(
            ambient_lighting(&slots, VoiceState::Idle)["color"],
            0xffaa00
        );

        slots[3] = Some(slot("failed", "error"));
        assert_eq!(
            ambient_lighting(&slots, VoiceState::Idle)["color"],
            0xff2020
        );

        assert_eq!(
            ambient_lighting(&slots, VoiceState::Recording)["color"],
            0x2e8b57
        );
        assert_eq!(
            ambient_lighting(&slots, VoiceState::Processing)["color"],
            0xffffff
        );
        assert_eq!(
            ambient_lighting(&slots, VoiceState::Error)["color"],
            0xff2020
        );
    }

    #[test]
    fn working_to_idle_latches_review_until_work_resumes() {
        let mut slots: [Option<Slot>; SLOTS] = Default::default();
        upsert(
            &mut slots,
            &json!({"pane_id":"p0","agent":"codex","agent_status":"working"}),
        );
        upsert(
            &mut slots,
            &json!({"pane_id":"p0","agent":"codex","agent_status":"idle"}),
        );
        assert!(slots[0].as_ref().unwrap().review_ready);
        assert_eq!(effective_status(slots[0].as_ref().unwrap()), "done");

        upsert(
            &mut slots,
            &json!({"pane_id":"p0","agent":"codex","agent_status":"idle"}),
        );
        assert!(slots[0].as_ref().unwrap().review_ready);

        upsert(
            &mut slots,
            &json!({"pane_id":"p0","agent":"codex","agent_status":"working"}),
        );
        assert!(!slots[0].as_ref().unwrap().review_ready);
    }

    #[test]
    fn rejected_light_update_is_not_cached() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let hub = Arc::new(Mutex::new(client));
        let (ack_tx, ack_rx) = mpsc::channel();
        let responder = thread::spawn(move || {
            let mut line = String::new();
            BufReader::new(server.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            ack_tx
                .send(HubAck {
                    id: request["id"].as_u64().unwrap(),
                    ok: false,
                    error: Some("firmware rejected update".to_string()),
                })
                .unwrap();
            server.flush().unwrap();
        });
        let slots: [Option<Slot>; SLOTS] = Default::default();
        let mut next_id = 1;
        let mut rendered = None;
        let error = render(
            &hub,
            &ack_rx,
            &mut next_id,
            &slots,
            VoiceState::Idle,
            &mut rendered,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("firmware rejected update"));
        assert!(rendered.is_none());
        responder.join().unwrap();
    }
}
