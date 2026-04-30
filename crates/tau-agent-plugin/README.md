# tau-agent-plugin

Plugin SDK for the [tau](https://github.com/tau-agent/tau) agent.

Everything a plugin author needs in one crate: the plugin trait, the tool
registration API, and re-exports of the protocol types from `tau-agent-base`.
Plugins run as subprocesses and communicate with the tau server over a
bidirectional RPC channel.

## License

MIT
