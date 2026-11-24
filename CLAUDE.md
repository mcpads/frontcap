# Working with Frontcap

For installation and a first browser run, follow [README.md](README.md).
The MCP server provides [operating instructions](crates/mcp/src/server_instructions.txt) when the client connects.

Start browser work with `bootstrap` and follow the returned `next_action`. Check the selected engine, clock status and taint before making timing claims. Observe the app after each input. Retrieve the original result if a mutation loses its reply. End a run with its owning surface and verify the stop result.

When modifying the project, follow the [code conventions](docs/contributing/conventions.md). Update the [documentation map](docs/README.md) when adding or moving a public document.
