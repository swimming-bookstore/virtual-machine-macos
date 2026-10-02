#!/usr/bin/env python3
"""Record docs/demo-computer-use.mp4 from the virtio desktop.

Frames are `vmagent screenshot` of :0, encoded on the host. Input is
vmagent click / key / type. Guest ffmpeg is left alone so YouTube can
use it.

Path: YouTube home, search tzuyang, channel, Videos, latest clip. No terminal.

Clicks are screen pixels of the 1280x800 virtio display. Element boxes
come from the page; they are mapped with mozInnerScreenX/Y.
"""
from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

DIR = os.environ.get("VM_DIR", "/tmp/vm-gui")
VMAGENT = os.environ.get("VMAGENT", str(Path(__file__).resolve().parents[1] / "bin/vmagent"))
OUT = Path(os.environ.get("OUT", "/tmp/demo-computer-use.mp4"))
QUERY = os.environ.get("QUERY", "tzuyang")
FPS = int(os.environ.get("FPS", "4"))


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


def run(*args: str, check: bool = True) -> subprocess.CompletedProcess:
    cmd = [VMAGENT, *args]
    print("+", " ".join(cmd), flush=True)
    return subprocess.run(cmd, check=check)


def guest(script: str, check: bool = True) -> subprocess.CompletedProcess:
    return run("run", "--dir", DIR, "--", script, check=check)


def click(x: int, y: int) -> None:
    run("click", "--dir", DIR, str(x), str(y))


def key(name: str) -> None:
    run("key", "--dir", DIR, name)


def type_text(text: str) -> None:
    run("type", "--dir", DIR, text)


def pause(secs: float) -> None:
    time.sleep(secs)


def xdo(script: str) -> None:
    guest(
        "export DISPLAY=:0 XAUTHORITY=/home/debian/.Xauthority "
        "DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus; " + script
    )


def eval_js(expression: str) -> str:
    r = subprocess.run(
        [VMAGENT, "firefox", "--dir", DIR, "eval", expression],
        capture_output=True,
        text=True,
    )
    out = (r.stdout or "").strip()
    if r.returncode != 0:
        err = (r.stderr or "").strip()
        raise SystemExit(err or out or "firefox eval failed")
    return out


def parse_point(raw: str) -> tuple[float, float] | None:
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


def screen_origin() -> tuple[float, float]:
    raw = eval_js(
        "JSON.stringify({x: window.mozInnerScreenX || 0, y: window.mozInnerScreenY || 0})"
    )
    pt = parse_point(raw)
    if not pt:
        raise SystemExit("no inner screen origin")
    return pt


def click_page(x: float, y: float) -> None:
    ox, oy = screen_origin()
    click(int(round(ox + x)), int(round(oy + y)))


def wait_js(expression: str, timeout: float = 20.0, pause_s: float = 0.5) -> str:
    t0 = time.time()
    last = ""
    while time.time() - t0 < timeout:
        try:
            last = eval_js(expression)
        except SystemExit:
            last = ""
        if last and last not in ("", "null", "false", "undefined"):
            return last
        time.sleep(pause_s)
    return last


def dismiss() -> None:
    try:
        eval_js(
            """
            (() => {
              const buttons = [...document.querySelectorAll('button, tp-yt-paper-button')];
              const btn = buttons.find(b => /accept all|reject all|i agree|^accept$|reject the use/i.test((b.innerText || b.getAttribute('aria-label') || '')));
              if (btn) btn.click();
              return btn ? 'ok' : '';
            })()
            """
        )
    except SystemExit:
        pass


def ensure_firefox() -> None:
    run("firefox", "--dir", DIR, "open", "https://www.youtube.com/")
    t0 = time.time()
    while time.time() - t0 < 30:
        try:
            href = eval_js("location.href")
            if "youtube.com" in href and wait_js(
                """
                (() => {
                  const el = document.querySelector('input#search, input[name=search_query], input[aria-label="Search"]');
                  if (!el) return '';
                  const r = el.getBoundingClientRect();
                  return r.width > 2 && r.height > 2 ? 'ok' : '';
                })()
                """,
                timeout=3,
                pause_s=0.4,
            ):
                return
        except SystemExit:
            pass
        time.sleep(0.6)
    raise SystemExit("youtube home never loaded")


def layout() -> None:
    xdo(
        """
        ff=$(xdotool search --onlyvisible --class firefox | head -1)
        if [ -z "$ff" ]; then
          echo 'no firefox window' >&2
          exit 1
        fi
        xdotool windowactivate --sync "$ff"
        xdotool windowmove "$ff" 0 28
        xdotool windowsize "$ff" 1280 772
        """
    )


def focus_firefox() -> None:
    xdo("id=$(xdotool search --onlyvisible --class firefox | head -1); xdotool windowactivate --sync $id")
    pause(0.3)


def click_sel(js_find: str, timeout: float = 20.0) -> bool:
    raw = wait_js(js_find, timeout=timeout, pause_s=0.6)
    print("target", raw[:300] if raw else "", flush=True)
    pt = parse_point(raw)
    if not pt:
        return False
    click_page(*pt)
    return True


def search_box_js() -> str:
    return """
        (() => {
          const el = document.querySelector('input#search, input[name=search_query], input[aria-label="Search"]');
          if (!el) return '';
          el.scrollIntoView({block: 'center', inline: 'nearest'});
          const r = el.getBoundingClientRect();
          if (r.width < 2 || r.height < 2) return '';
          return JSON.stringify({x: r.x + r.width / 2, y: r.y + r.height / 2});
        })()
    """


def channel_js() -> str:
    return """
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
    """


def videos_tab_js() -> str:
    return """
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
          return JSON.stringify({x: r.x + r.width / 2, y: r.y + r.height / 2});
        })()
    """


def latest_video_js() -> str:
    return """
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
    """


def on_channel() -> bool:
    try:
        raw = eval_js(
            """
            (() => {
              const href = location.href;
              const ok = /\\/@|\\/channel\\//.test(href) && !/\\/watch/.test(href);
              return ok ? href : '';
            })()
            """
        )
    except SystemExit:
        return False
    return bool(raw)


def wait_playing() -> bool:
    play_js = """
        (() => {
          const v = document.querySelector('video.html5-main-video') || document.querySelector('video');
          if (!v) return '';
          return JSON.stringify({href: location.href, t: v.currentTime||0, paused: !!v.paused, w: v.videoWidth||0});
        })()
    """
    t0 = time.time()
    while time.time() - t0 < 45:
        try:
            raw = eval_js(play_js)
        except SystemExit:
            raw = ""
        print("yt", raw, flush=True)
        if raw:
            try:
                st = json.loads(raw)
            except json.JSONDecodeError:
                st = {}
            if "/watch" in st.get("href", "") and not st.get("paused") and st.get("t", 0) >= 1.0 and st.get("w", 0) > 0:
                return True
        time.sleep(0.8)
    return False


def drive() -> None:
    layout()
    print(">> youtube home", flush=True)
    focus_firefox()
    pause(0.5)
    dismiss()
    print(">> search", QUERY, flush=True)
    if not click_sel(search_box_js(), timeout=12):
        raise SystemExit("no search box")
    pause(0.4)
    type_text(QUERY)
    pause(0.3)
    key("enter")
    pause(4)
    dismiss()
    print(">> channel home", flush=True)
    if not click_sel(channel_js(), timeout=20):
        raise SystemExit("no channel result")
    t0 = time.time()
    while time.time() - t0 < 15 and not on_channel():
        time.sleep(0.5)
    if not on_channel():
        raise SystemExit("channel page never loaded")
    pause(2.5)
    print(">> videos tab", flush=True)
    if not click_sel(videos_tab_js(), timeout=12):
        raise SystemExit("no videos tab")
    t1 = time.time()
    while time.time() - t1 < 12:
        try:
            if "/videos" in eval_js("location.pathname"):
                break
        except SystemExit:
            pass
        time.sleep(0.4)
    pause(3)
    print(">> latest video", flush=True)
    if not click_sel(latest_video_js(), timeout=20):
        raise SystemExit("no latest video")
    run("input", "--dir", DIR, "move 80 760")
    if not wait_playing():
        raise SystemExit("video never started playing")
    pause(10)
    print("done", flush=True)


def encode(frames_dir: Path, n: int) -> None:
    if n < 2:
        raise SystemExit("no frames")
    ff = find_ffmpeg()
    if OUT.exists():
        OUT.unlink()
    subprocess.run(
        [
            ff,
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-framerate",
            str(FPS),
            "-i",
            str(frames_dir / "%05d.png"),
            "-an",
            "-vf",
            "scale=trunc(iw/2)*2:trunc(ih/2)*2",
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
        check=True,
    )


def main() -> None:
    guest("pkill -f x11grab || true", check=False)
    ensure_firefox()
    layout()
    frames_dir = Path(tempfile.mkdtemp(prefix="vmagent-cu-"))
    stop = threading.Event()
    count = {"n": 0}

    def grab() -> None:
        while not stop.is_set():
            dest = frames_dir / f"{count['n']:05d}.png"
            r = subprocess.run(
                [VMAGENT, "screenshot", "--dir", DIR, "--out", str(dest)],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            if r.returncode == 0 and dest.exists() and dest.stat().st_size > 100:
                count["n"] += 1
            else:
                time.sleep(0.2)

    thread = threading.Thread(target=grab, daemon=True)
    thread.start()
    pause(0.8)
    try:
        drive()
    finally:
        stop.set()
        thread.join(timeout=8)
    encode(frames_dir, count["n"])
    shutil.rmtree(frames_dir, ignore_errors=True)
    if not OUT.exists() or OUT.stat().st_size < 1000:
        raise SystemExit("demo-computer-use.mp4 missing")
    print(f"wrote {OUT} ({OUT.stat().st_size} bytes, {count['n']} frames)", flush=True)


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        sys.exit(130)
