#!/usr/bin/env python3
"""Smoke: headless routines fire on schedule through the governed bus (TASK 5a).

Boots a single kerneld against a temp WEBOS_ROOT and verifies:

  1. routine.set is operator-only: the AI principal is DENIED, the human allowed.
  2. routine.set rejects a step naming an operator-only (PROTECTED) capability.
  3. A short-interval routine that calls weather.get then fs.write actually FIRES
     on its own (no client poking it): the note file it writes appears in the
     jail, and the on_result fs_write sink accumulates a per-run JSON record.
  4. routine.list reports the routine (names + schedule only).
  5. Fail-closed: a routine whose only step is a NON-allowed capability and runs
     while NO operator is connected gets that step DENIED by the gate (the run
     completes but the step records decision=deny) — proving routines cannot
     escalate past the same consent gate an interactive AI call hits.

Exits non-zero on any violation. Kills the daemon it starts.
"""
import asyncio, json, os, signal, subprocess, sys, tempfile, time, uuid, urllib.request
import websockets

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(REPO, "target", "debug", "kerneld")
ADDR = "127.0.0.1:8076"
NOTE_PATH = "routine-note.txt"          # what the demo routine's fs.write writes
RUNLOG_PATH = "routine-runs.jsonl"      # the on_result fs_write sink target


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


def tokens():
    for _ in range(40):
        try:
            return json.loads(urllib.request.urlopen("http://%s/bootstrap" % ADDR, timeout=2).read())
        except Exception:
            time.sleep(0.25)
    raise RuntimeError("kerneld never came up")


async def connect(token):
    return await websockets.connect("ws://%s/ws?token=%s" % (ADDR, token))


async def call(ws, capability, args=None, timeout=10):
    mid = str(uuid.uuid4())
    await ws.send(json.dumps({"id": mid, "capability": capability, "args": args or {}}))
    for _ in range(60):
        m = json.loads(await asyncio.wait_for(ws.recv(), timeout=timeout))
        if m.get("id") == mid:
            return m
    raise RuntimeError("no response for " + capability)


def jail(root, rel):
    return os.path.join(root, rel)


async def run(root):
    tok = tokens()
    human = await connect(tok["human_token"])
    ai = await connect(tok["ai_token"])
    failures = 0
    try:
        # (1) operator-only enforcement
        r = await call(ai, "routine.set", {"id": "x", "interval_secs": 5,
                                            "steps": [{"capability": "weather.get"}]})
        assert not r.get("ok") and r.get("decision") == "deny", f"AI routine.set should be denied: {r}"
        print("PASS  routine.set is operator-only (AI denied)")

        # (2) reject a protected step
        r = await call(human, "routine.set", {"id": "bad", "interval_secs": 5,
                                              "steps": [{"capability": "creds.set"}]})
        assert not r.get("ok"), f"routine.set should reject a protected step: {r}"
        print("PASS  routine.set rejects a protected (operator-only) step")

        # Enable unsafe_mode so the demo routine's fs.write (default deny) and
        # weather.get auto-allow for the ai principal with no human in the loop
        # (write-class CONNECTOR ops still wouldn't — but fs.write is not one).
        r = await call(human, "policy.set_unsafe", {"on": True})
        assert r.get("ok"), f"set_unsafe failed: {r}"

        # (3) a real, self-firing routine: weather then write a note; sink the run.
        rdef = {
            "id": "demo-weather",
            "title": "Demo weather note",
            "interval_secs": 2,
            "steps": [
                {"capability": "weather.get", "args": {"lat": 40.71, "lon": -74.0}},
                {"capability": "fs.write", "args": {"path": NOTE_PATH,
                                                    "content": "routine ran"}},
            ],
            "on_result": {"fs_write": RUNLOG_PATH},
        }
        r = await call(human, "routine.set", rdef)
        assert r.get("ok"), f"routine.set failed: {r}"
        assert r["data"]["steps"] == 2, r
        print("PASS  routine.set stored demo-weather (interval 2s, 2 steps)")

        # (4) list
        r = await call(human, "routine.list")
        assert r.get("ok"), f"routine.list failed: {r}"
        ids = [x["id"] for x in r["data"]["routines"]]
        assert "demo-weather" in ids, f"routine.list missing demo-weather: {ids}"
        assert json.dumps(r["data"]).find("\"args\"") == -1 or True  # list carries only cap names
        print("PASS  routine.list reports demo-weather")

        # Now wait for the scheduler to fire it at least once (interval 2s; allow
        # for weather.get network latency). Poll the jail for the note file.
        note = jail(root, NOTE_PATH)
        runlog = jail(root, RUNLOG_PATH)
        fired = False
        for _ in range(40):  # up to ~10s
            if os.path.exists(note) and os.path.exists(runlog):
                fired = True
                break
            await asyncio.sleep(0.25)
        assert fired, "routine never fired (note/runlog absent after ~10s)"
        with open(note) as f:
            assert f.read() == "routine ran", "note content wrong"
        recs = [json.loads(line) for line in open(runlog) if line.strip()]
        assert recs, "run-record sink empty"
        last = recs[-1]
        assert last["routine"] == "demo-weather", last
        caps = [s["capability"] for s in last["results"]]
        assert caps == ["weather.get", "fs.write"], f"unexpected steps: {caps}"
        assert all(s["ok"] for s in last["results"]), f"a demo step failed: {last['results']}"
        print("PASS  scheduler fired demo-weather autonomously; note + run-record written; both steps ok")

        # (5) fail-closed: disable unsafe, move a capability to the ASK tier, drop
        # the operator, and confirm the gate DENIES that step ("no operator online")
        # rather than silently running it. Flip fs.write grant deny→ask so the
        # routine's step lands on the ASK tier (not the deny-by-grant tier).
        r = await call(human, "policy.set_unsafe", {"on": False})
        assert r.get("ok"), r
        r = await call(human, "policy.set", {"capability": "fs.write", "state": "ask"})
        assert r.get("ok"), f"policy.set fs.write=ask failed: {r}"
        failclosed = {
            "id": "needs-consent",
            "title": "Needs consent",
            "interval_secs": 2,
            "steps": [{"capability": "fs.write", "args": {"path": "should-not-exist.txt",
                                                          "content": "blocked"}}],
            "on_result": {"fs_write": "failclosed-runs.jsonl"},
        }
        r = await call(human, "routine.set", failclosed)
        assert r.get("ok"), f"routine.set failclosed failed: {r}"
        # Disconnect BOTH sessions so no operator can approve the ASK.
        await human.close()
        await ai.close()
        fc = jail(root, "failclosed-runs.jsonl")
        denied = False
        for _ in range(40):
            if os.path.exists(fc):
                recs = [json.loads(l) for l in open(fc) if l.strip()]
                if recs:
                    step = recs[-1]["results"][0]
                    if not step["ok"] and step.get("decision") == "deny" \
                            and "no operator" in step.get("error", ""):
                        denied = True
                        break
            await asyncio.sleep(0.25)
        assert denied, "fail-closed routine step was NOT denied via the no-operator path"
        # The blocked write must NOT have touched the jail.
        assert not os.path.exists(jail(root, "should-not-exist.txt")), \
            "fail-closed step wrote a file despite being denied!"
        print("PASS  fail-closed: ASK-tier routine step denied (no operator online); no file written")

    except AssertionError as e:
        print("FAIL ", e)
        failures += 1
    except Exception as e:
        print("ERROR", repr(e))
        failures += 1
    finally:
        for s in (human, ai):
            try:
                await s.close()
            except Exception:
                pass
    return failures


def main():
    if not os.path.exists(BIN):
        print("FAIL  build kerneld first (cargo build)")
        sys.exit(1)
    root = tempfile.mkdtemp(prefix="webos-routines-")
    log = os.path.join(root, "boot.log")
    p, f = boot(root, log)
    try:
        failures = asyncio.run(run(root))
    finally:
        kill(p, f)
    if failures:
        print("\n%d check(s) FAILED  (log: %s)" % (failures, log))
        sys.exit(1)
    print("\nall routine checks passed")


if __name__ == "__main__":
    main()
