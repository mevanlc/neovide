#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["pynvim>=0.5.2,<0.7"]
# ///
"""Prepare and inspect an isolated packaged-app fixture; operate its UI separately.

uv run devdocs/macos_emoji_probe.py launch
uv run devdocs/macos_emoji_probe.py reset --mode insert
# Open Character Viewer and select an emoji in the fixture app.
uv run devdocs/macos_emoji_probe.py snapshot
uv run devdocs/macos_emoji_probe.py verify --text '😀x' --commits 1 --mode i
uv run devdocs/macos_emoji_probe.py close
"""

import argparse
import json
import os
import plistlib
import shutil
import subprocess
import time
from pathlib import Path

import pynvim

ROOT = Path(__file__).resolve().parents[1]
FIXTURE = ROOT / "target/emoji-picker-test"
STATE = FIXTURE / "state.json"
BUNDLE_ID = "com.neovide.neovide.emoji-test"


def connect(socket):
    if not Path(socket).name.startswith("neovide-emoji-"):
        raise ValueError("Only sockets created for the emoji fixture are accepted")
    return pynvim.attach("socket", path=socket)


def snapshot(nvim):
    return nvim.exec_lua("""
        return {
            mode = vim.api.nvim_get_mode().mode,
            lines = vim.api.nvim_buf_get_lines(0, 0, -1, false),
            cursor = vim.api.nvim_win_get_cursor(0),
            cmdline = vim.fn.getcmdline(),
            ime = vim.g.neovide_input_ime,
            commits = _G.emoji_probe_commits or {},
            pid = vim.fn.getpid(),
            server = vim.v.servername,
        }
    """)


def reset(nvim, mode, text):
    nvim.input("<C-\\><C-n><Esc>")
    nvim.exec_lua(
        """
        vim.g.neovide_input_ime = true
        vim.g.neovide_remember_window_size = false
        vim.g.neovide_remember_window_position = false
        if not _G.emoji_probe_original_commit then
            _G.emoji_probe_original_commit = neovide.commit_handler
            neovide.commit_handler = function(raw, formatted)
                table.insert(_G.emoji_probe_commits, {
                    raw = raw, formatted = formatted, mode = vim.api.nvim_get_mode().mode,
                })
                return _G.emoji_probe_original_commit(raw, formatted)
            end
        end
        _G.emoji_probe_commits = {}
        vim.api.nvim_set_current_buf(vim.api.nvim_create_buf(true, true))
        vim.api.nvim_buf_set_lines(0, 0, -1, false, vim.split(..., '\\n', { plain = true }))
        vim.api.nvim_win_set_cursor(0, { 1, 0 })
    """,
        text,
    )
    if mode == "insert":
        nvim.input("i")
    elif mode == "cmdline":
        nvim.input(":")
    elif mode == "terminal":
        nvim.command("terminal /bin/cat")
        nvim.command("startinsert")
    nvim.command("NeovideFocus")


def launch(source, native_tabs):
    if STATE.exists():
        previous = json.loads(STATE.read_text())
        try:
            existing = connect(previous["socket"])
        except (OSError, EOFError):
            pass
        else:
            existing.close()
            raise RuntimeError(
                "The emoji fixture is running; close it before relaunching"
            )
    FIXTURE.mkdir(parents=True, exist_ok=True)
    app = FIXTURE / "Neovide Emoji Test.app"
    shutil.copytree(source, app, dirs_exist_ok=True)
    info_file = app / "Contents/Info.plist"
    info = plistlib.loads(info_file.read_bytes())
    info.update(
        CFBundleIdentifier=BUNDLE_ID,
        CFBundleName="Neovide Emoji Test",
        CFBundleDisplayName="Neovide Emoji Test",
    )
    info_file.write_bytes(plistlib.dumps(info))
    subprocess.run(
        ["codesign", "--force", "--deep", "--sign", "-", str(app)], check=True
    )
    config = FIXTURE / "config.toml"
    config.write_text("# Isolated native emoji picker fixture.\n")
    socket = f"/tmp/neovide-emoji-{os.getpid()}.sock"
    STATE.write_text(
        json.dumps({"app": str(app), "bundle_id": BUNDLE_ID, "socket": socket})
    )
    command = [
        "/usr/bin/open",
        "-n",
        str(app),
        "--env",
        f"NEOVIDE_CONFIG={config}",
        "--args",
        "--no-fork",
        "--neovim-bin",
        shutil.which("nvim"),
        "--system-native-tabs" if native_tabs else "--no-system-native-tabs",
        "--",
        "--clean",
        "--listen",
        socket,
        "--cmd",
        "let g:neovide_remember_window_size=v:false | let g:neovide_remember_window_position=v:false",
    ]
    subprocess.run(command, cwd=FIXTURE, check=True)
    deadline = time.monotonic() + 20
    while True:
        try:
            nvim = connect(socket)
            if nvim.eval("exists('g:neovide_channel_id')"):
                break
            nvim.close()
        except (OSError, EOFError):
            pass
        if time.monotonic() >= deadline:
            raise TimeoutError(f"Fixture did not start at {socket}")
        time.sleep(0.1)
    try:
        reset(nvim, "insert", "")
        print(
            json.dumps(
                {
                    "app": str(app),
                    "bundle_id": BUNDLE_ID,
                    "socket": socket,
                    "state": snapshot(nvim),
                },
                ensure_ascii=False,
            )
        )
    finally:
        nvim.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "action", choices=["launch", "reset", "snapshot", "verify", "close"]
    )
    parser.add_argument(
        "--app", type=Path, default=ROOT / "target/release/bundle/osx/Neovide.app"
    )
    parser.add_argument("--socket", help="Override with another emoji fixture socket")
    parser.add_argument("--native-tabs", action="store_true")
    parser.add_argument("--mode", default="insert")
    parser.add_argument("--text", default="")
    parser.add_argument("--commits", type=int)
    args = parser.parse_args()
    if args.action == "launch":
        launch(args.app, args.native_tabs)
        return
    socket = args.socket or json.loads(STATE.read_text())["socket"]
    nvim = connect(socket)
    try:
        if args.action == "close":
            try:
                nvim.command("qa!")
            except EOFError:
                pass
            return
        if args.action == "reset":
            if args.mode not in ("insert", "normal", "cmdline", "terminal"):
                parser.error("reset mode must be insert, normal, cmdline, or terminal")
            reset(nvim, args.mode, args.text)
        state = snapshot(nvim)
        print(json.dumps(state, ensure_ascii=False))
        if args.action == "verify":
            actual = (
                state["cmdline"] if state["mode"] == "c" else "\n".join(state["lines"])
            )
            assert actual == args.text, (actual, args.text)
            assert state["mode"] == args.mode, (state["mode"], args.mode)
            if args.commits is not None:
                assert len(state["commits"]) == args.commits, state["commits"]
    finally:
        nvim.close()


if __name__ == "__main__":
    main()
