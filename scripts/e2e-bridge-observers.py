#!/usr/bin/env python3
"""Local-stack e2e for [[observers]] in the Aqua System bridge.

Local stack only: the siwx-oidc-matrix-server e2e harness (`e2e-harness/up.sh`:
siwx-oidc on 18081, Synapse edge on 18080); the peer helpers refuse any other
URL. Build first: `cargo build -p aqua-system-bridged --examples`.
Run: `python3 -I scripts/e2e-bridge-observers.py`.

Fresh identities under $OBSERVERS_E2E_DIR (default ~/.cache/observers-e2e):
bridge (the daemon's own state), tim (a peer who plays Tim), reader (the
observer). Never touches ~/.aqua-system-bridge or any live homeserver. It
covers: an invalid observer row at start; zero observers; the observer joining
Tim's DM (sends, edits, Tim's reply ingested, the observer's own message
dropped, no new DM); a restart (survey counts 2, m.direct kept); a rejected
file keeping the running lists; and the control (row removed = 3-member room =
not a DM).
"""
import json, os, re, shutil, socket, subprocess, sys, time

HOME = os.path.expanduser("~")
E = os.environ.get("OBSERVERS_E2E_DIR", f"{HOME}/.cache/observers-e2e")
WT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = f"{WT}/target/debug"
EX = f"{BIN}/examples"
SIWX, MX = "http://localhost:18081", "http://localhost:18080"
results, procs = [], []
ENV = {**os.environ, "NO_COLOR": "1", "RUST_LOG": "info,matrix_sdk=warn,matrix_sdk_crypto=warn,matrix_sdk_base=warn"}
BLOG = f"{E}/bridge.log"


def check(name, ok, detail=""):
    results.append((name, bool(ok)))
    print(f"[{'PASS' if ok else 'FAIL'}] {name} {detail}", flush=True)


def ident(who):
    return ["--key-file", f"{E}/{who}/{who}.pem", "--store-dir", f"{E}/{who}/store",
            "--siwx-url", SIWX, "--matrix-url", MX]


def peer_room(who, *args):
    out = subprocess.run([f"{EX}/peer_room", *ident(who), *args], capture_output=True, text=True, timeout=240)
    if out.returncode != 0:
        print(out.stderr[-2000:])
        raise SystemExit(f"peer_room {who} failed: {args}")
    return out.stdout


def whoami(who):
    out = subprocess.run([f"{EX}/peer_send_file", *ident(who), "--whoami"], capture_output=True, text=True, timeout=240)
    if out.returncode != 0:
        print(out.stderr[-2000:])
        raise SystemExit(f"whoami {who} failed")
    return re.search(r"mxid (\S+)", out.stdout).group(1)


def sock(req):
    s = socket.socket(socket.AF_UNIX)
    s.connect(f"{E}/bridge/bridge.sock")
    s.sendall((json.dumps(req) + "\n").encode())
    buf = b""
    while True:
        c = s.recv(65536)
        if not c:
            break
        buf += c
    s.close()
    return json.loads(buf.decode().splitlines()[0])


def log():
    return open(BLOG, errors="replace").read() if os.path.exists(BLOG) else ""


def wait_log(pattern, timeout=60, since=0):
    end = time.time() + timeout
    while time.time() < end:
        m = re.search(pattern, log()[since:], re.M)
        if m:
            return m
        time.sleep(1)
    return None


def start_bridge():
    f = open(BLOG, "a")
    d = subprocess.Popen([f"{BIN}/aqua-system-bridged", "--state-dir", f"{E}/bridge", "--siwx-url", SIWX,
                          "--matrix-url", MX, "--display-name", "E2E Bridge"], stdout=f, stderr=f, env=ENV)
    procs.append(d)
    return d


def stop(p):
    p.terminate()
    try:
        p.wait(timeout=30)
    except subprocess.TimeoutExpired:
        p.kill()


def send(to, md):
    r = sock({"op": "send_message", "to": to, "markdown": md, "origin": "e2e"})
    return r


def inbox(frm=None):
    req = {"op": "read_inbox", "mark_read": False, "since_seq": 0}
    if frm:
        req["from"] = frm
    return sock(req)


def wait_inbox_body(body, frm="tim", timeout=60):
    end = time.time() + timeout
    while time.time() < end:
        r = inbox(frm)
        if body in json.dumps(r):
            return True
        time.sleep(2)
    return False


def landed(room, text, timeout=45):
    """The message `text` is in `room` as Tim's peer sees it (read-raw of the real room)."""
    end = time.time() + timeout
    while time.time() < end:
        if text in peer_room("tim", "read-raw", "--room", room, "--limit", "30"):
            return True
        time.sleep(3)
    return False


def write_allow(text):
    p = f"{E}/bridge/allowlist.toml"
    open(p, "w").write(text)
    t = time.time() + 5 * len(results) + 5  # move mtime forward so a hot reload always fires
    os.utime(p, (t, t))


def main():
    for d in ("tim", "reader", "bridge"):
        shutil.rmtree(f"{E}/{d}", ignore_errors=True)
        os.makedirs(f"{E}/{d}", mode=0o700)
    open(BLOG, "w").close()
    tim, reader = whoami("tim"), whoami("reader")
    print("tim", tim, "reader", reader)
    recip = f'[[recipients]]\nname = "tim"\nmxid = "{tim}"\n'

    # ---- Phase 0: invalid observer at START is a start error -----------------
    bad_start = recip + '\n[[observers]]\nname = "voice-inbox"\nmxid = "not-an-mxid"\nroom_id = "!x:localhost"\n'
    open(f"{E}/bridge/allowlist.toml", "w").write(bad_start)
    d = start_bridge()
    try:
        rc = d.wait(timeout=60)
    except subprocess.TimeoutExpired:
        rc = None
        stop(d)
    check("start: an invalid observer row stops the daemon", rc not in (None, 0), f"rc={rc}")
    check("start: the journal names the row", "allow-list rejected: observer voice-inbox" in log())

    # ---- Phase 1: zero observers = today -------------------------------------
    open(f"{E}/bridge/allowlist.toml", "w").write(recip)
    mark = len(log())
    d = start_bridge()
    m = wait_log(r"mxid=(@[^\s\x1b]+)", 120, mark)
    check("zero observers: bridge connected", m)
    bridge = m.group(1)
    wait_log(r"cycle starting", 60, mark)
    room = peer_room("tim", "create", "--invite", bridge, "--is-direct", "--name", "Aqua System").strip()
    print("dm", room)
    check("zero observers: bridge joined the DM", wait_log(r"joined DM invite from allow-listed user", 90, mark))
    r = send("tim", "first report")
    txt = json.dumps(r)
    check("zero observers: send reaches Tim in the DM", r.get("ok") is True and landed(room, "first report"), txt[:200])
    check("zero observers: no new DM room", "created a new DM room" not in log())
    event_id = re.search(r"(\$[A-Za-z0-9_\-+/=:.]+)", txt)
    check("zero observers: the send names an event id", event_id, txt[:200])

    # ---- Phase 2: the observer joins Tim's DM ---------------------------------
    allow_obs = recip + f'\n[[observers]]\nname = "voice-inbox"\nmxid = "{reader}"\nroom_id = "{room}"\nnote = "e2e"\n'
    mark = len(log())
    write_allow(allow_obs)
    c = send("tim", "hot reload tick")  # any request reloads the file
    check("observer: `observer loaded` is logged on reload", wait_log(rf"observer loaded.*name=voice-inbox.*room={re.escape(room)}", 30, mark))
    peer_room("tim", "invite", "--room", room, "--user", reader)
    joined = peer_room("reader", "join-invited")
    check("observer: the reader joined Tim's DM", room in joined, joined.strip())
    members = peer_room("tim", "members", "--room", room).split()
    check("observer: the room now has three joined members", len(members) == 3, str(members))
    mark = len(log())
    r = send("tim", "second report, three members")
    txt = json.dumps(r)
    check("observer: send still lands in Tim's DM", r.get("ok") is True and landed(room, "second report, three members"), txt[:200])
    check("observer: no new DM room was created", "created a new DM room" not in log()[mark:])
    eid = re.search(r"(\$[A-Za-z0-9_\-+/=:.]+)", txt)
    if eid:
        er = sock({"op": "edit_message", "to": "tim", "event_id": eid.group(1), "markdown": "second report, edited", "origin": "e2e"})
        check("observer: an edit by event id works (replaces in place)", er.get("ok") is True and er["data"].get("replaces") == eid.group(1) and landed(room, "second report, edited"), json.dumps(er)[:200])
    peer_room("tim", "post", "--room", room, "--text", "tim reply with observer")
    check("observer: Tim's reply is ingested (read_inbox from tim)", wait_inbox_body("tim reply with observer"))
    peer_room("reader", "post", "--room", room, "--text", "observer chatter")
    time.sleep(8)
    check("observer: the observer's own message is NOT in the inbox", "observer chatter" not in json.dumps(inbox()))

    # ---- Phase 3: a restart survey/repair keeps the DM -----------------------
    stop(d)
    mark = len(log())
    d = start_bridge()
    wait_log(r"cycle starting", 120, mark)
    check("restart: m.direct repair removed nothing", "m.direct repair: removed" not in log()[mark:])
    joined_line = re.search(rf'joined room.*room_id={re.escape(room)}.*joined_members=(\d+)', log()[mark:])
    check("restart: the survey counts the DM as 2", joined_line and joined_line.group(1) == "2", joined_line.group(0) if joined_line else "no survey line")
    r = send("tim", "third report after restart")
    check("restart: send still lands in the same DM", r.get("ok") is True and landed(room, "third report after restart") and "created a new DM room" not in log()[mark:], json.dumps(r)[:200])
    peer_room("tim", "post", "--room", room, "--text", "tim reply after restart")
    check("restart: Tim's reply after restart is ingested", wait_inbox_body("tim reply after restart"))

    # ---- Phase 4: a rejected observer row keeps the running lists ------------
    mark = len(log())
    rejected = allow_obs + '\n[[recipients]]\nname = "mallory"\nmxid = "@mallory:localhost"\n\n[[observers]]\nname = "dup"\nmxid = "' + tim + '"\nroom_id = "!x:localhost"\n'
    write_allow(rejected)
    lr = sock({"op": "list_recipients"})
    names = json.dumps(lr)
    check("rejected row: mallory was NOT added, tim still listed", "mallory" not in names and '"tim"' in names, names[:300])
    check("rejected row: the reason is reported", "observer dup" in names)
    check("rejected row: journal line", wait_log(r"allow-list rejected: observer dup", 20, mark))
    audit = open(f"{E}/bridge/audit.jsonl").read()
    check("rejected row: audit row written", '"allowlist_rejected"' in audit)
    r = send("tim", "fourth report during a rejected file")
    check("rejected row: Tim is still reachable in his DM", r.get("ok") is True and landed(room, "fourth report during a rejected file") and "created a new DM room" not in log()[mark:])
    write_allow(allow_obs)
    lr = json.dumps(sock({"op": "list_recipients"}))
    check("rejected row: the fixed file clears the error", "observer dup" not in lr)

    # ---- Phase 5: negative control - the same room without the observer row ---
    mark = len(log())
    write_allow(recip)
    r = send("tim", "fifth report, observer row removed")
    check("control: with the row removed the 3-member room is not Tim's DM (new DM created)", "created a new DM room" in log()[mark:])

    stop(d)
    bad = [n for n, ok in results if not ok]
    print(f"\n{len(results) - len(bad)}/{len(results)} passed")
    if bad:
        print("FAILED:", *bad, sep="\n  ")
    return 1 if bad else 0


if __name__ == "__main__":
    try:
        rc = main()
    finally:
        for p in procs:
            if p.poll() is None:
                stop(p)
    sys.exit(rc)
