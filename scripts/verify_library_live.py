#!/usr/bin/env python3
"""Verify the connector library + GraphQL against the already-running kerneld on :8080."""
import asyncio, json, uuid, urllib.request
import websockets

tok = json.loads(urllib.request.urlopen("http://127.0.0.1:8080/bootstrap", timeout=3).read())

async def main():
    async with websockets.connect(f"ws://127.0.0.1:8080/ws?token={tok['human_token']}") as ws:
        async def call(cap, args=None):
            mid = str(uuid.uuid4())
            await ws.send(json.dumps({"id": mid, "capability": cap, "args": args or {}}))
            while True:
                m = json.loads(await asyncio.wait_for(ws.recv(), 30))
                if m.get("id") == mid:
                    return m

        lib = await call("library.list")
        items = lib["data"]["library"] if "library" in lib.get("data", {}) else lib["data"].get("connectors", lib["data"])
        names = [(c.get("id"), c.get("requires_cred", {}).get("name") if isinstance(c.get("requires_cred"), dict) else c.get("requires_cred")) for c in (items if isinstance(items, list) else [])]
        print("library.list:", lib["ok"], "| entries:", names)

        ins = await call("library.install", {"id": "countries"})
        print("install countries:", ins["ok"], ins.get("data") or ins.get("error"))

        gq = await call("conn.call", {"connector": "countries", "op": "list_countries"})
        if not gq["ok"]:
            # op id may differ; try describing
            d = await call("connector.describe", {"id": "countries"})
            ops = [o.get("op_id") for o in d.get("data", {}).get("ops", [])]
            print("countries ops:", ops)
            if ops:
                gq = await call("conn.call", {"connector": "countries", "op": ops[0]})
        data = (gq.get("data") or {}).get("data")
        cnt = len(data) if isinstance(data, list) else (len(data.get("countries", [])) if isinstance(data, dict) and "countries" in data else data)
        print("conn.call countries (GraphQL):", gq["ok"], "| countries:", cnt)

        insl = await call("library.install", {"id": "linear"})
        print("install linear:", insl["ok"], "| requires_cred:", insl.get("data", {}).get("requires_cred"))
        d = await call("connector.describe", {"id": "linear"})
        print("linear ops:", [o.get("op_id") for o in d.get("data", {}).get("ops", [])])

asyncio.run(main())
