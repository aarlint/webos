#!/usr/bin/env python3
"""Exercise the chat agent headlessly: a human client auto-approves tool consent."""
import asyncio, json, uuid, urllib.request, sys, time
import websockets

tok = json.loads(urllib.request.urlopen("http://127.0.0.1:8080/bootstrap", timeout=3).read())
prompt = sys.argv[1] if len(sys.argv) > 1 else "How many public repos does octocat have on GitHub? List their names and stars."

async def main():
    async with websockets.connect(f"ws://127.0.0.1:8080/ws?token={tok['human_token']}") as ws:
        pending = {}
        async def reader():
            async for raw in ws:
                m = json.loads(raw)
                if m.get("type") == "approval":
                    print(f"  [auto-approve] {(m.get('conn') or {}).get('connector')}.{(m.get('conn') or {}).get('op')} ({(m.get('conn') or {}).get('class')})")
                    await ws.send(json.dumps({"id": str(uuid.uuid4()), "capability": "approval.resolve",
                        "args": {"approvalId": m["approvalId"], "verdict": "allow_always",
                                 "grantKey": m.get("grantKey"), "capability": m.get("capability")}}))
                    continue
                fut = pending.pop(m.get("id"), None)
                if fut and not fut.done(): fut.set_result(m)
        rt = asyncio.create_task(reader())
        mid = str(uuid.uuid4()); fut = asyncio.get_event_loop().create_future(); pending[mid] = fut
        t0 = time.monotonic()
        await ws.send(json.dumps({"id": mid, "capability": "chat.send",
            "args": {"messages": [{"role": "user", "content": prompt}]}}))
        m = await asyncio.wait_for(fut, timeout=180)
        print(f"\n  elapsed: {time.monotonic()-t0:.1f}s | ok: {m['ok']}")
        if m["ok"]:
            print("  reply:", (m["data"].get("reply") or "")[:600])
            print("  surfaces:", m["data"].get("surfaces"))
        else:
            print("  error:", m.get("error"))
        rt.cancel()

asyncio.run(main())
