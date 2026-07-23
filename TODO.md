# Codex Micro + Herdr Linux Verification

## Goal

Make the physical Work Louder Codex Micro reliable as a Bluetooth control
surface for Herdr on this Omarchy Linux host.

## Done criteria

- The Codex Micro vendor HID channel is accessible without root after reconnect.
- Firmware/status queries work over Bluetooth as the normal user.
- All physical keys, dial directions/click, and joystick directions are decoded.
- All six Agent Key LEDs accept off, solid, flashing, and breathing updates.
- Herdr `working`, `blocked`, `done`, `idle`, and agent removal update LEDs live.
- The outer ring shows the highest-priority state across all agents and turns
  off when none needs attention.
- Mic push-to-talk and adjacent Enter work through Omarchy/Herdr.
- Agent, tab, and workspace navigation also activates the Herdr window in
  Omarchy/Hyprland.
- Agent keys, command keys, dial, and joystick perform their documented Herdr actions.
- Device sleep, Bluetooth disconnect/reconnect, daemon restart, and Herdr restart recover automatically.
- User services start cleanly and do not contend with another device owner.
- Focused tests, full workspace tests, static checks, and independent review pass.

## Streams

- Branch: `codex/herdr-linux-integration`
- Checkout: `/home/sbull/src/github.com/PlaneshiftDev/microd`
- Device: Bluetooth Codex Micro, VID `303A`, PID `8360`, firmware `v0.4.1`
- Herdr: `0.7.5`, default session socket

## Allowed actions

- Read and write this checkout.
- Build and run `microd` and `herdr-bridge`.
- Exercise physical controls and temporary RGB effects.
- Create narrowly scoped udev and user-service configuration.
- Create disposable Herdr test workspaces, tabs, panes, and local test processes.

## Forbidden actions

- No upstream push, pull request, or release without explicit approval.
- No production deployment, secret changes, billing changes, or customer data.
- No deletion or reset of unrelated user files, sessions, branches, or configuration.
- No broad HID permission rules or unrelated desktop configuration changes.

## Verification

- [x] Normal-user Bluetooth firmware/status query.
- [ ] Raw input capture for every physical control (AG00-AG04, physical mic
  `ACT10`, adjacent Enter `ACT11`, and both dial directions confirmed;
  remaining controls awaiting physical presses).
- [ ] RGB/effect matrix across all six Agent Keys (firmware ACKs confirmed;
  visual confirmation pending).
- [ ] Outer-ring visual confirmation (separate `v.oai.rgbcfg` commands for
  red, green, and blue snake all received firmware success ACKs).
- [x] Bluetooth disconnect and reconnect.
- [ ] Physical idle sleep and wake.
- [x] Herdr API compatibility tests.
- [x] Live Herdr state-to-RGB transitions (working, blocked, idle, unknown,
  and removal all produced firmware-acknowledged updates in an isolated
  session; visual confirmation pending).
- [x] Herdr control mapping tests.
- [x] Omarchy/Hyprland window activation from a physical Agent Key.
- [x] Daemon and Herdr failure recovery.
- [x] Voxtype ownership cleanup after bridge SIGKILL and supervised restart.
- [x] Persistent Linux user services.
- [x] Explicit service-to-direct-CLI ownership switch and service restoration.
- [x] `cargo fmt --check`.
- [x] `cargo clippy --workspace --all-targets -- -D warnings`.
- [x] `cargo test --workspace` (51 tests: 15 bridge, 36 microd).
- [x] Independent review.
- [ ] Final physical-device regression.

## Current findings

- `/etc/udev/rules.d/99-codex-micro.rules` grants the exact device `input`
  group access for Bluetooth and USB transports.
- Bluetooth exposes the vendor usage page `0xFF00` at `/dev/hidraw6`.
- `sys.version` and `device.status` round-trip successfully without root.
- The original bridge did not subscribe to Herdr's dedicated
  `pane.agent_status_changed` event and removed valid slots while replaying
  stale lifecycle events. It now reconciles authoritative state, subscribes
  per pane, handles pane moves, validates subscription and LED ACKs, and has
  eight focused tests.
- Bluetooth firmware v0.4.1 can interleave a long response and a joystick
  report inside one HID fragment. Parsing recovery plus bounded retries for
  idempotent commands passed 500/500 live status transactions. Completely
  intact embedded input objects are recovered, while torn action/direction
  fields fail closed so corruption can never fabricate an approve, interrupt,
  or navigation action.
- `microd.service` and `herdr-bridge.service` are installed and enabled.
  The runtime directory is mode `0700`, the socket is `0600`, and a second
  daemon refuses to steal the live socket.
- Bridge-owned dictation is marked under systemd's mode-`0700`
  `StateDirectory`. A live SIGKILL test stopped Voxtype on restart, removed
  the marker, and returned the bridge and Voxtype to active/idle.
- Work Louder's shipped SDK identifies `v.oai.rgbcfg` as the independent
  ambient-ring/global-key command. The daemon now sends it before the six
  `v.oai.thstatus` agent lights, retries it safely over BLE, and rejects
  firmware `result:false` acknowledgements.
- Herdr's focus APIs update its internal pane state but do not activate the
  terminal client in Hyprland. The bridge now follows successful pane, tab,
  and workspace focus with Hyprland's current Lua focus dispatcher, retains a
  legacy fallback, discovers the matching compositor instance at press time,
  and verifies the active window within one bounded deadline. A physical Agent
  Key moved focus from the `~` Kitty window on workspace 2 to the `herdr` Kitty
  window on workspace 1 while selecting pane `wK:p7`. The final installed
  binary also passed a synthetic `AG00` integration test with
  `HYPRLAND_INSTANCE_SIGNATURE` removed, moving from another Kitty window on
  workspace 3 to Herdr on workspace 1 while selecting pane `wK:p1`.
- SIGKILL recovery passed independently for both services. An isolated Herdr
  server stop/start recovered, and a real Codex Micro Bluetooth disconnect
  caused the expected HID failure, supervised restart, device rediscovery,
  and successful post-reconnect firmware query.
- The installed release binaries match the reviewed build hashes. The exact
  deployed daemon passed 20/20 concurrent and 500/500 sequential Bluetooth
  status transactions, and the documented direct-CLI ownership handoff
  returned firmware status before both services reconnected.
- Final independent review found no actionable Hyprland activation issues.
  The five focused compositor tests passed again, including 20/20 repetitions
  of the subprocess-deadline test.
- A disposable live Codex agent completed a turn successfully, but Herdr
  `0.7.5` exposed its settled state as `idle`, not `done`; the bridge's `done`
  mapping is covered by protocol fixtures. The bridge now latches a
  working-to-idle transition as ready-for-review until its Agent Key is
  pressed.

## Open questions

- Whether Bluetooth input reports survive device sleep without reopening HID.
- Whether the service recovers from physical idle sleep without a Bluetooth
  controller disconnect.
- Whether all documented lighting effects render on firmware `v0.4.1`.
- Whether every visual status/effect matches the intended physical LED output.
- Which current Herdr agent integration, if any, emits a persistent live
  `done` state rather than returning to `idle`.
