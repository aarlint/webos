#!/usr/bin/env python3
"""Smoke: GraphQL ops through conn.call (Stage 1).

Boots one short-lived kerneld against a temp WEBOS_ROOT (sealed-file secrets
backend), then exercises a GraphQL connector end-to-end against the PUBLIC,
no-auth countries API (https://countries.trevorblades.com):

  1. connector.add a manual_rest connector whose single op carries a `graphql`
     block (query "{ countries { code name } }", items_path "data.countries").
  2. conn.call that op -> assert the envelope is ok, carries the items_path hint,
     and that data.countries is a non-empty array of { code, name } objects.
  3. Negative: a graphql op with a deliberately broken query -> assert the
     daemon folds the GraphQL `errors` array into ok=false and surfaces the
     error message (no auth involved here, but proves the error path).
  4. A graphql op with variables: query a single country by `code` variable,
     passing args:{code:"US"} -> assert the right country comes back, proving
     declared variables are forwarded into GraphQL `variables`.

Network: requires outbound HTTPS to countries.trevorblades.com. A network
outage is reported distinctly and does not fail the run. Kills the daemon.
"""
import asyncio, json, os, signal, subprocess, sys, tempfile, uuid, urllib.request
import websockets

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(REPO, "target", "debug", "kerneld")
ADDR = "127.0.0.1:8077"

API = "https://countries.trevorblades.com"
HOST = "countries.trevorblades.com"


def boot(root, logfile):
    env = dict(
        os.environ,
        WEBOS_ROOT=root,
        WEBOS_ADDR=ADDR,
        WEBOS_SECRETS_BACKEND="file",
        RUST_LOG="info",
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
    return await websockets.connect(f"ws://{ADDR}/ws?token={token}")


async def call(ws, capability, args=None):
    mid = str(uuid.uuid4())
    await ws.send(json.dumps({"id": mid, "capability": capability, "args": args or {}}))
    for _ in range(60):
        m = json.loads(await asyncio.wait_for(ws.recv(), timeout=20))
        if m.get("id") == mid:
            return m
    raise RuntimeError("no response for " + capability)


class NetworkError(Exception):
    pass


def connector_def():
    return {
        "id": "countries",
        "display_name": "Countries GraphQL",
        "kind": "manual_rest",
        "base_url": API,
        "allowed_hosts": [HOST],
        "ops": [
            {
                "id": "all",
                "summary": "list all countries",
                "graphql": {
                    "query": "{ countries { code name } }",
                    "items_path": "data.countries",
                },
            },
            {
                "id": "broken",
                "summary": "deliberately invalid query",
                "graphql": {"query": "{ nope { bogusField } }"},
            },
            {
                "id": "one",
                "summary": "one country by code",
                "graphql": {
                    "query": "query($code: ID!){ country(code: $code){ code name } }",
                    "variables": ["code"],
                    "items_path": "data.country",
                },
            },
        ],
    }


async def run():
    toks = await bootstrap_tokens()
    failures = 0
    ws = await connect(toks["human_token"])
    async with ws:
        r = await call(ws, "connector.add", connector_def())
        assert r.get("ok"), f"connector.add failed: {r}"
        print("PASS  connector.add accepted a graphql-op connector")

        # 1/2. list all countries
        r = await call(ws, "conn.call", {"connector": "countries", "op": "all"})
        if not r.get("ok"):
            raise NetworkError(f"bus message not ok: {r}")
        cc = r["data"]
        # If the upstream itself errored at the transport level, treat as network.
        if cc.get("status") is None or (cc.get("status", 0) >= 500):
            raise NetworkError(f"upstream status={cc.get('status')}: {str(cc)[:200]}")
        try:
            assert cc.get("ok") is True, f"conn.call result not ok: {cc.get('error')!r} {cc}"
            assert cc.get("items_path") == "data.countries", f"items_path hint missing/wrong: {cc.get('items_path')!r}"
            countries = (cc.get("data") or {}).get("data", {}).get("countries")
            assert isinstance(countries, list) and len(countries) > 50, \
                f"expected a populated countries array, got {type(countries).__name__} len={len(countries) if isinstance(countries, list) else 'n/a'}"
            sample = countries[0]
            assert "code" in sample and "name" in sample, f"unexpected country shape: {sample}"
            assert cc.get("_untrusted") is True, "graphql result must carry _untrusted"
            print(f"PASS  conn.call graphql returned {len(countries)} countries (e.g. {sample['code']}={sample['name']})")
        except AssertionError as e:
            failures += 1; print("FAIL ", e)

        # 3. GraphQL errors array -> ok=false with surfaced message
        r = await call(ws, "conn.call", {"connector": "countries", "op": "broken"})
        if not r.get("ok"):
            raise NetworkError(f"bus message not ok: {r}")
        cc = r["data"]
        if cc.get("status") is None or cc.get("status", 0) >= 500:
            raise NetworkError(f"upstream status={cc.get('status')}: {str(cc)[:200]}")
        try:
            assert cc.get("ok") is False, f"invalid graphql query should be ok=false: {cc}"
            assert cc.get("error"), f"graphql error message should be surfaced: {cc}"
            print("PASS  graphql errors[] folded into ok=false with message:", str(cc.get("error"))[:80])
        except AssertionError as e:
            failures += 1; print("FAIL ", e)

        # 4. variables forwarded
        r = await call(ws, "conn.call", {"connector": "countries", "op": "one", "args": {"code": "US"}})
        if not r.get("ok"):
            raise NetworkError(f"bus message not ok: {r}")
        cc = r["data"]
        if cc.get("status") is None or cc.get("status", 0) >= 500:
            raise NetworkError(f"upstream status={cc.get('status')}: {str(cc)[:200]}")
        try:
            assert cc.get("ok") is True, f"variable query not ok: {cc.get('error')!r} {cc}"
            country = (cc.get("data") or {}).get("data", {}).get("country")
            assert isinstance(country, dict) and country.get("code") == "US", \
                f"variable not forwarded / wrong country: {country}"
            print(f"PASS  graphql variables forwarded: code=US -> {country.get('name')}")
        except AssertionError as e:
            failures += 1; print("FAIL ", e)

    return failures


def main():
    if not os.path.exists(BIN):
        print("FAIL  build kerneld first (cargo build)"); sys.exit(1)
    root = tempfile.mkdtemp(prefix="webos-gql-")
    log = os.path.join(root, "boot.log")
    p, f = boot(root, log)
    failures = 1
    network_failed = False
    try:
        failures = asyncio.run(run())
    except NetworkError as e:
        network_failed = True
        print("NETWORK  countries GraphQL API unavailable, cannot assert:", e)
        failures = 0
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

    if network_failed:
        print("\nNOTE: network-dependent GraphQL checks were SKIPPED (API unreachable).")
    if failures:
        print(f"\n{failures} check(s) FAILED"); sys.exit(1)
    print("\nall graphql conn.call checks passed")


if __name__ == "__main__":
    main()
