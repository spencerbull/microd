//! Vendor HID protocol for the Work Louder / OpenAI "Codex Micro".
//!
//! Protocol base per https://github.com/imliubo/codex-micro-4-core2
//! (docs/TECHNICAL.md), with corrections verified live against firmware
//! v0.4.1: `v.oai.thstatus` values must be integers, and device events use
//! compact keys `"m"`/`"p"` instead of `"method"`/`"params"`.

use anyhow::{Context, Result};
use hidapi::{HidApi, HidDevice};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

pub const VID: u16 = 0x303a;
pub const PID: u16 = 0x8360;
pub const VENDOR_USAGE_PAGE: u16 = 0xff00;
const REPORT_ID: u8 = 6;
const REPORT_BODY_LEN: usize = 63;
const CHUNK_MAX: usize = 61;
const MSG_TYPE_JSON: u8 = 2;
const INTER_CHUNK_DELAY: Duration = Duration::from_millis(4);

/// Effect enum values confirmed by live probing firmware v0.4.1: 0=off,
/// 1=solid, 3/4=flash variants, 6=breath (2 and 5 render nothing; brightness
/// `b` appears ignored but is sent for compatibility with observed traffic).
#[derive(Clone, Copy, clap::ValueEnum)]
pub enum Effect {
    Off,
    Solid,
    Flash,
    Flash2,
    Breath,
}

impl Effect {
    pub fn code(self) -> u8 {
        match self {
            Effect::Off => 0,
            Effect::Solid => 1,
            Effect::Flash => 3,
            Effect::Flash2 => 4,
            Effect::Breath => 6,
        }
    }

    pub fn from_name(name: &str) -> Effect {
        match name {
            "off" => Effect::Off,
            "flash" => Effect::Flash,
            "flash2" => Effect::Flash2,
            "breath" => Effect::Breath,
            _ => Effect::Solid,
        }
    }
}

pub fn light_param(slot: u8, color: u32, effect: u8, speed: u8) -> Value {
    json!({ "id": slot, "c": color, "b": 100, "e": effect, "s": speed })
}

pub fn list(api: &HidApi) -> Result<()> {
    let mut found = false;
    for info in api.device_list() {
        if info.vendor_id() == VID && info.product_id() == PID {
            found = true;
            println!(
                "{:04x}:{:04x} usage_page={:#06x} usage={:#04x} product={:?} path={}",
                info.vendor_id(),
                info.product_id(),
                info.usage_page(),
                info.usage(),
                info.product_string().unwrap_or("?"),
                info.path().to_string_lossy(),
            );
        }
    }
    if !found {
        println!("no Codex Micro ({VID:04x}:{PID:04x}) HID interfaces found");
        let total = api.device_list().count();
        println!("hidapi sees {total} HID interfaces total:");
        for info in api.device_list() {
            println!(
                "  {:04x}:{:04x} usage_page={:#06x} product={:?}",
                info.vendor_id(),
                info.product_id(),
                info.usage_page(),
                info.product_string().unwrap_or("?"),
            );
        }
    }
    Ok(())
}

pub fn open_vendor_interface(api: &HidApi) -> Result<HidDevice> {
    // Prefer the vendor-defined collection; fall back to any interface on the
    // device (over BLE HOGP macOS may expose a single combined service).
    let mut fallback = None;
    for info in api.device_list() {
        if info.vendor_id() != VID || info.product_id() != PID {
            continue;
        }
        if info.usage_page() == VENDOR_USAGE_PAGE {
            return info.open_device(api).context("open vendor HID interface");
        }
        fallback = Some(info.path().to_owned());
    }
    let path = fallback.context("Codex Micro not found (is it connected/paired?)")?;
    eprintln!("note: no interface with usage page 0xFF00 exposed; opening device anyway");
    api.open_path(&path).context("open Codex Micro HID device")
}

/// Send one JSON message, chunked into vendor reports.
pub fn send_json(dev: &HidDevice, msg: &Value) -> Result<()> {
    let mut line = serde_json::to_string(msg)?;
    line.push('\n');
    let bytes = line.as_bytes();
    for chunk in bytes.chunks(CHUNK_MAX) {
        let mut report = [0u8; 1 + REPORT_BODY_LEN];
        report[0] = REPORT_ID;
        report[1] = MSG_TYPE_JSON;
        report[2] = chunk.len() as u8;
        report[3..3 + chunk.len()].copy_from_slice(chunk);
        dev.write(&report).context("write vendor report")?;
        std::thread::sleep(INTER_CHUNK_DELAY);
    }
    Ok(())
}

/// Send a request and print any response matching its id (2s window).
pub fn request(dev: &HidDevice, method: &str, params: Option<Value>) -> Result<()> {
    let id = 1;
    let mut msg = json!({ "method": method, "id": id });
    if let Some(p) = params {
        msg["params"] = p;
    }
    send_json(dev, &msg)?;
    println!("sent {method}");

    let mut reader = Reader::default();
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        for value in reader.poll(dev)? {
            println!("recv: {value}");
            if value.get("id").and_then(Value::as_i64) == Some(id) {
                return Ok(());
            }
        }
    }
    println!("(no response within 2s — writes may still have taken effect)");
    Ok(())
}

/// Accumulates chunked vendor reports back into newline-terminated JSON lines.
#[derive(Default)]
pub struct Reader {
    buf: Vec<u8>,
    debug: bool,
}

impl Reader {
    pub fn debug() -> Reader {
        Reader { debug: true, ..Default::default() }
    }

    pub fn poll(&mut self, dev: &HidDevice) -> Result<Vec<Value>> {
        // A long idle timeout costs nothing in latency: read_timeout returns
        // as soon as a report arrives; the timeout only caps the idle wait,
        // so this just means fewer wakeups when the pad is quiet.
        self.poll_timeout(dev, 1000)
    }

    pub fn poll_timeout(&mut self, dev: &HidDevice, timeout_ms: i32) -> Result<Vec<Value>> {
        let mut raw = [0u8; 1 + REPORT_BODY_LEN];
        let n = dev
            .read_timeout(&mut raw, timeout_ms)
            .context("read vendor report")?;
        let mut out = Vec::new();
        if n == 0 {
            return Ok(out);
        }
        let mut body = &raw[..n];
        // Depending on transport, the report ID may or may not be present.
        if body.first() == Some(&REPORT_ID) && body.len() > REPORT_BODY_LEN - 1 {
            body = &body[1..];
        }
        if body.len() < 2 || body[0] != MSG_TYPE_JSON {
            if self.debug {
                eprintln!("raw non-JSON report ({n} bytes): {:02x?}", &raw[..n.min(16)]);
            }
            return Ok(out);
        }
        let len = (body[1] as usize).min(body.len() - 2);
        let fragment = &body[2..2 + len];
        // Recovery rule from the protocol notes: a fragment starting a new
        // request resets any incomplete buffer.
        if fragment.starts_with(b"{\"method\"") && !self.buf.is_empty() {
            self.buf.clear();
        }
        self.buf.extend_from_slice(fragment);
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            let line = &line[..line.len() - 1];
            if line.is_empty() {
                continue;
            }
            match serde_json::from_slice::<Value>(line) {
                Ok(v) => out.push(v),
                Err(e) => eprintln!("unparseable line ({e}): {}", String::from_utf8_lossy(line)),
            }
        }
        Ok(out)
    }
}
