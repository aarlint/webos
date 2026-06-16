#!/usr/bin/env python3
"""Smoke: operator-mounted REAL filesystem reader (mount.* + files.*).

Boots one short-lived kerneld against a temp WEBOS_ROOT (sealed-file secrets
backend so there's no Keychain prompt). Two real temp trees are created OUTSIDE
the sandbox:
    <mountdir>/         <- gets mounted
        hello.txt       "hello real world"
        sub/note.md     "nested"
        link_out        -> symlink to a file in <secretdir> (escape attempt)
    <secretdir>/        <- never mounted
        secret.txt      "TOP SECRET"

Checks (human principal unless noted):
  1. files.read with NO mounts configured -> rejected (real FS disabled).
  2. mount.add the mountdir -> ok; mount.list shows the canonical path.
  3. mount.add a non-existent path -> rejected.
  4. mount.add a path already covered by the mount -> rejected.
  5. files.read <mountdir>/hello.txt -> ok, content matches, byte count set.
  6. files.list <mountdir> -> lists hello.txt + sub/ (+ the symlink entry).
  7. files.read of the SECRET file (outside all mounts) -> rejected.
  8. files.read via "<mountdir>/../<secretbasename>/secret.txt" (traversal)
     -> rejected (canonicalization escapes the mount).
  9. files.read of the symlink that points OUTSIDE the mount -> rejected
     (canonicalize follows the link out of the mount root).
 10. files.read of a file in a sibling dir whose name is a STRING-PREFIX of the
     mount (".../mnt-evil/x") -> rejected (component-wise containment).
 11. AI principal: files.read inside the mount with no grant -> ASK (an approval
     prompt is pushed, not an immediate ok); operator denies -> denied.
 12. AI principal: mount.add -> DENY ("operator-only"), even though it's a real
     path (PROTECTED, never reachable by AI).
 13. files.read size cap: a >2MiB file in the mount -> rejected.
 14. mount.remove -> ok; afterwards files.read in the (now-unmounted) tree
     -> rejected.
 15. Persistence: the mount is written to settings.json (path present, and it's
     the canonical form, and it is NOT under a "creds"/secret key).

Kills the daemon at the end. Exits non-zero on any violation.
"""
import asyncio, json, os, signal, subprocess, sys, tempfile, uuid, urllib.request
import websockets

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(REPO, "target", "debug", "kerneld")
ADDR = "127.0.0.1:8082"


def boot(root, logfile):
    env = dict(os.environ, WEBOS_ROOT=root, WEBOS_ADDR=ADDR, WEBOS_SECRETS_BACKEND="file")
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


async def bootstrap():
    for _ in range(40):
        try:
            return json.loads(urllib.request.urlopen(f"http://{ADDR}/bootstrap", timeout=2).read())
        except Exception:
            await asyncio.sleep(0.25)
    raise RuntimeError("kerneld never came up")


async def connect(token):
    return await websockets.connect(f"ws://{ADDR}/ws?token={token}")


async def call(ws, capability, args=None, want_id=None):
    """Send an invocation; return (response_for_our_id, [other_pushed_messages])."""
    mid = want_id or str(uuid.uuid4())
    await ws.send(json.dumps({"id": mid, "capability": capability, "args": args or {}}))
    others = []
    for _ in range(60):
        m = json.loads(await asyncio.wait_for(ws.recv(), timeout=10))
        if m.get("id") == mid:
            return m, others
        others.append(m)
    raise RuntimeError("no response for " + capability)


async def run(mountdir, secretdir, bigfile_rel):
    toks = await bootstrap()
    human = await connect(toks["human_token"])
    ai = await connect(toks["ai_token"])
    failures = 0

    def check(cond, ok_msg, fail_msg):
        nonlocal failures
        if cond:
            print("PASS ", ok_msg)
        else:
            failures += 1
            print("FAIL ", fail_msg)

    hello = os.path.join(mountdir, "hello.txt")
    secret = os.path.join(secretdir, "secret.txt")
    link_out = os.path.join(mountdir, "link_out")
    # canonical mount (resolve symlinks like the daemon does, e.g. /var->/private/var on macOS)
    canon_mount = os.path.realpath(mountdir)

    # 1. no mounts yet -> real FS disabled
    r, _ = await call(human, "files.read", {"path": hello})
    check(not r.get("ok"), f"files.read with no mounts rejected: {r.get('error')}",
          f"files.read should be rejected with no mounts: {r}")

    # 2. mount.add ok + appears in mount.list canonically
    r, _ = await call(human, "mount.add", {"path": mountdir})
    check(r.get("ok") and r["data"]["path"] == canon_mount,
          f"mount.add stored canonical path {canon_mount}",
          f"mount.add failed or non-canonical: {r}")
    r, _ = await call(human, "mount.list", {})
    paths = [m["path"] for m in r["data"]["mounts"]] if r.get("ok") else []
    check(canon_mount in paths, "mount.list shows the canonical mount", f"mount.list missing mount: {r}")

    # 3. mount.add non-existent path
    r, _ = await call(human, "mount.add", {"path": os.path.join(secretdir, "does-not-exist")})
    check(not r.get("ok"), f"mount.add of missing path rejected: {r.get('error')}",
          f"mount.add of missing path should fail: {r}")

    # 4. mount.add a path already covered (a subdir of the mount)
    r, _ = await call(human, "mount.add", {"path": os.path.join(mountdir, "sub")})
    check(not r.get("ok") and "already covered" in (r.get("error") or ""),
          f"mount.add of already-covered subdir rejected: {r.get('error')}",
          f"mount.add of covered subdir should fail: {r}")

    # 5. files.read inside mount ok
    r, _ = await call(human, "files.read", {"path": hello})
    check(r.get("ok") and r["data"]["content"] == "hello real world" and r["data"]["bytes"] == 16,
          "files.read inside mount returns content + byte count",
          f"files.read inside mount failed: {r}")

    # 6. files.list inside mount
    r, _ = await call(human, "files.list", {"path": mountdir})
    names = [e["name"] for e in r["data"]["entries"]] if r.get("ok") else []
    check(r.get("ok") and "hello.txt" in names and "sub" in names,
          f"files.list inside mount: {sorted(names)}",
          f"files.list inside mount failed: {r}")

    # 7. files.read of the secret (outside all mounts)
    r, _ = await call(human, "files.read", {"path": secret})
    check(not r.get("ok"), f"files.read outside mounts rejected: {r.get('error')}",
          f"files.read outside mounts MUST be rejected: {r}")

    # 8. traversal escape via ..
    traversal = os.path.join(mountdir, "..", os.path.basename(secretdir), "secret.txt")
    r, _ = await call(human, "files.read", {"path": traversal})
    check(not r.get("ok"), f"traversal (..) escape rejected: {r.get('error')}",
          f"traversal escape MUST be rejected: {r}")

    # 9. symlink escape: link_out -> secret.txt (outside the mount).
    # canonicalize() follows the link to its real (out-of-mount) target, so the
    # containment check must reject it. Also assert the secret content did NOT
    # come back.
    r, _ = await call(human, "files.read", {"path": link_out})
    leaked = r.get("ok") and "TOP SECRET" in json.dumps(r.get("data"))
    check(not r.get("ok") and not leaked, f"symlink escape rejected: {r.get('error')}",
          f"symlink escape MUST be rejected (no secret content): {r}")

    # 10. string-prefix sibling: <mountdir>-evil/x.txt
    r, _ = await call(human, "files.read", {"path": os.path.join(mountdir + "-evil", "x.txt")})
    check(not r.get("ok"), f"string-prefix sibling rejected: {r.get('error')}",
          f"string-prefix sibling MUST be rejected: {r}")

    # 11. AI files.read -> ASK (approval pushed), then operator denies -> denied
    mid = "ai-read-1"
    await ai.send(json.dumps({"id": mid, "capability": "files.read", "args": {"path": hello}}))
    # the human socket should receive an approval push; resolve it as deny
    approval_id = None
    for _ in range(40):
        m = json.loads(await asyncio.wait_for(human.recv(), timeout=10))
        if m.get("type") == "approval" and m.get("capability") == "files.read":
            approval_id = m["approvalId"]
            break
    if approval_id:
        await call(human, "approval.resolve", {"approvalId": approval_id, "verdict": "deny"})
    # now read the AI's response
    ai_resp = None
    for _ in range(40):
        m = json.loads(await asyncio.wait_for(ai.recv(), timeout=10))
        if m.get("id") == mid:
            ai_resp = m
            break
    check(approval_id is not None and ai_resp is not None and not ai_resp.get("ok"),
          "AI files.read prompts the operator (ASK) and honors deny",
          f"AI files.read should ASK then deny: approval={approval_id} resp={ai_resp}")

    # 12. AI mount.add -> operator-only deny
    r, _ = await call(ai, "mount.add", {"path": secretdir}, want_id="ai-mount-1")
    check(not r.get("ok") and "operator-only" in (r.get("error") or ""),
          f"AI mount.add denied (operator-only): {r.get('error')}",
          f"AI mount.add MUST be operator-only: {r}")

    # 13. size cap
    r, _ = await call(human, "files.read", {"path": os.path.join(mountdir, bigfile_rel)})
    check(not r.get("ok") and "too large" in (r.get("error") or ""),
          f"files.read size cap enforced: {r.get('error')}",
          f"files.read should reject >2MiB file: {r}")

    # 14. mount.remove, then read is rejected again
    r, _ = await call(human, "mount.remove", {"path": mountdir})
    check(r.get("ok"), "mount.remove ok", f"mount.remove failed: {r}")
    r, _ = await call(human, "files.read", {"path": hello})
    check(not r.get("ok"), "files.read rejected after unmount", f"read should fail after unmount: {r}")

    await human.close()
    await ai.close()
    return failures


def settings_check(root):
    """After the daemon writes settings.json, re-add a mount and confirm it
    lands in settings.json under 'mounts' (a plaintext path list, not a secret)."""
    return os.path.join(root, "settings.json")


def main():
    if not os.path.exists(BIN):
        print("FAIL  build kerneld first (cargo build)"); sys.exit(1)
    root = tempfile.mkdtemp(prefix="webos-files-root-")
    mountdir = tempfile.mkdtemp(prefix="webos-files-mnt-")
    secretdir = tempfile.mkdtemp(prefix="webos-files-secret-")
    eviltwin = mountdir + "-evil"  # string-prefix sibling of the mount
    os.makedirs(eviltwin, exist_ok=True)

    with open(os.path.join(mountdir, "hello.txt"), "w") as fh:
        fh.write("hello real world")  # 16 bytes
    os.makedirs(os.path.join(mountdir, "sub"), exist_ok=True)
    with open(os.path.join(mountdir, "sub", "note.md"), "w") as fh:
        fh.write("nested")
    with open(os.path.join(secretdir, "secret.txt"), "w") as fh:
        fh.write("TOP SECRET")
    with open(os.path.join(eviltwin, "x.txt"), "w") as fh:
        fh.write("evil")
    # symlink inside the mount pointing OUT to the secret file
    try:
        os.symlink(os.path.join(secretdir, "secret.txt"), os.path.join(mountdir, "link_out"))
    except OSError:
        pass
    # a >2MiB file to exercise the read cap
    bigfile_rel = "big.bin"
    with open(os.path.join(mountdir, bigfile_rel), "wb") as fh:
        fh.write(b"\x00" * (2 * 1024 * 1024 + 16))

    log = os.path.join(root, "boot.log")
    p, f = boot(root, log)
    failures = 1
    try:
        failures = asyncio.run(run(mountdir, secretdir, bigfile_rel))

        # 15. persistence: re-add via a fresh connection so settings.json is
        # written, then inspect the file on disk.
        async def persist():
            toks = await bootstrap()
            ws = await connect(toks["human_token"])
            r, _ = await call(ws, "mount.add", {"path": mountdir})
            await ws.close()
            return r
        rp = asyncio.run(persist())
        sp = settings_check(root)
        ok = False
        canon_mount = os.path.realpath(mountdir)
        if rp.get("ok") and os.path.exists(sp):
            with open(sp) as sf:
                doc = json.load(sf)
            mounts = doc.get("mounts", [])
            ok = canon_mount in mounts and "creds" not in doc and "credentials" not in doc
        if ok:
            print("PASS  mount persisted to settings.json as a plaintext path (no secrets)")
        else:
            failures += 1
            print(f"FAIL  mount not persisted correctly: file={sp} resp={rp}")

        # secret-leak guard: the mount path is fine in the log, but the SECRET
        # FILE CONTENTS must never appear in the daemon log.
        with open(log) as lf:
            logtext = lf.read()
        if "TOP SECRET" in logtext:
            failures += 1
            print("FAIL  secret file contents leaked into the daemon log")
        else:
            print("PASS  no real-file contents in the daemon log")
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
    print("\nall mount/files checks passed")


if __name__ == "__main__":
    main()
