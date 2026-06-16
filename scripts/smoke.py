#!/usr/bin/env python3
"""Drive the kerneld bus as both principals to prove parity + the policy gate."""
import asyncio, json, sys, uuid
import websockets

async def call(ws, capability, args=None):
    inv = {"id": str(uuid.uuid4()), "capability": capability, "args": args or {}}
    await ws.send(json.dumps(inv))
    return json.loads(await ws.recv())

async def session(principal):
    url = f"ws://127.0.0.1:8080/ws?as={principal}"
    # retry while kerneld is still coming up
    for _ in range(20):
        try:
            ws = await websockets.connect(url)
            break
        except OSError:
            await asyncio.sleep(0.3)
    else:
        print("could not connect"); sys.exit(1)

    async with ws:
        print(f"\n=== principal: {principal} ===")
        r = await call(ws, "weather.get", {"lat": 40.71, "lon": -74.0})
        print(f"weather.get  -> ok={r['ok']}  {r.get('data',{}).get('summary') or r.get('error')}")
        r = await call(ws, "fs.write", {"path": "note.txt", "content": f"hello from {principal}"})
        print(f"fs.write     -> ok={r['ok']}  {r.get('data',{}).get('summary') or r.get('error')}  decision={r.get('decision','-')}")
        r = await call(ws, "fs.read", {"path": "note.txt"})
        print(f"fs.read      -> ok={r['ok']}  {r.get('data',{}).get('content') or r.get('error')}")
        r = await call(ws, "fs.write", {"path": "../escape.txt", "content": "x"})
        print(f"fs.write ..  -> ok={r['ok']}  {r.get('error','(allowed?!)')}")
        r = await call(ws, "ui.get", {"id": "home"})
        print(f"ui.get home  -> ok={r['ok']}  title={r.get('data',{}).get('title')}")
        r = await call(ws, "ai.compose", {"intent": "show me weather please"})
        print(f"ai.compose   -> ok={r['ok']}  generated surface id={r.get('data',{}).get('id')}")

async def main():
    await session("human")
    await session("ai")

asyncio.run(main())
