# Writing documentation

Use the root README for installation and a first run. Keep mobile setup next to the corresponding package, record on-disk formats under `docs/reference/`, and put build and contributor procedures under `docs/contributing/`. Link each new guide from the [documentation map](../README.md).

Describe what a caller can do with the current implementation. Include prerequisites, one command or operation that exercises the behavior, and how to recognize success. Specify the runtime and clock requirements for timing claims.

The MCP server supplies operating instructions and tool schemas to connected agents. Update those when an operation changes and keep README walkthroughs consistent with them. Use repository-relative links and commands that work from the stated directory.
