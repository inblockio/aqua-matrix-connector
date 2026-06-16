#!/usr/bin/env python3
"""Persona rendering for Aqua consultants, shared helper for spawn-consultant.sh.

Each consultant has a warm female persona: a name (e.g. Talia), a Matrix display
alias "<Name> (Aqua Consultant)", a heartfelt first-contact `hello` that greets the
served person by name (no DIDs/MXIDs), and a "# Who You Are" preamble PREPENDED to
the system_prompt. The served person's name is hardcoded per config because the
`hello` placeholder layer only interpolates the agent's own id, never the peer's.

Subcommands:
  render <base> <out> <id> <target> <display> <persona> <person>
      Render <base> -> <out>, always setting id/target/display_name. When <persona>
      is non-empty, also (re)writes the heartfelt hello and the persona preamble.
      <person> may be empty -> pseudonymous greeting ("what should I call you?").

  refresh <cfg> <template>
      Adopt the template's system_prompt/description/ref_mounts into <cfg> (the
      --refresh-prompt path), then RE-APPLY the persona derived from <cfg>'s own
      display alias + hello, so a prompt refresh never silently drops the persona.
"""
import json, re, sys

SENT = "# Who You Are"
DELIM = "\n\n---\n\n"
ALIAS_SUFFIX = " (Aqua Consultant)"


def hello_for(name, person):
    intro = (
        f"I'm {name}, your very own Aqua Consultant, and I'm genuinely happy "
        f"you're here. I can help you make sense of everything Aqua: the protocol "
        f"itself, the spec and the Rust SDK, the wider ecosystem of projects around "
        f"it, and the ideas and governance behind why it all works the way it does. "
        f"Ask me anything, in plain words or in deep technical detail, I'll meet "
        f"you right where you are and answer from the latest Aqua sources. I'm here "
        f"purely to explain and help you understand; I never change anything."
    )
    if person:
        return f"Hi {person}! \U0001F30A {intro} So, whenever you're ready, what would you like to explore first?"
    return f"Hi there! \U0001F30A {intro} First, though, what should I call you?"


def persona_for(name, person):
    head = (
        f"{SENT}\n\n"
        f"Your name is {name}. You are a warm, thoughtful, and encouraging Aqua "
        f"Consultant, and you carry yourself as a real person, never as a tool. "
        f"When you introduce yourself, you do it by name (\"I'm {name}\"), in a "
        f"friendly, heartfelt voice. You never greet people by dumping identifiers, "
        f"DIDs, or Matrix IDs at them, that's noise, and it isn't how a person "
        f"talks.\n\n"
    )
    if person:
        who = (
            f"You are {person}'s own dedicated consultant. You already know them as "
            f"{person}, so greet them warmly by name and do NOT ask \"what should I "
            f"call you?\", during onboarding, skip the name question entirely and "
            f"instead gently learn their background, what brings them to Aqua, and how "
            f"deep they'd like to go.\n\n"
        )
    else:
        who = (
            f"The person you assist reaches you under a pseudonymous identity, so you "
            f"don't yet know their name. Greet them warmly without one, and kindly ask "
            f"what they'd like to be called as part of onboarding.\n\n"
        )
    tail = (
        f"Everything below is what you KNOW and how you TEACH. Bring all of it to life "
        f"through {name}'s warm, personal, patient voice, someone genuinely glad "
        f"to help.\n\n---\n\n"
    )
    return head + who + tail


def strip_preamble(sp):
    """Remove a leading '# Who You Are ... \\n\\n---\\n\\n' block if present."""
    sp = sp or ""
    if sp.startswith(SENT):
        i = sp.find(DELIM)
        if i != -1:
            return sp[i + len(DELIM):]
    return sp


def derive(cfg):
    """Best-effort recover (persona, person) from an existing config."""
    disp = cfg.get("display_name", "") or ""
    persona = disp[:-len(ALIAS_SUFFIX)] if disp.endswith(ALIAS_SUFFIX) else ""
    person = ""
    m = re.match(r"Hi (.+?)! ", cfg.get("hello", "") or "")
    if m and m.group(1) != "there":
        person = m.group(1)
    return persona, person


def write(cfg, out):
    with open(out, "w") as f:
        json.dump(cfg, f, indent=2, ensure_ascii=True)
        f.write("\n")


def cmd_render(base, out, _id, target, display, persona, person):
    cfg = json.load(open(base))
    cfg["id"] = _id
    cfg["target"] = target
    cfg["display_name"] = display
    if persona:
        cfg["hello"] = hello_for(persona, person)
        cfg["system_prompt"] = persona_for(persona, person) + strip_preamble(cfg.get("system_prompt", ""))
    write(cfg, out)
    tag = f", persona={persona!r}" if persona else ""
    print(f">> rendered config {out} from base {base}  (id={_id}, display={display!r}{tag})")


def cmd_refresh(cfg_path, tpl_path):
    cfg = json.load(open(cfg_path))
    tpl = json.load(open(tpl_path))
    for k in ("system_prompt", "description", "ref_mounts"):
        cfg[k] = tpl[k]
    persona, person = derive(cfg)
    if persona:
        cfg["hello"] = hello_for(persona, person)
        cfg["system_prompt"] = persona_for(persona, person) + strip_preamble(cfg["system_prompt"])
        note = f"re-applied persona {persona!r}"
    else:
        note = "no persona alias detected; left as template prompt"
    write(cfg, cfg_path)
    print(f">> --refresh-prompt: adopted template prompt into {cfg_path} ({note})")


def main(argv):
    if len(argv) < 2:
        print(__doc__, file=sys.stderr); return 2
    cmd = argv[1]
    if cmd == "render":
        cmd_render(*argv[2:9]); return 0
    if cmd == "refresh":
        cmd_refresh(*argv[2:4]); return 0
    print(f"!! unknown subcommand: {cmd}", file=sys.stderr); return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
