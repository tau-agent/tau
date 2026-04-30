# tau-agent-plugin-worker

Default worker plugin for the [tau](https://github.com/tau-agent/tau) agent.

Provides the everyday tool surface a coding agent needs: `bash`, `read`,
`write`, `edit`, `get_file_skeleton`, `get_function`, and `diagnostics_scan`.
File edits use anchor-based addressing for robustness against whitespace
drift, and the skeleton/function tools are powered by tree-sitter.

Bundled with the default `tau` CLI.

## License

MIT
