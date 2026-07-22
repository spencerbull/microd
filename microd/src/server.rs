//! The microd daemon: owns the pad's vendor HID channel and serves a Unix
//! socket speaking newline-delimited JSON.
//!
//! Server -> client (broadcast to all connected clients):
//!   {"event":"key","key":"AG00","action":"press"|"release"|"step"}
//!   {"event":"joystick","angle":0.75,"deflection":1.0}
//!   {"event":"device_message","message":{...}}   // anything else the pad says
//!
//! Client -> server (one command per line; "id" is echoed in the reply):
//!   {"id":1,"cmd":"lights","lights":[{"slot":0,"color":65280,"effect":"breath","speed":50}]}
//!   {"id":2,"cmd":"clear"}
//!   {"id":3,"cmd":"raw","method":"device.status","params":{}}
//!   -> {"id":1,"ok":true} or {"id":1,"ok":false,"error":"..."}
//!   (device responses to "raw" arrive as a device_message broadcast)

use anyhow::{Context, Result};
use hidapi::{HidApi, HidDevice};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::device::{self, light_param, send_json, Effect, Reader};
use crate::gestures::{Gesture, GestureEngine};

type Clients = Arc<Mutex<Vec<Arc<Mutex<UnixStream>>>>>;
type Engine = Arc<Mutex<GestureEngine>>;

pub fn default_socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("MICROD_SOCKET") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".cache/microd/microd.sock")
}

pub fn run(api: &mut HidApi, path: PathBuf) -> Result<()> {
    // Wait for the pad instead of failing: with launchd KeepAlive this makes
    // unplug/replug self-healing without restart churn.
    let mut waiting = false;
    let dev_in = loop {
        match device::open_vendor_interface(api) {
            Ok(dev) => break dev,
            Err(e) => {
                if !waiting {
                    println!("waiting for Codex Micro ({e:#})");
                    waiting = true;
                }
                std::thread::sleep(std::time::Duration::from_secs(2));
                let _ = api.refresh_devices();
            }
        }
    };
    let dev_out = Arc::new(Mutex::new(device::open_vendor_interface(api)?));

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("create socket directory")?;
    }
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("bind {}", path.display()))?;
    println!("microd: Codex Micro <-> {}", path.display());

    let clients: Clients = Arc::new(Mutex::new(Vec::new()));
    let engine: Engine = Arc::new(Mutex::new(GestureEngine::new(Default::default())));

    // HID reader thread: decode pad traffic, run gesture recognition, and
    // broadcast both raw events and gestures to all clients.
    {
        let clients = Arc::clone(&clients);
        let engine = Arc::clone(&engine);
        std::thread::spawn(move || {
            let mut reader = Reader::default();
            loop {
                // Wake in time for pending long-press / double-tap deadlines.
                let timeout_ms = {
                    let engine = engine.lock().expect("engine lock");
                    match engine.next_deadline() {
                        Some(deadline) => deadline
                            .saturating_duration_since(std::time::Instant::now())
                            .as_millis()
                            .clamp(10, 1000) as i32,
                        None => 1000,
                    }
                };
                match reader.poll_timeout(&dev_in, timeout_ms) {
                    Ok(events) => {
                        let now = std::time::Instant::now();
                        let mut engine = engine.lock().expect("engine lock");
                        for ev in events {
                            let raw = translate(&ev);
                            broadcast(&clients, &raw);
                            for g in feed_engine(&mut engine, &raw, now) {
                                broadcast(&clients, &gesture_json(&g));
                            }
                        }
                        for g in engine.tick(now) {
                            broadcast(&clients, &gesture_json(&g));
                        }
                    }
                    Err(e) => {
                        eprintln!("HID read failed (device disconnected?): {e:#}");
                        std::process::exit(1);
                    }
                }
            }
        });
    }

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let stream = Arc::new(Mutex::new(stream));
        clients
            .lock()
            .expect("clients lock")
            .push(Arc::clone(&stream));
        let clients = Arc::clone(&clients);
        let dev_out = Arc::clone(&dev_out);
        let engine = Arc::clone(&engine);
        std::thread::spawn(move || {
            client_loop(&stream, &dev_out, &engine);
            // Drop this client from the broadcast list on disconnect.
            clients
                .lock()
                .expect("clients lock")
                .retain(|c| !Arc::ptr_eq(c, &stream));
        });
    }
    Ok(())
}

/// Decode a raw pad message into the generic event shape.
fn translate(ev: &Value) -> Value {
    let method = ev.get("m").or_else(|| ev.get("method")).and_then(Value::as_str);
    let p = ev.get("p").or_else(|| ev.get("params"));
    match (method, p) {
        (Some("v.oai.hid"), Some(p)) => {
            let action = match p.get("act").and_then(Value::as_i64) {
                Some(0) => "release",
                Some(1) => "press",
                Some(2) => "step",
                _ => "unknown",
            };
            json!({
                "event": "key",
                "key": p.get("k").and_then(Value::as_str).unwrap_or(""),
                "action": action,
            })
        }
        (Some("v.oai.rad"), Some(p)) => json!({
            "event": "joystick",
            "angle": p.get("a").and_then(Value::as_f64).unwrap_or(0.0),
            "deflection": p.get("d").and_then(Value::as_f64).unwrap_or(0.0),
        }),
        _ => json!({ "event": "device_message", "message": ev }),
    }
}

/// Feed a translated raw event into the gesture engine.
fn feed_engine(engine: &mut GestureEngine, raw: &Value, now: std::time::Instant) -> Vec<Gesture> {
    match raw.get("event").and_then(Value::as_str) {
        Some("key") => {
            let key = raw.get("key").and_then(Value::as_str).unwrap_or("");
            match raw.get("action").and_then(Value::as_str) {
                Some("press") => engine.key_press(key, now),
                Some("release") => engine.key_release(key, now),
                Some("step") => engine.step(key, now),
                _ => Vec::new(),
            }
        }
        Some("joystick") => {
            let angle = raw.get("angle").and_then(Value::as_f64).unwrap_or(0.0);
            let deflection = raw.get("deflection").and_then(Value::as_f64).unwrap_or(0.0);
            engine.joystick(angle, deflection)
        }
        _ => Vec::new(),
    }
}

fn gesture_json(g: &Gesture) -> Value {
    match g {
        Gesture::Tap { key } => json!({"event": "gesture", "type": "tap", "key": key}),
        Gesture::DoubleTap { key } => json!({"event": "gesture", "type": "double_tap", "key": key}),
        Gesture::LongPress { key } => json!({"event": "gesture", "type": "long_press", "key": key}),
        Gesture::Chord { key, held } => {
            json!({"event": "gesture", "type": "chord", "key": key, "held": held})
        }
        Gesture::Flick { direction } => {
            json!({"event": "gesture", "type": "flick", "direction": direction})
        }
    }
}

fn broadcast(clients: &Clients, msg: &Value) {
    let line = format!("{msg}\n");
    let list = clients.lock().expect("clients lock").clone();
    for client in list {
        let mut stream = client.lock().expect("client lock");
        // Dead clients get cleaned up by their own handler thread.
        let _ = stream.write_all(line.as_bytes());
    }
}

fn client_loop(stream: &Arc<Mutex<UnixStream>>, dev_out: &Arc<Mutex<HidDevice>>, engine: &Engine) {
    let reader = match stream.lock().expect("client lock").try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    for line in BufReader::new(reader).lines() {
        let Ok(line) = line else { break };
        let Ok(cmd) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = cmd.get("id").cloned().unwrap_or(Value::Null);
        let reply = match handle_command(&cmd, dev_out, engine) {
            Ok(()) => json!({ "id": id, "ok": true }),
            Err(e) => json!({ "id": id, "ok": false, "error": format!("{e:#}") }),
        };
        let mut s = stream.lock().expect("client lock");
        if s.write_all(format!("{reply}\n").as_bytes()).is_err() {
            break;
        }
    }
}

fn handle_command(cmd: &Value, dev_out: &Arc<Mutex<HidDevice>>, engine: &Engine) -> Result<()> {
    // Gesture config needs no device access.
    if cmd.get("cmd").and_then(Value::as_str) == Some("gesture_config") {
        let mut engine = engine.lock().expect("engine lock");
        if let Some(ms) = cmd.get("long_press_ms").and_then(Value::as_u64) {
            engine.config.long_press = std::time::Duration::from_millis(ms);
        }
        if let Some(ms) = cmd.get("double_tap_ms").and_then(Value::as_u64) {
            engine.config.double_window = std::time::Duration::from_millis(ms);
        }
        if let Some(keys) = cmd.get("double_tap_keys").and_then(Value::as_array) {
            engine.config.double_tap_keys = keys
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
        }
        return Ok(());
    }

    let dev = dev_out.lock().expect("device lock");
    match cmd.get("cmd").and_then(Value::as_str) {
        Some("lights") => {
            let lights = cmd
                .get("lights")
                .and_then(Value::as_array)
                .context("lights: missing 'lights' array")?;
            let params: Value = lights
                .iter()
                .map(|l| {
                    let slot = l.get("slot").and_then(Value::as_u64).unwrap_or(0) as u8;
                    let color = l.get("color").and_then(Value::as_u64).unwrap_or(0) as u32;
                    let effect = l
                        .get("effect")
                        .and_then(Value::as_str)
                        .map(Effect::from_name)
                        .unwrap_or(Effect::Solid);
                    let speed = l.get("speed").and_then(Value::as_u64).unwrap_or(50) as u8;
                    light_param(slot, color, effect.code(), speed)
                })
                .collect();
            send_json(&dev, &json!({"method": "v.oai.thstatus", "params": params, "id": 1}))
        }
        Some("clear") => {
            let params: Value = (0..6).map(|s| light_param(s, 0, 0, 50)).collect();
            send_json(&dev, &json!({"method": "v.oai.thstatus", "params": params, "id": 1}))
        }
        Some("raw") => {
            let method = cmd
                .get("method")
                .and_then(Value::as_str)
                .context("raw: missing 'method'")?;
            let mut msg = json!({ "method": method, "id": 1 });
            if let Some(p) = cmd.get("params") {
                msg["params"] = p.clone();
            }
            send_json(&dev, &msg)
        }
        other => anyhow::bail!("unknown cmd {other:?}"),
    }
}
