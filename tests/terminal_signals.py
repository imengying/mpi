"""Exercise the installed terminal lifecycle on Linux and macOS, using only stdlib."""

import ast
import errno
import fcntl
import http.server
import json
import os
from pathlib import Path
import pty
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time


def check(binary, mode, exit_signal):
    with tempfile.TemporaryDirectory(prefix="pi-signals-") as temp:
        root = Path(temp)
        (root / ".pi").mkdir()
        released = threading.Event()

        class Provider(http.server.BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_POST(self):
                self.rfile.read(int(self.headers["Content-Length"]))
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()

                def emit(event):
                    self.wfile.write(("data: " + json.dumps(event) + "\n\n").encode())
                    self.wfile.flush()

                if mode == "stream":
                    emit({"type": "response.output_text.delta", "output_index": 0,
                          "delta": "WAITING_FOR_SIGNAL"})
                    released.wait(20)
                elif mode == "picker":
                    item = {"type": "message", "role": "assistant", "content": [{
                        "type": "output_text", "text": "SAVED_FOR_SIGNAL"}]}
                    emit({"type": "response.output_item.done", "output_index": 0,
                          "item": item})
                    emit({"type": "response.completed", "response": {
                        "id": "r", "status": "completed", "output": [item],
                        "usage": {"input_tokens": 10, "output_tokens": 10}}})
                else:
                    call = {"type": "function_call", "id": "fc", "call_id": "c",
                            "name": "bash", "arguments": json.dumps({"command":
                            "sleep 60 & echo $! > child.pid; wait"})}
                    emit({"type": "response.output_item.done", "output_index": 0,
                          "item": call})
                    emit({"type": "response.completed", "response": {
                        "id": "r", "status": "completed", "output": [call],
                        "usage": {"input_tokens": 10, "output_tokens": 10}}})

        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Provider)
        config = {"providers": [{"name": "test", "api": "responses",
                  "base_url": f"http://127.0.0.1:{server.server_port}/v1",
                  "api_key": "unused", "models": [{"id": "m"}]}],
                  "default_model": "test/m"}
        (root / ".pi/config.json").write_text(json.dumps(config))
        master, slave = pty.openpty()
        original = termios.tcgetattr(slave)
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))

        def take_terminal():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)

        # Keep a shell-like terminal owner alive after pi exits. macOS revokes the
        # slave when its session leader exits, preventing inspection of its modes.
        release_reader, release_writer = os.pipe()
        owner = """
import os, subprocess, sys, termios
from pathlib import Path
Path('pi.mode').write_text(repr(termios.tcgetattr(0)))
child = subprocess.Popen([sys.argv[1]])
Path('pi.pid').write_text(str(child.pid))
Path('pi.exit').write_text(str(child.wait()))
os.read(int(sys.argv[2]), 1)
"""
        process = subprocess.Popen([sys.executable, "-c", owner, str(binary), str(release_reader)],
                                   stdin=slave, stdout=slave, stderr=slave,
                                   cwd=root, env={**os.environ, "HOME": temp,
                                                  "TERM": "xterm-256color"},
                                   pass_fds=(release_reader,), preexec_fn=take_terminal)
        os.close(release_reader)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        output = bytearray()
        pi_pid = None
        child_pid = None

        def exit_status():
            try:
                return int((root / "pi.exit").read_text())
            except (FileNotFoundError, ValueError):
                return None

        def read_available():
            if select.select([master], [], [], 0.02)[0]:
                try:
                    chunk = os.read(master, 65536)
                except OSError as error:
                    if error.errno == errno.EIO:
                        return False
                    raise
                output.extend(chunk)
                return bool(chunk)
            return False

        def until(condition):
            deadline = time.monotonic() + 10
            while not condition():
                assert process.poll() is None, (mode, process.returncode, output[-1000:])
                assert exit_status() is None, (mode, exit_status(), output[-1000:])
                assert time.monotonic() < deadline, (mode, "timeout", output[-1000:])
                read_available()

        def send(text):
            os.write(master, text.encode())

        try:
            until(lambda: b"\x1b[?1049h" in output)
            pi_pid = int((root / "pi.pid").read_text())
            original = ast.literal_eval((root / "pi.mode").read_text())
            if mode == "picker":
                send("save session\r")
                until(lambda: b"SAVED_FOR_SIGNAL" in output)
                send("/delete\r")
                until(lambda: "不删，继续".encode() in output)
            elif mode == "model":
                send("/model\r")
                until(lambda: "选择模型".encode() in output)
            elif mode in ("stream", "tool", "approval"):
                if mode == "tool":
                    send("/permissions\r")
                    until(lambda: "命令审核".encode() in output)
                    send("2")
                send("test signal\r")
                if mode == "stream":
                    until(lambda: b"WAITING_FOR_SIGNAL" in output)
                elif mode == "approval":
                    until(lambda: b"sleep 60" in output)
                else:
                    until(lambda: (root / "child.pid").is_file()
                          and bool((root / "child.pid").read_text().strip()))
                    child_pid = int((root / "child.pid").read_text())

            if mode == "normal":
                send("/exit\r")
            else:
                os.kill(pi_pid, exit_signal)
            until(lambda: exit_status() is not None)
            # An exited PTY stays readable at EOF on macOS.
            while read_available():
                pass
            expected_status = 0 if mode == "normal" else 128 + exit_signal
            assert exit_status() == expected_status, (mode, exit_status())
            restored = termios.tcgetattr(slave)
            assert restored == original, (mode, "terminal mode not restored", original, restored)
            assert b"\x1b[?1049l" in output, (mode, "alternate screen not restored")
            if child_pid:
                status = subprocess.run(["ps", "-o", "stat=", "-p", str(child_pid)],
                                        capture_output=True, text=True).stdout.strip()
                assert not status or status.startswith("Z"), ("surviving tool child", status)
            print(f"PASS {mode}: {'/exit' if mode == 'normal' else exit_signal.name}")
        finally:
            if pi_pid and exit_status() is None:
                try:
                    os.kill(pi_pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            if child_pid:
                try:
                    os.kill(child_pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            termios.tcsetattr(slave, termios.TCSANOW, original)
            os.close(release_writer)
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
            os.close(master)
            os.close(slave)
            released.set()
            server.shutdown()
            server.server_close()


if __name__ == "__main__":
    binary = Path(sys.argv[1]).resolve()
    try:
        check(binary, "normal", None)
        for exit_signal in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
            check(binary, "idle", exit_signal)
        for mode in ("model", "picker", "stream", "approval", "tool"):
            check(binary, mode, signal.SIGTERM)
    except Exception as error:
        if os.environ.get("GITHUB_ACTIONS"):
            message = str(error).replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")
            print("::error title=Terminal signal test::" + message)
        raise
