#!/usr/bin/env python3
"""Smoke: ui.patch operates on the FLAT json-render surface shape ({root,elements}).

Boots one short-lived kerneld against a temp WEBOS_ROOT (sealed-file secrets
backend so there's no Keychain prompt), then:
  1. ui.render a flat surface with two elements.
  2. ui.patch via `set` (props-only deep-merge) and re-read — props merged,
     untouched props preserved, id/title/root intact.
  3. ui.patch via `elements` (partial element merge + brand-new element insert)
     and re-read.
  4. ui.patch `root`/`title` retarget and re-read.
  5. ui.patch a LEGACY {widget} surface — must replace widget, not crash.
  6. ui.patch with no recognized field → error (not a panic).

Kills the daemon at the end. Exits non-zero on any violation.
"""
import asyncio, json, os, signal, subprocess, sys, tempfile, time, uuid, urllib.request
import websockets

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(REPO, "target", "debug", "kerneld")
ADDR = "127.0.0.1:8078"


def boot(root, logfile):
    env = dict(os.environ, WEBOS_ROOT=root, WEBOS_ADDR=ADDR, WEBOS_SECRETS_BACKEND="file")
    f = open(logfile, "w")
    p = subprocess.Popen([BIN], cwd=REPO, env=env, stdout=f, stderr=subprocess.STDOUT)
    return p, f


def kill(p, f):
    try:
        p.send_signal(signal.SIGINT)
        p.wait(timeout=5)
    except Exception:
        p.kill()
    finally:
        f.close()


async def connect():
    for _ in range(40):
        try:
            tok = json.loads(urllib.request.urlopen(f"http://{ADDR}/bootstrap", timeout=2).read())
            ws = await websockets.connect(f"ws://{ADDR}/ws?token={tok['human_token']}")
            return ws
        except Exception:
            await asyncio.sleep(0.25)
    raise RuntimeError("kerneld never came up")


async def call(ws, capability, args=None):
    mid = str(uuid.uuid4())
    await ws.send(json.dumps({"id": mid, "capability": capability, "args": args or {}}))
    for _ in range(40):
        m = json.loads(await asyncio.wait_for(ws.recv(), timeout=10))
        if m.get("id") == mid:
            return m
    raise RuntimeError("no response for " + capability)


FLAT = {
    "id": "patchme", "title": "Patch Me", "root": "stack",
    "elements": {
        "stack": {"type": "Stack", "props": {}, "children": ["head", "txt"]},
        "head": {"type": "Heading", "props": {"value": "Original", "level": 1}, "children": []},
        "txt": {"type": "Text", "props": {"value": "body"}, "children": []},
    },
}

LEGACY = {
    "id": "legacy", "title": "Legacy",
    "widget": {"type": "stack", "children": [{"type": "text", "value": "old"}]},
}


async def run():
    ws = await connect()
    failures = 0
    async with ws:
        # 0. render the flat surface
        r = await call(ws, "ui.render", {"surface": FLAT})
        assert r.get("ok"), f"ui.render failed: {r}"

        # 1. set: deep-merge props.value on head, leaving props.level intact
        r = await call(ws, "ui.patch", {"id": "patchme", "set": {"head": {"value": "Patched"}}})
        assert r.get("ok"), f"ui.patch set failed: {r}"
        r = await call(ws, "ui.get", {"id": "patchme"})
        s = r["data"]
        head = s["elements"]["head"]["props"]
        try:
            assert head["value"] == "Patched", f"set did not update value: {head}"
            assert head["level"] == 1, f"set clobbered untouched prop 'level': {head}"
            assert s["id"] == "patchme" and s["title"] == "Patch Me", "id/title not preserved"
            assert s["root"] == "stack", "root not preserved"
            assert s["elements"]["txt"]["props"]["value"] == "body", "sibling element changed"
            print("PASS  set deep-merges props, preserves siblings + id/title/root")
        except AssertionError as e:
            failures += 1; print("FAIL ", e)

        # 2. elements: partial merge on existing 'txt' + insert a brand-new element
        r = await call(ws, "ui.patch", {"id": "patchme", "elements": {
            "txt": {"props": {"value": "new body"}},
            "footer": {"type": "Text", "props": {"value": "footer"}, "children": []},
        }})
        assert r.get("ok"), f"ui.patch elements failed: {r}"
        r = await call(ws, "ui.get", {"id": "patchme"})
        s = r["data"]
        try:
            assert s["elements"]["txt"]["props"]["value"] == "new body", "elements merge missed value"
            assert s["elements"]["txt"]["type"] == "Text", "elements merge dropped type"
            assert s["elements"]["footer"]["props"]["value"] == "footer", "new element not inserted"
            assert s["elements"]["head"]["props"]["value"] == "Patched", "prior patch lost"
            print("PASS  elements merges existing + inserts new element")
        except AssertionError as e:
            failures += 1; print("FAIL ", e)

        # 3. root/title retarget
        r = await call(ws, "ui.patch", {"id": "patchme", "root": "footer", "title": "Renamed"})
        assert r.get("ok"), f"ui.patch root/title failed: {r}"
        r = await call(ws, "ui.get", {"id": "patchme"})
        s = r["data"]
        try:
            assert s["root"] == "footer", f"root not retargeted: {s.get('root')}"
            assert s["title"] == "Renamed", f"title not updated: {s.get('title')}"
            print("PASS  root/title retarget")
        except AssertionError as e:
            failures += 1; print("FAIL ", e)

        # 4. legacy {widget} backward-tolerance — must not crash
        r = await call(ws, "ui.render", {"surface": LEGACY})
        assert r.get("ok"), f"ui.render legacy failed: {r}"
        r = await call(ws, "ui.patch", {"id": "legacy", "widget": {
            "type": "stack", "children": [{"type": "text", "value": "replaced"}]}})
        assert r.get("ok"), f"ui.patch legacy widget failed (crash?): {r}"
        r = await call(ws, "ui.get", {"id": "legacy"})
        s = r["data"]
        try:
            assert s["widget"]["children"][0]["value"] == "replaced", "legacy widget not replaced"
            assert s["title"] == "Legacy", "legacy title not preserved"
            print("PASS  legacy {widget} patch replaces widget, no crash")
        except AssertionError as e:
            failures += 1; print("FAIL ", e)

        # 5. empty patch → error, not panic
        r = await call(ws, "ui.patch", {"id": "patchme"})
        try:
            assert not r.get("ok"), "empty patch should error"
            print("PASS  empty patch returns error, not panic:", r.get("error"))
        except AssertionError as e:
            failures += 1; print("FAIL ", e)

        # 6. patch a missing surface → error
        r = await call(ws, "ui.patch", {"id": "nope", "set": {"x": {}}})
        try:
            assert not r.get("ok"), "patch of missing surface should error"
            print("PASS  patch of missing surface returns error")
        except AssertionError as e:
            failures += 1; print("FAIL ", e)

    return failures


def main():
    if not os.path.exists(BIN):
        print("FAIL  build kerneld first (cargo build)"); sys.exit(1)
    root = tempfile.mkdtemp(prefix="webos-uipatch-")
    log = os.path.join(root, "boot.log")
    p, f = boot(root, log)
    failures = 1
    try:
        failures = asyncio.run(run())
    except Exception as e:
        print("ERROR", repr(e))
        try:
            with open(log) as lf:
                print(lf.read()[-1500:])
        except OSError:
            pass
        failures = 1
    finally:
        kill(p, f)
    if failures:
        print(f"\n{failures} check(s) FAILED"); sys.exit(1)
    print("\nall ui.patch checks passed")


if __name__ == "__main__":
    main()
