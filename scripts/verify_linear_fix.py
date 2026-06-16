#!/usr/bin/env python3
"""Verify the Linear column/chart fix against the running kerneld (real data)."""
import asyncio, json, uuid, urllib.request
import websockets

tok = json.loads(urllib.request.urlopen("http://127.0.0.1:8080/bootstrap", timeout=3).read())

async def main():
    async with websockets.connect(f"ws://127.0.0.1:8080/ws?token={tok['human_token']}") as ws:
        async def call(cap, args=None):
            mid = str(uuid.uuid4())
            await ws.send(json.dumps({"id": mid, "capability": cap, "args": args or {}}))
            while True:
                m = json.loads(await asyncio.wait_for(ws.recv(), 40))
                if m.get("id") == mid:
                    return m

        creds = await call("creds.list")
        names = [c.get("name") for c in (creds.get("data", {}).get("credentials", []))]
        print("creds:", names, "| LINEAR_TOKEN present:", "LINEAR_TOKEN" in names)

        print("re-install linear:", (await call("library.install", {"id": "linear"})).get("ok"))

        r = await call("conn.call", {"connector": "linear", "op": "my_issues"})
        nodes = (((r.get("data") or {}).get("data") or {}).get("data") or {}).get("viewer", {}).get("assignedIssues", {}).get("nodes")
        if isinstance(nodes, list) and nodes:
            n0 = nodes[0]
            print("conn.call my_issues ok:", r.get("data", {}).get("ok"), "| count:", len(nodes))
            print("  first issue fields -> title:", bool(n0.get("title")),
                  "state.name:", (n0.get("state") or {}).get("name"),
                  "assignee:", (n0.get("assignee") or {}).get("name") if n0.get("assignee") else None,
                  "createdAt:", bool(n0.get("createdAt")), "priority:", n0.get("priority"))
            statuses = {}
            for n in nodes:
                s = (n.get("state") or {}).get("name", "?"); statuses[s] = statuses.get(s, 0) + 1
            print("  status distribution (what the count-chart plots):", statuses)
        else:
            print("conn.call my_issues:", r.get("ok"), r.get("data") or r.get("error"))

        # ui.table with NO columns -> should fill curated columns + items_path
        t = await call("ui.table", {"connector": "linear", "op": "my_issues", "title": "My Linear Tickets"})
        surf = await call("ui.get", {"id": t["data"]["stored"]})
        tbl = surf["data"]["elements"]["tbl"]["props"]
        print("ui.table filled -> items:", tbl.get("items"), "| columns:", [(c["header"], c["path"]) for c in tbl.get("columns", [])])

        # ui.chart count mode
        c = await call("ui.chart", {"connector": "linear", "op": "my_issues", "type": "bar", "x": "state.name", "agg": "count", "title": "Tickets by status"})
        cs = await call("ui.get", {"id": c["data"]["stored"]})
        cp = cs["data"]["elements"]["chart"]["props"]
        print("ui.chart filled -> items:", cp.get("items"), "| x:", cp.get("x"), "| agg:", cp.get("agg"))

asyncio.run(main())
