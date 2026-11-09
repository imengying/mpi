"""Exercise the installed terminal lifecycle on Linux and macOS, using only stdlib."""

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

        process = subprocess.Popen([str(binary)], stdin=slave, stdout=slave, stderr=slave,
                                   cwd=root, env={**os.environ, "HOME": temp,
                                                  "TERM": "xterm-256color"},
                                   preexec_fn=take_terminal)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        output = bytearray()
        child_pid = None

        def read_available():
            if select.select([master], [], [], 0.02)[0]:
                output.extend(os.read(master, 65536))

        def until(condition):
            deadline = time.monotonic() + 10
            while not condition():
                assert process.poll() is None, (mode, process.returncode, output[-1000:])
                assert time.monotonic() < deadline, (mode, "timeout", output[-1000:])
                read_available()

        def send(text):
            os.write(master, text.encode())

        try:
            until(lambda: b"\x1b[?1049h" in output)
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

            process.send_signal(exit_signal)
            until(lambda: process.poll() is not None)
            while select.select([master], [], [], 0.05)[0]:
                output.extend(os.read(master, 65536))
            assert process.returncode == 128 + exit_signal, (mode, process.returncode)
            assert termios.tcgetattr(slave) == original, (mode, "terminal mode not restored")
            assert b"\x1b[?1049l" in output, (mode, "alternate screen not restored")
            if child_pid:
                status = subprocess.run(["ps", "-o", "stat=", "-p", str(child_pid)],
                                        capture_output=True, text=True).stdout.strip()
                assert not status or status.startswith("Z"), ("surviving tool child", status)
            print(f"PASS {mode}: {exit_signal.name}")
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()
            if child_pid:
                try:
                    os.kill(child_pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            termios.tcsetattr(slave, termios.TCSANOW, original)
            os.close(master)
            os.close(slave)
            released.set()
            server.shutdown()
            server.server_close()


if __name__ == "__main__":
    binary = Path(sys.argv[1]).resolve()
    for exit_signal in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        check(binary, "idle", exit_signal)
    for mode in ("model", "picker", "stream", "approval", "tool"):
        check(binary, mode, signal.SIGTERM)
