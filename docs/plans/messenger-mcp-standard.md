# Messenger MCP as standard connector tooling

Status: IMPLEMENTED (2026-09-29), rebased onto main 7424f84 with `edit_message` (#14) and
the inbox policy / handling states (#19) ported into the engine, and MERGED 2026-10-04 by
Tim's decision. The host bridge runs it from its next deploy. The consultant switch-over
is tracked separately in aqua-agents
(`docs/plans/consultant-messenger-integration.md`, draft tracking PR) and needs more
work before anything changes for a consultant. Supersedes the pre-rooms prototype
`3ed5cc6` (`wip/messenger-refactor-pre-rooms`), which was used as a reference only.

## Goal

Every agent that embeds aqua-matrix-connector gets the messenger tools
(`send_message`, `send_file`, `list_recipients`, `read_inbox`, `fetch_attachment`,
and `wait_for_reply` only where a profile enables it) by default:

- under its OWN identity,
- over its OWN live Matrix Client (never a second Client on its crypto store),
- with the same policy as the host "Aqua System" bridge.

## Decisions (Tim, 2026-09-29)

1. **MCP tool annotations** on every tool, per the MCP spec (as in
   github.com/jlxq0/matrix-mcp, MIT; conventions studied, no code copied):
   `title`, `readOnlyHint`, `destructiveHint`, `idempotentHint`, `openWorldHint`.

   | Tool | readOnly | destructive | idempotent | openWorld |
   |---|---|---|---|---|
   | `send_message` | false | false | false | true |
   | `send_file` | false | false | false | true |
   | `list_recipients` | true | false | true | false |
   | `read_inbox` | true | false | false (mark_read) | false |
   | `wait_for_reply` | true | false | false | false |
   | `fetch_attachment` | true | false | true (cached) | true (homeserver download) |

   Annotations are hints for the client UI and permission prompts, never a security
   boundary; the engine enforces the policy.
2. **Reply threading** (conventions as in github.com/IA-PieroCV/cc_matrix_channel, MIT;
   no code copied): `send_message` / `send_file` take an optional `reply_to` (an inbox
   `seq` or a Matrix event id). The first event carries `m.relates_to.m.in_reply_to`;
   when the original is in a thread the reply is a genuine thread reply
   (`rel_type: m.thread`, root as `event_id`, `m.in_reply_to` the original,
   `is_falling_back` false) and later chunks of a long message stay in the thread as
   plain thread messages. Inbox entries expose `event_id`, `in_reply_to` and
   `thread_root`, so a session sees that a message answers one it sent (compare with
   the event id `send_message` returns). A thread message whose `m.in_reply_to` is only
   the thread fallback is not reported as a reply (the matrix-mcp rule).
3. **`wait_for_reply` is OFF** in the shared default profile and for consultants and
   embedded agents: not listed in `tools/list`, not mentioned in any text, and refused
   by the engine if called anyway. Only `Profile::host()` enables it, explicitly, so
   the host behaviour is unchanged.
4. Rooms are first-class targets. The default allow-list of an embedded agent is its
   OWNER only. Inbound text is framed as untrusted data.

## Target model (rooms first-class)

A target is a person OR a room; both must be allow-listed.

- `to` resolves by name, MXID, `[[rooms]]` name or room id (`Engine::resolve_to` ->
  `Dest::Person(mxid)` / `Dest::Room(room_id)`); anything else is refused with the list
  of allowed names. A joined but unlisted room gets its own refusal.
- Allow-list file: `[[recipients]]` (people) and `[[rooms]]` (rooms). Hot reload, fail
  closed, unchanged for the host.
- Inbound (`Engine::classify_inbound`): a DM is kept when its sender is allow-listed;
  a room message when the ROOM is listed (sender recorded, content still untrusted);
  messages in unlisted group rooms are dropped and logged once per room, DMs from
  others once per event.
- Inbox entries keep `room_id`, `sender`, `room`; `read_inbox` / `wait_for_reply` take
  a person (their DMs only) or a room as `from`.
- `reply_to` must stay in the conversation: an inbox entry must be in the target room
  (room target) or be a DM from that person (person target). A bare event id not in
  the inbox is loaded from the destination room by the transport, which fails for an
  event that is not there. For a person, the DM the original lives in is preferred
  when it is still a valid 1:1.
- Invites (host daemon only): joined only from allow-listed people; only an
  `is_direct` invite that is not a listed room is recorded in `m.direct`.

## Crate layout (as implemented)

```
aqua-messenger          Matrix-free; also builds the stdio binary aqua-messenger-mcp
  allowlist   people + rooms; AllowList::new(file) (host) /
              owner_only(owner, extra_file, label) (embedded; the owner cannot be
              re-pointed; a broken extra file falls back to owner-only)
  profile     Profile { label, server_name, approver, default_to, tag_origin, tools,
              limits, attachments }; Profile::host() (6 tools, wait on),
              Profile::embedded_agent(label, owner) and Default (5 tools, wait off),
              with_wait_for_reply() / without_wait_for_reply()
  engine      Engine<T: Transport>: resolve_to / resolve_from / resolve_reply,
              classify_inbound, ingest, rate limit, caps, sensitive_path,
              list_recipients, fetch_attachment (allow-list re-check), audit,
              joined-room snapshot (SeenRoom)
              trait Transport { send_text(&Dest, md, Option<&ReplyRef>),
                                send_file(&Dest, path, caption, Option<&ReplyRef>),
                                fetch(FetchRequest) }
  inbox       + event_id lookup, in_reply_to, thread_root (old lines still load)
  attachments, format (label-driven untrusted framing), ratelimit
  jsonrpc     tool defs + annotations generated from the Profile; describe(profile)
  proto       Request (+ Describe; `to` optional with a default; reply_to)
  mcp         run_stdio(backend, fallback), SocketBackend, `impl Backend for Engine`
  server      bind_socket (mode 600, refuses a second owner), serve(listener, engine)

aqua-messenger-matrix   aqua-messenger + aqua-matrix-agent + matrix-sdk
  media       media_ref, fetch (authenticated media, cache off), decrypt_verified
  inbound     ingest_event (reply/thread fields), register_message_handler,
              backfill, survey_rooms, joined_member_count, is_group
  outbound    reply_target, send_text, send_file into a resolved room
  inproc      LiveClient: Transport over the agent's live AgentClient
              AgentMessenger::enable_default(state_dir, label, owner_mxid)
              .attach(&agent) / .detach() / .backfill(&agent) / .serve_mcp(sock, bin)
              McpEndpoint::mcp_config() (server key `messenger`), ::allowed_tools()
  examples/embedded_agent.rs

aqua-system-bridge{,d}  thin host front end: Profile::host() + file allow-list;
                        QueueTransport into the existing cycle loop; invite policy,
                        m.direct repair and the daemon's DM resolution stay here

aqua-matrix-agent (core) reply module: ReplyTarget, first/continuation relations,
                        reply_fields; send_to_room_chunked_reply,
                        send_media_to_room_reply, reply_target_in_room, dm_room_for
aqua-matrix-relay (core) MessageHandler::on_cycle_start(&AgentClient) /
                        on_cycle_end(), default no-ops: the attach/detach seam
```

Dependency direction: all four messenger/bridge crates are connector crates; none
names an agents-side crate. `scripts/check-dep-direction.sh` now lists them.

## Tool merge with the consultant tools

One MCP server key, `messenger`, replaces `md`. Still ONE `--mcp-config` per run.

| Tool | Consultant today | After | Semantics / overlap rule |
|---|---|---|---|
| `send_markdown_file(filename, markdown)` | `mcp__md__...` | stays on `md` for now | Not ported yet: either kept on the `md` server for one release next to `messenger`, or moved into the engine (`.md` written under its state dir, sent via `Transport::send_file`, inline fallback and the `fired` flag). Open item in the aqua-agents tracking doc. |
| `send_message(to?, markdown, reply_to?)` | none | new | For OUT-OF-BAND notes or other allow-listed targets. The run's answer is still the streamed reply; the tool text says so, to avoid double answers. |
| `send_file(to?, path, reply_to?)` | none | new | Path-based, sensitive-path guard (incl. the agent's messenger dir). |
| `list_recipients` | none | new | Owner, extra people and rooms. |
| `read_inbox` | none | new | History and data, framed untrusted. The owner's current turn still arrives as the prompt. |
| `fetch_attachment(inbox_seq)` | non-text types declined | new | `augment_prompt_with_media` keeps inlining `.md`/`.txt` and points other types at `fetch_attachment(inbox_seq=N)`; the contract must allow `Read` on `<messenger dir>/attachments/`. Voice notes keep the `voice_turn` path. |
| `wait_for_reply` | none | OFF | Decision 3. |
| `ask_human` | removed | unchanged | Legacy host daemon only. |

Deprecations: `aqua-matrix-md-mcp`, `MdBridge`, `MD_SERVER_KEY`/`MD_FILE_TOOL` stay for
one release after the fleet moves, then are removed. `MdBridge` uses
`send_file_with_refresh`, which rebuilds a Client in place; moving to the messenger
also retires that second-Client path for consultants.

## Default policy (embedded)

- Allow-list = the OWNER only (`AllowList::owner_only`); the owner entry is built in.
- More people or rooms only through an explicit extra allow-list file (same TOML),
  named in the instance config. Missing file = owner-only; broken file = owner-only.
- `to` defaults to the owner; no origin tag (the agent speaks for itself).
- Same limits as the host: 20 sends / 10 min per target, 20 kB message, 10 MiB file,
  50 MiB fetched attachment, 14-day attachment retention.
- `send_file` refuses the messenger state, credentials and key/env/token-named files.

## Untrusted framing

- Every inbound body, filename, room name and attachment is returned inside
  `UNTRUSTED USER-AUTHORED DATA from Matrix (<label> inbox)` /
  `UNTRUSTED USER-SUPPLIED FILE` framing. Bodies are JSON strings, so no content can
  close the frame. Room messages name room AND sender.
- The media reference (for E2EE, the file key) never leaves the engine: the inbox over
  the wire carries only mime type and declared size (reconciled `fetch_attachment`,
  PR #11), and `fetch_attachment` re-checks the allow-list at fetch time.
- In an embedded agent, instructions reach the model ONLY through the relay's turn
  prompt. The inbox tools return data.

## One Client per crypto store

- Host daemon: unchanged design. Commands queue into the cycle loop and run inline on
  the live Client (downloads included); an `M_UNKNOWN_TOKEN` command is carried to the
  next cycle's Client.
- Embedded: `LiveClient` holds the agent's own `AgentClient` between `attach` and
  `detach`. Each Matrix operation holds a read guard for its duration; `detach` takes
  the write guard, waits for in-flight operations, removes the inbound handler and
  drops every reference. Call `attach` from `on_cycle_start` and `detach` from
  `on_cycle_end` (the relay calls the latter before it removes its handlers and
  rotates). While detached, tools fail fast with "not connected, retry shortly".
- The messenger never calls `*_with_refresh` helpers or `AgentClient::connect`.

## Tests (this branch)

- Unit: aqua-messenger 42 (incl. host texts byte-identical to origin/main 3fae6e0,
  annotations on every tool in every profile, `reply_to` in both send schemas,
  wait_for_reply absent from default/embedded, owner-only allow-list, reply/thread
  inbox fields); engine integration 11 (mock transport; rooms, reply_to resolution
  and refusals, profile gating, caps, rate slots, key hiding, fetch cache, socket
  backend + describe); aqua-matrix-agent reply 4 (wire format of plain / threaded /
  continuation relations, inbound parsing incl. thread fallback); aqua-system-bridged 6
  (queue transport with Dest + reply, fetch gate, key hiding, fail-closed rooms).
- Local-stack e2e (siwx-e2eh stack, 18080/18081): 55/55, including threaded reply
  (inbound `thread_root`, outbound genuine thread reply on the wire, the peer's reply
  to our message shows `in_reply_to` = our event id), plain reply via `send_file`,
  refusals, host tools and annotations, embedded default tools without
  `wait_for_reply`, a daemon token rotation mid-run.

## Before merge

See the PR checklist: consultant integration through the aqua-agents tracking PR;
one-Client attach/detach through reconnect and token rotation tested in an embedding
agent; 7af0650 (true 1:1 DM resolver) on main; a canary plan; the `md` server kept for
one release.
