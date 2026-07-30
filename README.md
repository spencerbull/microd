# herdr-micro

Two apps connecting the Work Louder / OpenAI **Codex Micro** macropad to
[herdr](https://herdr.dev), split so the device layer is reusable:

```
Codex Micro  <-- vendor HID -->  microd  <-- unix socket -->  herdr-bridge  <-- socket API -->  herdr
```

## microd — generic pad daemon

Owns the pad's vendor HID channel (the one the ChatGPT desktop app uses for
Agent Key status lights) and serves newline-delimited JSON on a Unix socket
(`$MICROD_SOCKET`, `$XDG_RUNTIME_DIR/microd/microd.sock` on Linux, or
`~/.cache/microd/microd.sock` as a fallback). Any app can connect; events are
broadcast to all clients.

Events out:

```json
{"event":"key","key":"AG00","action":"press"}        // also "release"; dial steps use "step" with ENC_CW/ENC_CC; click is ENC_CLK
{"event":"joystick","angle":0.75,"deflection":1.0}   // angle in turns: 0=right 0.25=down 0.5=left 0.75=up
{"event":"device_message","message":{...}}           // anything else the pad says (battery, responses)
```

On top of raw events, microd runs a device-agnostic **gesture layer** and
broadcasts recognized gestures alongside them:

```json
{"event":"gesture","type":"tap","key":"AG00"}
{"event":"gesture","type":"long_press","key":"AG00"}      // fires at the hold threshold
{"event":"gesture","type":"double_tap","key":"AG00"}      // only for opted-in keys
{"event":"gesture","type":"chord","key":"AG02","held":["ACT12"]}
{"event":"gesture","type":"flick","direction":"up"}       // joystick, fires once per deflection
```

Semantics: every physically held key acts as a chord modifier, chords
repeatedly while held, and never emits its own tap for that hold. A modifier
held past the long-press threshold still chords (the long_press also fires at
the threshold — don't bind long_press on keys you use as modifiers). Dial
steps while keys are held emit as chorded steps (`key: "ENC_CW"/"ENC_CC"`)
and consume the held key's tap; bare steps stay raw-only. Keys in the
double-tap set have taps deferred by the double window (default 250 ms); all
other taps emit instantly on release. Long-press threshold default 450 ms.
Gesture config is in-memory: clients should re-send `gesture_config` after
(re)connecting.

Commands in (one per line, `id` echoed back as `{"id":...,"ok":true|false}`):

```json
{"id":1,"cmd":"lights","lights":[{"slot":0,"color":65280,"effect":"breath","speed":50}]}
{"id":2,"cmd":"lighting_config","ambient":{"effect":"solid","brightness":1.0,"speed":0.0,"magic":0.0,"color":65280},"keys":{"effect":"off","brightness":0.0,"speed":0.0,"magic":0.0,"color":0}}
{"id":3,"cmd":"clear"}
{"id":4,"cmd":"raw","method":"device.status","params":{}}
{"id":5,"cmd":"gesture_config","long_press_ms":450,"double_tap_ms":250,"double_tap_keys":["AG00","AG01"]}
```

Effects follow Work Louder's SDK: `off`, `solid`, `snake`, `rainbow`,
`breath`, `gradient`, and `shallow_breath`. Colors are 24-bit RGB ints.

```bash
cargo run -p microd -- run       # the daemon
cargo run -p microd -- list     # direct-HID utilities (daemon not required):
cargo run -p microd -- status   #   firmware version, battery, layer
cargo run -p microd -- demo     #   color-cycle the six Agent Keys
cargo run -p microd -- light 0 00ff00 --effect breath
cargo run -p microd -- watch    #   print raw pad events
cargo run -p microd -- raw v.oai.thstatus '[{"id":0,"c":255,"b":100,"e":1,"s":50}]'
```

## herdr-bridge — herdr integration

Connects to microd's socket and herdr's JSON API socket
(`$HERDR_SOCKET_PATH` or `~/.config/herdr/herdr.sock`).

The six Agent Keys mirror up to six herdr agents:

| herdr status | light |
|---|---|
| working | breathing blue |
| blocked | flashing amber |
| done | solid green |
| idle | dim white |
| unknown | dim purple |
| no agent | off |

The outer ambient ring summarizes the most important state across all six
agents: red error, then amber blocked/approval, then green ready for review,
then blue working, otherwise off. A working agent that returns to Herdr's
`idle` state is latched green until its Agent Key is pressed or a new pane
focus transition selects it from a Herdr client, because current screen-based
Codex, Claude, and Devin detection does not expose a persistent `done` state.
The pane merely remaining selected while Herdr is behind another app does not
acknowledge the alert. An authoritative `blocked`, `working`, or error state
always overrides that latch. Dictation
temporarily overrides the ring with teal while recording and white while
Voxtype is processing, then restores the aggregate agent state.

Herdr currently derives these providers' statuses from terminal output. The
bridge therefore treats only Herdr's explicit `blocked` status as waiting for
user input; it does not guess from punctuation or generic question marks.
Provider-native input-request hooks need a separate, explicit event contract.

Controls:

| control | action |
|---|---|
| Agent Key `AG00`–`AG05` | focus the Herdr window and that agent's pane; empty slot falls back to workspace N |
| dial rotate | cycle focus across live agents |
| dial click | zoom toggle on the focused pane |
| `ACT06` | send `enter` to the focused pane (approve) |
| `ACT07` | send `esc` to the focused pane (deny/interrupt) |
| `ACT08` | jump to the next **blocked** agent |
| mic key `ACT10` | optional Omarchy/Voxtype push-to-talk with `--voxtype`; release types the transcription without submitting |
| key next to mic `ACT12` | send `enter` to the focused pane |
| joystick left/right | previous/next tab in the focused workspace |
| joystick up/down | previous/next workspace |

The microphone key uses the computer's microphone; the Codex Micro itself
only sends press/release events. On Omarchy, install and enable Dictation
(Voxtype) first, then start the bridge with `--voxtype`. The user service
forces typed output and disables Voxtype's automatic and spoken-word
submission modes for each Codex Micro recording. It also ignores the companion
`ACT11` event observed when the mic is pressed, leaving the adjacent `ACT12`
Enter key as the explicit send action. These per-recording overrides do not
change global Voxtype preferences. The bridge records ownership in its private
state directory before starting capture and runs an ownership-aware
`voxtype record stop` on normal exit, crash restart, or forced termination,
without stopping a recording it did not start.

```bash
cargo run -p microd -- run &     # start the daemon first
cargo run -p herdr-bridge                  # portable default
cargo run -p herdr-bridge -- --voxtype    # optional Omarchy dictation
```

## Linux (Bluetooth or USB)

Linux exposes the Codex Micro vendor collection through `hidraw`, which is
root-only by default. Do not run `microd` with `sudo`; install the included
exact-device udev rule and add your account to the `input` group instead. The
rule covers both Bluetooth (`0005`) and USB (`0003`) without opening access to
unrelated HID devices:

```bash
sudo install -Dm644 udev/99-codex-micro.rules \
  /etc/udev/rules.d/99-codex-micro.rules
sudo usermod --append --groups input "$USER"
sudo udevadm control --reload-rules
sudo udevadm trigger --action=change --subsystem-match=hidraw
```

Log out and back in if `input` was newly added to your account, then reconnect
the pad so Bluetooth or USB recreates the matching `hidraw` node. Verify that
the node is readable and writable as your normal user:

```bash
id -nG | tr ' ' '\n' | grep -x input
for device in /dev/hidraw*; do
  if udevadm info --query=path --name="$device" |
    grep -Eiq '000[35]:303A:8360'; then
    stat -c '%n %A %U:%G' "$device"
    test -r "$device" && test -w "$device"
  fi
done
```

The matching line should show mode `crw-rw----` and group `input`. If the loop
prints nothing, reconnect the pad and try again. Avoid `chmod` on
`/dev/hidraw*`: device nodes are recreated on every reconnect, and a broad rule
would expose unrelated keyboards and security devices.

Build and install the binaries and user units without `sudo`:

```bash
cargo build --release --workspace
install -Dm755 target/release/microd ~/.local/bin/microd
install -Dm755 target/release/herdr-bridge ~/.local/bin/herdr-bridge
install -Dm644 systemd/microd.service ~/.config/systemd/user/microd.service
install -Dm644 systemd/herdr-bridge.service ~/.config/systemd/user/herdr-bridge.service
systemctl --user daemon-reload
systemctl --user enable --now herdr-bridge.service
```

The bridge pulls in `microd.service`, waits for Herdr's default session socket,
and restarts after a Bluetooth or Herdr disconnect. On Hyprland it also uses
the compositor's supported focus dispatcher to bring the terminal window
titled `herdr` to the foreground after pane, tab, or workspace navigation.
Logs are available with `journalctl --user -u microd -u herdr-bridge`.

The packaged unit leaves Voxtype disabled so it also works on Linux systems
without Omarchy Dictation. Enable the optional microphone mapping with a user
override:

```ini
# systemctl --user edit herdr-bridge.service
[Service]
ExecStart=
ExecStart=%h/.local/bin/herdr-bridge --voxtype
```

Linux ownership is an explicit two-mode switch because the vendor channel has
no multi-writer arbitration:

```bash
# Release the device before opening ChatGPT's Codex Micro integration:
systemctl --user stop microd.service

# Return exclusive ownership to Herdr (also starts microd):
systemctl --user start herdr-bridge.service
```

If ChatGPT should own the pad after every login, disable both units instead of
leaving the Herdr pair enabled.

## Running it (launchd + tray)

Three LaunchAgents (in `~/Library/LaunchAgents/`):

- `dev.planeshift.microd` — always on; waits for the pad to appear, survives unplug/replug
- `dev.planeshift.herdr-bridge` — loaded/unloaded by the tray to switch pad ownership
- `dev.planeshift.micro-tray` — menu bar switch (`tray/`, single-file Swift app)

The tray icon shows the pad's owner (`⌨H` = herdr, `⌨C` = Codex). Modes:
**Auto** (default — Codex owns the pad while the ChatGPT app is running, herdr
otherwise), or forced **herdr** / **Codex**. Switching to Codex unloads the
bridge and clears the Agent Key lights; microd keeps running in every mode
(shared input reads don't conflict — only LED writes do).

```bash
./scripts/build.sh   # release build + codesign microd with the `microd-dev`
                     # identity so Input Monitoring approval survives rebuilds
launchctl bootstrap gui/$UID ~/Library/LaunchAgents/dev.planeshift.microd.plist
launchctl bootstrap gui/$UID ~/Library/LaunchAgents/dev.planeshift.micro-tray.plist
# the tray manages dev.planeshift.herdr-bridge itself
```

Both daemons run with `ProcessType Background` + `Nice 5`, block on I/O when
idle (≈1 wakeup/s each), and use a few MB of RSS.

## macOS notes

- The terminal app running microd needs **Input Monitoring** permission
  (System Settings → Privacy & Security), or opens fail with
  `kIOReturnNotPermitted` (0xE00002E2).
- hidapi must be built with the `macos-shared-device` feature (already set);
  the default exclusive open of a keyboard-class device fails with
  `kIOReturnNotPrivileged` (0xE00002C1) unless root.
- Wired USB-C is the reliable transport; over BLE the pad sleeps and macOS
  merges the interfaces.

## Codex Micro protocol facts (verified live against firmware v0.4.1)

Base protocol per the reverse-engineered notes in
[codex-micro-4-core2](https://github.com/imliubo/codex-micro-4-core2/blob/main/docs/TECHNICAL.md),
with corrections found by probing the real device:

- Vendor interface: VID `0x303A` PID `0x8360`, usage page `0xFF00`, report ID 6.
  63-byte reports: `[msg_type=2][payload_len][<=61 bytes UTF-8 JSON]`, JSON
  lines newline-terminated and chunked across reports (~4 ms between chunks).
- `v.oai.thstatus` params: array of `{"id":0-5,"c":<24-bit RGB int>,"b":<0-100>,
  "e":<effect int>,"s":<0-100>}`. **Values must be integers** — the float/string
  forms in the emulator docs are ACKed but not rendered.
- Official SDK effect enum: `0`=off, `1`=solid, `2`=snake, `3`=rainbow,
  `4`=breath, `5`=gradient, `6`=shallow breath. Earlier probing correctly
  observed the visuals but misnamed code 3 as a flash effect.
- `v.oai.rgbcfg` independently controls the outer `ambient` ring and global
  `keys` zone. Both compact side objects contain `e`, `b`, `s`, `m`, and `c`.
- Device→host events use compact keys `{"m":"v.oai.hid","p":{...}}` (not
  `method`/`params`): keys `AG00`–`AG05`, `ACT06`–`ACT12`, `act` 1/0=press/release,
  2=encoder step (`ENC_CW`/`ENC_CC`, click=`ENC_CLK`); joystick =
  `v.oai.rad` with `a` (angle in turns) and `d` (deflection 0-1), release `0,0`.
- Host requests that work: `sys.version`, `device.status`, `v.oai.thstatus`,
  `v.oai.rgbcfg`.

## herdr API notes

- The server expects **one request per connection**; a second request on the
  same connection is ignored. The bridge keeps one dedicated connection for
  `events.subscribe` and uses one-shot connections for commands.
- Closing a tab or workspace does **not** emit `pane.closed`/`pane.exited` for
  its panes, and `agent.list` can retain stale records for those panes. The
  bridge reconciles against the union of `agent.list` and `pane.list` (detected
  but unregistered agents appear only in the latter).
- `events.subscribe` replays buffered session history to each new subscriber —
  expect a burst of stale events at connect (the bridge verifies panes exist
  before slotting them).

## Caveats

The vendor protocol is private OpenAI/Work Louder behavior and can change with
firmware or ChatGPT app updates. The channel is effectively single-owner: run
microd or the ChatGPT desktop app's integration, not both.

On firmware v0.4.1 over Bluetooth, low-deflection joystick reports can be
continuous and the firmware can occasionally merge an input report into a
multi-report response. microd preserves cleanly framed interleaving, discards
irrecoverably malformed combined JSON, and retries only known idempotent
status/version/light operations. Arbitrary raw commands are never retried.
A one-shot input report that collides with a malformed firmware response can
still be lost; USB-C is the strict-reliability transport until firmware fixes
that Bluetooth race.
