#!/usr/bin/env python3
"""Persona rendering for Aqua consultants, shared helper for spawn-consultant.sh.

Each consultant has a warm female persona: a name (e.g. Talia), a Matrix display
alias "<Name> (Aqua Consultant)", a first-contact `hello` (the welcome) that greets the
served person by name (no DIDs/MXIDs), and a "# Who You Are" preamble PREPENDED to
the system_prompt. The served person's name is hardcoded per config because the
`hello` placeholder layer only interpolates the agent's own id, never the peer's.

Subcommands:
  render <base> <out> <id> <target> <display> <persona> <person> [<voice>]
      Render <base> -> <out>, always setting id/target/display_name. When <persona>
      is non-empty, also (re)writes the hello and the persona preamble.
      <person> may be empty -> pseudonymous greeting ("what should I call you?").
      <voice> (on/off) = the config's final voice.enabled, which decides the hello's voice
      line; omitted/empty = read it from <base>.

  refresh <cfg> <template>
      Adopt the template's system_prompt/description/ref_mounts into <cfg> (the
      --refresh-prompt path), then RE-APPLY the persona derived from <cfg>'s own
      display alias + hello, so a prompt refresh never silently drops the persona.
      The hello is re-rendered to the canonical text, voice line per <cfg>'s voice.enabled.

  hello-of <cfg> [<mxid>]
      Print the hello <cfg> carries, {user_id}/{did} replaced by <mxid> like the agent does.

  derive <cfg>
      Print D_PERSONA=/D_PERSON= shell assignments recovered from <cfg> (for eval).

  preview <base> <keep 0|1> <refresh 0|1> <display> <persona> <person> <voice> [<mxid>]
      Print the hello a spawn with these inputs would leave in the config. Writes nothing.

The hello is the consultant's own first message: the relay sends it into the DM room it
creates (initiate_dm) and the agent appends its "What's new" list on first contact.
"""
import json, re, sys

SENT = "# Who You Are"
DELIM = "\n\n---\n\n"
ALIAS_SUFFIX = " (Aqua Consultant)"


def hello_for(name, person, voice=False):
    """The consultant's own first message (Markdown), sent by the relay into the DM it creates.

    The literal prefix "Hi <person>! " / "Hi there! " is load-bearing: derive() parses it back
    (and it matches the older wording too, which existing configs keep). The voice line is
    present only when voice is enabled, so it has to follow the config's FINAL voice.enabled.
    No em/en dashes (tests enforce it).
    """
    who = person or "there"
    lines = [
        f"Hi {who}! \U0001F30A I'm {name}, your own Aqua Consultant. I'm an AI assistant, "
        f"and I'm here just for you whenever you have a question about Aqua.",
        "",
        "In one sentence: Aqua is inblock.io's protocol for trust that travels with your data. "
        "Every signature, AI action and file carries its own proof, so anyone can check what happened.",
        "",
        "**Ask me anything, in your own words.** You don't need a technical background, and if "
        "you're a developer I'm happy to go deep. For example:",
        '- "What is Aqua, and why would I use it?"',
        '- "How could Aqua help in my work?"',
        '- "What\'s the difference between AquaFire, AquaNode and AquaAgents?"',
        '- "Walk me through signing and verifying a document, step by step."',
        '- "Show me where the SDK checks a signature."',
        "",
        "**Good to know**",
        "- I only talk with you; this chat is one-to-one.",
    ]
    if voice:
        lines.append("- You can type, or send me a voice message.")
    lines += [
        "- I explain things and show you where my answers come from. I can't change anything "
        "or act on your behalf.",
        "- Like any AI, I can occasionally be wrong. When something matters, ask me for the source.",
        "",
        "So, what would you like to explore first?" if person
        else "Before we start, what should I call you?",
    ]
    return "\n".join(lines)


def voice_on(cfg):
    """True iff the config has voice.enabled == true (the agent's own gate)."""
    v = cfg.get("voice")
    return isinstance(v, dict) and v.get("enabled") is True


def flag(s):
    return str(s).strip().lower() in ("1", "on", "true", "yes")


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


def cmd_render(base, out, _id, target, display, persona, person, voice=""):
    cfg = json.load(open(base))
    # voice: the config's FINAL voice.enabled (spawn resolves --voice > base config before the
    # render and patches voice.enabled to match afterwards). Empty = read it from the base.
    voice = flag(voice) if voice != "" else voice_on(cfg)
    cfg["id"] = _id
    cfg["target"] = target
    cfg["display_name"] = display
    if persona:
        cfg["hello"] = hello_for(persona, person, voice)
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
        cfg["hello"] = hello_for(persona, person, voice_on(cfg))
        cfg["system_prompt"] = persona_for(persona, person) + strip_preamble(cfg["system_prompt"])
        note = f"re-applied persona {persona!r}"
    else:
        note = "no persona alias detected; left as template prompt"
    write(cfg, cfg_path)
    print(f">> --refresh-prompt: adopted template prompt into {cfg_path} ({note})")


def subst(hello, mxid):
    """What the agent actually sends: it replaces {user_id}/{did} with its own MXID."""
    if mxid:
        hello = hello.replace("{user_id}", mxid).replace("{did}", mxid)
    return hello


def cmd_derive(cfg_path):
    """Shell assignments D_PERSONA / D_PERSON recovered from a config (for eval)."""
    import shlex
    persona, person = derive(json.load(open(cfg_path)))
    print(f"D_PERSONA={shlex.quote(persona)}")
    print(f"D_PERSON={shlex.quote(person)}")


def cmd_hello_of(cfg_path, mxid=""):
    print(subst(json.load(open(cfg_path)).get("hello") or "", mxid))


def cmd_preview(base, keep, refresh, display, persona, person, voice, mxid=""):
    """The hello a real spawn would leave in the config, simulated without writing anything:
    render (unless --keep-config), then refresh (with --refresh-prompt), same functions."""
    cfg = json.load(open(base))
    v = flag(voice)
    if not flag(keep):
        cfg["display_name"] = display
        if persona:
            cfg["hello"] = hello_for(persona, person, v)
    if flag(refresh):
        p, n = derive(cfg)
        if p:
            cfg["hello"] = hello_for(p, n, v)
    print(subst(cfg.get("hello") or "", mxid))


def main(argv):
    if len(argv) < 2:
        print(__doc__, file=sys.stderr); return 2
    cmd = argv[1]
    if cmd == "render":
        cmd_render(*argv[2:10]); return 0
    if cmd == "derive":
        cmd_derive(argv[2]); return 0
    if cmd == "hello-of":
        cmd_hello_of(*argv[2:4]); return 0
    if cmd == "preview":
        cmd_preview(*argv[2:10]); return 0
    if cmd == "refresh":
        cmd_refresh(*argv[2:4]); return 0
    print(f"!! unknown subcommand: {cmd}", file=sys.stderr); return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
