#!/usr/bin/env python3
"""Call ai.compose and show whether the model (not the fallback) built the Surface."""
import asyncio, json, uuid, urllib.request, sys
import websockets

tok = json.loads(urllib.request.urlopen("http://127.0.0.1:8080/bootstrap", timeout=3).read())
intent = sys.argv[1] if len(sys.argv) > 1 else "show octocat's public repos as a sortable table"

async def main():
    async with websockets.connect(f"ws://127.0.0.1:8080/ws?token={tok['human_token']}") as ws:
        mid = str(uuid.uuid4())
        await ws.send(json.dumps({"id": mid, "capability": "ai.compose", "args": {"intent": intent}}))
        # model cold-load can be slow
        for _ in range(40):
            m = json.loads(await asyncio.wait_for(ws.recv(), timeout=70))
            if m.get("id") == mid:
                s = m.get("data", {})
                print("ok:", m.get("ok"), "| surface id:", s.get("id"), "| title:", s.get("title"))
                print(json.dumps(s, indent=1)[:900])
                return

asyncio.run(main())
