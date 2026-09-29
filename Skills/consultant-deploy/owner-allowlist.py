#!/usr/bin/env python3
"""owner-allowlist.py, make sure a consultant's Owner is on the Aqua System allow-list.

Every consultant has exactly ONE authoritative Owner: its single target peer (--target, or the
kept config's target; for --generic that is Tim). Tim's standing rule (2026-09-29): the Owner is
always on ~/.aqua-system-bridge/allowlist.toml, so the Aqua System identity can always reach it.
The automatic addition covers Owners only, nothing else.

Usage:
  owner-allowlist.py check|apply --path FILE --mxid OWNER --container NAME --who TEXT
                     (--label LABEL | --generic)

  check  read-only preview for --print-run / --print-onboarding: prints what would be added,
         or "already present", or why a real spawn would abort. Always exits 0.
  apply  the real step (spawn-consultant.sh, before any container change). Exit 0 = the Owner
         is on the list (already, or added now); exit 1 = could not ensure it (fail closed).

The bridge daemon (crates/aqua-system-bridge/src/allowlist.rs, parse_all) rejects the WHOLE file
on one bad entry, which silences the bridge, so every write is validated against the same rules
before it replaces the file:
  - [[recipients]]: name non-empty after trim; mxid structurally @localpart:server, no whitespace
  - [[rooms]]: name non-empty, not starting with @ or !; room_id !opaque:server; room ids unique
  - names unique across recipients AND rooms (trimmed, ASCII case-insensitive)
MXIDs compare ASCII case-insensitively (the bridge's eq_ignore_ascii_case; Synapse lowercases
localparts).

Write discipline: flock on "<file>.lock"; backup "<file>.bak-<label>-<ts>" (only on change);
new content goes to a temp file in the same dir (mode 600), is parsed with tomllib and checked
against the rules above (plus: old entries unchanged, exactly one new entry), then renamed over
the file atomically. The daemon hot-reloads on mtime change.
"""
import argparse
import datetime
import fcntl
import json
import os
import re
import shutil
import sys
import tempfile
import time
import tomllib

TAG = "owner allow-list"
LOCK_WAIT_S = 60


def say(msg):
    print(f">> {TAG}: {msg}", file=sys.stderr)


def shout(msg):
    print(f"!! {TAG}: {msg}", file=sys.stderr)


def ascii_lower(s):
    return "".join(c.lower() if c.isascii() else c for c in s)


def is_valid_mxid(s):
    if not isinstance(s, str) or not s.startswith("@") or ":" not in s[1:]:
        return False
    local, server = s[1:].split(":", 1)
    return bool(local) and bool(server) and not any(c.isspace() for c in s)


def is_valid_room_id(s):
    if not isinstance(s, str) or not s.startswith("!") or ":" not in s[1:]:
        return False
    local, server = s[1:].split(":", 1)
    return bool(local) and bool(server) and not any(c.isspace() for c in s)


def validate(text):
    """Parse like the bridge's parse_all. Returns (recipients, rooms) or raises ValueError."""
    try:
        data = tomllib.loads(text)
    except tomllib.TOMLDecodeError as e:
        raise ValueError(f"TOML parse error: {e}")
    recipients = data.get("recipients", [])
    rooms = data.get("rooms", [])
    if not isinstance(recipients, list) or not isinstance(rooms, list):
        raise ValueError("recipients/rooms must be arrays of tables")
    seen = set()
    for r in recipients:
        if not isinstance(r, dict) or not isinstance(r.get("name"), str) or not isinstance(r.get("mxid"), str):
            raise ValueError(f"[[recipients]] entry {r!r} needs string name and mxid")
        if "note" in r and not isinstance(r["note"], str):
            raise ValueError(f"[[recipients]] {r['name']!r}: note must be a string")
        if not r["name"].strip():
            raise ValueError(f"allow-list entry for {r['mxid']} has an empty name")
        if not is_valid_mxid(r["mxid"]):
            raise ValueError(f"allow-list entry {r['name']!r}: {r['mxid']!r} is not a valid MXID")
        key = ascii_lower(r["name"].strip())
        if key in seen:
            raise ValueError(f"allow-list has duplicate name {r['name']!r}")
        seen.add(key)
    seen_ids = set()
    for r in rooms:
        if not isinstance(r, dict) or not isinstance(r.get("name"), str) or not isinstance(r.get("room_id"), str):
            raise ValueError(f"[[rooms]] entry {r!r} needs string name and room_id")
        name = r["name"].strip()
        if not name:
            raise ValueError(f"[[rooms]] entry for {r['room_id']} has an empty name")
        if name.startswith("@") or name.startswith("!"):
            raise ValueError(f"[[rooms]] name {r['name']!r} must be a short handle")
        if not is_valid_room_id(r["room_id"].strip()):
            raise ValueError(f"[[rooms]] entry {r['name']!r}: {r['room_id']!r} is not a valid room id")
        if ascii_lower(name) in seen:
            raise ValueError(f"allow-list has duplicate name {r['name']!r}")
        seen.add(ascii_lower(name))
        if r["room_id"].strip() in seen_ids:
            raise ValueError(f"[[rooms]] lists room {r['room_id']!r} twice")
        seen_ids.add(r["room_id"].strip())
    return recipients, rooms


def toml_str(s):
    # JSON basic-string escapes (\" \\ \n \uXXXX) are valid TOML basic-string escapes.
    return json.dumps(s, ensure_ascii=False)


def insert_entry(text, block):
    """Insert the new [[recipients]] block right after the last existing [[recipients]] entry
    (before the next table header and the comments/blank lines that lead into it), so the
    array stays contiguous; with no recipients yet, append at the end."""
    lines = text.splitlines(keepends=True)
    if lines and not lines[-1].endswith("\n"):
        lines[-1] += "\n"
    header = re.compile(r"^\s*\[")
    last_rcpt = None
    for i, l in enumerate(lines):
        if re.match(r"^\s*\[\[\s*recipients\s*\]\]", l):
            last_rcpt = i
    at = len(lines)
    if last_rcpt is not None:
        for j in range(last_rcpt + 1, len(lines)):
            if header.match(lines[j]):
                at = j
                while at > last_rcpt + 1 and (not lines[at - 1].strip() or lines[at - 1].lstrip().startswith("#")):
                    at -= 1
                break
    new = lines[:at]
    if new and new[-1].strip():
        new.append("\n")
    new.append(block)
    rest = lines[at:]
    if rest and rest[0].strip():
        new.append("\n")
    return "".join(new + rest)


def plan(text, a):
    """Decide against the current text. Returns ("present", name) | ("add", name) | raises."""
    recipients, rooms = validate(text)
    owner = ascii_lower(a.mxid)
    for r in recipients:
        if ascii_lower(r["mxid"]) == owner:
            return "present", r["name"]
    if a.generic:
        raise ValueError(f"the operator {a.mxid} (Owner of the generic consultant) is not on the list; "
                         "Tim is always allow-listed, add him by hand, never automatically")
    taken = {ascii_lower(r["name"].strip()) for r in recipients} | {ascii_lower(r["name"].strip()) for r in rooms}
    for cand in (a.label, f"{a.label}-owner"):
        if ascii_lower(cand) not in taken:
            return "add", cand
    raise ValueError(f"names {a.label!r} and {a.label + '-owner'!r} are both taken by other entries; "
                     "add the Owner by hand with a free name")


def entry_block(name, a):
    note = f"Owner of {a.container} ({a.who}), auto-added by spawn-consultant.sh {datetime.date.today().isoformat()}"
    return f"[[recipients]]\nname = {toml_str(name)}\nmxid = {toml_str(a.mxid)}\nnote = {toml_str(note)}\n"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("mode", choices=["check", "apply"])
    ap.add_argument("--path", required=True)
    ap.add_argument("--mxid", required=True)
    ap.add_argument("--container", required=True)
    ap.add_argument("--who", default="")
    g = ap.add_mutually_exclusive_group(required=True)
    g.add_argument("--label")
    g.add_argument("--generic", action="store_true")
    a = ap.parse_args()
    dry = a.mode == "check"
    abort = "a real spawn would ABORT" if dry else "ABORTING"

    def fail(msg):
        shout(f"{msg} ({abort})")
        sys.exit(0 if dry else 1)

    if not is_valid_mxid(a.mxid):
        fail(f"Owner {a.mxid!r} is not an MXID")
    path = a.path
    d = os.path.dirname(os.path.abspath(path))

    lock = None
    if not dry:
        try:
            lock = open(path + ".lock", "a")
            os.chmod(path + ".lock", 0o600)
            deadline = time.monotonic() + LOCK_WAIT_S
            while True:
                try:
                    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    break
                except BlockingIOError:
                    if time.monotonic() > deadline:
                        fail(f"could not lock {path}.lock within {LOCK_WAIT_S}s")
                    time.sleep(0.2)
        except OSError as e:
            fail(f"cannot open lock file {path}.lock: {e}")

    try:
        with open(path, encoding="utf-8") as f:
            text = f.read()
    except OSError as e:
        fail(f"cannot read {path}: {e.strerror or e}")
    try:
        verdict, name = plan(text, a)
    except ValueError as e:
        fail(f"{path}: {e}")

    if verdict == "present":
        say(f"Owner {a.mxid} already present as {name!r} in {path}, nothing to do")
        return
    block = entry_block(name, a)
    if dry:
        say(f"would add to {path} (a real spawn writes it):\n" + "".join("     " + l for l in block.splitlines(True)))
        return

    new_text = insert_entry(text, block)
    try:
        old_r, old_rooms = validate(text)
        new_r, new_rooms = validate(new_text)
        if new_rooms != old_rooms or len(new_r) != len(old_r) + 1:
            raise ValueError("unexpected change in the existing entries")
        added = [r for r in new_r if r not in old_r]
        if len(added) != 1 or added[0]["mxid"] != a.mxid or added[0]["name"] != name:
            raise ValueError("the new entry did not come out as intended")
    except ValueError as e:
        fail(f"refusing to write, the result would not validate: {e}")

    ts = time.strftime("%Y%m%dT%H%M%S")
    backup = f"{path}.bak-{a.label}-{ts}"
    tmp = None
    try:
        shutil.copy2(path, backup)
        os.chmod(backup, 0o600)
        fd, tmp = tempfile.mkstemp(dir=d, prefix=".allowlist.", suffix=".tmp")
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            os.fchmod(f.fileno(), 0o600)
            f.write(new_text)
            f.flush()
            os.fsync(f.fileno())
        with open(tmp, encoding="utf-8") as f:
            validate(f.read())
        os.replace(tmp, path)
        tmp = None
        with open(path, encoding="utf-8") as f:
            if plan(f.read(), a)[0] != "present":
                raise ValueError("Owner not found after the write")
    except (OSError, ValueError) as e:
        if tmp and os.path.exists(tmp):
            os.unlink(tmp)
        if os.path.exists(backup):
            try:
                with open(path, encoding="utf-8") as f:
                    validate(f.read())
            except (OSError, ValueError):
                shutil.copy2(backup, path)
                shout(f"restored {path} from {backup}")
        fail(f"write failed: {e}")
    say(f"added Owner {a.mxid} as {name!r} to {path} (backup {backup})")


if __name__ == "__main__":
    main()
