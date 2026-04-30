# tau-agent-plugin-tasks

Task-system plugin for the [tau](https://github.com/tau-agent/tau) agent.

Provides the `task_*` tools used by tau's task board: filing tasks, planning,
scheduling, dispatching worker sessions, and the merge queue. Backed by a
SQLite database in the project's `.tau` directory.

Bundled with the default `tau` CLI; published separately so embedders can opt
in or out.

## License

MIT
