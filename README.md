# tau

A coding agent harness, written in Rust.

`tau` drives an LLM through a conversation about your codebase: reading files,
running commands, editing code, and coordinating larger pieces of work across
multiple sessions. It targets Anthropic, OpenAI-compatible, and other
providers.

Tau is heavily inspired by the [Pi coding agent](https://pi.dev/).
It has been used to build itself and is getting more capable every day.

## Highlights

- **Fast Terminal UI.** tau ships as a TUI: a keyboard-driven Ratatui interface
  with session pickers, streaming model output, task board views, and theme
  support.

- **Client/server split.** The agent runs as a long-lived server; the TUI
  is a thin client that connects over a Unix socket. That means you can
  detach and reattach without losing session state, drive the same server
  from scripts via [`tau-agent-client`](crates/tau-agent-client), or work on
  multiple projects sharing the same long-running agent daemon.

- **Task system.** Instead of treating every prompt as a one-shot, tau has a
  task board: file work as tasks, let a planner refine the spec, and the
  scheduler dispatches workers in isolated git worktrees — running disjoint
  tasks in parallel and serialising conflicting ones. Approved tasks flow
  through a merge queue.

- **Plugin system.** Tools are provided by plugins that run as subprocesses
  and speak a typed RPC protocol to the server. The default plugins ship the
  worker tools (`bash`, `read`, `write`, `edit`, tree-sitter-powered code
  navigation, diagnostics) and the task board, but anything can be swapped or
  added. See [`tau-agent-plugin`](crates/tau-agent-plugin) for the SDK.

- **Sandboxed plugins.** Plugin subprocesses are executed in configurable
  sandbox profiles, e.g., in Docker, or another machine via ssh.

## Install

```sh
cargo install tau-agent
```

Then run `tau` in a project directory.

## Workspace layout

| Crate | Purpose |
|---|---|
| [`tau-agent`](crates/tau-agent) | The `tau` CLI binary |
| [`tau-agent-lib`](crates/tau-agent-lib) | Library bundling the full agent |
| [`tau-agent-tui`](crates/tau-agent-tui) | Ratatui-based terminal UI |
| [`tau-agent-engine`](crates/tau-agent-engine) | Core agent loop and providers |
| [`tau-agent-plugin`](crates/tau-agent-plugin) | Plugin SDK |
| [`tau-agent-plugin-worker`](crates/tau-agent-plugin-worker) | Default tool plugin |
| [`tau-agent-plugin-tasks`](crates/tau-agent-plugin-tasks) | Task board plugin |
| [`tau-agent-client`](crates/tau-agent-client) | Unix-socket client library |
| [`tau-agent-base`](crates/tau-agent-base) | Shared protocol types |

## License

MIT
