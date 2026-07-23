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

pub(crate) fn request_timeout(attempts: usize) -> Duration {
    if attempts > 1 {
        Duration::from_millis(500)
    } else {
        Duration::from_secs(2)
    }
}

/// Effect enum values documented by Work Louder's Codex Micro SDK.
///
/// The older reverse-engineered table incorrectly called code 3 "flash";
/// it is the firmware's rainbow animation.
#[derive(Clone, Copy, clap::ValueEnum)]
pub enum Effect {
    Off,
    Solid,
    Snake,
    Rainbow,
    Breath,
    Gradient,
    ShallowBreath,
}

impl Effect {
    pub fn code(self) -> u8 {
        match self {
            Effect::Off => 0,
            Effect::Solid => 1,
            Effect::Snake => 2,
            Effect::Rainbow => 3,
            Effect::Breath => 4,
            Effect::Gradient => 5,
            Effect::ShallowBreath => 6,
        }
    }

    pub fn from_name(name: &str) -> Option<Effect> {
        match name {
            "off" => Some(Effect::Off),
            "solid" => Some(Effect::Solid),
            "snake" => Some(Effect::Snake),
            "rainbow" => Some(Effect::Rainbow),
            "breath" => Some(Effect::Breath),
            "gradient" => Some(Effect::Gradient),
            "shallow_breath" => Some(Effect::ShallowBreath),
            _ => None,
        }
    }
}

pub fn light_param(slot: u8, color: u32, effect: u8, speed: u8) -> Value {
    json!({
        "id": slot,
        "c": color,
        "b": 100,
        "e": effect,
        "s": speed,
        "sk": 0,
        "sa": 0,
    })
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

/// Send a request and print the matching response, retrying known idempotent
/// operations when Bluetooth corrupts a firmware reply.
pub fn request(dev: &HidDevice, method: &str, params: Option<Value>) -> Result<()> {
    let attempts = if matches!(
        method,
        "v.oai.thstatus" | "v.oai.rgbcfg" | "device.status" | "sys.version"
    ) {
        3
    } else {
        1
    };
    let mut reader = Reader::default();
    for attempt in 1..=attempts {
        let id = attempt as i64;
        let mut msg = json!({ "method": method, "id": id });
        if let Some(p) = params.clone() {
            msg["params"] = p;
        }
        send_json(dev, &msg)?;
        println!("sent {method} (attempt {attempt}/{attempts})");

        let timeout = request_timeout(attempts);
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            for value in reader.poll(dev)? {
                println!("recv: {value}");
                if value.get("id").and_then(Value::as_i64) == Some(id) {
                    if let Some(error) = value.get("error") {
                        anyhow::bail!("Codex Micro rejected {method}: {error}");
                    }
                    let rejected = response_rejected(&value);
                    if rejected {
                        anyhow::bail!("Codex Micro rejected {method}: {value}");
                    }
                    return Ok(());
                }
            }
        }
        if attempt < attempts {
            eprintln!("no response to {method}; retrying ({attempt}/{attempts})");
        }
    }
    anyhow::bail!("no response to {method} after {attempts} attempt(s)")
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

/// Accumulates chunked vendor reports back into newline-terminated JSON lines.
#[derive(Default)]
pub struct Reader {
    buf: Vec<u8>,
    suspended: Vec<Vec<u8>>,
    debug: bool,
}

impl Reader {
    pub fn debug() -> Reader {
        Reader {
            debug: true,
            ..Default::default()
        }
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
        if n == 0 {
            return Ok(Vec::new());
        }
        let mut body = &raw[..n];
        // Depending on transport, the report ID may or may not be present.
        if body.first() == Some(&REPORT_ID) && body.len() > REPORT_BODY_LEN - 1 {
            body = &body[1..];
        }
        if body.len() < 2 || body[0] != MSG_TYPE_JSON {
            if self.debug {
                eprintln!(
                    "raw non-JSON report ({n} bytes): {:02x?}",
                    &raw[..n.min(16)]
                );
            }
            return Ok(Vec::new());
        }
        let len = (body[1] as usize).min(body.len() - 2);
        let fragment = &body[2..2 + len];
        if self.debug {
            eprintln!("JSON fragment: {:?}", String::from_utf8_lossy(fragment));
        }
        Ok(self.push_fragment(fragment))
    }

    fn push_fragment(&mut self, fragment: &[u8]) -> Vec<Value> {
        let mut out = Vec::new();
        // Bluetooth firmware can insert a fresh joystick/key message between
        // chunks of a longer reply. Every JSON message starts with `{`; save
        // the interrupted message and resume it after the inserted message.
        if messages_can_interleave(&self.buf, fragment) {
            if self.suspended.len() == 16 {
                self.suspended.remove(0);
            }
            self.suspended.push(std::mem::take(&mut self.buf));
        }
        self.buf.extend_from_slice(fragment);
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
            line.pop();
            while line.last().is_some_and(|byte| *byte == b'\r' || *byte == 0) {
                line.pop();
            }
            if line.is_empty() {
                self.resume_suspended();
                continue;
            }
            match parse_json_line(&line) {
                Some(value) => out.push(value),
                None => {
                    if let Some(value) = salvage_embedded_event(&line) {
                        eprintln!(
                            "salvaged input event from interleaved line: {}",
                            String::from_utf8_lossy(&line)
                        );
                        out.push(value);
                    } else {
                        eprintln!("unparseable line: {}", String::from_utf8_lossy(&line));
                    }
                }
            }
            self.resume_suspended();
        }
        out
    }

    fn resume_suspended(&mut self) {
        if self.buf.is_empty() {
            if let Some(suspended) = self.suspended.pop() {
                self.buf = suspended;
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum MessageKind {
    Event,
    Response,
    Unknown,
}

fn message_kind(bytes: &[u8]) -> MessageKind {
    if bytes.starts_with(b"{\"m\":") || bytes.starts_with(b"{\"method\":") {
        MessageKind::Event
    } else if bytes.starts_with(b"{\"id\":")
        || bytes.starts_with(b"{\"result\":")
        || bytes.starts_with(b"{\"error\":")
    {
        MessageKind::Response
    } else {
        MessageKind::Unknown
    }
}

fn messages_can_interleave(current: &[u8], incoming: &[u8]) -> bool {
    let current = message_kind(current);
    let incoming = message_kind(incoming);
    current != MessageKind::Unknown && incoming != MessageKind::Unknown && current != incoming
}

fn parse_json_line(line: &[u8]) -> Option<Value> {
    if let Ok(value) = serde_json::from_slice(line) {
        return Some(value);
    }
    // BLE traffic occasionally resumes an interrupted message one byte early
    // or late. Accept only repairs that produce one complete JSON object.
    for (index, byte) in line.iter().enumerate().skip(1) {
        if *byte == b'{' {
            if let Ok(value) = serde_json::from_slice(&line[index..]) {
                return Some(value);
            }
        }
    }
    if !line.starts_with(b"{") && line.ends_with(b"}") {
        let mut repaired = Vec::with_capacity(line.len() + 1);
        repaired.push(b'{');
        repaired.extend_from_slice(line);
        if let Ok(value) = serde_json::from_slice(&repaired) {
            return Some(value);
        }
    }
    None
}

/// Bluetooth firmware v0.4.1 can weave a response continuation through a
/// joystick/key report before either message reaches its newline. The command
/// response is no longer generally reconstructable. Recover only a completely
/// intact embedded input object; if any action/direction field is itself torn,
/// fail closed rather than borrowing a plausible value from the response.
fn salvage_embedded_event(line: &[u8]) -> Option<Value> {
    let text = std::str::from_utf8(line).ok()?;
    const RAD_PREFIX: &str = r#"{"m":"v.oai.rad","p":{"a":"#;
    if let Some(start) = text.find(RAD_PREFIX) {
        let event = &text[start + RAD_PREFIX.len()..];
        let unit_interval = |value: f64| (0.0..=1.0).contains(&value);
        let (angle, event) = leading_number(event, r#","d":"#, unit_interval)?;
        let (deflection, _) = leading_number(event, "}}", unit_interval)?;
        return Some(json!({"m":"v.oai.rad","p":{"a":angle,"d":deflection}}));
    }
    const HID_PREFIX: &str = r#"{"m":"v.oai.hid","p":{"k":""#;
    if let Some(start) = text.find(HID_PREFIX) {
        let event = &text[start + HID_PREFIX.len()..];
        let key_end = event.find('"')?;
        let key = &event[..key_end];
        if !known_key(key) {
            return None;
        }
        let event = event[key_end + 1..].strip_prefix(r#","act":"#)?;
        let valid_action = |value: f64| value.fract() == 0.0 && (0.0..=2.0).contains(&value);
        let (action, _) = leading_number(event, "}}", valid_action)?;
        return Some(json!({"m":"v.oai.hid","p":{"k":key,"act":action as u8}}));
    }
    None
}

fn leading_number<'a>(
    text: &'a str,
    terminator: &str,
    valid: impl Fn(f64) -> bool,
) -> Option<(f64, &'a str)> {
    let length = text
        .char_indices()
        .take_while(|(_, character)| {
            character.is_ascii_digit() || matches!(character, '-' | '+' | '.' | 'e' | 'E')
        })
        .map(|(offset, character)| offset + character.len_utf8())
        .last()?;
    let value = text[..length].parse().ok()?;
    let rest = text[length..].strip_prefix(terminator)?;
    valid(value).then_some((value, rest))
}

fn known_key(value: &str) -> bool {
    matches!(
        value,
        "AG00"
            | "AG01"
            | "AG02"
            | "AG03"
            | "AG04"
            | "AG05"
            | "ACT06"
            | "ACT07"
            | "ACT08"
            | "ACT09"
            | "ACT10"
            | "ACT11"
            | "ACT12"
            | "ENC_CW"
            | "ENC_CC"
            | "ENC_CLK"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_timeout_preserves_the_single_attempt_firmware_budget() {
        assert_eq!(request_timeout(1), Duration::from_secs(2));
        assert_eq!(request_timeout(3), Duration::from_millis(500));
    }

    #[test]
    fn fragmented_json_is_reassembled() {
        let mut reader = Reader::default();
        assert!(reader.push_fragment(b"{\"id\":1,").is_empty());
        assert_eq!(
            reader.push_fragment(b"\"result\":{\"ok\":1}}\n"),
            vec![json!({"id":1,"result":{"ok":1}})]
        );
    }

    #[test]
    fn nested_object_at_fragment_boundary_is_not_a_new_message() {
        let mut reader = Reader::default();
        assert!(reader
            .push_fragment(
                b"{\"id\":8,\"result\":{\"padding\":\"xxxxxxxxxxxxxxxxxxxxxxxxxx\",\"x\":"
            )
            .is_empty());
        assert_eq!(
            reader.push_fragment(b"{\"ok\":1}}}\n"),
            vec![json!({
                "id":8,
                "result":{
                    "padding":"xxxxxxxxxxxxxxxxxxxxxxxxxx",
                    "x":{"ok":1}
                }
            })]
        );
    }

    #[test]
    fn fresh_bluetooth_message_recovers_from_interrupted_reply() {
        let mut reader = Reader::default();
        assert!(reader
            .push_fragment(b"{\"error\":{\"code\":400,\"message\":\"JSON")
            .is_empty());
        assert_eq!(
            reader.push_fragment(b"{\"m\":\"v.oai.rad\",\"p\":{\"a\":0.5,\"d\":1.0}}\n"),
            vec![json!({"m":"v.oai.rad","p":{"a":0.5,"d":1.0}})]
        );
        assert_eq!(
            reader.push_fragment(b" error\"},\"id\":null}\r\r\n"),
            vec![json!({"error":{"code":400,"message":"JSON error"},"id":null})]
        );
        assert_eq!(
            reader.push_fragment(b"{\"id\":8,\"result\":{\"ok\":1}}\n"),
            vec![json!({"id":8,"result":{"ok":1}})]
        );
    }

    #[test]
    fn repairs_observed_bluetooth_boundary_skew() {
        assert_eq!(
            parse_json_line(br#"{{"m":"v.oai.rad","p":{"a":0.5,"d":1.0}}"#),
            Some(json!({"m":"v.oai.rad","p":{"a":0.5,"d":1.0}}))
        );
        assert_eq!(
            parse_json_line(
                br#""error":{"code":400,"message":"JSON error - EmptyInput"},"id":null}"#
            ),
            Some(json!({
                "error":{"code":400,"message":"JSON error - EmptyInput"},
                "id":null
            }))
        );
        assert!(parse_json_line(b"not json").is_none());
    }

    #[test]
    fn torn_mid_fragment_merge_does_not_fabricate_or_wedge_later_input() {
        let mut reader = Reader::default();
        assert!(reader
            .push_fragment(
                b"{\"result\":{\"vers{\"m\":\"v.oai.rad\",\"p\":{\"a\":0.185796,\"d\"ion\":\"v"
            )
            .is_empty());
        assert!(reader.push_fragment(b":0.026256}}\n").is_empty());
        assert!(reader
            .push_fragment(
                b"0.4.1\",\"profile_index\":0,\"layer_index\":1,\"battery\":37,\"is_charging\":false},\"id\":32,\"method\":\"device.status\"}\n"
            )
            .is_empty());
        assert_eq!(
            reader.push_fragment(b"{\"m\":\"v.oai.rad\",\"p\":{\"a\":0.75,\"d\":1.0}}\n"),
            vec![json!({"m":"v.oai.rad","p":{"a":0.75,"d":1.0}})]
        );
    }

    #[test]
    fn intact_embedded_key_action_is_salvaged() {
        assert_eq!(
            salvage_embedded_event(
                br#"{"result":{"vers{"m":"v.oai.hid","p":{"k":"ACT12","act":1}}garbage"#
            ),
            Some(json!({"m":"v.oai.hid","p":{"k":"ACT12","act":1}}))
        );
    }

    #[test]
    fn direct_release_is_never_replaced_by_a_later_response_number() {
        assert_eq!(
            salvage_embedded_event(
                br#"{"result":{"vers{"m":"v.oai.hid","p":{"k":"ACT06","act":0}},"ok":1}}"#
            ),
            Some(json!({"m":"v.oai.hid","p":{"k":"ACT06","act":0}}))
        );
    }

    #[test]
    fn direct_low_deflection_is_never_replaced_by_a_later_response_number() {
        assert_eq!(
            salvage_embedded_event(
                br#"{"result":{"vers{"m":"v.oai.rad","p":{"a":0.25,"d":0.0}},"ok":1}}"#
            ),
            Some(json!({"m":"v.oai.rad","p":{"a":0.25,"d":0.0}}))
        );
    }

    #[test]
    fn torn_angle_never_borrows_a_response_number() {
        assert_eq!(
            salvage_embedded_event(
                br#"{"m":"v.oai.rad","p":{"a":0.{"result":{"profile_index":0,"layer_index":1,"battery":36}}75,"d":1.0}}"#
            ),
            None
        );
    }

    #[test]
    fn torn_action_and_deflection_never_borrow_response_ok() {
        assert_eq!(
            salvage_embedded_event(
                br#"{"m":"v.oai.hid","p":{"k":"ACT06","act"garbage{"result":{"ok":1}}"#
            ),
            None
        );
        assert_eq!(
            salvage_embedded_event(
                br#"{"m":"v.oai.rad","p":{"a":0.25,"d"garbage{"result":{"ok":1}}"#
            ),
            None
        );
    }

    #[test]
    fn response_fields_after_a_broken_event_prefix_are_never_borrowed() {
        assert_eq!(
            salvage_embedded_event(br#"{"m":"v.oai.hid"broken{"result":{"k":"ACT06","act":1}}x"#),
            None
        );
        assert_eq!(
            salvage_embedded_event(br#"{"m":"v.oai.rad"broken{"result":{"a":0.25,"d":1.0}}x"#),
            None
        );
    }

    #[test]
    fn salvage_rejects_invalid_utf8_unknown_keys_and_out_of_range_values() {
        assert_eq!(salvage_embedded_event(b"\xff{\"m\":\"v.oai.rad\""), None);
        assert_eq!(
            salvage_embedded_event(br#"{"m":"v.oai.hid","p":{"k":"NOPE","act":1}}"#),
            None
        );
        assert_eq!(
            salvage_embedded_event(br#"{"m":"v.oai.hid","p":{"k":"ACT06","act":3}}"#),
            None
        );
        assert_eq!(
            salvage_embedded_event(br#"{"m":"v.oai.rad","p":{"a":1.5,"d":1.0}}"#),
            None
        );
    }
}
