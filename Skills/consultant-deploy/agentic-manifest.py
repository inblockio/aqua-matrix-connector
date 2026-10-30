#!/usr/bin/env python3
# The run shape of a NEW consultant on agentic, in the format aqua-ops tools/host-move/carry.py writes for the agents it
# moved (so agentic's agents.py place / run / logs / stop treat it like any of them):
#   agentic-manifest.py --out DIR --name NAME --image IMAGE --home HOME
#                       [--mount SRC:DST:ro|rw ...] [--env NAME ...]
# writes DIR/NAME.json (the shape) and DIR/NAME.env (0600, NAME=value lines for docker --env-file). SRC, and what it
# resolves to, must lie below HOME (agentic mirrors nuc10's home below /srv/agents/home); a ro mount belongs to deploy
# (1000), a rw mount to the image's agent user (1001). The limits and caps are the ones spawn-consultant.sh gives podman (2 GiB,
# 2 CPUs, 512 pids, every capability dropped, no-new-privileges, /tmp on tmpfs); user, workdir and entrypoint stay the
# image's own. --env values come from this process's environment, never argv; a name that is unset is left out.
# Prints the names and counts, never a value.
import argparse, json, os, re, sys

AGENT_UID = 1001  # the image's `agent` user: the owner of the persist trees on agentic
DEPLOY_UID = 1000
# podman's default capability set: a manifest dropping all of it makes agents.py pass --cap-drop ALL
PODMAN_DEFAULT_CAPS = ["CAP_CHOWN", "CAP_DAC_OVERRIDE", "CAP_FOWNER", "CAP_FSETID", "CAP_KILL", "CAP_NET_BIND_SERVICE",
                       "CAP_SETFCAP", "CAP_SETGID", "CAP_SETPCAP", "CAP_SETUID", "CAP_SYS_CHROOT"]


def fail(msg):
    print(f"!! agentic-manifest: {msg}", file=sys.stderr)
    sys.exit(2)


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--out", required=True)
    p.add_argument("--name", required=True)
    p.add_argument("--image", required=True)
    p.add_argument("--home", required=True)
    p.add_argument("--mount", action="append", default=[])
    p.add_argument("--env", action="append", default=[])
    a = p.parse_args()
    if not re.fullmatch(r"aqua-agent-[a-z0-9][a-z0-9_.-]{1,70}", a.name):
        fail(f"bad container name {a.name!r}")
    home = os.path.realpath(a.home)
    mounts = []
    for m in a.mount:
        src, dst, mode = m.rsplit(":", 2) if m.count(":") >= 2 else (m, "", "")
        if mode not in ("ro", "rw") or not dst.startswith("/"):
            fail(f"mount {m!r} is not SRC:DST:ro|rw")
        path, real = os.path.abspath(src), os.path.realpath(src)
        if not (path.startswith(home + "/") and real.startswith(home + "/")) or not os.path.exists(real):
            fail(f"mount source {src!r} is missing or not below {home}")
        mounts.append({"dir": os.path.isdir(real), "dst": dst, "other_owners": 0,
                       "owner": DEPLOY_UID if mode == "ro" else AGENT_UID, "ro": mode == "ro",
                       "src": os.path.relpath(path, home)})
    env = {}
    for n in a.env:
        if not re.fullmatch(r"[A-Z_][A-Z0-9_]*", n):
            fail(f"bad env name {n!r}")
        v = os.environ.get(n)
        if v is None:
            continue
        if "\n" in v or "\r" in v:
            fail(f"env {n} holds a line break")
        env[n] = v
    shape = {"cap_add": [], "cap_drop": PODMAN_DEFAULT_CAPS, "cmd": [], "entrypoint": [], "env_names": list(env),
             "image": a.image, "memory": 2147483648, "mounts": mounts, "name": a.name, "nano_cpus": 2000000000,
             "networks": [], "pids": 512, "security_opt": ["no-new-privileges"], "tmpfs": ["/tmp"], "user": "",
             "userns_keep_id": False, "workdir": ""}
    os.makedirs(a.out, mode=0o700, exist_ok=True)
    with open(f"{a.out}/{a.name}.json", "w") as f:
        json.dump(shape, f, indent=1, sort_keys=True)
        f.write("\n")
    envf = f"{a.out}/{a.name}.env"
    fd = os.open(envf, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as f:
        f.write("".join(f"{k}={v}\n" for k, v in env.items()))
    os.chmod(envf, 0o600)
    print(f">> agentic manifest: {a.out}/{a.name}.json, mounts={len(mounts)} env_names={','.join(env) or '-'}")


main()
