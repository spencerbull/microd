# microd

Companion daemon and CLI for the **Codex Micro** macropad (the OpenAI × Work
Louder device, hardware family of the Creator Micro 2).

microd owns the pad's vendor HID channel — the same one the ChatGPT desktop
app uses for its Agent Key status lights — and serves newline-delimited JSON
on a Unix socket (`$MICROD_SOCKET` or `~/.cache/microd/microd.sock`), so any
program can react to the pad and drive its RGB lights without touching HID.

> Unaffiliated community tool. Not associated with or endorsed by OpenAI or
> Work Louder. The vendor protocol is private host behavior (base documentation
> from the [codex-micro-4-core2](https://github.com/imliubo/codex-micro-4-core2)
> emulator project, corrected against real firmware v0.4.1) and may change
> with firmware or app updates. The channel is effectively single-owner: run
> microd or the ChatGPT desktop app's pad integration, not both at once.

## Run

```bash
cargo install microd
microd run                 # the daemon
microd status              # one-off CLI (no daemon needed): battery, layer
microd demo                # RGB color cycle
microd light 0 00ff00 --effect breath
microd watch               # print raw pad events
```

## Socket protocol

Events broadcast to every connected client:

```json
{"event":"key","key":"AG00","action":"press"}          // release; dial steps use "step" (ENC_CW/ENC_CC), click ENC_CLK
{"event":"joystick","angle":0.75,"deflection":1.0}     // angle in turns: 0=right 0.25=down 0.5=left 0.75=up
{"event":"device_message","message":{}}                // anything else the pad says
```

A device-agnostic **gesture layer** runs on top and broadcasts alongside:

```json
{"event":"gesture","type":"tap","key":"AG00"}
{"event":"gesture","type":"long_press","key":"AG00"}
{"event":"gesture","type":"double_tap","key":"AG00"}
{"event":"gesture","type":"chord","key":"AG02","held":["ACT12"]}
{"event":"gesture","type":"chord","key":"ENC_CW","held":["ACT12"]}
{"event":"gesture","type":"flick","direction":"up"}
```

Every physically held key acts as a chord modifier, chords repeatedly while
held, and never emits its own tap for that hold; dial steps while keys are
held arrive as chorded steps. Keys opted into double-tap have taps deferred by
the double window; all other taps emit instantly. Defaults: long-press 450 ms,
double window 250 ms.

Commands (one JSON object per line; `id` echoed back):

```json
{"id":1,"cmd":"lights","lights":[{"slot":0,"color":65280,"effect":"breath","speed":50}]}
{"id":2,"cmd":"clear"}
{"id":3,"cmd":"raw","method":"device.status","params":{}}
{"id":4,"cmd":"gesture_config","long_press_ms":450,"double_tap_ms":250,"double_tap_keys":["AG00"]}
```

Effects: `off`, `solid`, `flash`, `flash2`, `breath`. Colors are 24-bit RGB
integers. Gesture config is in-memory — re-send it after reconnecting.

## macOS notes

- The process running microd needs **Input Monitoring** permission. macOS ties
  the grant to the binary's code signature: sign your builds with a stable
  (self-signed is fine) identity so approval survives rebuilds.
- Wired USB-C is the reliable transport; over BLE the pad power-saves
  aggressively. microd waits for the device and survives unplug/replug.
