#!/usr/bin/env python3
"""Smoke: the Connector Library (Stage 2).

Boots one short-lived kerneld against a temp WEBOS_ROOT (sealed-file secrets
backend) with the bundled `library/` manifests, then exercises the library
end-to-end:

  1. library.list -> assert linear, github-public, and countries all appear;
     none is installed yet; linear declares requires_cred LINEAR_TOKEN and is a
     manual_rest connector (we do NOT install it — it needs a real token).
  2. library.install github-public -> appears in connector.list; conn.call
     list_repos returns a populated repo array (REST op, network).
  3. library.install countries -> conn.call the graphql op returns countries
     (GraphQL op, network).
  4. After installs, library.list re-tags github-public + countries installed=true.
  5. Security: the AI principal is DENIED library.install (PROTECTED, human-only)
     even though it may library.list. This needs no network.

Network-dependent steps (2,3) degrade to a NETWORK note if the upstream APIs are
unreachable; the catalog + security checks (1,4,5) are offline and always run.
Kills the daemon; leaves no kerneld running.
"""
import asyncio, json, os, signal, subprocess, sys, tempfile, uuid, urllib.request
import websockets

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(REPO, "target", "debug", "kerneld")
ADDR = "127.0.0.1:8076"


def boot(root, logfile):
    env = dict(
        os.environ,
        WEBOS_ROOT=root,
        WEBOS_ADDR=ADDR,
        WEBOS_SECRETS_BACKEND="file",
        RUST_LOG="info",
    )
    # cwd=REPO so the default library/ dir (relative to cwd) resolves to the
    # bundled manifests — same convention as the web/ ServeDir.
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


async def run():
    toks = await bootstrap_tokens()
    failures = 0
    human = await connect(toks["human_token"])
    ai = await connect(toks["ai_token"])
    async with human, ai:
        # 1. catalog lists the bundled manifests, none installed yet
        r = await call(human, "library.list")
        assert r.get("ok"), f"library.list failed: {r}"
        cat = {it["id"]: it for it in r["data"]["connectors"]}
        try:
            for need in ("linear", "github-public", "countries"):
                assert need in cat, f"library is missing '{need}': have {list(cat)}"
            assert not cat["github-public"]["installed"], "github-public should start uninstalled"
            assert not cat["countries"]["installed"], "countries should start uninstalled"
            lin = cat["linear"]
            assert lin["kind"] == "manual_rest", f"linear kind unexpected: {lin['kind']}"
            assert not lin["installed"], "linear should start uninstalled"
            rc = lin.get("requires_cred")
            assert rc and rc.get("name") == "LINEAR_TOKEN", f"linear must require LINEAR_TOKEN: {rc}"
            assert cat["github-public"].get("requires_cred") in (None, {}), \
                "github-public is no-auth and must not require a credential"
            print(f"PASS  library.list shows {len(cat)} connectors incl linear/github-public/countries")
            print(f"PASS  linear advertises requires_cred LINEAR_TOKEN (not installed in this smoke)")
        except AssertionError as e:
            failures += 1; print("FAIL ", e)

        # 5. (offline) AI principal is denied library.install; may still list.
        r = await call(ai, "library.list")
        try:
            assert r.get("ok"), f"AI should be allowed library.list: {r}"
            print("PASS  AI principal allowed library.list (GOVERNABLE)")
        except AssertionError as e:
            failures += 1; print("FAIL ", e)
        r = await call(ai, "library.install", {"id": "countries"})
        try:
            assert not r.get("ok"), f"AI must NOT be able to install: {r}"
            assert r.get("decision") == "deny", f"expected deny decision, got: {r}"
            print("PASS  AI principal DENIED library.install (PROTECTED, operator-only)")
        except AssertionError as e:
            failures += 1; print("FAIL ", e)
        # The AI's denied install must not have minted a connector.
        r = await call(human, "connector.list")
        try:
            ids = [c["id"] for c in r["data"]["connectors"]]
            assert "countries" not in ids, f"denied AI install leaked a connector: {ids}"
            print("PASS  denied AI install registered no connector")
        except AssertionError as e:
            failures += 1; print("FAIL ", e)

        # 2. operator installs github-public, then calls it (REST, network)
        r = await call(human, "library.install", {"id": "github-public"})
        try:
            assert r.get("ok"), f"library.install github-public failed: {r}"
            assert r["data"].get("installed") == "github-public", f"unexpected install result: {r['data']}"
            assert "requires_cred" not in r["data"], "no-auth install must not surface requires_cred"
            print("PASS  library.install github-public")
        except AssertionError as e:
            failures += 1; print("FAIL ", e)
        r = await call(human, "connector.list")
        try:
            ids = [c["id"] for c in r["data"]["connectors"]]
            assert "github-public" in ids, f"installed connector not in connector.list: {ids}"
            print("PASS  installed connector appears in connector.list")
        except AssertionError as e:
            failures += 1; print("FAIL ", e)

        # 3. operator installs countries (graphql, network)
        r = await call(human, "library.install", {"id": "countries"})
        try:
            assert r.get("ok"), f"library.install countries failed: {r}"
            print("PASS  library.install countries")
        except AssertionError as e:
            failures += 1; print("FAIL ", e)

        # 4. (offline) catalog now tags the two installed
        r = await call(human, "library.list")
        try:
            cat2 = {it["id"]: it for it in r["data"]["connectors"]}
            assert cat2["github-public"]["installed"], "github-public should now be installed"
            assert cat2["countries"]["installed"], "countries should now be installed"
            assert not cat2["linear"]["installed"], "linear should remain uninstalled"
            print("PASS  library.list re-tags installed connectors")
        except AssertionError as e:
            failures += 1; print("FAIL ", e)

        # network: REST list_repos (degrade locally so earlier offline failures
        # are never lost to a network outage)
        r = await call(human, "conn.call", {"connector": "github-public", "op": "list_repos", "args": {"user": "octocat"}})
        cc = r.get("data") or {}
        if not r.get("ok") or cc.get("status") is None or cc.get("status", 0) >= 500:
            print("NETWORK  github API unavailable, skipping list_repos assertion:", str(cc)[:160] or r.get("error"))
        else:
            try:
                assert cc.get("ok") is True, f"github list_repos not ok: {cc}"
                repos = cc.get("data")
                assert isinstance(repos, list) and len(repos) > 0, f"expected a repo array, got {type(repos).__name__}"
                assert "name" in repos[0], f"unexpected repo shape: {repos[0]}"
                print(f"PASS  conn.call list_repos returned {len(repos)} repos (e.g. {repos[0].get('name')})")
            except AssertionError as e:
                failures += 1; print("FAIL ", e)

        # network: GraphQL countries
        r = await call(human, "conn.call", {"connector": "countries", "op": "all"})
        cc = r.get("data") or {}
        if not r.get("ok") or cc.get("status") is None or cc.get("status", 0) >= 500:
            print("NETWORK  countries API unavailable, skipping graphql assertion:", str(cc)[:160] or r.get("error"))
        else:
            try:
                assert cc.get("ok") is True, f"countries graphql not ok: {cc.get('error')!r} {cc}"
                countries = (cc.get("data") or {}).get("data", {}).get("countries")
                assert isinstance(countries, list) and len(countries) > 50, \
                    f"expected populated countries, got {type(countries).__name__}"
                assert cc.get("items_path") == "data.countries", f"items_path hint wrong: {cc.get('items_path')!r}"
                print(f"PASS  conn.call countries graphql returned {len(countries)} countries")
            except AssertionError as e:
                failures += 1; print("FAIL ", e)

    return failures


def main():
    if not os.path.exists(BIN):
        print("FAIL  build kerneld first (cargo build)"); sys.exit(1)
    root = tempfile.mkdtemp(prefix="webos-lib-")
    log = os.path.join(root, "boot.log")
    p, f = boot(root, log)
    failures = 1
    try:
        failures = asyncio.run(run())
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

    if failures:
        print(f"\n{failures} check(s) FAILED"); sys.exit(1)
    print("\nall connector-library checks passed")


if __name__ == "__main__":
    main()
