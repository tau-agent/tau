# tau-agent-lib

Library crate for the [tau](https://github.com/tau-agent/tau) LLM agent.

Bundles the agent engine, client, plugin runtime, session storage, and the
default plugins (worker tools, task board) into a single dependency for
embedding tau in another binary.

Most users want the `tau-agent` crate, which provides the `tau` CLI built on
top of this library.

## License

MIT
