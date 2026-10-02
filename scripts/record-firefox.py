#!/usr/bin/env python3
"""Record docs/demo-firefox.mp4 from WebDriver BiDi.

Frames are browsingContext.captureScreenshot. Input is input.performActions
through the helper on 9334. Nothing is clicked on the X desktop.

Path: YouTube home → search → channel home → Videos → latest video.
"""
from __future__ import annotations

import base64
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import threading
import time
from pathlib import Path

CMD_PORT = int(os.environ.get("BIDI_CMD_PORT", "9334"))
OUT = Path(os.environ.get("OUT", "/tmp/demo-firefox.mp4"))
QUERY = os.environ.get("QUERY", "tzuyang")
FPS = 8
DIR = os.environ.get("VM_DIR", "/tmp/vm-gui")
VMAGENT = os.environ.get("VMAGENT", str(Path(__file__).resolve().parents[1] / "bin/vmagent"))


def find_ffmpeg() -> str:
    env = os.environ.get("FF")
    if env:
        return env
    for p in (
        shutil.which("ffmpeg"),
        str(Path.home() / ".local/bin/ffmpeg"),
        "/opt/homebrew/bin/ffmpeg",
        "/usr/bin/ffmpeg",
    ):
        if p and Path(p).is_file():
            return p
    raise SystemExit("ffmpeg not found on the host")


def ensure_bidi() -> None:
    try:
        socket.create_connection(("127.0.0.1", CMD_PORT), 1).close()
        return
    except OSError:
        pass
    subprocess.run(
        [
            VMAGENT,
            "ssh",
            "--dir",
            DIR,
            "-f",
            "-N",
            "-o",
            "ExitOnForwardFailure=yes",
            "-L",
            f"{CMD_PORT}:127.0.0.1:{CMD_PORT}",
            "debian@vm",
        ],
        check=True,
    )
    t0 = time.time()
    while time.time() - t0 < 8:
        try:
            socket.create_connection(("127.0.0.1", CMD_PORT), 1).close()
            return
        except OSError:
            time.sleep(0.2)
    raise SystemExit("cannot reach guest BiDi on 127.0.0.1:%s" % CMD_PORT)


def cmd(action: str, arg: str = "", timeout: float = 40.0) -> str:
    s = socket.create_connection(("127.0.0.1", CMD_PORT), 5)
    s.settimeout(timeout)
    s.sendall(json.dumps({"action": action, "arg": arg}).encode() + b"\n")
    raw = b""
    while b"\n" not in raw and len(raw) < 8_000_000:
        chunk = s.recv(65536)
        if not chunk:
            break
        raw += chunk
    s.close()
    msg = json.loads(raw.decode() or "{}")
    if not msg.get("ok"):
        raise SystemExit(msg.get("err") or f"{action} failed")
    return msg.get("out") or ""


def js(expression: str) -> str:
    return cmd("eval", expression)


def point_from(raw: str) -> tuple[float, float] | None:
    if not raw:
        return None
    try:
        st = json.loads(raw)
    except json.JSONDecodeError:
        return None
    if not isinstance(st, dict):
        return None
    try:
        return float(st["x"]), float(st["y"])
    except (KeyError, TypeError, ValueError):
        return None


def click_xy(x: float, y: float) -> bool:
    try:
        cmd("click", f"{x} {y}")
    except SystemExit:
        return False
    return True


def dismiss() -> None:
    js(
        """
        (() => {
          const buttons = [...document.querySelectorAll('button, tp-yt-paper-button')];
          const btn = buttons.find(b => /accept all|reject all|i agree|^accept$|reject the use/i.test((b.innerText || b.getAttribute('aria-label') || '')));
          if (btn) btn.click();
          return btn ? 'ok' : '';
        })()
        """
    )


def wait_js(expression: str, timeout: float = 20.0, pause: float = 0.6) -> str:
    t0 = time.time()
    last = ""
    while time.time() - t0 < timeout:
        dismiss()
        last = js(expression)
        if last:
            return last
        time.sleep(pause)
    return last


def on_youtube_home() -> bool:
    href = js("location.href")
    if not href:
        return False
    return "youtube.com" in href and "/watch" not in href and "/results" not in href and "/@" not in href and "/channel/" not in href


def prep_home() -> None:
    """Load YouTube home and drop extra tabs before the camera starts."""
    subprocess.run(
        [VMAGENT, "firefox", "--dir", DIR, "open", "https://www.youtube.com/"],
        check=True,
    )
    ensure_bidi()
    t0 = time.time()
    while time.time() - t0 < 25:
        try:
            cmd("close")
            if on_youtube_home() and wait_js(
                """
                (() => {
                  const el = document.querySelector('input#search, input[name=search_query], input[aria-label="Search"]');
                  if (!el) return '';
                  const r = el.getBoundingClientRect();
                  return r.width > 2 && r.height > 2 ? 'ok' : '';
                })()
                """,
                timeout=2,
                pause=0.4,
            ):
                return
        except SystemExit:
            pass
        time.sleep(0.5)
    raise SystemExit("youtube home never loaded")


def click_search() -> bool:
    raw = wait_js(
        """
        (() => {
          const el = document.querySelector('input#search, input[name=search_query], input[aria-label="Search"]');
          if (!el) return '';
          el.scrollIntoView({block: 'center', inline: 'nearest'});
          const r = el.getBoundingClientRect();
          if (r.width < 2 || r.height < 2) return '';
          return JSON.stringify({x: r.x + r.width / 2, y: r.y + r.height / 2});
        })()
        """,
        timeout=12,
    )
    pt = point_from(raw)
    if not pt:
        return False
    return click_xy(*pt)


def click_channel() -> bool:
    raw = wait_js(
        """
        (() => {
          const want = /tzuyang|tuzyang/i;
          const nodes = [
            ...document.querySelectorAll('ytd-channel-renderer a#main-link, ytd-channel-renderer a.channel-link, a#channel-title, ytd-channel-name a, a#main-link'),
            ...document.querySelectorAll('a[href*="/@"], a[href*="/channel/"]'),
          ];
          const el = nodes.find((a) => {
            const href = a.getAttribute('href') || '';
            if (!/\\/@|\\/channel\\//.test(href)) return false;
            if (/\\/videos|\\/shorts|\\/streams|\\/playlists|\\/community/.test(href)) return false;
            const text = (a.innerText || '') + ' ' + (a.getAttribute('title') || '') + ' ' + href;
            return want.test(text) || want.test(href);
          });
          if (!el) return '';
          el.scrollIntoView({block: 'center', inline: 'nearest'});
          const r = el.getBoundingClientRect();
          if (r.width < 2 || r.height < 2) return '';
          return JSON.stringify({x: r.x + Math.min(r.width / 2, 80), y: r.y + r.height / 2, href: el.href});
        })()
        """,
        timeout=20,
    )
    print("channel", raw, flush=True)
    pt = point_from(raw)
    if not pt:
        return False
    return click_xy(*pt)


def on_channel() -> bool:
    raw = js(
        """
        (() => {
          const href = location.href;
          const ok = /\\/@|\\/channel\\//.test(href) && !/\\/watch/.test(href);
          return ok ? href : '';
        })()
        """
    )
    return bool(raw)


def click_videos_tab() -> bool:
    raw = wait_js(
        """
        (() => {
          const nodes = [...document.querySelectorAll('yt-tab-shape, tp-yt-paper-tab, a[href*="/videos"]')];
          const el = nodes.find((n) => {
            const label = (n.getAttribute('tab-title') || n.getAttribute('aria-label') || n.innerText || '').trim();
            const href = n.getAttribute('href') || '';
            return /^videos$/i.test(label) || /\\/videos\\/?$/.test(href);
          });
          if (!el) return '';
          el.scrollIntoView({block: 'center', inline: 'nearest'});
          const r = el.getBoundingClientRect();
          if (r.width < 2 || r.height < 2) return '';
          return JSON.stringify({x: r.x + r.width / 2, y: r.y + r.height / 2, href: el.href || location.href});
        })()
        """,
        timeout=12,
    )
    print("videos tab", raw, flush=True)
    pt = point_from(raw)
    if not pt:
        return False
    return click_xy(*pt)


def click_latest_video() -> bool:
    raw = wait_js(
        """
        (() => {
          const items = [...document.querySelectorAll('ytd-rich-item-renderer, ytd-grid-video-renderer')];
          const el = items.find((item) => {
            const a = item.querySelector('a[href*="/watch"]');
            if (!a) return false;
            const r = item.getBoundingClientRect();
            return r.width > 40 && r.height > 40 && r.y + r.height / 2 > 80 && r.y < innerHeight - 20;
          });
          if (!el) return '';
          el.scrollIntoView({block: 'center', inline: 'nearest'});
          const a = el.querySelector('a[href*="/watch"]');
          const r = el.getBoundingClientRect();
          const y = Math.min(Math.max(r.y + Math.min(r.height / 2, 90), 90), innerHeight - 20);
          return JSON.stringify({x: r.x + Math.min(r.width / 2, 160), y, href: a.href, title: (a.getAttribute('title') || a.innerText || '').trim().slice(0, 80)});
        })()
        """,
        timeout=30,
        pause=0.8,
    )
    print("latest", raw, flush=True)
    pt = point_from(raw)
    if not pt:
        return False
    return click_xy(*pt)


def play() -> bool:
    play_js = """
        (() => {
          const v = document.querySelector('video.html5-main-video') || document.querySelector('video');
          if (v) {
            try { v.muted = true; const p = v.play(); if (p && p.catch) p.catch(() => {}); } catch (e) {}
            const btn = document.querySelector('.ytp-large-play-button, .ytp-play-button');
            if (v.paused && btn) { try { btn.click(); } catch (e) {} }
          }
          return JSON.stringify({href: location.href, title: document.title, has: !!v, t: v ? (v.currentTime||0) : 0, paused: v ? !!v.paused : true, w: v ? (v.videoWidth||0) : 0});
        })()
    """
    t1 = time.time()
    while time.time() - t1 < 45:
        raw = js(play_js)
        print("yt", raw, flush=True)
        if raw:
            try:
                st = json.loads(raw)
            except json.JSONDecodeError:
                st = {}
            if (
                "/watch" in st.get("href", "")
                and st.get("has")
                and not st.get("paused")
                and st.get("t", 0) >= 1.0
                and st.get("w", 0) > 0
            ):
                return True
        time.sleep(0.6)
    return False


def drive() -> None:
    dismiss()
    print(">> search", QUERY, flush=True)
    if click_search():
        time.sleep(0.6)
        cmd("type", QUERY)
        time.sleep(0.8)
        cmd("key", "\ue007")
    else:
        cmd("goto", "https://www.youtube.com/results?search_query=" + QUERY)
    time.sleep(3.5)
    dismiss()
    print(">> channel home", flush=True)
    if not click_channel():
        raise SystemExit("no channel result")
    t0 = time.time()
    while time.time() - t0 < 15 and not on_channel():
        time.sleep(0.5)
    if not on_channel():
        raise SystemExit("channel page never loaded")
    time.sleep(3.0)
    print(">> videos tab", flush=True)
    if not click_videos_tab():
        href = js("location.origin + location.pathname.replace(/\\/+$/, '') + '/videos'")
        if href:
            cmd("goto", href)
    t1 = time.time()
    while time.time() - t1 < 12:
        if "/videos" in js("location.pathname"):
            break
        time.sleep(0.4)
    time.sleep(3.5)
    print(">> latest video", flush=True)
    if not click_latest_video():
        raise SystemExit("no latest video")
    if not play():
        raise SystemExit("video never started playing")
    time.sleep(12)
    print("done", flush=True)


def main() -> None:
    prep_home()
    print("tabs", cmd("tabs"), flush=True)
    if OUT.exists():
        OUT.unlink()
    ff = subprocess.Popen(
        [
            find_ffmpeg(),
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "mjpeg",
            "-framerate",
            str(FPS),
            "-i",
            "-",
            "-an",
            "-vf",
            "scale=1280:720:force_original_aspect_ratio=decrease,pad=1280:720:(ow-iw)/2:(oh-ih)/2,setsar=1",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-preset",
            "ultrafast",
            "-movflags",
            "+faststart",
            str(OUT),
        ],
        stdin=subprocess.PIPE,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    )
    stop = threading.Event()

    def grab() -> None:
        period = 1.0 / FPS
        while not stop.is_set():
            t0 = time.time()
            try:
                data = cmd("screenshot", timeout=8)
            except Exception:
                data = ""
            if data and ff.stdin:
                try:
                    ff.stdin.write(base64.b64decode(data))
                    ff.stdin.flush()
                except (BrokenPipeError, OSError):
                    return
            dt = time.time() - t0
            if dt < period:
                time.sleep(period - dt)

    thread = threading.Thread(target=grab, daemon=True)
    thread.start()
    time.sleep(0.4)
    if ff.poll() is not None:
        err = ff.stderr.read().decode(errors="replace") if ff.stderr else ""
        raise SystemExit("ffmpeg failed: " + err)
    try:
        drive()
    finally:
        stop.set()
        thread.join(timeout=3)
        try:
            ff.stdin.close()
        except OSError:
            pass
        try:
            ff.send_signal(signal.SIGINT)
        except OSError:
            pass
        try:
            ff.wait(timeout=8)
        except subprocess.TimeoutExpired:
            ff.kill()
    if not OUT.exists() or OUT.stat().st_size < 1000:
        raise SystemExit("demo-firefox.mp4 missing")
    print(f"wrote {OUT} ({OUT.stat().st_size} bytes)", flush=True)


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        sys.exit(130)
