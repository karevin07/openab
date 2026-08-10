# Workspaces

## Overview

A single OAB bot instance can serve multiple projects. Workspaces can be selected explicitly with a `[[ws:...]]` [control directive](control-directives.md), or automatically by binding a platform channel to a workspace.

When a workspace is set, the agent:
- Uses the workspace path as its working directory
- Loads steering rules from `AGENTS.md` and `.kiro/steering/`
- Activates skills from `.kiro/skills/`
- Has correct git context (branch, remote, history)

## Configuration

Define a narrow security root, aliases, and optional channel bindings in `config.toml`:

```toml
[workspace]
root = "~/projects"
discover_repositories = true
discovery_excludes = ["private-deployment"]

[workspace.aliases]
openab = "~/projects/openab"
infra  = "~/projects/infra-cdk"
web    = "~/projects/frontend"

[workspace.channels.discord]
"123456789012345678" = "@openab"
"234567890123456789" = "@web"
```

Paths starting with `~` expand to the bot's home directory (`$HOME`).

With `discover_repositories = true`, every direct child containing `.git`
becomes an alias automatically. For example, `~/projects/frontend/.git`
creates `@frontend`. Hidden directories, excluded names, nested repositories,
and symlinks resolving outside `workspace.root` are ignored. Explicit entries
under `[workspace.aliases]` take precedence over discovered aliases.

For Discord, bindings use the parent text channel ID. A message in a bound
channel creates a thread whose ACP session starts in that workspace; later
messages in the thread keep the same immutable workspace. An explicit
`[[ws:...]]` on the first message overrides the channel default.

## Usage

Reference aliases with `@` prefix in the first message:

```
@Bot [[ws:@openab]] help me debug the smoke tests
```

Or use raw paths:

```
@Bot [[ws:~/projects/myapp]] investigate the build failure
```

## Security Boundary

All workspace paths are validated before use:

1. **Must be absolute** — relative paths (e.g., `../secrets`) are rejected
2. **`~` expands to bot home** — not the requesting user's home
3. **Canonicalized** — symlinks, `..`, `.` are resolved
4. **Must be within `workspace.root`** — paths outside are rejected
5. **Must be a directory** — file paths are rejected
6. **Must exist** — non-existent paths are rejected with a clear error showing the expanded path

Automatic discovery does not grant filesystem access by itself. Containerized
deployments must mount every repository that should be visible to the bot. Do
not mount deployment directories containing bot tokens or unrelated secrets.

## Session Behavior

- Workspace is set **once** at session creation and is immutable
- The workspace persists across session suspend/resume and eviction rebuilds
- To change workspace, start a new session
- If workspace resolution fails, no session is created (clean failure)

## Error Messages

| Scenario | Error |
|----------|-------|
| Unknown alias | `Unknown workspace alias @foo. Available: openab, infra, web` |
| Relative path | `Workspace path must be absolute (start with ~ or /): relative/path` |
| Outside root | `Workspace path is outside allowed directory: /etc/passwd` |
| Not a directory | `Workspace path is not a directory: /home/bot/Cargo.toml` |
| Does not exist | `Workspace path does not exist: ~/nope (expanded to /home/bot/nope)` |

## See Also

- [Control Directives](control-directives.md) — full directive syntax and rules
- [Config Reference](config-reference.md#workspace) — root, aliases, and channel bindings
