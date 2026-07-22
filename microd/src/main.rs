//! microd: generic companion daemon + CLI for the Work Louder / OpenAI
//! Codex Micro. Owns the vendor HID channel; the `run` subcommand serves pad
//! events and light control over a Unix socket for other apps (e.g.
//! herdr-bridge). The other subcommands talk HID directly for one-off use.

mod device;
mod gestures;
mod server;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};
use std::time::Duration;

use device::Effect;

#[derive(Parser)]
#[command(name = "microd", about = "Codex Micro daemon and device CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon: dispatch pad events / accept light commands on a socket
    Run {
        /// Socket path (default: $MICROD_SOCKET or ~/.cache/microd/microd.sock)
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
    },
    /// Enumerate matching HID interfaces (no writes)
    List,
    /// Request sys.version from the device
    Version,
    /// Request device.status (profile, layer, battery)
    Status,
    /// Set one agent key light: slot 0-5, color as RRGGBB hex
    Light {
        slot: u8,
        color: String,
        /// Effect: off | solid | flash | flash2 | breath
        #[arg(long, default_value = "solid")]
        effect: Effect,
        /// Effect speed 0-100
        #[arg(long, default_value_t = 50)]
        speed: u8,
    },
    /// Turn all six agent key lights off
    Clear,
    /// Cycle a short color demo across all six agent keys
    Demo,
    /// Print incoming vendor events (key presses, joystick) until Ctrl-C
    Watch,
    /// Send a raw JSON-RPC message (method + params as JSON) and print responses
    Raw {
        method: String,
        /// Params as a JSON literal, e.g. '[{"id":0,"c":65280,"b":100,"e":1,"s":50}]'
        params: Option<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut api = hidapi::HidApi::new().context("initialize hidapi")?;

    match cli.command {
        Command::Run { socket } => {
            let path = socket.unwrap_or_else(server::default_socket_path);
            server::run(&mut api, path)
        }
        Command::List => device::list(&api),
        Command::Version => device::request(&device::open_vendor_interface(&api)?, "sys.version", None),
        Command::Status => device::request(&device::open_vendor_interface(&api)?, "device.status", None),
        Command::Light { slot, color, effect, speed } => {
            if slot > 5 {
                bail!("slot must be 0-5");
            }
            let color = u32::from_str_radix(color.trim_start_matches('#'), 16)
                .context("color must be RRGGBB hex")?;
            let params = json!([device::light_param(slot, color, effect.code(), speed)]);
            device::request(&device::open_vendor_interface(&api)?, "v.oai.thstatus", Some(params))
        }
        Command::Clear => {
            let params: Value = (0..6).map(|id| device::light_param(id, 0, 0, 50)).collect();
            device::request(&device::open_vendor_interface(&api)?, "v.oai.thstatus", Some(params))
        }
        Command::Demo => demo(&device::open_vendor_interface(&api)?),
        Command::Watch => watch(&device::open_vendor_interface(&api)?),
        Command::Raw { method, params } => {
            let params = params
                .map(|p| serde_json::from_str::<Value>(&p))
                .transpose()
                .context("params must be valid JSON")?;
            device::request(&device::open_vendor_interface(&api)?, &method, params)
        }
    }
}

fn demo(dev: &hidapi::HidDevice) -> Result<()> {
    let colors: [u32; 6] = [0xff0000, 0xff8800, 0xffff00, 0x00ff00, 0x0088ff, 0x8800ff];
    println!("cycling colors (Ctrl-C to stop early)...");
    for step in 0..12usize {
        let params: Value = (0..6u8)
            .map(|slot| {
                let c = colors[(slot as usize + step) % colors.len()];
                let effect = if step % 2 == 0 { Effect::Breath } else { Effect::Solid };
                device::light_param(slot, c, effect.code(), 50)
            })
            .collect();
        device::send_json(dev, &json!({"method": "v.oai.thstatus", "params": params, "id": step + 10}))?;
        std::thread::sleep(Duration::from_millis(700));
    }
    let params: Value = (0..6).map(|id| device::light_param(id, 0, 0, 50)).collect();
    device::send_json(dev, &json!({"method": "v.oai.thstatus", "params": params, "id": 99}))?;
    println!("demo done, lights cleared");
    Ok(())
}

fn watch(dev: &hidapi::HidDevice) -> Result<()> {
    println!("watching for vendor events (Ctrl-C to stop)...");
    let mut reader = device::Reader::debug();
    loop {
        for value in reader.poll(dev)? {
            println!("{value}");
        }
    }
}
