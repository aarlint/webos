#!/usr/bin/env python3
"""Smoke: the MCP connector kind (TASK 4), stdio transport, end-to-end.

Hermetic/offline — drives a real kerneld against a stdlib mock MCP server
(scripts/mock_mcp_stdio.py) over a spawned stdio child. Verifies:

  1. connector.add (kind=mcp, stdio) accepts the transport block and adds 0 ops
     (tools are discovered live, not authored).
  2. connector.connect spawns the child, initializes, lists tools, and maps them
     to ops; describe() shows them.
  3. Class derivation is AUTHORITATIVE: get_greeting → read, create_thing →
     write, and get_but_destructive (read NAME, destructiveHint=true) → write.
  4. conn.call a read tool returns the tool result AND proves the per-connector
     secret was injected into the CHILD ENV (env_secret_present=true) — while the
     secret value never appears in argv, the connector file, or the daemon logs.
  5. A WRITE-class mcp op is NOT blanket-allowed by unsafe_mode (still gated).
  6. connector.disconnect tears the client down; conn.call then reports offline.
  7. connector.connect / disconnect / refresh_tools are operator-only: the AI
     principal is DENIED.

Exits non-zero on any violation.
"""
import asyncio, json, os, signal, subprocess, sys, tempfile, uuid, urllib.request
import websockets

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(REPO, "target", "debug", "kerneld")
MOCK = os.path.join(REPO, "scripts", "mock_mcp_stdio.py")
ADDR = "127.0.0.1:8079"
SECRET_NAME = "MOCK_MCP_TOKEN"
SECRET_VALUE = "mcp-secret-DO-NOT-LOG-91a2b3c4"  # canary
CID = "mcptest-mock"


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


async def connect_ws(which="human_token"):
    for _ in range(40):
        try:
            tok = json.loads(urllib.request.urlopen(f"http://{ADDR}/bootstrap", timeout=2).read())
            return await websockets.connect(f"ws://{ADDR}/ws?token={tok[which]}")
        except Exception:
            await asyncio.sleep(0.25)
    raise RuntimeError("kerneld never came up")


async def call(ws, capability, args=None, timeout=15):
    mid = str(uuid.uuid4())
    await ws.send(json.dumps({"id": mid, "capability": capability, "args": args or {}}))
    while True:
        m = json.loads(await asyncio.wait_for(ws.recv(), timeout=timeout))
        if m.get("id") == mid:
            return m
        # ignore server-pushed activity/approval frames


def fail(msg):
    print("FAIL ", msg)
    raise AssertionError(msg)


async def run(root, logfile):
    human = await connect_ws("human_token")
    ai = await connect_ws("ai_token")
    try:
        # ── seed the per-connector secret ──
        r = await call(human, "creds.set", {"name": SECRET_NAME, "value": SECRET_VALUE})
        assert r.get("ok"), f"creds.set: {r}"

        # ── 1. add the mcp/stdio connector ──
        r = await call(human, "connector.add", {
            "id": CID, "display_name": "Mock MCP", "kind": "mcp",
            "transport": {
                "kind": "stdio",
                "command": sys.executable,
                "args": [MOCK],
                "env_cred_refs": {SECRET_NAME: SECRET_NAME},
            },
        })
        assert r.get("ok"), f"connector.add: {r}"
        assert r["data"]["op_count"] == 0, f"mcp add should seed 0 ops: {r}"
        print("OK  add: mcp/stdio connector added with 0 ops (tools discovered on connect)")

        # ── 2. connect → discover + map tools ──
        r = await call(human, "connector.connect", {"id": CID}, timeout=30)
        assert r.get("ok"), f"connector.connect: {r}"
        assert r["data"]["tool_count"] == 3, f"expected 3 tools: {r}"
        assert r["data"]["server"] == "mock-mcp", f"server name: {r}"
        print("OK  connect: server=%s tools=%d" % (r["data"]["server"], r["data"]["tool_count"]))

        # ── 3. describe + authoritative class derivation ──
        r = await call(human, "connector.describe", {"id": CID})
        assert r.get("ok"), f"describe: {r}"
        ops = {o["op_id"]: o for o in r["data"]["ops"]}
        assert r["data"]["kind"] == "mcp", f"describe kind: {r}"
        for need in ("get_greeting", "get_but_destructive", "create_thing"):
            assert need in ops, f"missing op {need}: {list(ops)}"
        assert ops["get_greeting"]["class"] == "read", f"get_greeting should be read: {ops['get_greeting']}"
        assert ops["create_thing"]["class"] == "write", f"create_thing should be write: {ops['create_thing']}"
        assert ops["get_but_destructive"]["class"] == "write", \
            f"read-named + destructiveHint must be WRITE: {ops['get_but_destructive']}"
        print("OK  class derivation: get_greeting=read, create_thing=write, "
              "get_but_destructive=write (destructiveHint downgraded a read name)")

        # ── 4. conn.call a read tool (human=allow) + env-injection proof ──
        r = await call(human, "conn.call", {
            "connector": CID, "op": "get_greeting", "args": {"name": "webos"},
        })
        assert r.get("ok"), f"conn.call get_greeting: {r}"
        env = r["data"]
        assert env["_untrusted"] is True, f"mcp result must be flagged untrusted: {env}"
        # data.data is the serialized CallToolResult; pull its text content.
        content = env["data"]["content"]
        payload = json.loads(content[0]["text"])
        assert payload["echo_args"] == {"name": "webos"}, f"args not echoed: {payload}"
        assert payload["env_secret_present"] is True, "secret was NOT injected into the child env!"
        assert payload["env_secret_len"] == len(SECRET_VALUE), \
            f"injected secret length mismatch: {payload}"
        print("OK  conn.call get_greeting: args echoed, secret present in child env "
              "(len=%d) — value itself never returned" % payload["env_secret_len"])

        # ── 7. lifecycle verbs are operator-only (ai denied) ──
        for cap in ("connector.connect", "connector.disconnect", "connector.refresh_tools"):
            r = await call(ai, cap, {"id": CID})
            assert not r.get("ok") and r.get("decision") == "deny", f"{cap} must deny ai: {r}"
        print("OK  protected: ai DENIED connector.connect/disconnect/refresh_tools")

        # refresh on a live client (human)
        r = await call(human, "connector.refresh_tools", {"id": CID}, timeout=30)
        assert r.get("ok") and r["data"]["tool_count"] == 3, f"refresh: {r}"
        print("OK  refresh_tools: re-listed 3 tools on the live client")

        # ── 5. WRITE-class mcp op is NOT blanket-allowed by unsafe_mode ──
        # Turn unsafe_mode ON, then close the only human so an AI ASK has no
        # operator to approve. A READ op under unsafe_mode is auto-allowed; a
        # WRITE mcp op is held for ASK (is_write_conn) → "no operator online"
        # deny. That contrast proves write ops are NOT blanket-allowed.
        r = await call(human, "policy.set_unsafe", {"on": True})
        assert r.get("ok"), f"set_unsafe: {r}"
        await human.close()
        await asyncio.sleep(0.3)  # let the disconnect deregister the human session
        r = await call(ai, "conn.call", {"connector": CID, "op": "get_greeting", "args": {"name": "u"}})
        assert r.get("ok"), f"READ mcp op SHOULD be auto-allowed under unsafe_mode: {r}"
        r = await call(ai, "conn.call", {"connector": CID, "op": "create_thing", "args": {"label": "x"}})
        assert not r.get("ok") and r.get("decision") == "deny", \
            f"WRITE mcp op must NOT be blanket-allowed by unsafe_mode: {r}"
        assert "no operator" in (r.get("error") or ""), f"expected ASK→no-operator deny: {r}"
        print("OK  write-gating: under unsafe_mode read=allowed, write=held-for-ASK "
              "(no operator → deny) — write ops never blanket-allowed")

        # ── 6. disconnect → conn.call reports offline ──
        r = await call(ai, "conn.call", {"connector": CID, "op": "get_greeting", "args": {}})
        assert r.get("ok"), "sanity: read still served while connected"
        # reconnect a human to drive disconnect
        human2 = await connect_ws("human_token")
        try:
            r = await call(human2, "connector.disconnect", {"id": CID})
            assert r.get("ok"), f"disconnect: {r}"
            r = await call(human2, "conn.call", {"connector": CID, "op": "get_greeting", "args": {}})
            assert not r.get("ok") and "not connected" in (r.get("error") or ""), \
                f"conn.call after disconnect should fail offline: {r}"
            print("OK  disconnect: client torn down; conn.call reports offline")
        finally:
            await human2.close()
    finally:
        try:
            await human.close()
        except Exception:
            pass
        await ai.close()


def scan_artifacts(root, logfile):
    ok = True
    # 1. secret value must not appear in the daemon log
    with open(logfile) as f:
        log = f.read()
    if SECRET_VALUE in log:
        print("FAIL  secret value found in daemon log"); ok = False
    else:
        print("OK  secret value absent from daemon log")
    # 2. secret value must not appear in the persisted connector file (proves it
    #    was NOT baked into argv/env_cred_refs on disk — only a NAME ref is stored)
    cf = os.path.join(root, "connectors", CID + ".json")
    if os.path.exists(cf):
        with open(cf) as f:
            body = f.read()
        if SECRET_VALUE in body:
            print("FAIL  secret value found in connector file", cf); ok = False
        else:
            print("OK  secret value absent from connector file (only the cred NAME ref is stored)")
        # the connector file should carry the transport block + the mapped ops
        cdef = json.loads(body)
        assert cdef.get("kind") == "mcp" and cdef.get("transport"), "connector file missing mcp transport"
        assert len(cdef.get("ops", [])) == 3, f"connector file should persist 3 mapped ops: {len(cdef.get('ops', []))}"
        print("OK  connector file persisted transport + 3 mapped ops")
    return ok


def main():
    if not os.path.exists(BIN):
        print("FAIL  build kerneld first (cargo build)"); sys.exit(1)
    root = tempfile.mkdtemp(prefix="webos-mcp-")
    logfile = os.path.join(root, "kerneld.log")
    failures = 0
    p, f = boot(root, logfile)
    try:
        asyncio.run(run(root, logfile))
    except AssertionError as e:
        failures += 1
    except Exception as e:
        print("ERROR", repr(e)); failures += 1
    finally:
        kill(p, f)
    if not scan_artifacts(root, logfile):
        failures += 1
    if failures:
        print("\n%d check(s) FAILED" % failures); sys.exit(1)
    print("\nall mcp checks passed")


if __name__ == "__main__":
    main()
