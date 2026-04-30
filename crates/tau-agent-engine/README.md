# tau-agent-engine

Core agent loop and LLM provider implementations for the
[tau](https://github.com/tau-agent/tau) agent.

Drives the conversation between the model, tool calls, and the plugin
runtime. Most users should depend on `tau-agent-lib` (which re-exports what
embedders need) rather than this crate directly.

## License

MIT
