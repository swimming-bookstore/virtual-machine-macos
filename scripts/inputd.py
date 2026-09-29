#!/usr/bin/env python3
"""Computer-use input for the guest. One uinput device, stdin commands.

Commands, one per line:
  key <name> [down|up]
  type <text>
  move <x> <y>
  button <left|right|middle> [down|up]
  scroll <n>          negative is down

Prints ok or an error line, then flushes. Keys are Linux KEY_* names
without the prefix (enter, a, leftctrl, f4).
"""

import ctypes
import os
import sys
import time

UINPUT = "/dev/uinput"
EV_SYN, EV_KEY, EV_REL, EV_ABS = 0, 1, 2, 3
SYN_REPORT, REL_WHEEL = 0, 8
ABS_X, ABS_Y = 0, 1
BTN_LEFT, BTN_RIGHT, BTN_MIDDLE = 0x110, 0x111, 0x112
UI_SET_EVBIT, UI_SET_KEYBIT, UI_SET_RELBIT, UI_SET_ABSBIT = (
    0x40045564,
    0x40045565,
    0x40045566,
    0x40045567,
)
UI_DEV_SETUP, UI_DEV_CREATE, UI_ABS_SETUP = 0x405C5503, 0x5501, 0x401C5504

WIDTH, HEIGHT = 1280, 800

# Names an agent actually types. Values are Linux KEY_* codes.
KEYS = {
    "esc": 1, "1": 2, "2": 3, "3": 4, "4": 5, "5": 6, "6": 7, "7": 8, "8": 9, "9": 10,
    "0": 11, "minus": 12, "equal": 13, "backspace": 14, "tab": 15, "q": 16, "w": 17,
    "e": 18, "r": 19, "t": 20, "y": 21, "u": 22, "i": 23, "o": 24, "p": 25,
    "leftbrace": 26, "rightbrace": 27, "enter": 28, "leftctrl": 29, "a": 30, "s": 31,
    "d": 32, "f": 33, "g": 34, "h": 35, "j": 36, "k": 37, "l": 38, "semicolon": 39,
    "apostrophe": 40, "grave": 41, "leftshift": 42, "backslash": 43, "z": 44, "x": 45,
    "c": 46, "v": 47, "b": 48, "n": 49, "m": 50, "comma": 51, "dot": 52, "slash": 53,
    "rightshift": 54, "leftalt": 56, "space": 57, "capslock": 58, "f1": 59, "f2": 60,
    "f3": 61, "f4": 62, "f5": 63, "f6": 64, "f7": 65, "f8": 66, "f9": 67, "f10": 68,
    "f11": 87, "f12": 88, "rightctrl": 97, "rightalt": 100, "up": 103, "left": 105,
    "right": 106, "down": 108, "delete": 111, "home": 102, "end": 107, "pageup": 104,
    "pagedown": 109,
}

# Character to (key, shift). Unlisted characters are rejected.
CHARS = {}
for c, k in zip("abcdefghijklmnopqrstuvwxyz", "abcdefghijklmnopqrstuvwxyz"):
    CHARS[c] = (k, False)
    CHARS[c.upper()] = (k, True)
for c, k in zip("0123456789", "0123456789"):
    CHARS[c] = (k, False)
CHARS.update({
    " ": ("space", False), "\n": ("enter", False), "\t": ("tab", False),
    "-": ("minus", False), "_": ("minus", True), "=": ("equal", False),
    "+": ("equal", True), "[": ("leftbrace", False), "{": ("leftbrace", True),
    "]": ("rightbrace", False), "}": ("rightbrace", True), "\\": ("backslash", False),
    "|": ("backslash", True), ";": ("semicolon", False), ":": ("semicolon", True),
    "'": ("apostrophe", False), '"': ("apostrophe", True), "`": ("grave", False),
    "~": ("grave", True), ",": ("comma", False), "<": ("comma", True),
    ".": ("dot", False), ">": ("dot", True), "/": ("slash", False),
    "?": ("slash", True), "!": ("1", True), "@": ("2", True), "#": ("3", True),
    "$": ("4", True), "%": ("5", True), "^": ("6", True), "&": ("7", True),
    "*": ("8", True), "(": ("9", True), ")": ("0", True),
})

BUTTONS = {"left": BTN_LEFT, "right": BTN_RIGHT, "middle": BTN_MIDDLE}


class Setup(ctypes.Structure):
    _fields_ = [
        ("bustype", ctypes.c_uint16),
        ("vendor", ctypes.c_uint16),
        ("product", ctypes.c_uint16),
        ("version", ctypes.c_uint16),
        ("name", ctypes.c_char * 80),
        ("ff_effects_max", ctypes.c_uint32),
    ]


class AbsSetup(ctypes.Structure):
    _fields_ = [
        ("code", ctypes.c_uint16),
        ("_pad", ctypes.c_uint16),
        ("value", ctypes.c_int32),
        ("minimum", ctypes.c_int32),
        ("maximum", ctypes.c_int32),
        ("fuzz", ctypes.c_int32),
        ("flat", ctypes.c_int32),
        ("resolution", ctypes.c_int32),
    ]


def ioctl(fd, req, arg=0):
    r = libc.ioctl(fd, req, arg)
    if r < 0:
        raise OSError(ctypes.get_errno(), os.strerror(ctypes.get_errno()))


libc = ctypes.CDLL(None, use_errno=True)


def open_device():
    fd = os.open(UINPUT, os.O_WRONLY | os.O_NONBLOCK)
    for bit in (EV_KEY, EV_REL, EV_ABS):
        ioctl(fd, UI_SET_EVBIT, bit)
    for code in KEYS.values():
        ioctl(fd, UI_SET_KEYBIT, code)
    for code in BUTTONS.values():
        ioctl(fd, UI_SET_KEYBIT, code)
    ioctl(fd, UI_SET_RELBIT, REL_WHEEL)
    for code in (ABS_X, ABS_Y):
        ioctl(fd, UI_SET_ABSBIT, code)
    setup = Setup(0x03, 1, 1, 1, b"vmagent", 0)
    ioctl(fd, UI_DEV_SETUP, ctypes.byref(setup))
    for code, maximum in ((ABS_X, WIDTH - 1), (ABS_Y, HEIGHT - 1)):
        abs_setup = AbsSetup(code, 0, 0, 0, maximum, 0, 0, 0)
        ioctl(fd, UI_ABS_SETUP, ctypes.byref(abs_setup))
    ioctl(fd, UI_DEV_CREATE)
    return fd


def emit(fd, kind, code, value):
    # input_event is timeval, type, code, value. SYN_REPORT is a second event.
    event = (0).to_bytes(16, "little")
    event += kind.to_bytes(2, "little") + code.to_bytes(2, "little")
    event += value.to_bytes(4, "little", signed=True)
    os.write(fd, event)
    if kind != EV_SYN:
        syn = (0).to_bytes(16, "little") + (0).to_bytes(8, "little")
        os.write(fd, syn)


def tap(fd, code):
    emit(fd, EV_KEY, code, 1)
    emit(fd, EV_KEY, code, 0)


def do_key(fd, parts):
    if len(parts) not in (2, 3):
        raise ValueError("key <name> [down|up]")
    name = parts[1].lower()
    if name not in KEYS:
        raise ValueError(f"unknown key {name}")
    action = parts[2] if len(parts) == 3 else "tap"
    if action == "tap":
        tap(fd, KEYS[name])
    elif action == "down":
        emit(fd, EV_KEY, KEYS[name], 1)
    elif action == "up":
        emit(fd, EV_KEY, KEYS[name], 0)
    else:
        raise ValueError("key action is down, up, or omitted")


def do_type(fd, text):
    for ch in text:
        if ch not in CHARS:
            raise ValueError(f"cannot type {ch!r}")
        key, shift = CHARS[ch]
        if shift:
            emit(fd, EV_KEY, KEYS["leftshift"], 1)
        tap(fd, KEYS[key])
        if shift:
            emit(fd, EV_KEY, KEYS["leftshift"], 0)


def do_move(fd, parts):
    if len(parts) != 3:
        raise ValueError("move <x> <y>")
    x, y = int(parts[1]), int(parts[2])
    if not (0 <= x < WIDTH and 0 <= y < HEIGHT):
        raise ValueError(f"pointer is {WIDTH}x{HEIGHT}")
    emit(fd, EV_ABS, ABS_X, x)
    emit(fd, EV_ABS, ABS_Y, y)


def do_button(fd, parts):
    if len(parts) not in (2, 3):
        raise ValueError("button <left|right|middle> [down|up]")
    name = parts[1].lower()
    if name not in BUTTONS:
        raise ValueError("button is left, right, or middle")
    action = parts[2] if len(parts) == 3 else "tap"
    if action == "tap":
        tap(fd, BUTTONS[name])
    elif action == "down":
        emit(fd, EV_KEY, BUTTONS[name], 1)
    elif action == "up":
        emit(fd, EV_KEY, BUTTONS[name], 0)
    else:
        raise ValueError("button action is down, up, or omitted")


def do_scroll(fd, parts):
    if len(parts) != 2:
        raise ValueError("scroll <n>")
    emit(fd, EV_REL, REL_WHEEL, int(parts[1]))


def handle(fd, line):
    parts = line.split()
    if not parts:
        return
    cmd = parts[0]
    if cmd == "key":
        do_key(fd, parts)
    elif cmd == "type":
        do_type(fd, line.split(" ", 1)[1] if " " in line else "")
    elif cmd == "move":
        do_move(fd, parts)
    elif cmd == "button":
        do_button(fd, parts)
    elif cmd == "scroll":
        do_scroll(fd, parts)
    else:
        raise ValueError(f"unknown command {cmd}")


def main():
    try:
        fd = open_device()
    except OSError as e:
        sys.stderr.write(f"cannot open {UINPUT}: {e}\n")
        return 1
    time.sleep(0.2)
    for line in sys.stdin:
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        try:
            handle(fd, line)
        except (ValueError, OSError) as e:
            sys.stdout.write(f"error: {e}\n")
            sys.stdout.flush()
            continue
        sys.stdout.write("ok\n")
        sys.stdout.flush()
    return 0


if __name__ == "__main__":
    sys.exit(main())
