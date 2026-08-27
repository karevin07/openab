# Slash Commands

OpenAB registers Discord slash commands for session control and agent management. Most work in both guild threads and DMs — the exception is `/auth`, which is **DM-only** for security (see [`/auth`](#auth) below).

## Commands

| Command | Description | Requires active session? |
|---------|-------------|--------------------------|
| `/models` | Select the AI model via dropdown menu | Yes |
| `/agents` | Select the agent mode via dropdown menu | Yes |
| `/cancel` | Cancel the current in-flight operation | Yes |
| `/cancel-all` | Cancel the operation and drop buffered messages | Yes |
| `/reset` | Reset the conversation session (clear history, start fresh) | Yes |
| `/workspace status` | Show the current session workspace and channel default | No |
| `/workspace list` | List configured workspace aliases | No |
| `/session status` | Show session lifecycle state and workspace | No |
| `/session detach` | Suspend the live ACP process while preserving its resumable session and workspace | Yes |
| `/session close` | Close session state and drop buffered messages | Yes |
| `/auth` | Authenticate the backend agent via device flow (**DM-only**) | No |
| `/usage` | Show backend account usage and billing information | Yes |
| `/remind` | Set a one-shot delayed reminder to mention users/roles | No |
| `/export-thread` | Export thread/DM as `.txt` (default: last 100 messages) | No |

All responses are **ephemeral** — only the user who invoked the command sees the reply.

## Platform Support

| Platform | Supported | Notes |
|----------|-----------|-------|
| Discord (guild channels/threads) | ✅ | Commands are registered globally; workspace/session commands enforce configured channel and user access |
| Discord (DMs) | ✅ | Requires `allow_dm = true`; `/auth` remains DM-only |
| Slack | ❌ | Slack blocks third-party slash commands in threads; see [slack.md](slack.md#slash-commands-are-not-supported-on-slack) |

## How They Work

### `/models` and `/agents`

These read `configOptions` from the ACP `initialize` / `session/new` response and present them as a Discord Select Menu.

When the user picks an option, OpenAB sends `session/set_config_option` to the ACP backend.

**Agent support varies:**

| Agent | `/models` | `/agents` |
|-------|-----------|-----------|
| openab-agent | ✅ Returns available models via `configOptions` in `session/new` response | ❌ |
| kiro-cli | ✅ Returns available models via `models` fallback | ✅ Returns modes (`kiro_default`, `kiro_planner`) via `modes` fallback |
| claude-code | ❌ No `configOptions` emitted | ❌ |
| codex | ❌ | ❌ |
| gemini | ❌ | ❌ |
| cursor-agent | ❌ (tracking: #493) | ❌ |
| copilot | ❌ (tracking: #496) | ❌ |

If the agent doesn't expose options, the user sees: `⚠️ No model options available. Start a conversation first by @mentioning the bot.`

> **Backward compatibility:** `openab-agent` returns `configOptions` in the `session/new` response (alongside `sessionId`). ACP clients that only read `sessionId` will continue to work — `configOptions` is additive. Clients that support `/models` should read `configOptions[].options` to populate the model picker. Each model option includes a `provider` field (`"anthropic"` or `"openai"`) for routing.

> **Note:** Discord Select Menus are limited to 25 items. If the agent returns more, only the first 25 are shown with a count of how many were truncated.

### `/cancel`

Sends a `session/cancel` JSON-RPC notification to the ACP backend. This aborts in-flight LLM requests and tool calls immediately — no need to wait for the current response to finish.

### `/reset`

Cancels any in-flight operation, then removes the session from the pool. The ACP process terminates once the last reference is released. The next message in the thread or DM will automatically create a fresh session.

This is equivalent to the `sessions close` + `sessions new` pattern used by [OpenClaw ACPX](https://github.com/openclaw/acpx).

**What gets cleared:**
- Conversation history
- ACP process and connection
- Suspended session state (no resume after reset)

**What is preserved:**
- Bot identity and system prompt (re-applied on next session creation)
- Config settings in `config.toml`

### `/workspace status` and `/workspace list`

`/workspace status` shows the immutable workspace assigned to the current session, the workspace bound to the Discord parent channel, and the available alias names. It works before a session exists, which makes it useful for checking routing before sending the first prompt.

`/workspace list` shows up to 25 configured aliases and their resolved config paths. Discord thread commands inherit the parent channel binding. Responses are ephemeral and commands fail closed outside configured channels/DMs or for users outside `allowed_users`.

### `/session status`, `/session detach`, and `/session close`

`/session status` reports one of four pool states: active, suspended, persisted (awaiting restore), or no session. It also shows the session workspace when one is assigned.

`/session detach` refuses an in-flight turn, removes the idle live ACP process, and preserves the ACP session ID and workspace as suspended state. It also writes an external-handoff marker, so Discord messages cannot reload the session concurrently. The local client removes that marker after its ACP process exits; the next Discord message then restores the updated session with `session/load`.

`/session close` cancels any in-flight operation, removes active/suspended/persisted state, clears the session workspace metadata, and drops buffered messages. The next message in the same thread starts a fresh session. `/reset` remains available as a backward-compatible equivalent for resetting the current conversation.

### `/export-thread`

Fetches the current Discord thread or DM history and returns a `.txt` file as an ephemeral follow-up. The transcript includes message timestamps, author names and IDs, message text, and attachment URLs.

**Optional parameters** (mutually exclusive — use at most one):

| Parameter | Type | Description |
|-----------|------|-------------|
| `limit` | Integer | Export only the most recent N messages (1–5000) |
| `since` | String | Export messages after this message ID (right-click → Copy Message ID) |
| `days` | Integer | Export messages from the last N days (1–365). Rolling N×24h window from now. |
| `all` | Boolean | Export all messages (up to 5000) |

If no parameter is provided, the **last 100 messages** are exported.

**Examples:**
```
/export-thread                              → last 100 messages (default)
/export-thread limit:500                    → most recent 500 messages
/export-thread since:1503744866100842698    → messages after this specific message
/export-thread days:3                       → messages from the last 3 days (rolling 72h)
/export-thread all:true                     → export all (cap 5000)
```

**Constraints:**
- Only works in allowed Discord threads or enabled DMs.
- Specifying more than one filter returns an error.
- Very large exports may be truncated to fit Discord's attachment size limit.

## Passing CLI Commands via @mention

In addition to slash commands, you can pass built-in CLI commands directly after an @mention:

```
@MyBot /compact
@MyBot /clear
@MyBot /model claude-sonnet-4
```

These are forwarded as-is to the ACP session as a prompt. Any command the underlying CLI supports in its interactive mode works here. This is the recommended workaround for agents that don't expose `configOptions`.

## `/remind`

Set a one-shot delayed reminder that mentions users or roles in the channel after a specified delay.

**Syntax:**
```
/remind targets:<@user @role ...> message:<text> delay:<duration>
```

**Parameters:**

| Parameter | Required | Description |
|-----------|----------|-------------|
| `targets` | Yes | Space-separated @mentions (users and/or roles) |
| `message` | Yes | Reminder text |
| `delay` | Yes | Duration before firing: `1m` to `30d` (supports `m`, `h`, `d` and combinations like `1h30m`) |

**Constraints:**
- Only humans can use `/remind` (bots are rejected)
- Minimum delay: 1 minute
- Maximum delay: 30 days
- Maximum message length: 1800 characters
- Maximum 5 active reminders per user
- Maximum 10 mention targets per reminder (use a @role for larger groups)
- `@everyone` and `@here` in messages are automatically neutralized (will not trigger mass mentions)
- One-shot only (fires once, then removed)
- Reminders persist across bot restarts (stored in `$HOME/.openab/control.db` when Discord support is enabled)

**Examples:**
```
/remind targets:@Alice @Bob message:Review PR #42 delay:2h
/remind targets:@Reviewers message:Stand-up time delay:30m
/remind targets:@Charlie message:Check deployment delay:1d
```

**When fired, the bot posts:**
```
⏰ Reminder from @sender:
"Review PR #42"
cc @Alice @Bob
```

## `/auth`

Trigger the backend agent's device-flow authentication. OAB executes the command defined in `OPENAB_AGENT_AUTH_COMMAND`, captures the device code URL from stdout/stderr, and relays it to the user as an ephemeral Discord message.

**Flow:**
1. User runs `/auth`
2. OAB executes `$OPENAB_AGENT_AUTH_COMMAND` (e.g. `codex login --device-auth`)
3. OAB captures the device URL + code from the command's output
4. OAB sends an ephemeral reply with the URL and code
5. User opens the URL in their browser, enters the code
6. The auth command exits successfully → OAB confirms "✅ Authentication successful!"

**Requirements:**
- `OPENAB_AGENT_AUTH_COMMAND` environment variable must be set
- The auth command must use OAuth device flow (print URL + code to stdout, then block until authorized)
- No interactive stdin input required (headless-compatible)
- Must be invoked in a **DM** with the bot (rejected in guild channels/threads for security)

**Timeout:** 14 minutes. If the user doesn't authorize within that window, the process is killed and the user is prompted to run `/auth` again. (Reduced from 15min to leave headroom for Discord's interaction token TTL.)

**Behavior notes:**
- Only users in the `allowed_users` list can invoke `/auth`
- Bot users are rejected — `/auth` is for human operators only
- A 30-second URL-collection window waits for the auth command to print its URL. Slow-starting CLIs that take longer may show "no output".
- Only one `/auth` flow can run at a time (single-flight). A second concurrent invocation is rejected with "already in progress".

**Error cases:**
- `OPENAB_AGENT_AUTH_COMMAND` not set → immediate error message
- Invoked by a bot user → rejected
- Invoked outside a DM (in a guild channel/thread) → rejected for security
- Auth command fails to start → error message
- Auth command exits **before** printing a login URL (within the 30s window) → warning that no URL was produced, with a retry prompt
- Auth command exits with non-zero → failure message with exit code
- Timeout → process killed, retry prompt
