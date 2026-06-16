#!/usr/bin/env python3
"""End-to-end protocol verification of the connector spine + security floor."""
import asyncio, json, uuid, urllib.request
import websockets

BASE = "http://127.0.0.1:8080"

PRESET = {
    "id": "github-public", "display_name": "GitHub (public)", "kind": "manual_rest",
    "base_url": "https://api.github.com", "allowed_hosts": ["api.github.com"],
    "ops": [{"id": "list_repos", "method": "GET", "path_template": "/users/{user}/repos",
             "allowed_query": ["per_page"], "class": "read",
             "summary": "List a user's public repositories",
             "default_args": {"user": "octocat", "per_page": "30"}}],
}
SSRF_DEF = {"id": "meta-test", "display_name": "Metadata", "base_url": "https://169.254.169.254",
            "ops": [{"id": "get", "method": "GET", "path_template": "/latest/meta-data/", "class": "read"}]}
HTTPBIN = {"id": "httpbin-test", "display_name": "httpbin", "base_url": "https://httpbin.org",
           "ops": [{"id": "post", "method": "POST", "path_template": "/post", "class": "write", "summary": "test write"}]}


def tokens():
    return json.loads(urllib.request.urlopen(BASE + "/bootstrap", timeout=3).read())


class Client:
    def __init__(self, ws, approver=None):
        self.ws, self.approver, self.pending, self.approvals = ws, approver, {}, []

    async def reader(self):
        async for raw in self.ws:
            m = json.loads(raw)
            if m.get("type") == "approval":
                self.approvals.append(m)
                if self.approver:
                    v = self.approver(m)
                    await self.ws.send(json.dumps({"id": str(uuid.uuid4()), "capability": "approval.resolve",
                        "args": {"approvalId": m["approvalId"], "verdict": v,
                                 "grantKey": m.get("grantKey"), "capability": m.get("capability")}}))
                continue
            fut = self.pending.pop(m.get("id"), None)
            if fut and not fut.done():
                fut.set_result(m)

    async def call(self, cap, args=None, timeout=30):
        mid = str(uuid.uuid4())
        fut = asyncio.get_event_loop().create_future()
        self.pending[mid] = fut
        await self.ws.send(json.dumps({"id": mid, "capability": cap, "args": args or {}}))
        return await asyncio.wait_for(fut, timeout)


async def main():
    tok = tokens()
    print("bootstrap: got human+ai tokens:", bool(tok.get("human_token")) and bool(tok.get("ai_token")))
    # reject a bogus token
    try:
        async with websockets.connect(f"ws://127.0.0.1:8080/ws?token=deadbeef"):
            print("P0 bogus token: CONNECTED (BAD)")
    except Exception as e:
        print("P0 bogus token rejected:", type(e).__name__)

    hws = await websockets.connect(f"ws://127.0.0.1:8080/ws?token={tok['human_token']}")
    aws = await websockets.connect(f"ws://127.0.0.1:8080/ws?token={tok['ai_token']}")
    human = Client(hws, approver=lambda m: "allow_always" if (m.get("conn") or {}).get("class") == "read" else "deny")
    ai = Client(aws)
    tasks = [asyncio.create_task(human.reader()), asyncio.create_task(ai.reader())]

    r = await human.call("connector.add", PRESET)
    print("A human connector.add:", r["ok"], r.get("data") or r.get("error"))

    r = await ai.call("connector.add", PRESET)
    print("B ai connector.add (expect deny):", r["ok"], "—", r.get("error"))

    r = await human.call("conn.call", {"connector": "github-public", "op": "list_repos", "args": {"user": "octocat"}})
    arr = (r.get("data") or {}).get("data")
    print("C human conn.call:", r["ok"], "| repos:", len(arr) if isinstance(arr, list) else arr,
          "| first:", arr[0].get("name") if isinstance(arr, list) and arr else None,
          "| stars field:", arr[0].get("stargazers_count") if isinstance(arr, list) and arr else None)

    r = await ai.call("conn.call", {"connector": "github-public", "op": "list_repos", "args": {"user": "octocat"}})
    last = human.approvals[-1] if human.approvals else {}
    print("D ai conn.call (ungoverned→ASK→allow_always):", r["ok"],
          "| approval.conn:", last.get("conn"), "| grantKey:", (last.get("grantKey") or "")[:60], "…")

    before = len(human.approvals)
    r = await ai.call("conn.call", {"connector": "github-public", "op": "list_repos", "args": {"user": "octocat"}})
    print("E ai conn.call again (grant persisted):", r["ok"], "| new approvals:", len(human.approvals) - before, "(expect 0)")

    await human.call("connector.add", SSRF_DEF)
    r = await human.call("conn.call", {"connector": "meta-test", "op": "get", "args": {}})
    print("F SSRF guard (169.254.169.254):", "BLOCKED" if not r["ok"] else "ALLOWED(BAD)", "—", r.get("error"))

    await human.call("policy.set_unsafe", {"on": True})
    await human.call("connector.add", HTTPBIN)
    before = len(human.approvals)
    r = await ai.call("conn.call", {"connector": "httpbin-test", "op": "post", "args": {}})
    print("G unsafe_mode + write op: approval STILL fired:", len(human.approvals) - before > 0,
          "| call result ok:", r["ok"], "(operator denied) —", r.get("error"))
    await human.call("policy.set_unsafe", {"on": False})

    for t in tasks:
        t.cancel()
    await hws.close(); await aws.close()


asyncio.run(main())
