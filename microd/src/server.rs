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
//!   {"id":2,"cmd":"lighting_config","ambient":{"color":65280,"effect":"solid","brightness":1.0,"speed":0.0,"magic":0.0},"keys":{"color":0,"effect":"off","brightness":0.0,"speed":0.0,"magic":0.0}}
//!   {"id":3,"cmd":"clear"}
//!   {"id":4,"cmd":"raw","method":"device.status","params":{}}
//!   -> {"id":1,"ok":true} or {"id":1,"ok":false,"error":"..."}
//!   (device responses to "raw" arrive as a device_message broadcast)

use anyhow::{Context, Result};
use hidapi::{HidApi, HidDevice};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use crate::device::{self, light_param, send_json, Effect, Reader};
use crate::gestures::{Gesture, GestureEngine};

#[derive(Clone)]
struct Client {
    id: u64,
    tx: mpsc::SyncSender<String>,
    shutdown: Arc<UnixStream>,
}

type Clients = Arc<Mutex<Vec<Client>>>;
type Engine = Arc<Mutex<GestureEngine>>;
type Pending = Arc<Mutex<HashMap<i64, mpsc::Sender<Value>>>>;
type RequestGate = Arc<Mutex<()>>;

#[derive(Debug)]
struct DeviceTransportFailure(String);

impl fmt::Display for DeviceTransportFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for DeviceTransportFailure {}

fn device_transport_failure(error: impl fmt::Display) -> anyhow::Error {
    DeviceTransportFailure(error.to_string()).into()
}

pub fn default_socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("MICROD_SOCKET") {
        return PathBuf::from(p);
    }
    if let Ok(p) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(p).join("microd/microd.sock");
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".cache/microd/microd.sock")
}

pub fn run(api: &mut HidApi, path: PathBuf) -> Result<()> {
    let _socket_lock = prepare_socket_path(&path)?;

    // Wait for the pad instead of failing so a service supervisor can keep
    // one stable daemon across boot ordering and a late Bluetooth connection.
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

    let listener = UnixListener::bind(&path).with_context(|| format!("bind {}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .context("secure microd socket")?;
    println!("microd: Codex Micro <-> {}", path.display());

    let clients: Clients = Arc::new(Mutex::new(Vec::new()));
    let engine: Engine = Arc::new(Mutex::new(GestureEngine::new(Default::default())));
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    let request_gate: RequestGate = Arc::new(Mutex::new(()));
    let next_device_id = Arc::new(AtomicI64::new(1));
    let next_client_id = AtomicU64::new(1);

    // HID reader thread: decode pad traffic, run gesture recognition, and
    // broadcast both raw events and gestures to all clients.
    {
        let clients = Arc::clone(&clients);
        let engine = Arc::clone(&engine);
        let pending = Arc::clone(&pending);
        std::thread::spawn(move || {
            let mut reader = if std::env::var_os("MICROD_DEBUG_FRAGMENTS").is_some() {
                Reader::debug()
            } else {
                Reader::default()
            };
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
                            complete_pending(&pending, &ev);
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
        let Ok(mut stream) = stream else { continue };
        stream.set_write_timeout(Some(Duration::from_millis(100)))?;
        let reader = stream.try_clone()?;
        let shutdown = Arc::new(stream.try_clone()?);
        let client_id = next_client_id.fetch_add(1, Ordering::Relaxed);
        let (client_tx, client_rx) = mpsc::sync_channel::<String>(256);
        clients.lock().expect("clients lock").push(Client {
            id: client_id,
            tx: client_tx.clone(),
            shutdown,
        });
        std::thread::spawn(move || {
            while let Ok(line) = client_rx.recv() {
                if stream.write_all(line.as_bytes()).is_err() {
                    break;
                }
            }
        });
        let clients = Arc::clone(&clients);
        let dev_out = Arc::clone(&dev_out);
        let engine = Arc::clone(&engine);
        let pending = Arc::clone(&pending);
        let request_gate = Arc::clone(&request_gate);
        let next_device_id = Arc::clone(&next_device_id);
        std::thread::spawn(move || {
            client_loop(
                reader,
                client_tx,
                &dev_out,
                &engine,
                &pending,
                &request_gate,
                &next_device_id,
            );
            // Drop this client from the broadcast list on disconnect.
            clients
                .lock()
                .expect("clients lock")
                .retain(|client| client.id != client_id);
        });
    }
    Ok(())
}

fn prepare_socket_path(path: &PathBuf) -> Result<File> {
    let parent = path.parent().context("socket path has no parent")?;
    let created_parent = !parent.exists();
    std::fs::create_dir_all(parent).context("create socket directory")?;
    if created_parent {
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            .context("secure created socket directory")?;
    }

    let lock_path = path.with_extension("sock.lock");
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .context("open microd socket lock")?;
    std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o600))
        .context("secure microd socket lock")?;
    // SAFETY: flock only observes the valid file descriptor owned by `lock`.
    let lock_result = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if lock_result != 0 {
        anyhow::bail!(
            "another microd process owns the socket lock at {}",
            lock_path.display()
        );
    }

    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_socket() => {
            anyhow::bail!("refusing to replace non-socket path at {}", path.display());
        }
        Ok(_) => match UnixStream::connect(path) {
            Ok(_) => anyhow::bail!("microd is already listening at {}", path.display()),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                ) =>
            {
                std::fs::remove_file(path).context("remove stale microd socket")?;
            }
            Err(e) => return Err(e).context("inspect existing microd socket"),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).context("inspect microd socket path"),
    }
    Ok(lock)
}

/// Decode a raw pad message into the generic event shape.
fn translate(ev: &Value) -> Value {
    let method = ev
        .get("m")
        .or_else(|| ev.get("method"))
        .and_then(Value::as_str);
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
    let mut dead = Vec::new();
    for client in list {
        if client.tx.try_send(line.clone()).is_err() {
            let _ = client.shutdown.shutdown(Shutdown::Both);
            dead.push(client.id);
        }
    }
    if !dead.is_empty() {
        clients
            .lock()
            .expect("clients lock")
            .retain(|client| !dead.contains(&client.id));
    }
}

fn complete_pending(pending: &Pending, response: &Value) {
    let Some(id) = response.get("id").and_then(Value::as_i64) else {
        return;
    };
    if let Some(waiter) = pending.lock().expect("pending lock").remove(&id) {
        let _ = waiter.send(response.clone());
    }
}

fn client_loop(
    reader: UnixStream,
    reply_tx: mpsc::SyncSender<String>,
    dev_out: &Arc<Mutex<HidDevice>>,
    engine: &Engine,
    pending: &Pending,
    request_gate: &RequestGate,
    next_device_id: &AtomicI64,
) {
    for line in BufReader::new(reader).lines() {
        let Ok(line) = line else { break };
        let Ok(cmd) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = cmd.get("id").cloned().unwrap_or(Value::Null);
        let reply =
            match handle_command(&cmd, dev_out, engine, pending, request_gate, next_device_id) {
                Ok(()) => json!({ "id": id, "ok": true }),
                Err(e) if e.downcast_ref::<DeviceTransportFailure>().is_some() => {
                    eprintln!("fatal Codex Micro transport failure: {e:#}");
                    std::process::exit(1);
                }
                Err(e) => json!({ "id": id, "ok": false, "error": format!("{e:#}") }),
            };
        if reply_tx.send(format!("{reply}\n")).is_err() {
            break;
        }
    }
}

fn handle_command(
    cmd: &Value,
    dev_out: &Arc<Mutex<HidDevice>>,
    engine: &Engine,
    pending: &Pending,
    request_gate: &RequestGate,
    next_device_id: &AtomicI64,
) -> Result<()> {
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

    if cmd.get("cmd").and_then(Value::as_str) == Some("clear") {
        send_device_request(
            dev_out,
            pending,
            request_gate,
            next_device_id,
            "v.oai.rgbcfg",
            Some(json!({
                "ambient": lighting_side(&json!({"effect":"off"}))?,
                "keys": lighting_side(&json!({"effect":"off"}))?,
            })),
        )?;
        let params: Value = (0..6).map(|s| light_param(s, 0, 0, 50)).collect();
        return send_device_request(
            dev_out,
            pending,
            request_gate,
            next_device_id,
            "v.oai.thstatus",
            Some(params),
        );
    }

    let (method, params) = match cmd.get("cmd").and_then(Value::as_str) {
        Some("lights") => {
            let lights = cmd
                .get("lights")
                .and_then(Value::as_array)
                .context("lights: missing 'lights' array")?;
            let params: Vec<Value> = lights
                .iter()
                .map(|l| -> Result<Value> {
                    let slot = l.get("slot").and_then(Value::as_u64).unwrap_or(0) as u8;
                    let color = l.get("color").and_then(Value::as_u64).unwrap_or(0) as u32;
                    let effect = match l.get("effect").and_then(Value::as_str) {
                        Some(name) => Effect::from_name(name)
                            .with_context(|| format!("lights: unknown effect '{name}'"))?,
                        None => Effect::Solid,
                    };
                    let speed = l.get("speed").and_then(Value::as_u64).unwrap_or(50) as u8;
                    Ok(light_param(slot, color, effect.code(), speed))
                })
                .collect::<Result<_>>()?;
            ("v.oai.thstatus", Some(Value::Array(params)))
        }
        Some("lighting_config") => {
            let ambient = cmd
                .get("ambient")
                .context("lighting_config: missing 'ambient' object")?;
            let keys = cmd
                .get("keys")
                .context("lighting_config: missing 'keys' object")?;
            (
                "v.oai.rgbcfg",
                Some(json!({
                    "ambient": lighting_side(ambient)?,
                    "keys": lighting_side(keys)?,
                })),
            )
        }
        Some("raw") => {
            let method = cmd
                .get("method")
                .and_then(Value::as_str)
                .context("raw: missing 'method'")?;
            (method, cmd.get("params").cloned())
        }
        other => anyhow::bail!("unknown cmd {other:?}"),
    };
    send_device_request(
        dev_out,
        pending,
        request_gate,
        next_device_id,
        method,
        params,
    )
}

fn lighting_side(side: &Value) -> Result<Value> {
    let effect = match side.get("effect").and_then(Value::as_str) {
        Some(name) => Effect::from_name(name)
            .with_context(|| format!("lighting_config: unknown effect '{name}'"))?,
        None => Effect::Off,
    };
    let brightness = side.get("brightness").and_then(Value::as_f64).unwrap_or(
        if matches!(effect, Effect::Off) {
            0.0
        } else {
            1.0
        },
    );
    let speed = side.get("speed").and_then(Value::as_f64).unwrap_or(0.0);
    let magic = side.get("magic").and_then(Value::as_f64).unwrap_or(0.0);
    let color = side.get("color").and_then(Value::as_u64).unwrap_or(0) as u32;
    for (name, value) in [
        ("brightness", brightness),
        ("speed", speed),
        ("magic", magic),
    ] {
        if !(0.0..=1.0).contains(&value) {
            anyhow::bail!("lighting_config: {name} must be between 0 and 1");
        }
    }
    Ok(json!({
        "e": effect.code(),
        "b": brightness,
        "s": speed,
        "m": magic,
        "c": color,
    }))
}

fn send_device_request(
    dev_out: &Arc<Mutex<HidDevice>>,
    pending: &Pending,
    request_gate: &RequestGate,
    next_device_id: &AtomicI64,
    method: &str,
    params: Option<Value>,
) -> Result<()> {
    let _request_guard = request_gate.lock().expect("request gate lock");
    let attempts = if matches!(
        method,
        "v.oai.thstatus" | "v.oai.rgbcfg" | "device.status" | "sys.version"
    ) {
        3
    } else {
        1
    };
    for attempt in 1..=attempts {
        let id = next_device_id.fetch_add(1, Ordering::Relaxed);
        let mut request = json!({"method": method, "id": id});
        if let Some(params) = params.clone() {
            request["params"] = params;
        }
        let (tx, rx) = mpsc::channel();
        pending.lock().expect("pending lock").insert(id, tx);
        let send_result = {
            let dev = dev_out.lock().expect("device lock");
            send_json(&dev, &request)
        };
        if let Err(e) = send_result {
            pending.lock().expect("pending lock").remove(&id);
            return Err(device_transport_failure(format!(
                "write {method} to Codex Micro: {e:#}"
            )));
        }
        let response = match rx.recv_timeout(device::request_timeout(attempts)) {
            Ok(response) => response,
            Err(mpsc::RecvTimeoutError::Timeout) if attempt < attempts => {
                pending.lock().expect("pending lock").remove(&id);
                eprintln!(
                    "Codex Micro {method} acknowledgement timed out; retrying ({attempt}/{attempts})"
                );
                continue;
            }
            Err(mpsc::RecvTimeoutError::Timeout) if attempts > 1 => {
                pending.lock().expect("pending lock").remove(&id);
                return Err(device_transport_failure(format!(
                    "{method} acknowledgement timed out after {attempts} attempts"
                )));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                pending.lock().expect("pending lock").remove(&id);
                return Err(device_transport_failure(format!(
                    "{method} acknowledgement channel disconnected"
                )));
            }
            Err(e) => {
                pending.lock().expect("pending lock").remove(&id);
                return Err(e).context("wait for Codex Micro firmware acknowledgement");
            }
        };
        if let Some(error) = response.get("error") {
            anyhow::bail!("Codex Micro rejected {method}: {error}");
        }
        let rejected = response_rejected(&response);
        if rejected {
            anyhow::bail!("Codex Micro rejected {method}: {response}");
        }
        return Ok(());
    }
    unreachable!("at least one device request attempt")
}

fn response_rejected(response: &Value) -> bool {
    match response.get("result") {
        Some(Value::Bool(ok)) => !ok,
        Some(Value::Number(ok)) => ok.as_i64() == Some(0),
        Some(Value::Object(result)) => match result.get("ok") {
            Some(Value::Bool(ok)) => !ok,
            Some(Value::Number(ok)) => ok.as_i64() == Some(0),
            _ => false,
        },
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

    fn temp_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "microd-{label}-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn translates_every_vendor_input_shape() {
        assert_eq!(
            translate(&json!({"m":"v.oai.hid","p":{"k":"AG05","act":1}})),
            json!({"event":"key","key":"AG05","action":"press"})
        );
        assert_eq!(
            translate(&json!({"m":"v.oai.hid","p":{"k":"ACT12","act":0}})),
            json!({"event":"key","key":"ACT12","action":"release"})
        );
        assert_eq!(
            translate(&json!({"m":"v.oai.hid","p":{"k":"ENC_CW","act":2}})),
            json!({"event":"key","key":"ENC_CW","action":"step"})
        );
        assert_eq!(
            translate(&json!({"m":"v.oai.rad","p":{"a":0.75,"d":1.0}})),
            json!({"event":"joystick","angle":0.75,"deflection":1.0})
        );
        assert_eq!(
            translate(&json!({"id":7,"result":{"ok":1}})),
            json!({"event":"device_message","message":{"id":7,"result":{"ok":1}}})
        );
    }

    #[test]
    fn firmware_response_completes_only_the_matching_waiter() {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = mpsc::channel();
        pending.lock().unwrap().insert(41, tx);
        complete_pending(&pending, &json!({"id":40,"result":{"ok":1}}));
        assert!(rx.try_recv().is_err());
        assert!(pending.lock().unwrap().contains_key(&41));
        let response = json!({"id":41,"result":{"ok":1}});
        complete_pending(&pending, &response);
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(50)).unwrap(),
            response
        );
        assert!(pending.lock().unwrap().is_empty());
    }

    #[test]
    fn full_client_queues_are_disconnected_without_blocking_broadcasts() {
        use std::io::Read;

        let (tx, _rx) = mpsc::sync_channel(1);
        tx.send("already full".to_string()).unwrap();
        let (server, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let clients: Clients = Arc::new(Mutex::new(vec![Client {
            id: 1,
            tx,
            shutdown: Arc::new(server),
        }]));
        broadcast(&clients, &json!({"event":"test"}));
        assert!(clients.lock().unwrap().is_empty());
        assert_eq!(peer.read(&mut [0; 1]).unwrap(), 0);
    }

    #[test]
    fn transport_failures_are_distinguishable_from_firmware_rejections() {
        let transport = device_transport_failure("write failed");
        assert!(transport.downcast_ref::<DeviceTransportFailure>().is_some());
        let firmware = anyhow::anyhow!("firmware rejected request");
        assert!(firmware.downcast_ref::<DeviceTransportFailure>().is_none());
    }

    #[test]
    fn firmware_false_results_are_rejected_for_vendor_commands() {
        assert!(response_rejected(&json!({"result": false})));
        assert!(response_rejected(&json!({"result": 0})));
        assert!(response_rejected(&json!({"result": {"ok": false}})));
        assert!(!response_rejected(&json!({"result": true})));
        assert!(!response_rejected(&json!({"result": {"ok": 1}})));
    }

    #[test]
    fn lighting_config_uses_the_official_compact_side_shape() {
        assert_eq!(
            lighting_side(&json!({
                "effect": "snake",
                "brightness": 1.0,
                "speed": 0.4,
                "magic": 0.0,
                "color": 0x0066ff,
            }))
            .unwrap(),
            json!({"e":2,"b":1.0,"s":0.4,"m":0.0,"c":0x0066ff})
        );
        assert!(lighting_side(&json!({"effect":"solid","brightness":1.1})).is_err());
        assert!(lighting_side(&json!({"effect":"flash"})).is_err());
    }

    #[test]
    fn socket_setup_preserves_existing_parent_permissions() {
        let root = temp_root("permissions");
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = root.join("microd.sock");
        let lock = prepare_socket_path(&path).unwrap();
        let mode = std::fs::metadata(&root).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o755);
        drop(lock);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn socket_setup_secures_only_a_directory_it_created() {
        let root = temp_root("created-parent");
        std::fs::create_dir(&root).unwrap();
        let parent = root.join("microd");
        let path = parent.join("microd.sock");
        let lock = prepare_socket_path(&path).unwrap();
        let mode = std::fs::metadata(&parent).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o700);
        drop(lock);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn socket_setup_refuses_non_socket_targets() {
        let root = temp_root("non-socket");
        std::fs::create_dir(&root).unwrap();
        let path = root.join("microd.sock");
        std::fs::write(&path, b"keep me").unwrap();
        let error = prepare_socket_path(&path).unwrap_err();
        assert!(format!("{error:#}").contains("refusing to replace non-socket"));
        assert_eq!(std::fs::read(&path).unwrap(), b"keep me");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn socket_setup_removes_only_a_stale_socket() {
        let root = temp_root("stale-socket");
        std::fs::create_dir(&root).unwrap();
        let path = root.join("microd.sock");
        drop(UnixListener::bind(&path).unwrap());
        let lock = prepare_socket_path(&path).unwrap();
        assert!(!path.exists());
        drop(lock);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn socket_setup_rejects_a_live_listener() {
        let root = temp_root("live-socket");
        std::fs::create_dir(&root).unwrap();
        let path = root.join("microd.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let error = prepare_socket_path(&path).unwrap_err();
        assert!(format!("{error:#}").contains("already listening"));
        drop(listener);
        std::fs::remove_dir_all(root).unwrap();
    }
}
