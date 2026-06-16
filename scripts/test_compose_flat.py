#!/usr/bin/env python3
"""Smoke: ai.compose must return a FLAT json-render Surface ({root,elements})
with catalog-valid component types and resolvable child references.

Works whether the model produced the Surface or the deterministic fallback did
(both paths now emit the flat shape). Exits non-zero on any violation.
"""
import asyncio, json, sys, uuid, urllib.request
import websockets

# Mirror of ALLOWED_COMPONENTS in src/model.rs / the catalog in ui/src/surface.tsx.
CATALOG = {
    "Stack", "Row", "Grid", "Card", "Heading", "Text", "Metric", "Badge", "Progress",
    "KeyValue", "Icon", "Table", "Detail", "Chart", "Sparkline", "Input", "Toggle", "Button",
}

INTENTS = [
    "show me weather please",
    "show octocat's public repos as a sortable table",
    "make a dashboard of recent activity",
]


async def call(ws, capability, args=None):
    mid = str(uuid.uuid4())
    await ws.send(json.dumps({"id": mid, "capability": capability, "args": args or {}}))
    for _ in range(60):  # model cold-load can be slow
        m = json.loads(await asyncio.wait_for(ws.recv(), timeout=80))
        if m.get("id") == mid:
            return m
    raise RuntimeError("no response for " + capability)


def assert_flat_spec(s):
    assert isinstance(s, dict), f"surface is not an object: {type(s)}"
    root = s.get("root")
    elements = s.get("elements")
    assert isinstance(root, str) and root, f"missing/empty root: {root!r}"
    assert isinstance(elements, dict) and elements, f"missing/empty elements: {elements!r}"
    assert "widget" not in s, "legacy 'widget' key leaked into a flat spec"
    assert root in elements, f"root '{root}' not present in elements"
    for key, el in elements.items():
        t = el.get("type")
        assert t in CATALOG, f"element '{key}' has non-catalog type {t!r}"
        for child in (el.get("children") or []):
            assert child in elements, f"element '{key}' references missing child '{child}'"


async def session():
    tok = json.loads(urllib.request.urlopen("http://127.0.0.1:8080/bootstrap", timeout=5).read())
    url = f"ws://127.0.0.1:8080/ws?token={tok['human_token']}"
    for _ in range(30):
        try:
            ws = await websockets.connect(url)
            break
        except OSError:
            await asyncio.sleep(0.3)
    else:
        print("FAIL: could not connect to kerneld")
        sys.exit(1)

    failures = 0
    async with ws:
        for intent in INTENTS:
            m = await call(ws, "ai.compose", {"intent": intent})
            ok = m.get("ok")
            s = m.get("data", {})
            try:
                assert ok, f"ai.compose ok=false: {m.get('error')}"
                assert_flat_spec(s)
                comps = sorted({el.get("type") for el in s["elements"].values()})
                print(f"PASS  intent={intent!r:50}  id={s.get('id')}  root={s.get('root')}  types={comps}")
            except AssertionError as e:
                failures += 1
                print(f"FAIL  intent={intent!r}: {e}")
                print(json.dumps(s, indent=1)[:600])

        # Edit mode (builder drag): context.add must also yield a flat spec.
        add = {"connector": "x", "op": "y", "args": {}, "items": "", "path": "name", "label": "Name"}
        m = await call(ws, "ai.compose", {"intent": "Add field name", "context": {"surface": None, "add": add}})
        s = m.get("data", {})
        try:
            assert m.get("ok"), f"edit-mode ai.compose ok=false: {m.get('error')}"
            assert_flat_spec(s)
            print(f"PASS  edit-mode merge   id={s.get('id')}  root={s.get('root')}")
        except AssertionError as e:
            failures += 1
            print(f"FAIL  edit-mode merge: {e}")
            print(json.dumps(s, indent=1)[:600])

    if failures:
        print(f"\n{failures} check(s) FAILED")
        sys.exit(1)
    print("\nall flat-spec checks passed")


asyncio.run(session())
