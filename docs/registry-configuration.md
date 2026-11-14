# Registry configuration

Use `asagiri config show` to read the running engine's registry settings.
`asagiri config set` updates the keys in a JSON object, saves them in the engine's
`~/.asagiri/daemon.json`, and leaves unspecified settings intact:

```sh
asagiri config set '{"max-concurrent-downloads":2,"max-concurrent-uploads":3}'
asagiri config show
```

Use `asagiri config set -` to read the object from standard input. On Windows,
the command runs in Asagiri's WSL distribution and updates the engine's file.
Editing the file directly requires restarting the engine. API and command
updates apply to new transfers; a transfer already in progress keeps the
settings it started with. Invalid settings are rejected before saving.

| Setting | Default | Accepted values |
|---|---|---|
| `registry-mirrors` | `[]` | Up to 16 Docker Hub mirror URLs, in fallback order |
| `insecure-registries` | `[]` | Up to 64 explicit authorities or literal-IP CIDRs |
| `max-concurrent-downloads` | `4` | 1–16 layer downloads per pull; the small image config is fetched separately |
| `max-concurrent-uploads` | `5` | 1–16 blob uploads per push, including the image config |

These are the supported daemon keys. Unknown keys, invalid URLs, noninteger
limits and files larger than 64 KiB produce an error. The existing Settings
window and `asagiri registry-mirror` command edit the same mirror list while
preserving transport policy and transfer limits. When `daemon.json` does not
specify `registry-mirrors`, Asagiri imports the legacy
`registry-mirrors.json` list. An explicit empty list disables that fallback.

## Private registries

HTTPS with certificate verification is the default. Loopback registries keep
their existing HTTP behavior. To use a private HTTP registry or a self-signed
certificate, opt in to its exact authority:

```sh
asagiri config set '{"insecure-registries":["registry.example.internal:5000"]}'
docker login registry.example.internal:5000
docker pull registry.example.internal:5000/team/image:latest
```

Asagiri prefers HTTPS for an explicitly configured registry, accepts its
self-signed certificate, and falls back to HTTP if the HTTPS connection
fails. A mirror explicitly written as an HTTP URL uses HTTP:

```sh
asagiri config set '{
  "insecure-registries":["mirror.example.internal:5000"],
  "registry-mirrors":["http://mirror.example.internal:5000"]
}'
```

Authorities contain a hostname or IP and an optional port, without a scheme,
path, username or password. An entry for `registry.example.internal:5000`
does not cover another port, subdomains or a different redirect destination.
Certificate exceptions are checked again for each TLS destination, including
token servers and redirects. Removing an exception creates a new connection pool that verifies that
endpoint again for subsequent transfers. HTTP token authentication outside
loopback is restricted to the same explicitly configured registry authority.
Upload-session locations must remain on the registry's origin.

IPv4 and IPv6 CIDRs such as `10.20.0.0/16` and `fd12::/48` apply to image
references that contain a literal IP. They do not resolve hostnames into
trusted addresses; list hostname authorities explicitly. BuildKit accepts
individual registry authorities. List each registry used in a Dockerfile
explicitly instead of relying on CIDR matching for an unknown build target.

The managed BuildKit uses the same mirror and explicit registry exceptions.
Changing those settings refreshes its configuration. Transfer limits govern
Asagiri's image pull/push workers; BuildKit manages its own transfer workers.
Docker Hub credentials are never forwarded to a mirror.

The local Docker API exposes these settings through
`GET /asagiri/daemon-config` and `POST /asagiri/daemon-config`. The POST body
is the same JSON object accepted by `asagiri config set`.
