#!/usr/bin/env python3
"""Smoke: the MCP connector kind over the HTTP (streamable-http) transport
(TASK 3), end-to-end and hermetic/offline.

Drives a real kerneld against a stdlib mock streamable-http MCP server
(scripts/mock_mcp_http.py). Because the egress floor refuses plaintext/local
endpoints in production, the daemon is booted with the OFF-by-default dev flag
WEBOS_ALLOW_LOCAL_MCP=1 so the http transport can be exercised against the mock.

Verifies:

  1. connector.add (kind=mcp, http) accepts the transport block, 0 ops seeded.
  2. connector.connect builds the rmcp streamable-http client against the mock
     url, runs initialize + list_tools, and maps tools -> ops; describe shows them.
  3. Class derivation is AUTHORITATIVE over http too: get_greeting -> read,
     create_thing -> write, get_but_destructive (destructiveHint) -> write.
  4. conn.call proxies to the live http client; the result proves the
     Authorization: Bearer <token> from auth_cred_ref reached the server
     (auth_present=true, auth_len matches) — value never returned.
  5. connector.disconnect tears the client down; conn.call then reports offline.
  6. PRODUCTION-SAFETY: with the dev flag OFF, the SAME local http url is
     rejected at connect time by the egress floor (a separate short-lived daemon).
  7. The token VALUE never appears in the connector file or the daemon log.

Exits non-zero on any violation.
"""
import asyncio, json, os, signal, socket, subprocess, sys, tempfile, uuid, urllib.request
import websockets

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(REPO, "target", "debug", "kerneld")
MOCK = os.path.join(REPO, "scripts", "mock_mcp_http.py")
SECRET_NAME = "LINEAR_TOKEN"
SECRET_VALUE = "lin_api_DO-NOT-LOG-7f3a9c2e1b8d"  # canary
CID = "mcphttp-mock"


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def boot(root, addr, logfile, allow_local=True):
    env = dict(os.environ, WEBOS_ROOT=root, WEBOS_ADDR=addr, WEBOS_SECRETS_BACKEND="file")
    if allow_local:
        env["WEBOS_ALLOW_LOCAL_MCP"] = "1"
    else:
        env.pop("WEBOS_ALLOW_LOCAL_MCP", None)
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


def start_mock():
    p = subprocess.Popen([sys.executable, MOCK, "0"], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    port = int(p.stdout.readline().decode().strip())  # mock prints its bound port
    return p, port


async def connect_ws(addr, which="human_token"):
    for _ in range(40):
        try:
            tok = json.loads(urllib.request.urlopen(f"http://{addr}/bootstrap", timeout=2).read())
            return await websockets.connect(f"ws://{addr}/ws?token={tok[which]}")
        except Exception:
            await asyncio.sleep(0.25)
    raise RuntimeError("kerneld never came up")


async def call(ws, capability, args=None, timeout=20):
    mid = str(uuid.uuid4())
    await ws.send(json.dumps({"id": mid, "capability": capability, "args": args or {}}))
    while True:
        m = json.loads(await asyncio.wait_for(ws.recv(), timeout=timeout))
        if m.get("id") == mid:
            return m
        # ignore server-pushed activity/approval frames


async def run(addr, url):
    human = await connect_ws(addr, "human_token")
    try:
        # seed the Linear-style token under a NAME
        r = await call(human, "creds.set", {"name": SECRET_NAME, "value": SECRET_VALUE})
        assert r.get("ok"), f"creds.set: {r}"

        # 1. add the mcp/http connector
        r = await call(human, "connector.add", {
            "id": CID, "display_name": "Mock MCP HTTP", "kind": "mcp",
            "transport": {"kind": "http", "url": url, "auth_cred_ref": SECRET_NAME},
        })
        assert r.get("ok"), f"connector.add: {r}"
        assert r["data"]["op_count"] == 0, f"mcp add should seed 0 ops: {r}"
        print("OK  add: mcp/http connector added with 0 ops (tools discovered on connect)")

        # 2. connect -> initialize + list_tools -> ops
        r = await call(human, "connector.connect", {"id": CID}, timeout=30)
        assert r.get("ok"), f"connector.connect: {r}"
        assert r["data"]["tool_count"] == 3, f"expected 3 tools: {r}"
        assert r["data"]["server"] == "mock-mcp-http", f"server name: {r}"
        print("OK  connect: streamable-http client up, server=%s tools=%d"
              % (r["data"]["server"], r["data"]["tool_count"]))

        # 3. describe + authoritative class derivation
        r = await call(human, "connector.describe", {"id": CID})
        assert r.get("ok"), f"describe: {r}"
        ops = {o["op_id"]: o for o in r["data"]["ops"]}
        assert r["data"]["kind"] == "mcp", f"describe kind: {r}"
        assert ops["get_greeting"]["class"] == "read", f"get_greeting read: {ops['get_greeting']}"
        assert ops["create_thing"]["class"] == "write", f"create_thing write: {ops['create_thing']}"
        assert ops["get_but_destructive"]["class"] == "write", \
            f"read-named + destructiveHint must be WRITE: {ops['get_but_destructive']}"
        # host label should be mcp:http:127.0.0.1
        assert r["data"]["host"].startswith("mcp:http:"), f"host label: {r['data']['host']}"
        print("OK  describe: class derivation correct over http; host=%s" % r["data"]["host"])

        # 4. conn.call -> proxies to http client; Bearer token reached the server
        r = await call(human, "conn.call", {"connector": CID, "op": "get_greeting", "args": {"name": "webos"}})
        assert r.get("ok"), f"conn.call get_greeting: {r}"
        env = r["data"]
        assert env["_untrusted"] is True, f"mcp result must be flagged untrusted: {env}"
        payload = json.loads(env["data"]["content"][0]["text"])
        assert payload["echo_args"] == {"name": "webos"}, f"args not echoed: {payload}"
        assert payload["transport"] == "http", f"wrong transport: {payload}"
        assert payload["auth_present"] is True, "Bearer token did NOT reach the server!"
        assert payload["auth_len"] == len(SECRET_VALUE), f"token length mismatch: {payload}"
        print("OK  conn.call: proxied over http; Authorization: Bearer reached server "
              "(len=%d) — value never returned" % payload["auth_len"])

        # 5. disconnect -> offline
        r = await call(human, "connector.disconnect", {"id": CID})
        assert r.get("ok"), f"disconnect: {r}"
        r = await call(human, "conn.call", {"connector": CID, "op": "get_greeting", "args": {}})
        assert not r.get("ok") and "not connected" in (r.get("error") or ""), \
            f"conn.call after disconnect should fail offline: {r}"
        print("OK  disconnect: http client torn down; conn.call reports offline")
    finally:
        await human.close()


async def run_no_flag(addr, url):
    """With the dev flag OFF, the SAME local http url must be refused at connect."""
    human = await connect_ws(addr, "human_token")
    try:
        await call(human, "creds.set", {"name": SECRET_NAME, "value": SECRET_VALUE})
        # add may reject http outright (flag off) — that's also a valid refusal.
        r = await call(human, "connector.add", {
            "id": CID, "display_name": "Mock MCP HTTP", "kind": "mcp",
            "transport": {"kind": "http", "url": url, "auth_cred_ref": SECRET_NAME},
        })
        if not r.get("ok"):
            assert "https" in (r.get("error") or ""), f"unexpected add error: {r}"
            print("OK  production-safety: flag OFF -> local http url rejected at connector.add")
            return
        # if add tolerated it, connect must refuse via the egress floor.
        r = await call(human, "connector.connect", {"id": CID}, timeout=20)
        assert not r.get("ok"), f"connect should be refused with flag OFF: {r}"
        err = r.get("error") or ""
        assert ("https" in err) or ("SSRF" in err) or ("non-public" in err), f"unexpected connect error: {r}"
        print("OK  production-safety: flag OFF -> local http endpoint refused at connect (%s)" % err.split('(')[0].strip())
    finally:
        await human.close()


def scan_artifacts(root, logfile):
    ok = True
    with open(logfile) as f:
        log = f.read()
    if SECRET_VALUE in log:
        print("FAIL  token value found in daemon log"); ok = False
    else:
        print("OK  token value absent from daemon log")
    cf = os.path.join(root, "connectors", CID + ".json")
    if os.path.exists(cf):
        with open(cf) as f:
            body = f.read()
        if SECRET_VALUE in body:
            print("FAIL  token value found in connector file", cf); ok = False
        else:
            print("OK  token value absent from connector file (only the cred NAME ref is stored)")
        cdef = json.loads(body)
        assert cdef.get("kind") == "mcp" and cdef.get("transport", {}).get("kind") == "http", \
            "connector file missing mcp/http transport"
        assert cdef["transport"].get("auth_cred_ref") == SECRET_NAME, "auth_cred_ref not persisted as a NAME"
        assert len(cdef.get("ops", [])) == 3, f"connector file should persist 3 mapped ops: {len(cdef.get('ops', []))}"
        print("OK  connector file persisted http transport (auth_cred_ref=NAME) + 3 mapped ops")
    return ok


def main():
    if not os.path.exists(BIN):
        print("FAIL  build kerneld first (cargo build)"); sys.exit(1)

    failures = 0
    mock, port = start_mock()
    url = f"http://127.0.0.1:{port}/mcp"

    # ── main path: flag ON ──
    root = tempfile.mkdtemp(prefix="webos-mcphttp-")
    logfile = os.path.join(root, "kerneld.log")
    addr = "127.0.0.1:%d" % free_port()
    p, f = boot(root, addr, logfile, allow_local=True)
    try:
        asyncio.run(run(addr, url))
    except AssertionError:
        failures += 1
    except Exception as e:
        print("ERROR", repr(e)); failures += 1
    finally:
        kill(p, f)
    if not scan_artifacts(root, logfile):
        failures += 1

    # ── production-safety path: flag OFF ──
    root2 = tempfile.mkdtemp(prefix="webos-mcphttp-noflag-")
    logfile2 = os.path.join(root2, "kerneld.log")
    addr2 = "127.0.0.1:%d" % free_port()
    p2, f2 = boot(root2, addr2, logfile2, allow_local=False)
    try:
        asyncio.run(run_no_flag(addr2, url))
    except AssertionError:
        failures += 1
    except Exception as e:
        print("ERROR (no-flag)", repr(e)); failures += 1
    finally:
        kill(p2, f2)

    try:
        mock.send_signal(signal.SIGINT)
        mock.wait(timeout=5)
    except Exception:
        mock.kill()

    if failures:
        print("\n%d check(s) FAILED" % failures); sys.exit(1)
    print("\nall mcp http checks passed")


if __name__ == "__main__":
    main()
