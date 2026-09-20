#!/usr/bin/env python3
"""Exercise the real Herdr API with simulated Micro events in an isolated session.

Use --gui for a real Ghostty/Hyprland focus check. Without it only the compositor
is stubbed. This never connects to the hardware daemon or the user's API socket.
"""
import argparse
import json
import os
from pathlib import Path
import shlex
import socket
import subprocess
import sys
import tempfile
import threading
import time


def eventually(check, label, timeout=8):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if check():
            return
        time.sleep(0.05)
    raise AssertionError(f"Timed out: {label}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--herdr", default="herdr")
    parser.add_argument("--bridge", default="target/release/herdr-bridge")
    parser.add_argument("--gui", action="store_true")
    parser.add_argument("--artifacts", required=True)
    args = parser.parse_args()
    artifacts = Path(args.artifacts).resolve()
    artifacts.mkdir(parents=True, exist_ok=True)
    bridge_bin = str(Path(args.bridge).resolve())
    checks = []
    with tempfile.TemporaryDirectory(prefix="micro-check-") as directory:
        root = Path(directory)
        api_path, hub_path = root / "api.sock", root / "micro.sock"
        env = {k: v for k, v in os.environ.items() if not k.startswith("HERDR_")}
        env.update(HERDR_SOCKET_PATH=str(api_path),
                   HERDR_CLIENT_SOCKET_PATH=str(root / "api-client.sock"),
                   HERDR_CONFIG_PATH=str(root / "config.toml"),
                   XDG_CONFIG_HOME=str(root / "config"),
                   XDG_STATE_HOME=str(root / "state"),
                   HERDR_MICRO_WINDOW_CLASS="dev.planeshift.micro-validation")
        (root / "config.toml").write_text('[ui]\nagent_panel_sort = "priority"\n')
        processes, files = [], []
        hub = socket.socket(socket.AF_UNIX)
        hub.bind(str(hub_path))
        hub.listen(1)
        hub.settimeout(10)
        connection = None
        frames = []
        send_lock = threading.Lock()

        def launch(argv, name, proc_env=env):
            output = open(artifacts / name, "w")
            files.append(output)
            process = subprocess.Popen(argv, env=proc_env, stdout=output, stderr=subprocess.STDOUT)
            processes.append(process)
            return process

        def api(method, params=None):
            with socket.socket(socket.AF_UNIX) as client:
                client.settimeout(3)
                client.connect(str(api_path))
                client.sendall((json.dumps({"id": "micro-check", "method": method, "params": params or {}}) + "\n").encode())
                reply = json.loads(client.makefile().readline())
                if "error" in reply:
                    raise AssertionError(reply)
                return reply["result"]

        def send(message):
            with send_lock:
                connection.sendall((json.dumps(message) + "\n").encode())

        def key(name, action="press"):
            send({"event": "key", "key": name, "action": action})

        def current():
            return api("pane.current")["pane"]["pane_id"]

        def focus_key(name, target, action="press"):
            key(name, action)
            eventually(lambda: current() == target, f"{name} -> {target}")
            checks.append(f"{name} focused {target}")

        try:
            launch([args.herdr, "server"], "server.log")
            eventually(api_path.exists, "isolated server socket")
            first = api("workspace.create", {"cwd": directory, "label": "Micro test A", "focus": False})
            p1 = first["root_pane"]["pane_id"]
            p2 = api("pane.split", {"pane_id": p1, "direction": "right", "cwd": directory, "focus": False})["pane"]["pane_id"]
            second = api("workspace.create", {"cwd": directory, "label": "Micro test B", "focus": False})
            p3 = second["root_pane"]["pane_id"]
            third = api("tab.create", {"workspace_id": first["workspace"]["workspace_id"], "cwd": directory, "focus": False})
            p4 = third["root_pane"]["pane_id"]
            for pane, state in [(p1, "working"), (p2, "blocked"), (p3, "idle"), (p4, "idle")]:
                api("pane.report_agent", {"pane_id": pane, "source": "micro-validation", "agent": "codex", "state": state})

            original_window = None
            if args.gui:
                original_window = json.loads(subprocess.check_output(["hyprctl", "-j", "activewindow"])).get("address")
                assert original_window, "GUI verification requires an existing window to focus away to"
                launch(["ghostty", "--class=" + env["HERDR_MICRO_WINDOW_CLASS"],
                        "--gtk-single-instance=false", "-e", args.herdr], "ghostty.log")
                def windows():
                    return [w for w in json.loads(subprocess.check_output(["hyprctl", "-j", "clients"]))
                            if w.get("class") == env["HERDR_MICRO_WINDOW_CLASS"]]
                eventually(lambda: len(windows()) == 1, "visible test terminal")
                address = windows()[0]["address"]
            else:
                stub = root / "bin"
                stub.mkdir()
                (stub / "hyprctl").write_text("#!" + sys.executable + "\nimport sys,json\na=sys.argv[1:]\nprint(json.dumps([{'instance':'test','time':1,'wl_socket':'wayland-micro'}]) if a==['instances','-j'] else json.dumps([{'class':'dev.planeshift.micro-validation','address':'0x123'}]) if a==['-j','clients'] else json.dumps({'address':'0x123'}) if a==['-j','activewindow'] else 'ok')\n")
                (stub / "hyprctl").chmod(0o755)
                env["PATH"] = str(stub) + os.pathsep + env["PATH"]
                env["WAYLAND_DISPLAY"] = "wayland-micro"

            bridge = launch([bridge_bin, "--socket", str(api_path), "--hub", str(hub_path), "--order", "auto"], "bridge.log")
            connection, _ = hub.accept()
            connection.settimeout(None)

            def read_hub():
                try:
                    for line in connection.makefile():
                        command = json.loads(line)
                        frames.append(command)
                        send({"id": command["id"], "ok": True})
                except (OSError, ValueError):
                    pass

            reader = threading.Thread(target=read_hub, daemon=True)
            reader.start()
            eventually(lambda: any(f.get("cmd") == "lights" for f in frames), "initial LED frame")
            initial = next(f for f in frames if f.get("cmd") == "lights")
            assert initial["lights"][0]["color"] == 0xffaa00, initial
            checks.append("auto ordering: blocked agent first, amber LED command")
            if args.gui:
                assert original_window != address, "GUI verification requires a different original window"
                subprocess.run(["hyprctl", "dispatch", f'hl.dsp.focus({{ window = "address:{original_window}" }})'], check=True, stdout=subprocess.DEVNULL)
                eventually(lambda: json.loads(subprocess.check_output(["hyprctl", "-j", "activewindow"])).get("address") == original_window, "focus moved away before pad event")
            focus_key("AG00", p2)
            if args.gui:
                eventually(lambda: json.loads(subprocess.check_output(["hyprctl", "-j", "activewindow"])).get("address") == address, "real compositor focus")
                checks.append("real Hyprland active-address confirmation")
            focus_key("AG01", p1)
            focus_key("ENC_CC", p2, "step")
            key("ENC_CLK")
            eventually(lambda: api("pane.layout", {"pane_id": p2})["layout"]["zoomed"], "dial zoom")
            key("ENC_CLK")
            eventually(lambda: not api("pane.layout", {"pane_id": p2})["layout"]["zoomed"], "dial unzoom")
            checks.append("dial zoom on/off")
            for angle, pane in [(0.0, p4), (0.5, p2)]:
                send({"event": "joystick", "angle": angle, "deflection": 1.0})
                eventually(lambda: current() == pane, "joystick tab navigation")
                send({"event": "joystick", "angle": angle, "deflection": 0.0})
            checks.append("joystick next/previous tab")
            send({"event": "joystick", "angle": 0.25, "deflection": 1.0})
            eventually(lambda: current() == p3, "joystick next workspace")
            send({"event": "joystick", "angle": 0.25, "deflection": 0.0})
            checks.append("joystick next workspace")
            proof = root / "enter-proof"
            api("pane.send_text", {"pane_id": p3, "text": "touch " + shlex.quote(str(proof))})
            key("ACT12")
            eventually(proof.exists, "dedicated Enter key")
            checks.append("ACT12 executes only in the isolated test shell")
            # Removing an agent must clear its key within a reconciliation.
            before_release = next(f for f in reversed(frames) if f.get("cmd") == "lights")
            assert all(light["effect"] != "off" for light in before_release["lights"][:4]), before_release
            frame_count = len(frames)
            api("pane.release_agent", {"pane_id": p3, "source": "micro-validation", "agent": "codex"})
            def released_frame():
                latest = next((f for f in reversed(frames[frame_count:]) if f.get("cmd") == "lights"), None)
                return latest and all(light["effect"] != "off" for light in latest["lights"][:3]) and latest["lights"][3]["effect"] == "off"
            eventually(released_frame, "new three-agent LED frame after release")
            checks.append("released agent removed from LED slots")
            assert bridge.poll() is None, "bridge exited during controls"
            time.sleep(0.3)
            bridge_log = (artifacts / "bridge.log").read_text()
            assert "failed:" not in bridge_log and "reconcile failed" not in bridge_log, bridge_log
            checks.append("bridge log contains no control/reconciliation failures")
            (artifacts / "led-commands.json").write_text(json.dumps(frames, indent=2))
            (artifacts / "results.json").write_text(json.dumps({"gui": args.gui, "checks": checks, "herdr": api("ping")}, indent=2))
            print(json.dumps({"result": "PASS", "gui": args.gui, "checks": checks}, indent=2))
        finally:
            try:
                api("server.stop")  # Only this harness's isolated socket.
            except (OSError, ValueError, AssertionError):
                pass
            if connection:
                connection.close()
            hub.close()
            for process in reversed(processes):
                if process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait()
            for output in files:
                output.close()


if __name__ == "__main__":
    main()
