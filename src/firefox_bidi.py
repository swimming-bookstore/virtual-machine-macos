"""One BiDi session for the Firefox this tool started.

Firefox allows one session, and a second socket cannot join it or end it.
This process owns the socket. Commands arrive on a localhost TCP port and
reuse that session.

`BIDI_ACTION` set: send one command to that helper and print the result.
No action: own the session (started from `firefox open`).
"""

import json, os, select, socket, sys

PORT = int(os.environ.get("BIDI_PORT", "9333"))
CMD_PORT = int(os.environ.get("BIDI_CMD_PORT", "9334"))


def recvn(s, n):
    buf = b""
    while len(buf) < n:
        chunk = s.recv(n - len(buf))
        if not chunk:
            raise SystemExit("firefox closed the BiDi session")
        buf += chunk
    return buf


def ws_connect(port):
    s = socket.create_connection(("127.0.0.1", port), 10)
    key = "dGhlIHNhbXBsZSBub25jZQ=="
    req = (
        f"GET /session HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n"
        "Upgrade: websocket\r\nConnection: Upgrade\r\n"
        f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    )
    s.sendall(req.encode())
    buf = b""
    while b"\r\n\r\n" not in buf:
        chunk = s.recv(4096)
        if not chunk:
            raise SystemExit("firefox closed the debug port")
        buf += chunk
    head = buf.split(b"\r\n\r\n", 1)[0].decode("latin1", "replace")
    if "101" not in head.split("\r\n", 1)[0]:
        raise SystemExit("firefox debug port is not BiDi: " + head.split("\r\n", 1)[0])
    return s


def frame(payload):
    data = payload.encode()
    n = len(data)
    mask = b"\x11\x22\x33\x44"
    hdr = bytearray([0x81])
    if n < 126:
        hdr.append(0x80 | n)
    elif n < 65536:
        hdr.append(0x80 | 126)
        hdr += n.to_bytes(2, "big")
    else:
        hdr.append(0x80 | 127)
        hdr += n.to_bytes(8, "big")
    hdr += mask
    return bytes(hdr) + bytes(b ^ mask[i % 4] for i, b in enumerate(data))


def recv_json(s, pending):
    while True:
        ready, _, _ = select.select([s], [], [], 30)
        if not ready:
            raise SystemExit("firefox did not answer")
        hdr = recvn(s, 2)
        opcode = hdr[0] & 0x0F
        length = hdr[1] & 0x7F
        if length == 126:
            length = int.from_bytes(recvn(s, 2), "big")
        elif length == 127:
            length = int.from_bytes(recvn(s, 8), "big")
        if hdr[1] & 0x80:
            mask = recvn(s, 4)
            payload = bytes(b ^ mask[i % 4] for i, b in enumerate(recvn(s, length)))
        else:
            payload = recvn(s, length)
        if opcode == 0x8:
            raise SystemExit("firefox closed the BiDi session")
        if opcode != 0x1 or not payload:
            continue
        msg = json.loads(payload.decode())
        if msg.get("id") in pending:
            return msg


class Bidi:
    def __init__(self, port):
        self.s = ws_connect(port)
        self.seq = 0

    def call(self, method, params):
        self.seq += 1
        self.s.sendall(frame(json.dumps({"id": self.seq, "method": method, "params": params})))
        msg = recv_json(self.s, {self.seq})
        err = msg.get("error")
        if msg.get("type") == "error" or err:
            text = err if isinstance(err, str) else json.dumps(err or msg.get("message") or "bidi error")
            detail = msg.get("message")
            if isinstance(detail, str) and detail and detail not in text:
                text = f"{text}: {detail}"
            raise SystemExit(text)
        return msg.get("result") or {}


def tops(bidi):
    tree = bidi.call("browsingContext.getTree", {"maxDepth": 0})
    contexts = tree.get("contexts") or []
    return [c for c in contexts if not c.get("parent")]


def active(rows):
    for c in rows:
        if str(c.get("url", "")).startswith("http"):
            return c
    return rows[0] if rows else None


def unwrap_eval(msg):
    result = (msg.get("result") if isinstance(msg, dict) else None) or {}
    if not isinstance(result, dict):
        return ""
    inner = result.get("result") if isinstance(result.get("result"), dict) else result
    if inner.get("type") in (None, "undefined"):
        return ""
    if inner.get("type") == "string":
        return inner.get("value", "")
    if "value" in inner:
        val = inner["value"]
        return val if isinstance(val, str) else json.dumps(val)
    return ""


def eval_js(bidi, ctx, expression):
    msg = bidi.call(
        "script.evaluate",
        {
            "expression": expression,
            "target": {"context": ctx},
            "awaitPromise": True,
            "resultOwnership": "none",
        },
    )
    return unwrap_eval({"result": msg} if "result" not in msg else msg)


def perform(bidi, ctx, actions):
    bidi.call("browsingContext.activate", {"context": ctx})
    bidi.call("input.performActions", {"context": ctx, "actions": actions})
    bidi.call("input.releaseActions", {"context": ctx})


def click_at(bidi, ctx, x, y, button=0):
    perform(
        bidi,
        ctx,
        [
            {
                "type": "pointer",
                "id": "mouse",
                "parameters": {"pointerType": "mouse"},
                "actions": [
                    {"type": "pointerMove", "x": float(x), "y": float(y), "origin": "viewport", "duration": 80},
                    {"type": "pause", "duration": 40},
                    {"type": "pointerDown", "button": int(button)},
                    {"type": "pause", "duration": 40},
                    {"type": "pointerUp", "button": int(button)},
                ],
            }
        ],
    )


def type_text(bidi, ctx, text):
    actions = []
    for ch in text:
        actions.append({"type": "keyDown", "value": ch})
        actions.append({"type": "keyUp", "value": ch})
        actions.append({"type": "pause", "duration": 40})
    perform(bidi, ctx, [{"type": "key", "id": "kbd", "actions": actions}])


def handle(bidi, action, arg):
    rows = tops(bidi)
    cur = active(rows)
    if action == "tabs":
        lines = []
        for c in rows:
            mark = "*" if cur and c.get("context") == cur.get("context") else " "
            lines.append(f"{mark} {c.get('context', '')}  {c.get('url', '')}  {c.get('title', '')}")
        return "\n".join(lines)
    if action == "goto":
        if not cur:
            raise SystemExit("no tab")
        bidi.call("browsingContext.activate", {"context": cur["context"]})
        bidi.call(
            "browsingContext.navigate",
            {"context": cur["context"], "url": arg, "wait": "complete"},
        )
        return arg
    if action == "eval":
        if not cur:
            raise SystemExit("no tab")
        return eval_js(bidi, cur["context"], arg)
    if action == "click":
        if not cur:
            raise SystemExit("no tab")
        parts = arg.split()
        if len(parts) < 2:
            raise SystemExit("click needs x y")
        click_at(bidi, cur["context"], parts[0], parts[1], parts[2] if len(parts) > 2 else 0)
        return arg
    if action == "type":
        if not cur:
            raise SystemExit("no tab")
        type_text(bidi, cur["context"], arg)
        return arg
    if action == "key":
        if not cur:
            raise SystemExit("no tab")
        perform(
            bidi,
            cur["context"],
            [{"type": "key", "id": "kbd", "actions": [{"type": "keyDown", "value": arg}, {"type": "keyUp", "value": arg}]}],
        )
        return arg
    if action == "screenshot":
        if not cur:
            raise SystemExit("no tab")
        shot = bidi.call(
            "browsingContext.captureScreenshot",
            {
                "context": cur["context"],
                "format": {"type": "image/jpeg", "quality": 0.5},
            },
        )
        return shot.get("data") or ""
    if action == "close":
        if arg:
            bidi.call("browsingContext.close", {"context": arg})
            return arg
        closed = []
        for c in rows:
            if cur and c.get("context") == cur.get("context"):
                continue
            bidi.call("browsingContext.close", {"context": c["context"]})
            closed.append(c["context"])
        return "\n".join(closed)
    raise SystemExit("unknown firefox action")


def serve():
    srv = socket.socket()
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", CMD_PORT))
    srv.listen(4)
    sys.stderr.write(f"bidi listen {CMD_PORT}\n")
    sys.stderr.flush()
    bidi = Bidi(PORT)
    bidi.call("session.new", {"capabilities": {}})
    sys.stderr.write(f"bidi ready {CMD_PORT}\n")
    sys.stderr.flush()
    while True:
        conn, _ = srv.accept()
        try:
            raw = b""
            while b"\n" not in raw and len(raw) < 8_000_000:
                chunk = conn.recv(65536)
                if not chunk:
                    break
                raw += chunk
            req = json.loads(raw.decode() or "{}")
            out = handle(bidi, req.get("action", ""), req.get("arg", ""))
            conn.sendall(json.dumps({"ok": True, "out": out}).encode() + b"\n")
        except SystemExit as e:
            conn.sendall(json.dumps({"ok": False, "err": str(e)}).encode() + b"\n")
        except Exception as e:
            conn.sendall(json.dumps({"ok": False, "err": str(e)}).encode() + b"\n")
        finally:
            conn.close()


def client(action, arg):
    s = socket.create_connection(("127.0.0.1", CMD_PORT), 5)
    s.sendall(json.dumps({"action": action, "arg": arg}).encode() + b"\n")
    raw = b""
    while b"\n" not in raw and len(raw) < 8_000_000:
        chunk = s.recv(65536)
        if not chunk:
            break
        raw += chunk
    msg = json.loads(raw.decode() or "{}")
    if not msg.get("ok"):
        raise SystemExit(msg.get("err") or "bidi error")
    out = msg.get("out") or ""
    if out:
        sys.stdout.write(out if out.endswith("\n") else out + "\n")


if __name__ == "__main__":
    action = os.environ.get("BIDI_ACTION", "")
    arg = os.environ.get("BIDI_ARG", "")
    if action:
        client(action, arg)
    else:
        serve()
