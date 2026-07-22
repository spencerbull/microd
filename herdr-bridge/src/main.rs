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
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const SLOTS: usize = 6;

#[derive(Parser)]
#[command(name = "herdr-bridge", about = "Bridge microd pad events onto herdr")]
struct Cli {
    /// Path to the herdr API socket (default: $HERDR_SOCKET_PATH or ~/.config/herdr/herdr.sock)
    #[arg(long)]
    socket: Option<PathBuf>,
    /// Path to the microd socket (default: $MICROD_SOCKET or ~/.cache/microd/microd.sock)
    #[arg(long)]
    hub: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy)]
enum PadEvent {
    /// Agent Key AG00-AG05 pressed
    AgentKey(usize),
    /// Command key ACT06-ACT12 pressed
    Act(u8),
    /// Dial step: +1 clockwise, -1 counter-clockwise
    EncStep(i32),
    /// Dial click
    EncClick,
    Joy(JoyDir),
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
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let herdr_socket = cli.socket.unwrap_or_else(default_herdr_socket);
    let hub_socket = cli.hub.unwrap_or_else(default_hub_socket);

    // microd connection: reader thread decodes pad events; the write half
    // (shared) carries LED commands. Wait for both sockets instead of
    // failing so launchd KeepAlive gets a self-healing pair.
    let hub = wait_connect(&hub_socket, "microd");
    let hub_writer = Arc::new(Mutex::new(hub.try_clone()?));
    let (pad_tx, pad_rx) = mpsc::channel::<PadEvent>();
    let hub_thread = std::thread::spawn(move || hub_loop(hub, pad_tx));

    // herdr: one dedicated connection for the event subscription; everything
    // else goes through one-shot connections (the server expects one request
    // per connection).
    let stream = wait_connect(&herdr_socket, "herdr");
    stream.set_read_timeout(Some(Duration::from_millis(250)))?;
    let mut writer = stream.try_clone()?;
    send_line(
        &mut writer,
        &json!({"id": "sub", "method": "events.subscribe", "params": {"subscriptions": [
            {"type": "pane.created"},
            {"type": "pane.updated"},
            {"type": "pane.closed"},
            {"type": "pane.exited"},
            {"type": "pane.agent_detected"},
            {"type": "tab.closed"},
            {"type": "workspace.closed"},
        ]}}),
    )?;
    let mut lines = BufReader::new(stream);

    let mut slots: [Option<Slot>; SLOTS] = Default::default();
    let mut rendered: Option<Value> = None;
    let mut line = String::new();

    reconcile(&herdr_socket, &mut slots);
    render(&hub_writer, &slots, &mut rendered)?;

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
            actions.push(PadEvent::EncStep(enc_delta.signum()));
        }
        let mut need_reconcile = false;
        for action in actions {
            if let Err(e) = handle_pad_event(action, &herdr_socket, &slots) {
                eprintln!("{action:?} failed: {e:#}");
                // A dead pane means our slot state is stale; rebuild it.
                need_reconcile |= format!("{e:#}").contains("pane_not_found");
            }
        }
        if need_reconcile {
            reconcile(&herdr_socket, &mut slots);
            render(&hub_writer, &slots, &mut rendered)?;
        }

        line.clear();
        match lines.read_line(&mut line) {
            Ok(0) => anyhow::bail!("herdr closed the socket"),
            Ok(_) => {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                handle_message(&msg, &mut slots, &herdr_socket);
                // Closing a tab or workspace does not emit pane.closed for the
                // panes inside it, so reconcile against the server on any
                // removal-shaped event.
                if matches!(
                    msg.get("event").and_then(Value::as_str),
                    Some("tab_closed" | "workspace_closed" | "pane_closed" | "pane_exited")
                ) {
                    reconcile(&herdr_socket, &mut slots);
                }
                render(&hub_writer, &slots, &mut rendered)?;
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
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
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".cache/microd/microd.sock")
}

/// Read microd's event stream, decode into PadEvents.
fn hub_loop(hub: UnixStream, tx: mpsc::Sender<PadEvent>) {
    // Joystick gesture arming: fire once when deflection crosses 0.9, re-arm
    // once it falls back below 0.3.
    let mut joy_armed = true;
    for line in BufReader::new(hub).lines() {
        let Ok(line) = line else { return };
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let pad_event = match msg.get("event").and_then(Value::as_str) {
            Some("key") => decode_key(&msg),
            Some("joystick") => decode_joystick(&msg, &mut joy_armed),
            _ => None,
        };
        if let Some(pe) = pad_event {
            if tx.send(pe).is_err() {
                return;
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
    slots: &[Option<Slot>; SLOTS],
    rendered: &mut Option<Value>,
) -> Result<()> {
    let lights: Value = slots
        .iter()
        .enumerate()
        .map(|(i, slot)| {
            let (color, effect) = match slot.as_ref().map(|s| s.status.as_str()) {
                Some("working") => (0x0066ff, "breath"),
                Some("blocked") => (0xffaa00, "flash"),
                Some("done") => (0x00ff00, "solid"),
                Some("idle") => (0x303030, "solid"),
                Some(_) => (0x300840, "solid"),
                None => (0, "off"),
            };
            json!({ "slot": i, "color": color, "effect": effect, "speed": 50 })
        })
        .collect();
    if rendered.as_ref() == Some(&lights) {
        return Ok(());
    }
    let msg = json!({ "cmd": "lights", "lights": lights });
    let mut stream = hub.lock().expect("hub lock");
    stream
        .write_all(format!("{msg}\n").as_bytes())
        .context("send lights to microd")?;
    *rendered = Some(lights);
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

/// Update slot state from a pane lifecycle event.
fn handle_message(msg: &Value, slots: &mut [Option<Slot>; SLOTS], socket: &PathBuf) {
    let Some(event) = msg.get("event").and_then(Value::as_str) else {
        return;
    };
    let Some(pane) = msg.pointer("/data/pane") else {
        // pane.closed carries only pane_id
        if let Some(pane_id) = msg.pointer("/data/pane_id").and_then(Value::as_str) {
            remove(slots, pane_id);
        }
        return;
    };
    let pane_id = pane.get("pane_id").and_then(Value::as_str).unwrap_or_default();
    match event {
        "pane_closed" | "pane_exited" => remove(slots, pane_id),
        _ => {
            if pane.get("agent").is_some() {
                // The event hub replays recent history to new subscribers, so
                // an event can describe a pane that no longer exists. Verify
                // before creating a new slot (updates to known slots are fine).
                let known = slots
                    .iter()
                    .flatten()
                    .any(|s| s.pane_id == pane_id);
                if known || pane_exists(socket, pane_id) {
                    upsert(slots, pane);
                }
            } else {
                remove(slots, pane_id);
            }
        }
    }
}

fn pane_exists(socket: &PathBuf, pane_id: &str) -> bool {
    one_shot(
        socket,
        &json!({"id": "chk", "method": "pane.get", "params": {"pane_id": pane_id}}),
    )
    .is_ok()
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
    let new = Slot { pane_id: pane_id.to_string(), label, status };

    if let Some(existing) = slots
        .iter_mut()
        .flatten()
        .find(|s| s.pane_id == new.pane_id)
    {
        *existing = new;
    } else if let Some(free) = slots.iter_mut().find(|s| s.is_none()) {
        println!("agent {} ({}) -> slot", new.label, new.pane_id);
        *free = Some(new);
    } else {
        eprintln!("more than {SLOTS} agents; {} not shown", new.pane_id);
    }
}

fn remove(slots: &mut [Option<Slot>; SLOTS], pane_id: &str) {
    for slot in slots.iter_mut() {
        if slot.as_ref().is_some_and(|s| s.pane_id == pane_id) {
            println!("agent gone: {pane_id}");
            *slot = None;
        }
    }
}

/// Rebuild slot state from the server. Live agents are the union of
/// registered agents (agent.list, validated against panes that still exist —
/// the registry can hold stale records for panes removed via tab/workspace
/// close) and panes with a detected agent, which don't appear in agent.list.
fn reconcile(socket: &PathBuf, slots: &mut [Option<Slot>; SLOTS]) {
    let fetch = |method: &str| {
        one_shot_response(socket, &json!({"id": "r", "method": method, "params": {}}))
    };
    let (agents, panes) = match (fetch("agent.list"), fetch("pane.list")) {
        (Ok(a), Ok(p)) => (a, p),
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("reconcile failed: {e:#}");
            return;
        }
    };
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
    for agent in live {
        upsert(slots, agent);
    }
}

// ---------------------------------------------------------------------------
// pad actions -> herdr

fn handle_pad_event(ev: PadEvent, socket: &PathBuf, slots: &[Option<Slot>; SLOTS]) -> Result<()> {
    match ev {
        PadEvent::AgentKey(idx) => {
            if let Some(slot) = slots.get(idx).and_then(Clone::clone) {
                println!("key AG{idx:02} -> focus {} ({})", slot.label, slot.pane_id);
                focus_pane(socket, &slot.pane_id)?;
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
        PadEvent::Act(n) => println!("ACT{n:02} pressed (unmapped)"),
        PadEvent::EncStep(dir) => cycle_agent_focus(socket, slots, dir)?,
        PadEvent::EncClick => {
            println!("dial click -> zoom toggle");
            one_shot(socket, &json!({"id": "zoom", "method": "pane.zoom", "params": {}}))?;
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
    )
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
    )
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
    match slots.iter().flatten().find(|s| s.status == status) {
        Some(slot) => {
            println!("ACT08 -> focus {} agent {} ({})", status, slot.label, slot.pane_id);
            focus_pane(socket, &slot.pane_id)
        }
        None => {
            println!("ACT08: no {status} agent");
            Ok(())
        }
    }
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
    )
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
    )
}
