# tau-agent-base

Shared types, wire protocol, and small utilities used across the
[tau](https://github.com/tau-agent/tau) agent workspace.

This crate is part of tau's internal layering. It is published so that
plugin authors and downstream embedders can depend on the same protocol
types the server and clients use, but the public surface is intentionally
minimal — see `tau-agent-plugin` for the plugin SDK.

## License

MIT
