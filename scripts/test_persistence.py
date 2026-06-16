#!/usr/bin/env python3
"""Smoke: state persistence across a kerneld restart (TASK 3).

Verifies:
  1. A non-secret grant set via policy.set and unsafe_mode survive a restart
     (settings.json).
  2. A credential set via creds.set survives a restart — its NAME comes back
     from creds.list, while its VALUE is never returned and never appears in
     the daemon's tracing logs (sealed at rest).

Run standalone — it boots two short-lived kerneld processes against a temp
WEBOS_ROOT and kills them. Exits non-zero on any violation.
"""
import asyncio, json, os, signal, subprocess, sys, tempfile, time, uuid, urllib.request
import websockets

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(REPO, "target", "debug", "kerneld")
SECRET_VALUE = "sk-live-DO-NOT-LOG-7f3a9c2e1b"  # canary; must never hit logs/wire
SECRET_NAME = "TEST_API_TOKEN"
GRANT_CAP = "fs.write"  # defaults to "deny"; we flip it to "allow"


def boot(root, logfile, force_backend=None):
    env = dict(os.environ, WEBOS_ROOT=root, WEBOS_ADDR="127.0.0.1:8077")
    if force_backend:
        env["WEBOS_SECRETS_BACKEND"] = force_backend
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
            tok = json.loads(urllib.request.urlopen("http://127.0.0.1:8077/bootstrap", timeout=2).read())
            ws = await websockets.connect(f"ws://127.0.0.1:8077/ws?token={tok['human_token']}")
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


async def phase_write(root):
    ws = await connect()
    async with ws:
        r = await call(ws, "policy.set", {"capability": GRANT_CAP, "state": "allow"})
        assert r.get("ok"), f"policy.set failed: {r}"
        r = await call(ws, "policy.set_unsafe", {"on": True})
        assert r.get("ok"), f"policy.set_unsafe failed: {r}"
        r = await call(ws, "creds.set", {"name": SECRET_NAME, "value": SECRET_VALUE})
        assert r.get("ok"), f"creds.set failed: {r}"
        assert SECRET_VALUE not in json.dumps(r), "creds.set response echoed the secret value!"
        print("WROTE  grant fs.write=allow, unsafe_mode=true, cred", SECRET_NAME)


async def phase_verify(root):
    ws = await connect()
    async with ws:
        r = await call(ws, "policy.get")
        assert r.get("ok"), f"policy.get failed: {r}"
        data = r["data"]
        assert data["unsafe_mode"] is True, "unsafe_mode did NOT persist across restart"
        grant = next((g for g in data["grants"] if g["capability"] == GRANT_CAP), None)
        assert grant and grant["state"] == "allow", f"grant did NOT persist: {grant}"
        print("VERIFY grant fs.write=%s, unsafe_mode=%s (persisted)" % (grant["state"], data["unsafe_mode"]))

        r = await call(ws, "creds.list")
        assert r.get("ok"), f"creds.list failed: {r}"
        names = [c["name"] for c in r["data"]["credentials"]]
        assert SECRET_NAME in names, f"cred name did NOT persist: {names}"
        assert SECRET_VALUE not in json.dumps(r), "creds.list leaked the secret value!"
        print("VERIFY cred", SECRET_NAME, "name persisted; value NOT returned")


def scan_logs(*logfiles):
    leaked = False
    for lf in logfiles:
        with open(lf) as f:
            body = f.read()
        if SECRET_VALUE in body:
            print("FAIL  secret value found in", lf)
            leaked = True
    if not leaked:
        print("VERIFY secret value absent from all daemon logs")
    return not leaked


def scan_disk(root):
    """The secret value must not sit in plaintext anywhere in the jail."""
    ok = True
    for dirpath, _dirs, files in os.walk(root):
        for name in files:
            p = os.path.join(dirpath, name)
            try:
                with open(p, "rb") as f:
                    blob = f.read()
            except OSError:
                continue
            if SECRET_VALUE.encode() in blob:
                print("FAIL  secret value found in plaintext at", p)
                ok = False
    if ok:
        print("VERIFY secret value not in plaintext anywhere in the jail")
    return ok


def run_cycle(label, force_backend):
    """One write→restart→verify cycle in a fresh jail. Returns failure count."""
    print("\n=== cycle: %s ===" % label)
    root = tempfile.mkdtemp(prefix="webos-persist-")
    log1 = os.path.join(root, "boot1.log")
    log2 = os.path.join(root, "boot2.log")
    failures = 0
    try:
        p1, f1 = boot(root, log1, force_backend)
        try:
            asyncio.run(phase_write(root))
        finally:
            kill(p1, f1)
        time.sleep(0.3)

        with open(log1) as f:
            head = f.read()
        backend = "keychain" if "OS keychain" in head else ("sealed file" if "encrypted-file" in head else "?")
        print("BACKEND chosen:", backend)

        p2, f2 = boot(root, log2, force_backend)
        try:
            asyncio.run(phase_verify(root))
        finally:
            kill(p2, f2)

        if not scan_logs(log1, log2):
            failures += 1
        # The keychain backend stores values in the OS store, not the jail, so a
        # plaintext-on-disk scan is only meaningful for the sealed-file backend.
        if backend == "sealed file":
            if not scan_disk(root):
                failures += 1
        else:
            print("SKIP  disk plaintext scan (keychain backend keeps values in OS store)")
    except AssertionError as e:
        print("FAIL ", e)
        failures += 1
    except Exception as e:
        print("ERROR", repr(e))
        failures += 1
    return failures


def main():
    if not os.path.exists(BIN):
        print("FAIL  build kerneld first (cargo build)")
        sys.exit(1)
    failures = 0
    # Deterministic, prompt-free path first: the Pi/headless encrypted-file
    # backend. This is the one CI should rely on.
    failures += run_cycle("forced sealed-file backend", "file")
    # Then the auto backend (keychain on macOS dev; sealed file on a headless
    # Pi). On macOS an unsigned binary may trigger an interactive Keychain
    # authorization dialog; if it does and no one clicks, this cycle can time
    # out. That is an environmental prompt, not a persistence failure — set
    # WEBOS_SECRETS_BACKEND=file to skip the keychain entirely.
    if os.environ.get("WEBOS_SKIP_KEYCHAIN_CYCLE") != "1":
        failures += run_cycle("auto backend", None)

    if failures:
        print("\n%d check(s) FAILED" % failures)
        sys.exit(1)
    print("\nall persistence checks passed")


if __name__ == "__main__":
    main()
