# AI clients through MCP

Asagiri provides a local stdio MCP server. Access starts disabled. Each client
launches `asagiri mcp` and talks to it through standard input and output.

Open **Settings → AI integrations**, enable **MCP access**, and copy the
connection configuration into your client's MCP settings. The configuration
uses the installed CLI's absolute path. The same controls are available in
the Mac, Windows and Linux windows, even while the engine is stopped.

From the command line:

```sh
asagiri mcp enable
asagiri mcp config
```

`config` prints a `mcpServers` entry with the correct executable for your
installation. Run the configured command as your own user. It uses Asagiri's
local Unix socket or Windows named pipe and does not start the engine.
Start Asagiri normally before asking for live container information.

The default permission allows these tools:

| Tool | Information returned |
| --- | --- |
| `asagiri_status` | Engine version, availability and resource state |
| `asagiri_list_containers` | Container identities, images, state and ports |
| `asagiri_inspect_container` | One container's state, selected configuration and resource limits |
| `asagiri_container_logs` | Recent stdout and stderr, with timestamps |
| `asagiri_list_images` | Local image identities, tags, digests and sizes |
| `asagiri_list_volumes` | Volume identities and usage |
| `asagiri_list_networks` | Network identities and address ranges |

Metadata excludes environment values, commands, labels, mount sources and
driver options. Application logs can still contain secrets. Enable access
only for clients you trust, and treat container output as untrusted data.

To let clients start, stop and restart existing containers, turn on
**Allow container changes** separately, or run:

```sh
asagiri mcp allow-changes on
```

That permission adds `asagiri_start_container`, `asagiri_stop_container` and
`asagiri_restart_container`. Shell execution, container creation and deletion,
builds and pruning are outside this interface.

Revoke access in Settings or with:

```sh
asagiri mcp disable
```

Connected clients lose permission to make new requests. Disabling access also
clears permission to change containers. Requests already sent to the engine
can finish; cancelling an MCP request closes its connection but cannot undo
an action already accepted by the engine. `asagiri mcp status` prints the
current permissions.

Permissions are stored per user in `~/.asagiri/integrations/mcp.json` on Mac
and Linux, or `%LOCALAPPDATA%\Asagiri\integrations\mcp.json` on Windows. Missing,
unreadable or invalid settings deny access. The MCP tools cannot edit these
settings.

The server implements MCP `2025-11-25`. Lists default to 50 records and allow
up to 100, with a total and a truncation indicator. Logs default to 200 lines,
allow up to 1,000, and return at most 32 KiB of log bytes with an explicit
truncation flag. Four tool calls may run concurrently; each has a 45-second
deadline. Discovery updates when permissions change. The transport accepts
messages up to 64 KiB and limits each response to 512 KiB.
