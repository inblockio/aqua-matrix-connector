# Recovery Behavior

How the agent infrastructure on this host survives crashes, restarts, network blips, server outages, and identity drift. Read this when something is broken or before changing recovery-critical config. For the *design* (why three daemons, why stream sync, identity model) see [`ARCHITECTURE.md`](ARCHITECTURE.md).

## At a glance — what recovers without human intervention

| Scenario | Recovery | Time |
|---|---|---|
| `kill -9` on a Matrix daemon | systemd respawns (`Restart=always`) | ~5s |
| Matrix daemon panics / OOM | same | ~5s |
| Matrix daemon exits cleanly (shouldn't, defensive) | same | ~5s |
| Claude inside `claude-bridge` tmux exits (`/quit`, crash, OOM) | Supervisor's 10s tick re-runs `tmux new-session` | ≤10s |
| `claude-bridge` supervisor itself dies | systemd respawns the supervisor, which respawns tmux | ~5s |
| Matrix sync stream errors (homeserver reachable but transient) | matrix-sdk-internal retry | seconds |
| Matrix sync stream errors (homeserver/network down for a while) | our loop catches it, sleeps 5s, calls `client.sync()` again | 5s after server is back |
| Access token expires while daemon is running | matrix-sdk uses the token until rejected; on restart, refresh-grant exchanges for new token | next restart, ~1 call to `/token` |
| WSL reboots | wsl.conf `systemd=true` + linger + units `enabled` brings all three up | seconds after WSL itself starts |

## At a glance — what requires human intervention

| Scenario | Why | What to do |
|---|---|---|
| Restart-loop guard exceeded (`StartLimitBurst=10` in `StartLimitIntervalSec=300`) | Daemon failed >10 times in 5 minutes — usually means a real upstream problem | Fix root cause, then `systemctl --user reset-failed <unit>` to re-enable restarts |
| Refresh token expired (>24h since last refresh-grant) | Standalone siwx-oidc TTL boundary | Nothing — next start does a fresh OAuth code flow, mints a new `device_id`, and the in-code store-mismatch handler wipes + rebuilds the SQLite crypto store. Tim sees one cross-signing rebootstrap. |
| siwx-oidc server permanently down | Cannot authenticate at all | Fix siwx-oidc, then `systemctl --user reset-failed` if guard tripped |
| Disk full | SQLite writes fail | Free space, then `systemctl --user restart <unit>` |
| All `.pem` files lost | Identities derive from these | Accept the new identity (binary auto-generates), notify counterparties (they see a new Matrix user) |

## Component-by-component

### `aqua-matrix-heartbeat.service`

- `Restart=always`, `RestartSec=5s`, crash-loop guard `StartLimitBurst=10 / IntervalSec=300`
- On restart: `connect()` runs the three-tier session resolution
  1. cached access token still valid → reuse (one `/whoami` call, ~50ms)
  2. cached refresh_token + did → `siwx_oidc_auth::refresh()` → new access token, **same `device_id`**, crypto store untouched (one `/token` call)
  3. neither → fresh OAuth code flow → **new `device_id`**, store-mismatch handler wipes `matrix-sdk-*.sqlite3*` and retries `restore_session` once, cross-signing re-bootstraps
- Stream sync (`client.sync()` in background tokio task): on `Err(e)`, the loop catches, sleeps 5s, retries. No restart of the systemd unit needed for transient sync errors.
- Watermark: initialized to `now()` on every start so commands sent before the daemon was online are NOT re-processed (in particular: `#shell restart` does not loop)

### `aqua-matrix-claude-channel.service`

Identical recovery profile to heartbeat — same `Restart=always`, same three-tier auth, same stream-sync recovery, same watermark behavior.

Differences in failure modes specific to claude-channel:
- Per-message `claude -p` invocation has a 180s timeout; on timeout the daemon replies with `[claude-channel error] claude -p timed out after 180s` and stays running.
- `claude` binary missing: every prompt returns `[claude-channel error] failed to spawn ... claude -p`. Daemon does not crash.

### `claude-bridge.service`

- `Type=simple` with a bash supervisor as the main process. Supervisor:
  1. Loops every 10s
  2. Checks `tmux has-session -t claude-bridge`
  3. If missing, runs `tmux new-session -d -s claude-bridge -- ~/.local/bin/claude --dangerously-skip-permissions`
  4. Logs `[claude-bridge supervisor] spawned tmux session` to the journal
- If the inner `claude` process exits (any cause), the tmux session ends and is recreated on the next tick (≤10s downtime)
- If the supervisor itself is killed, `Restart=always` brings it back in ~5s, then it immediately respawns the tmux session
- `ExecStop=/usr/bin/tmux kill-session -t claude-bridge` makes `systemctl --user stop` clean

## Crash-loop guard explained

Each unit declares:

```
StartLimitIntervalSec=300
StartLimitBurst=10
```

systemd refuses to restart the unit if it has already restarted 10 times in the last 300 seconds. The unit transitions to `failed`. This prevents log spam / CPU burn when a daemon is fundamentally broken (siwx-oidc unreachable, disk full, etc.).

To re-enable restarts after fixing the root cause:

```bash
systemctl --user reset-failed aqua-matrix-heartbeat
systemctl --user start aqua-matrix-heartbeat
```

## Identity / device_id state recovery

Detailed in [`ARCHITECTURE.md` § "Identity and device-id persistence"](ARCHITECTURE.md#identity-and-device-id-persistence). One-paragraph summary for operators:

`<store_dir>/config.toml` persists `access_token`, `refresh_token`, `did`, `user_id`, `device_id`, and `expires_at_unix`. On startup the daemon picks the cheapest valid tier (cached access token → refresh grant → fresh OAuth). The first two tiers preserve `device_id` and therefore the SQLite crypto store. Tier three is the only path that triggers a crypto-store wipe + cross-signing rebootstrap. This means the only restart that causes visible disruption to your Element timeline is one that happens >24h after the last successful refresh.

## Manual recovery procedures

### "Daemon stuck in `failed` state"

```bash
systemctl --user status <unit>          # look at the last error
systemctl --user reset-failed <unit>    # clear the failed flag
systemctl --user start <unit>           # try again
```

### "Restart loop, want to debug live"

```bash
systemctl --user stop <unit>            # halt the auto-restart cycle
~/aqua-matrix-hello/target/debug/aqua-matrix-agent --heartbeat   # or whatever mode
# ... iterate. Ctrl+C when done. systemctl --user start <unit> to resume normal supervision.
```

### "Wipe crypto store but keep OIDC creds"

```bash
systemctl --user stop <unit>
rm -f ~/.aqua-matrix-<unit-name>/matrix-sdk-*.sqlite3*
systemctl --user start <unit>
```

### "Wipe everything for this identity, start fresh"

```bash
systemctl --user stop <unit>
rm -rf ~/.aqua-matrix-<unit-name>/       # everything: store + config + cached session
# Optionally also: rm ~/aqua-matrix-hello/<unit>.pem  # changes the underlying DID/Matrix account
systemctl --user start <unit>
```

(Removing `config.toml` forces fresh OIDC client registration. Removing the `.pem` forces a brand-new identity — counterparties will see a new Matrix user.)

### "Tail logs"

```bash
journalctl --user -u aqua-matrix-heartbeat -f
journalctl --user -u aqua-matrix-claude-channel -f
journalctl --user -u claude-bridge -f
```

### "Restart a daemon over Matrix when local shell is unavailable"

From the configured `--target` account (Tim), DM the heartbeat identity:

- `#shell restart` → restarts `aqua-matrix-heartbeat`
- `#shell respawn-channel` → restarts `aqua-matrix-claude-channel`
- `#shell respawn` → restarts `claude-bridge`

If the heartbeat itself is the wedged one, this won't work — you need local shell or another path in.

## Diagnostic decision tree

**No heartbeat status payload in 10+ min**

1. `systemctl --user is-active aqua-matrix-heartbeat` — `active` / `failed` / `activating`?
   - `failed` → `journalctl --user -u aqua-matrix-heartbeat -n 50`. Common: crash-loop guard tripped → `reset-failed` + fix root cause.
   - `activating` (indefinitely) → fresh-auth path stuck. Check siwx-oidc connectivity: `curl https://siwx-oidc.inblock.io/.well-known/openid-configuration`.
   - `active` but silent → sync stream might be wedged. Send `#shell ping` from Tim. No reply → daemon is alive but its sync is stuck → `systemctl --user restart aqua-matrix-heartbeat`.

**Claude channel does not respond to a DM**

1. `systemctl --user is-active aqua-matrix-claude-channel` — must be `active`.
2. Locally test `~/.local/bin/claude -p hi`. If claude itself is broken, the daemon cannot reply.
3. Check journal for `claude -p exited with status` or `claude -p timed out` — these are reported back to Element as `[claude-channel error]`. If Element shows the prompt was received but no error message arrived either, the reply send itself failed (look for `claude-channel reply send failed` in the journal).
4. Last resort: `systemctl --user restart aqua-matrix-claude-channel` (or `#shell respawn-channel` from heartbeat).

**Bridge tmux session is gone but unit is `active`**

Should NOT happen with the Type=simple supervisor design. If it does:
- `journalctl --user -u claude-bridge -n 30` — look for `spawn failed` lines and their reason
- Most likely: `claude` binary missing or PATH wrong. Verify `~/.local/bin/claude --version` works.

**`device_id` rotates on every restart, store keeps wiping**

Should NOT happen if refresh tokens are working. Check:
1. `grep refresh_token ~/.aqua-matrix-heartbeat/config.toml` — if missing, the daemon authenticated against an old siwx-oidc that did not issue refresh tokens. Next fresh auth (against the new server) fixes it.
2. `grep '^did' ~/.aqua-matrix-heartbeat/config.toml` — must also be present for the refresh-grant call.
3. Journal: `refresh grant failed` → most likely refresh token expired (>24h since last refresh). Fresh auth runs and mints a new device. One-time event, not a recurring problem unless restarts happen with >24h gaps.

**"All three are running but Tim still does not see them in Element"**

Less of a recovery issue, more of an onboarding one. Both Matrix daemons send a one-shot `[hello]` message on every start. If Tim sees no hello:
- Verify the daemons did NOT crash on hello: `journalctl --user -u <unit> -n 30 | grep hello`
- Verify the DM rooms exist on the server: `~/aqua-matrix-hello/target/debug/aqua-matrix-agent --read --key-file heartbeat.pem --store-dir ~/.aqua-matrix-heartbeat --target <tim>` (but stop the unit first to avoid SQLite contention)
- Tim's Element may have an unaccepted room invite from the claude-channel identity if this is the first time he is seeing it

## What this document does not cover

- The design rationale for two-identity architecture, sync model, identity persistence: [`ARCHITECTURE.md`](ARCHITECTURE.md).
- Day-to-day operations (sending a message, running the e2e test, attaching to the bridge): per-skill docs under [`../Skills/`](../Skills/).
- siwx-oidc / Synapse / Element server-side recovery: out of scope, see the companion repos in CLAUDE.md.
