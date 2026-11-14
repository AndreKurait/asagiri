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

The Linux integration suite exercises this interface with the official MCP
Python SDK 2.3.0, including typed discovery/results and live revocation.
The SDK is a test dependency; the Asagiri server does not require Python.

# Local browser

Run the browser interface as your own user:

```sh
asagiri web password
asagiri web
```

The first command asks you to choose and confirm a password without echoing
it. The second prints a local URL and stays in the foreground. Open that URL
and sign in with username **asagiri** and your chosen password. Start the
Asagiri engine normally before viewing live resources.

The browser lists containers, images, volumes and networks, and shows
container details and recent logs. To enable start, stop and restart:

```sh
asagiri web allow-changes on
```

Refresh the browser after changing permissions. Use `allow-changes off` to
return to read access, `asagiri web disable` to revoke access, or Ctrl-C in
the server's terminal to close its listener. `asagiri web enable` re-enables
a configured password, with container changes still off. Setting a new
password also clears permission to change containers. MCP permissions are
independent.

The server binds only to `127.0.0.1`, choosing a free port by default.
`asagiri web --port 8090` chooses a specific local port. It uses the same
native engine connection and operation allowlist as MCP. It does not start
an engine, proxy arbitrary Docker requests, or listen on a LAN interface.

Authentication uses standard HTTP Basic and an Argon2id password hash in
the private per-user file `integrations/web.json` beside MCP's settings.
Passwords must contain at least 12 characters, at most 256 UTF-8 bytes, and
no control characters. For noninteractive setup, `asagiri web password
--password-stdin` accepts one line from standard input; never put a password
in a command-line argument. Missing, unreadable or invalid settings deny
access. Every request verifies credentials against the current file, so
password changes and revocation apply to new requests. Already accepted
engine actions can finish.

The interface requires the exact local Host header and checks Origin and
browser fetch metadata. Requests that call engine tools must send JSON from
that same origin. Pages and API responses are private and uncached, with no
external scripts or resources. The server allows eight HTTP connections
and four authentication or engine workers, limits headers to 16 KiB and
request bodies to 64 KiB, and bounds slow reads and engine calls. Resource,
log and response limits match MCP. Application logs can contain secrets;
only enable access for people and software you trust on this machine.
