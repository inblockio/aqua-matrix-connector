---
name: claude-channel
description: Matrix LLM channel — DMs from --target are forwarded to `claude -p` and the stdout is DM'd back.
---

# Matrix Claude Channel

Separate Matrix identity (`claude-channel.pem` + `~/.aqua-matrix-claude-channel/` store) running the `aqua-matrix-claude-p` binary. Each inbound DM from the configured `--target` becomes a fresh `claude -p <prompt>` invocation; stdout is sent back as a Matrix reply. See [`docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md) for the full design and the one-identity-per-surface rationale.

## Quick check

```bash
systemctl --user is-active aqua-matrix-claude-channel    # expect: active
journalctl --user -u aqua-matrix-claude-channel -f       # live log
cat ~/.aqua-matrix-claude-channel/config.toml            # session cache + OIDC
```

## Install

The unit ships at `systemd/aqua-matrix-claude-channel.service`. First run also generates the identity:

```bash
cp ~/aqua-matrix-agent/systemd/aqua-matrix-claude-channel.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now aqua-matrix-claude-channel
```

Verify the new identity was minted:

```bash
ls ~/aqua-matrix-agent/claude-channel.pem                # the Ed25519 key
journalctl --user -u aqua-matrix-claude-channel -n 30 | grep "agent DID"
```

## Sending it a prompt

From `--target` (Tim's account in Element), DM the **claude-channel identity** any plain prose. The daemon will:

1. See the message via stream sync (~1 sec).
2. Spawn `claude -p "<your message>"`, resuming the user's prior session
   (`--resume`) so the conversation is continuous across DMs. There is no
   total-runtime cap; an inactivity watchdog stops a run only if `claude` emits
   no output for 600s.
3. DM back the stdout. The final, complete reply is transmission-checked and
   retried until the homeserver acknowledges it, so the last update is never
   silently dropped.

Stateless per message: each invocation starts fresh, no conversation continuity. For continuity, attach locally to the `claude-bridge` tmux session (see `claude-bridge` skill).

## Respawn

From a local shell: `systemctl --user restart aqua-matrix-claude-channel`. This is a different unit from `claude-bridge.service`, the local interactive tmux session; don't confuse them. There is no Matrix-side restart command (the `#shell` command channel went away with the retired ops daemon).

## Sync model

Stream sync (`client.sync(...)` running forever in a background tokio task). Event handler dispatches incoming messages. No polling. Reply latency = `claude -p` latency, not a polling interval.

## Failure modes

- **`claude -p timed out`** in the reply: the prompt was too complex or claude hung. The daemon stays running; just send another shorter prompt.
- **`claude -p exited with status ...`**: claude CLI rejected the prompt. Stderr is included in the reply.
- **Empty reply (`[claude-channel] (no output)`)**: claude produced no stdout. Check `claude --version` works locally; also check `which claude`.
- **No reply at all**: check `systemctl --user is-active aqua-matrix-claude-channel`. If active but unresponsive, `systemctl --user restart aqua-matrix-claude-channel` locally.

## Limits

- Stateless conversation. Each Matrix message is a one-shot. For multi-turn flows use the local `claude-bridge` tmux.
- Output is truncated at ~16KB so very long claude outputs get a `[...truncated]` tail.
- No allow-list beyond `--target`: only DMs from the configured target identity are processed. Anyone else who DMs the claude-channel identity is silently ignored.
