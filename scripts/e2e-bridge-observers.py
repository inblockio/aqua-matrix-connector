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
not a DM). A second scenario covers a second 2-member DM with Tim (sends and
edits stay in the observed DM), a row removed and added again while the reader
is joined (no stale member snapshot) and the bridge joining a DM that already
holds the reader (mark_dm). A third scenario covers a pending invitee (a
stranger, and another allow-listed person) in the observed DM, and the deploy
guard `aqua-system-bridged --check-allowlist FILE` runs on its own. The
script prints the sha256 of the binaries it runs and fails when one is older
than the sources: rebuild before every run.
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


def landed(room, text, timeout=45, who="tim"):
    """The message `text` is in `room` as `who` (a member) sees it (read-raw of the real room)."""
    end = time.time() + timeout
    while time.time() < end:
        if text in peer_room(who, "read-raw", "--room", room, "--limit", "30"):
            return True
        time.sleep(3)
    return False


def write_allow(text):
    p = f"{E}/bridge/allowlist.toml"
    open(p, "w").write(text)
    t = time.time() + 5 * len(results) + 5  # move mtime forward so a hot reload always fires
    os.utime(p, (t, t))


def fresh(extra=()):
    """New identities and a new bridge state dir; returns (tim, reader, recipients toml)."""
    for d in ("tim", "reader", "bridge", *extra):
        shutil.rmtree(f"{E}/{d}", ignore_errors=True)
        os.makedirs(f"{E}/{d}", mode=0o700)
    open(BLOG, "w").close()
    tim, reader = whoami("tim"), whoami("reader")
    print("tim", tim, "reader", reader)
    return tim, reader, f'[[recipients]]\nname = "tim"\nmxid = "{tim}"\n'


def survey(room, since=0):
    """(joined_members, is_direct) of the connect-time survey line for `room`."""
    m = re.search(rf'joined room.*room_id={re.escape(room)}.*?joined_members=(\d+) is_direct=(\w+)', log()[since:])
    return (m.group(1), m.group(2)) if m else (None, None)


def scenario_basic():
    tim, reader, recip = fresh()

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
    check("restart: the DM with three members is still in m.direct (is_direct)", survey(room, mark) == ("2", "true"), str(survey(room, mark)))
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


def scenario_second_dm():
    """Review findings B1/F2 and B2/F1: Tim has a second clean 1:1 with the bridge."""
    tim, reader, recip = fresh()
    open(f"{E}/bridge/allowlist.toml", "w").write(recip)
    d = start_bridge()
    m = wait_log(r"mxid=(@[^\s\x1b]+)", 120)
    check("two DMs: bridge connected", m)
    wait_log(r"cycle starting", 60)
    dm1 = peer_room("tim", "create", "--invite", m.group(1), "--is-direct", "--name", "Aqua System").strip()
    check("two DMs: the bridge joined DM1", wait_log(r"joined DM invite from allow-listed user", 90))
    mark = len(log())
    dm2 = peer_room("tim", "create", "--invite", m.group(1), "--is-direct", "--name", "Aqua System 2").strip()
    check("two DMs: the bridge joined DM2", wait_log(r"joined DM invite from allow-listed user", 90, mark))
    print("dm1", dm1, "dm2", dm2)
    # DM1 is the most recently used room, so it is the connector's pick
    peer_room("tim", "post", "--room", dm1, "--text", "ping in dm1")
    wait_inbox_body("ping in dm1")
    r = send("tim", "pre-join report")
    check("two DMs: before the join a send lands in DM1 (newest)", r.get("ok") is True and landed(dm1, "pre-join report"), json.dumps(r)[:200])
    pre = re.search(r"(\$[A-Za-z0-9_\-+/=:.]+)", json.dumps(r)).group(1)

    mark = len(log())
    write_allow(recip + f'\n[[observers]]\nname = "voice-inbox"\nmxid = "{reader}"\nroom_id = "{dm1}"\n')
    sock({"op": "list_recipients"})
    check("two DMs: `observer loaded` for DM1", wait_log(rf"observer loaded.*room={re.escape(dm1)}", 30, mark))
    peer_room("tim", "invite", "--room", dm1, "--user", reader)
    check("two DMs: the reader joined DM1", dm1 in peer_room("reader", "join-invited"))
    r = send("tim", "post-join report")
    check("F2/B1: after the join the send STAYS in the observed DM1", r.get("ok") is True and landed(dm1, "post-join report"), json.dumps(r)[:200])
    check("F2/B1: and does not go to DM2", not landed(dm2, "post-join report", 8))
    er = sock({"op": "edit_message", "to": "tim", "event_id": pre, "markdown": "pre-join report, edited", "origin": "e2e"})
    check("F2/B1: an edit of a message sent BEFORE the join works", er.get("ok") is True and er["data"].get("replaces") == pre and landed(dm1, "pre-join report, edited"), json.dumps(er)[:200])
    check("F2/B1: no new DM room", "created a new DM room" not in log()[mark:])

    # a reconnect while DM1 has three members and the row is in force
    stop(d)
    mark = len(log())
    d = start_bridge()
    wait_log(r"cycle starting", 120, mark)
    check("B2/F1: reconnect with 3 members and the row: DM1 counts 2 and stays in m.direct", survey(dm1, mark) == ("2", "true"), str(survey(dm1, mark)))
    r = send("tim", "after reconnect")
    check("B2/F1: a send after the reconnect still goes to DM1", r.get("ok") is True and landed(dm1, "after reconnect"))
    peer_room("tim", "post", "--room", dm1, "--text", "tim reply after reconnect")
    check("B2/F1: Tim's reply in DM1 is ingested as a DM message", wait_inbox_body("tim reply after reconnect"))

    # the row goes away while the reader is still joined: the stale snapshot (2) must not decide
    mark = len(log())
    write_allow(recip)
    sock({"op": "list_recipients"})
    peer_room("tim", "post", "--room", dm1, "--text", "tim reply row removed")
    time.sleep(10)
    check("B2/F1: row removed, reader still in: Tim's reply in the 3-member DM1 is dropped", "tim reply row removed" not in json.dumps(inbox()))
    r = send("tim", "send while row removed")
    check("B2/F1: row removed: the send goes to the clean DM2, no new DM", r.get("ok") is True and landed(dm2, "send while row removed") and "created a new DM room" not in log()[mark:], json.dumps(r)[:200])
    # and back: the stale snapshot (3) must not decide either
    write_allow(recip + f'\n[[observers]]\nname = "voice-inbox"\nmxid = "{reader}"\nroom_id = "{dm1}"\n')
    sock({"op": "list_recipients"})
    peer_room("tim", "post", "--room", dm1, "--text", "tim reply row back")
    check("B2/F1: row back: Tim's reply in DM1 is ingested again", wait_inbox_body("tim reply row back"))
    r = send("tim", "send with row back")
    check("B2/F1: row back: sends return to DM1", r.get("ok") is True and landed(dm1, "send with row back"))

    # mark_dm accepts a DM that already holds the observer (the bridge joins late)
    stop(d)
    mark = len(log())
    bridge = m.group(1)
    dm3 = peer_room("tim", "create", "--invite", reader, "--invite", bridge, "--is-direct", "--name", "Aqua System 3").strip()
    check("mark_dm: the reader joined DM3 before the bridge", dm3 in peer_room("reader", "join-invited"))
    write_allow(recip
                + f'\n[[observers]]\nname = "voice-inbox"\nmxid = "{reader}"\nroom_id = "{dm1}"\n'
                + f'\n[[observers]]\nname = "voice-inbox-3"\nmxid = "{reader}"\nroom_id = "{dm3}"\n')
    d = start_bridge()
    check("mark_dm: the bridge joined DM3 as a DM", wait_log(rf"joined DM invite from allow-listed user", 120, mark))
    wait_log(r"cycle starting", 60, mark)
    check("mark_dm: no `mark_dm failed`", "mark_dm failed" not in log()[mark:])
    check("mark_dm: DM3 counts 2 and is in m.direct", survey(dm3, mark) == ("2", "true"), str(survey(dm3, mark)))
    stop(d)

    # B3: the bridge's own account can not be an observer (its session is persisted by now)
    mark = len(log())
    write_allow(recip + f'\n[[observers]]\nname = "self"\nmxid = "{bridge}"\nroom_id = "{dm1}"\n')
    d = start_bridge()
    try:
        rc = d.wait(timeout=60)
    except subprocess.TimeoutExpired:
        rc = None
        stop(d)
    check("B3: the bridge's own account as an observer stops the start", rc not in (None, 0) and "own account" in log()[mark:], f"rc={rc}")
    # the deploy guard sees it too, with this state dir's persisted account
    rc2, out2 = check_allowlist(f"{E}/bridge/allowlist.toml", f"{E}/bridge")
    check("check mode: refuses the bridge's own account as an observer", rc2 == 1 and "own account" in out2 and "refuses to start" in out2, out2[-200:])


def check_allowlist(path, state):
    out = subprocess.run([f"{BIN}/aqua-system-bridged", "--state-dir", state, "--check-allowlist", path],
                         capture_output=True, text=True, timeout=60, env=ENV)
    return out.returncode, out.stdout + out.stderr


def scenario_check_mode():
    """The deploy guard: `aqua-system-bridged --check-allowlist FILE` (no stack, no connection)."""
    d = f"{E}/check"
    shutil.rmtree(d, ignore_errors=True)
    os.makedirs(d, mode=0o700)
    good = ('[[recipients]]\nname = "tim"\nmxid = "@tim:localhost"\n\n[[observers]]\nname = "voice-inbox"\n'
            'mxid = "@reader:localhost"\nroom_id = "!dm:localhost"\n')
    cases = [
        ("good.toml", good, 0, "allowlist OK", "1 observers"),
        ("bad-observer.toml", good + '\n[[observers]]\nname = "x"\nmxid = "nope"\nroom_id = "!y:localhost"\n', 1, "refuses to start", "observer x"),
        ("two-in-a-room.toml", good + '\n[[observers]]\nname = "y"\nmxid = "@other:localhost"\nroom_id = "!dm:localhost"\n', 1, "refuses to start", "already has the observer"),
        ("syntax.toml", "not toml [[", 1, "fail closed", "parse error"),
        ("bad-recipient.toml", '[[recipients]]\nname = "x"\nmxid = "bad"\n', 1, "fail closed", "not a valid MXID"),
    ]
    for name, text, want_rc, want1, want2 in cases:
        p = f"{d}/{name}"
        open(p, "w").write(text)
        rc, out = check_allowlist(p, d)
        check(f"check mode: {name} exits {want_rc}", rc == want_rc and want1 in out and want2 in out, f"rc={rc} {out[-160:]!r}")
    rc, out = check_allowlist(f"{d}/missing.toml", d)
    check("check mode: a missing file exits 1", rc == 1 and "cannot read" in out, out[-120:])
    check("check mode: touches no state", sorted(os.listdir(d)) == sorted(n for n, *_ in cases), str(os.listdir(d)))


def scenario_invites():
    """Second review F1 and C1: a pending invitee knocks an observed room out of the DM."""
    tim, reader, recip = fresh(("tim2", "mallory"))
    tim2, mallory = whoami("tim2"), whoami("mallory")
    recip += f'\n[[recipients]]\nname = "tim2"\nmxid = "{tim2}"\n'
    open(f"{E}/bridge/allowlist.toml", "w").write(recip)
    d = start_bridge()
    m = wait_log(r"mxid=(@[^\s\x1b]+)", 120)
    check("invites: bridge connected", m)
    bridge = m.group(1)
    wait_log(r"cycle starting", 60)
    rooms = {}
    for who, key, name in (("tim", "dm1", "Aqua System"), ("tim", "dm2", "Aqua System 2"), ("tim2", "dmp", "Aqua System tim2")):
        mark = len(log())
        rooms[key] = peer_room(who, "create", "--invite", bridge, "--is-direct", "--name", name).strip()
        check(f"invites: the bridge joined {key}", wait_log(r"joined DM invite from allow-listed user", 90, mark))
    dm1, dm2, dmp = rooms["dm1"], rooms["dm2"], rooms["dmp"]
    print("dm1", dm1, "dm2", dm2, "dmp", dmp)
    peer_room("tim", "post", "--room", dm1, "--text", "ping in dm1")
    wait_inbox_body("ping in dm1")
    mark = len(log())
    write_allow(recip + f'\n[[observers]]\nname = "voice-inbox"\nmxid = "{reader}"\nroom_id = "{dm1}"\n')
    sock({"op": "list_recipients"})
    check("invites: `observer loaded` for DM1", wait_log(rf"observer loaded.*room={re.escape(dm1)}", 30, mark))
    peer_room("tim", "invite", "--room", dm1, "--user", reader)
    check("invites: the reader joined DM1", dm1 in peer_room("reader", "join-invited"))
    r = send("tim", "control report")
    check("invites: control: with the reader joined the send stays in DM1", r.get("ok") is True and landed(dm1, "control report"), json.dumps(r)[:160])

    # grok F1: an invited stranger
    mark = len(log())
    peer_room("tim", "invite", "--room", dm1, "--user", mallory)
    time.sleep(10)  # the bridge's sync has to deliver the invite first
    r = send("tim", "report with a pending invitee")
    check("F1: a pending invitee in the observed DM: the report does NOT go there", r.get("ok") is True and not landed(dm1, "report with a pending invitee", 8), json.dumps(r)[:160])
    check("F1: it goes to Tim's clean DM2 (no new DM)", landed(dm2, "report with a pending invitee") and "created a new DM room" not in log()[mark:])
    peer_room("tim", "post", "--room", dm1, "--text", "tim after the invite")
    time.sleep(10)
    check("F1: Tim's message in the room with a pending invitee is not ingested as a DM", "tim after the invite" not in json.dumps(inbox()))

    # re-review C1: another allow-listed person invited into the observed DM
    mark = len(log())
    peer_room("tim", "invite", "--room", dm1, "--user", tim2)
    time.sleep(10)
    r = send("tim2", "message for tim2")
    check("C1: a message to tim2 (invited into Tim's observed DM) does NOT land there", r.get("ok") is True and not landed(dm1, "message for tim2", 8), json.dumps(r)[:160])
    check("C1: it lands in tim2's own DM", landed(dmp, "message for tim2", who="tim2") and "created a new DM room" not in log()[mark:])

    # last grok pass F1: the row is removed and the reader has left, but invitees are still pending;
    # the room was observed earlier in this process, so it still counts them. Tim has no other DM.
    peer_room("reader", "leave", "--room", dm1)
    peer_room("tim", "leave", "--room", dm2)
    mark = len(log())
    write_allow(recip)
    sock({"op": "list_recipients"})
    time.sleep(10)
    r = send("tim", "send after the row is gone")
    check("F1b: row removed, reader left, invitees pending: the send does NOT go into that room", r.get("ok") is True and not landed(dm1, "send after the row is gone", 8), json.dumps(r)[:160])
    check("F1b: the bridge opened a new DM with Tim instead", "created a new DM room" in log()[mark:])
    stop(d)


def binary_report():
    """Say which binaries run, and refuse a build older than the sources."""
    import hashlib
    crates = ("aqua-system-bridged", "aqua-system-bridge", "aqua-messenger", "aqua-messenger-matrix", "aqua-matrix-agent")
    newest = max(os.path.getmtime(os.path.join(r, f)) for c in crates for r, _, fs in os.walk(f"{WT}/crates/{c}/src") for f in fs if f.endswith(".rs"))
    newest = max([newest] + [os.path.getmtime(f"{WT}/crates/aqua-system-bridged/examples/{f}") for f in os.listdir(f"{WT}/crates/aqua-system-bridged/examples")])
    for name, path in (("aqua-system-bridged", f"{BIN}/aqua-system-bridged"), ("peer_room", f"{EX}/peer_room")):
        h = hashlib.sha256(open(path, "rb").read()).hexdigest()[:16]
        stale = os.path.getmtime(path) < newest
        print(f"binary {name} sha256 {h} built {time.strftime('%Y-%m-%d %H:%M:%S', time.localtime(os.path.getmtime(path)))}{'  STALE: older than the sources' if stale else ''}")
        check(f"binary {name} is newer than every source file", not stale)


def main():
    binary_report()
    todo = sys.argv[1:] or ["check_mode", "basic", "second_dm", "invites"]
    for name in todo:
        globals()[f"scenario_{name}"]()
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
