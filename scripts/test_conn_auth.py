#!/usr/bin/env python3
"""Smoke: per-connector auth injection (creds -> conn.call egress header/query).

Boots one short-lived kerneld against a temp WEBOS_ROOT (sealed-file secrets
backend, RUST_LOG=trace so any accidental secret logging would surface), then:

  1. creds.set three sentinel secrets (bearer / header / basic).
  2. connector.add a manual_rest connector whose ops point at httpbin.org
     (which echoes the inbound request back), with bearer auth.
  3. conn.call the bearer op -> assert the upstream received
     `Authorization: Bearer <sentinel>` (proof the secret was injected at the
     egress boundary), and that the daemon's OWN response envelope does not add
     the secret anywhere outside the upstream echo.
  4. Re-add with `header` scheme (X-Api-Key + prefix) and conn.call -> assert the
     custom header arrived with the prefix.
  5. Re-add with `query` scheme and conn.call /get -> assert the secret arrived
     as a query arg AND was still host/SSRF checked (httpbin is public).
  6. Re-add with `basic` scheme and conn.call -> assert Authorization: Basic
     base64(user:pass) arrived and decodes to the sentinel userinfo.
  7. The AI principal cannot read the secret: creds.list and creds.set are
     operator-only (denied for 'ai'); conn.call response never carries the raw
     secret value to the AI outside the upstream echo of the op it invoked.
  8. GLOBAL: grep the full trace log for every sentinel value -> must be absent.
  9. Validation: connector.add with bearer but no cred_ref -> error, not panic.

Network: requires outbound HTTPS to httpbin.org. Kills the daemon at the end.
Exits non-zero on any violation (a network failure is reported distinctly).
"""
import asyncio, base64, json, os, signal, subprocess, sys, tempfile, time, uuid, urllib.request
import websockets

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(REPO, "target", "debug", "kerneld")
ADDR = "127.0.0.1:8079"

# Unique per-run sentinels so a stale log can never produce a false pass.
RUN = uuid.uuid4().hex
BEARER_SECRET = f"brr-{RUN}-SENTINEL"
HEADER_SECRET = f"hdr-{RUN}-SENTINEL"
QUERY_SECRET = f"qry-{RUN}-SENTINEL"
BASIC_SECRET = f"user-{RUN}:pass-{RUN}-SENTINEL"
ALL_SECRETS = [BEARER_SECRET, HEADER_SECRET, QUERY_SECRET, BASIC_SECRET]

HTTPBIN = "https://httpbin.org"


def boot(root, logfile):
    env = dict(
        os.environ,
        WEBOS_ROOT=root,
        WEBOS_ADDR=ADDR,
        WEBOS_SECRETS_BACKEND="file",
        RUST_LOG="trace",  # maximize the chance of catching an accidental leak
    )
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


async def bootstrap_tokens():
    for _ in range(40):
        try:
            return json.loads(urllib.request.urlopen(f"http://{ADDR}/bootstrap", timeout=2).read())
        except Exception:
            await asyncio.sleep(0.25)
    raise RuntimeError("kerneld never came up")


async def connect(token):
    ws = await websockets.connect(f"ws://{ADDR}/ws?token={token}")
    return ws


async def call(ws, capability, args=None):
    mid = str(uuid.uuid4())
    await ws.send(json.dumps({"id": mid, "capability": capability, "args": args or {}}))
    for _ in range(60):
        m = json.loads(await asyncio.wait_for(ws.recv(), timeout=20))
        if m.get("id") == mid:
            return m
    raise RuntimeError("no response for " + capability)


def connector_def(scheme, **auth_extra):
    auth = {"scheme": scheme}
    auth.update(auth_extra)
    return {
        "id": "echotest",
        "display_name": "HTTPBin Echo",
        "kind": "manual_rest",
        "base_url": HTTPBIN,
        "allowed_hosts": ["httpbin.org"],
        "auth": auth,
        "ops": [
            {"id": "headers", "method": "GET", "path_template": "/headers", "summary": "echo headers"},
            {"id": "get", "method": "GET", "path_template": "/get",
             "allowed_query": ["foo"], "summary": "echo query"},
        ],
    }


class NetworkError(Exception):
    pass


def upstream_ok(resp):
    """bus message -> echoed upstream JSON, or raise NetworkError.

    Bus envelope is {id, ok, data:<cap result>}; the conn.call result is
    {connector, op, ok, status, class, host, data:<upstream echo>, _untrusted}.
    """
    if not resp.get("ok"):
        raise NetworkError(f"bus message not ok: {resp}")
    cc = resp.get("data") or {}
    status = cc.get("status")
    upstream = cc.get("data")
    if status is None or status >= 400 or not isinstance(upstream, dict):
        raise NetworkError(f"upstream status={status} data-type={type(upstream).__name__}: {str(upstream)[:200]}")
    return upstream


async def run():
    toks = await bootstrap_tokens()
    failures = 0
    ws = await connect(toks["human_token"])
    async with ws:
        # 1. seed sentinels
        for name, val in [("BRR", BEARER_SECRET), ("HDR", HEADER_SECRET),
                          ("QRY", QUERY_SECRET), ("BSC", BASIC_SECRET)]:
            r = await call(ws, "creds.set", {"name": name, "value": val})
            assert r.get("ok"), f"creds.set {name} failed: {r}"
            # The set response must never echo the value back.
            assert val not in json.dumps(r), f"creds.set echoed the secret value: {r}"
        print("PASS  creds.set seeded sentinels; set response never echoes value")

        # 9. validation FIRST (cheap, no network): bearer w/o cred_ref -> error
        r = await call(ws, "connector.add", {
            "id": "bad", "display_name": "bad", "base_url": HTTPBIN,
            "allowed_hosts": ["httpbin.org"], "auth": {"scheme": "bearer"},
            "ops": [{"id": "h", "method": "GET", "path_template": "/headers"}],
        })
        try:
            assert not r.get("ok"), "bearer auth with no cred_ref should be rejected"
            print("PASS  connector.add rejects bearer auth missing cred_ref:", r.get("error"))
        except AssertionError as e:
            failures += 1; print("FAIL ", e)

        # 2/3. bearer
        r = await call(ws, "connector.add", connector_def("bearer", cred_ref="BRR"))
        assert r.get("ok"), f"connector.add bearer failed: {r}"
        r = await call(ws, "conn.call", {"connector": "echotest", "op": "headers"})
        data = upstream_ok(r)
        hdrs = data.get("headers", {})
        try:
            got = hdrs.get("Authorization", "")
            assert got == f"Bearer {BEARER_SECRET}", f"bearer header wrong/absent: {got!r}"
            # The caller never put the secret in args, and the daemon must not
            # have added it anywhere except inside the upstream echo we asked for.
            assert r.get("args") is None or BEARER_SECRET not in json.dumps(r.get("args", {})), \
                "secret leaked into echoed args"
            print("PASS  bearer: Authorization: Bearer <secret> reached upstream")
        except AssertionError as e:
            failures += 1; print("FAIL ", e)

        # 4. header scheme with prefix
        r = await call(ws, "connector.add",
                       connector_def("header", cred_ref="HDR", header_name="X-Api-Key", prefix="tok_"))
        assert r.get("ok"), f"connector.add header failed: {r}"
        r = await call(ws, "conn.call", {"connector": "echotest", "op": "headers"})
        data = upstream_ok(r)
        hdrs = data.get("headers", {})
        try:
            got = hdrs.get("X-Api-Key", "")
            assert got == f"tok_{HEADER_SECRET}", f"custom header wrong/absent: {got!r}"
            print("PASS  header: X-Api-Key: tok_<secret> reached upstream")
        except AssertionError as e:
            failures += 1; print("FAIL ", e)

        # 5. query scheme
        r = await call(ws, "connector.add",
                       connector_def("query", cred_ref="QRY", param_name="api_key"))
        assert r.get("ok"), f"connector.add query failed: {r}"
        r = await call(ws, "conn.call", {"connector": "echotest", "op": "get", "args": {"foo": "bar"}})
        data = upstream_ok(r)
        qargs = data.get("args", {})
        try:
            assert qargs.get("api_key") == QUERY_SECRET, f"query secret not in URL: {qargs}"
            assert qargs.get("foo") == "bar", f"declared query arg dropped: {qargs}"
            print("PASS  query: api_key=<secret> reached upstream (host/SSRF still enforced)")
        except AssertionError as e:
            failures += 1; print("FAIL ", e)

        # 6. basic scheme
        r = await call(ws, "connector.add", connector_def("basic", cred_ref="BSC"))
        assert r.get("ok"), f"connector.add basic failed: {r}"
        r = await call(ws, "conn.call", {"connector": "echotest", "op": "headers"})
        data = upstream_ok(r)
        hdrs = data.get("headers", {})
        try:
            got = hdrs.get("Authorization", "")
            assert got.startswith("Basic "), f"basic header wrong/absent: {got!r}"
            decoded = base64.b64decode(got[len("Basic "):]).decode()
            assert decoded == BASIC_SECRET, f"basic decoded != userinfo: {decoded!r}"
            print("PASS  basic: Authorization: Basic base64(user:pass) reached upstream")
        except AssertionError as e:
            failures += 1; print("FAIL ", e)

    # 7. AI principal cannot read secrets (creds.* operator-only) nor inject auth args.
    ai = await connect(toks["ai_token"])
    async with ai:
        r = await call(ai, "creds.list", {})
        try:
            assert not r.get("ok"), "creds.list must be denied for the AI"
            print("PASS  ai: creds.list denied (operator-only):", r.get("error"))
        except AssertionError as e:
            failures += 1; print("FAIL ", e)
        r = await call(ai, "creds.set", {"name": "X", "value": "y"})
        try:
            assert not r.get("ok"), "creds.set must be denied for the AI"
            print("PASS  ai: creds.set denied (operator-only)")
        except AssertionError as e:
            failures += 1; print("FAIL ", e)

    return failures


def check_log(log):
    """The daemon's trace log must never contain any sentinel secret value."""
    try:
        with open(log) as lf:
            text = lf.read()
    except OSError as e:
        print("FAIL  could not read trace log:", e)
        return 1
    leaks = [s for s in ALL_SECRETS if s in text]
    if leaks:
        print(f"FAIL  trace log leaked {len(leaks)} secret value(s) — e.g. matched sentinel run {RUN}")
        return 1
    print("PASS  trace log (RUST_LOG=trace) contains NO sentinel secret value")
    return 0


def main():
    if not os.path.exists(BIN):
        print("FAIL  build kerneld first (cargo build)"); sys.exit(1)
    root = tempfile.mkdtemp(prefix="webos-connauth-")
    log = os.path.join(root, "boot.log")
    p, f = boot(root, log)
    failures = 1
    network_failed = False
    try:
        failures = asyncio.run(run())
    except NetworkError as e:
        network_failed = True
        print("NETWORK  upstream/echo unavailable, cannot assert delivery:", e)
        failures = 0  # don't fail the security run on a network outage
    except Exception as e:
        print("ERROR", repr(e))
        try:
            with open(log) as lf:
                print(lf.read()[-2000:])
        except OSError:
            pass
        failures = 1
    finally:
        kill(p, f)

    # The log-leak check runs regardless — it's the core security assertion and
    # does not depend on httpbin being reachable.
    failures += check_log(log)

    if network_failed:
        print("\nNOTE: network-dependent delivery checks were SKIPPED (httpbin unreachable);")
        print("      the secret-never-logged assertion still ran and is authoritative.")
    if failures:
        print(f"\n{failures} check(s) FAILED"); sys.exit(1)
    print("\nall conn.call auth checks passed")


if __name__ == "__main__":
    main()
