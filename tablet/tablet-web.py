"""Serve the tablet page and bridge it to the local VNC server.

GET /settings gives the page its colors and frame rate from tablet.env.
GET /screen tells whether the Gliff screen is up; POST /screen with
"on\n<password>" or "off\n<password>" puts it up or takes it away by
starting or stopping the gliff-tablet-vnc user unit.
"""

import hmac
import os
import subprocess
import time

from websockify.websocketproxy import ProxyRequestHandler, WebSocketProxy

HERE = os.path.dirname(os.path.abspath(__file__))
# gliff's config directory, as the installer found it.
CONFIG_DIR = os.environ.get("GLIFF_CONFIG_DIR") or os.path.join(
    os.environ.get("XDG_CONFIG_HOME") or os.path.expanduser("~/.config"), "gliff"
)
PASSWORD_FILE = os.path.join(CONFIG_DIR, "vnc-password")
SETTINGS_FILE = os.path.join(CONFIG_DIR, "tablet.env")
UNIT = "gliff-tablet-vnc.service"
# The newest viewer holds the screen. Each connection runs in a process of
# its own, so the newest one is named in a file the others watch.
LATEST = os.path.join(os.environ.get("XDG_RUNTIME_DIR", "/tmp"), "gliff-tablet-latest")


def password():
    with open(PASSWORD_FILE, encoding="utf-8") as f:
        return f.readline().rstrip("\n")[:8]


def page_settings():
    """The page's part of tablet.env, as `key=value` lines."""
    values = {"colors": "full", "fps": "0"}
    try:
        with open(SETTINGS_FILE, encoding="utf-8") as f:
            for line in f:
                key, _, value = line.strip().partition("=")
                if key == "GLIFF_TABLET_COLORS" and value in ("full", "256"):
                    values["colors"] = value
                elif key == "GLIFF_TABLET_FPS" and value in ("0", "5", "10", "15"):
                    values["fps"] = value
    except OSError:
        pass
    return "\n".join("%s=%s" % kv for kv in values.items())


def settings_stamp():
    try:
        return os.stat(SETTINGS_FILE).st_mtime_ns
    except OSError:
        return 0


def state():
    out = subprocess.run(
        ["systemctl", "--user", "is-active", UNIT], capture_output=True, text=True
    ).stdout.strip()
    return out or "unknown"


class LoginWatch:
    """The server's side of the page's RFB 3.8 login, read as it passes, to
    tell when the page has logged in: its version, the security types
    gliff-server offers (one), the challenge for a password, then the
    result."""

    def __init__(self):
        self.data = b""
        # None until the result is in, then whether the login succeeded.
        self.done = None

    def feed(self, chunk):
        if self.done is not None:
            return self.done
        self.data += chunk
        d = self.data
        if len(d) < 13:
            return None
        n = d[12]
        if n == 0:
            self.done = False
            return False
        if len(d) < 13 + n:
            return None
        types = d[13 : 13 + n]
        # VNC Authentication sends a 16-byte challenge before the result.
        result_at = 13 + n + (16 if 2 in types and 1 not in types else 0)
        if len(d) < result_at + 4:
            return None
        self.done = d[result_at : result_at + 4] == b"\0\0\0\0"
        return self.done


class Handler(ProxyRequestHandler):
    def new_websocket_client(self):
        # Pages from before this check retry forever; turn them away.
        if "v=2" not in self.path:
            raise self.CClose(4003, "old page, reload it")
        self.viewer = "%d-%d" % (os.getpid(), time.monotonic_ns())
        self.stamp = settings_stamp()
        # Named the newest viewer only once it has logged in, so someone
        # without the password cannot take the screen from the tablet.
        self.published = False
        self.login = LoginWatch()
        try:
            super().new_websocket_client()
        except self.CClose:
            # gliff-server drops the older viewer as soon as the newer one
            # logs in, which can be before the newer bridge has read the
            # login reply and named itself: wait a moment for that name
            # before saying the close was anything else.
            if self.replaced_soon():
                raise self.CClose(4001, "replaced")
            raise

    def send_frames(self, bufs=None):
        # The server's bytes on their way to the page.
        if not self.published and bufs:
            for chunk in bufs:
                if self.login.feed(chunk):
                    with open(LATEST, "w", encoding="utf-8") as f:
                        f.write(self.viewer)
                    self.published = True
                    break
        return super().send_frames(bufs)

    def replaced_soon(self, wait=1.0):
        """Whether a newer viewer names itself within `wait` seconds."""
        deadline = time.monotonic() + wait
        while True:
            if self.replaced():
                return True
            if time.monotonic() >= deadline:
                return False
            time.sleep(0.05)

    def replaced(self):
        # A viewer still logging in holds nothing to be replaced from.
        if not getattr(self, "published", False):
            return False
        try:
            with open(LATEST, encoding="utf-8") as f:
                latest = f.read().strip()
        except OSError:
            return False
        return bool(latest) and latest != self.viewer

    def send_ping(self, data=b""):
        # Called about once a second from the proxy loop (heartbeat=1): end
        # this connection when a newer viewer has taken the screen, with a
        # reason the page reads so it stops reconnecting.
        if self.replaced():
            raise self.CClose(4001, "replaced")
        # New colors or frame rate take a new connection: the page asks for
        # its pixel format when it connects.
        if settings_stamp() != self.stamp:
            raise self.CClose(4002, "settings")

    def reply(self, code, text):
        body = (text + "\n").encode()
        self.send_response(code)
        self.send_header("Content-Type", "text/plain; charset=utf-8")
        self.send_header("Cache-Control", "no-store")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def end_headers(self):
        if self.path.endswith(".html"):
            self.send_header("Cache-Control", "no-cache")
        super().end_headers()

    def do_GET(self):
        path = self.path.split("?")[0]
        if path == "/screen":
            return self.reply(200, state())
        if path == "/settings":
            return self.reply(200, page_settings())
        if path == "/":
            # Served in place, not redirected: iOS 8 Safari drops the
            # #fragment, which carries the password, across a redirect.
            self.path = "/tablet.html"
        super().do_GET()

    def do_POST(self):
        if self.path.split("?")[0] != "/screen":
            return self.send_error(404)
        length = min(int(self.headers.get("Content-Length") or 0), 256)
        lines = self.rfile.read(length).decode("utf-8", "replace").split("\n")
        action = lines[0].strip()
        given = lines[1][:8] if len(lines) > 1 else ""
        if not hmac.compare_digest(given.encode(), password().encode()):
            time.sleep(2)
            return self.reply(403, "wrong password")
        if action not in ("on", "off"):
            return self.reply(400, "on or off")
        verb = "start" if action == "on" else "stop"
        subprocess.run(["systemctl", "--user", verb, UNIT], check=False)
        return self.reply(200, state())


if __name__ == "__main__":
    WebSocketProxy(
        RequestHandlerClass=Handler,
        listen_host="0.0.0.0",
        listen_port=6080,
        target_host="127.0.0.1",
        target_port=5900,
        web=os.path.join(HERE, "novnc"),
        file_only=True,
        heartbeat=1,
    ).start_server()
