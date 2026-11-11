# Sharing

Asagiri does not operate a hosted sharing service. Sharing is disabled until you
configure a server that implements the Share protocol.

Set `ASAGIRI_SHARE_SERVER=share.example.org:443` before starting Asagiri, or set
`server` in `~/.asagiri/share.json`. Then run `asagiri share login`. Your server
operator controls sign-in, domain names, quotas, abuse reporting, and retention.
Use a server you trust. A shared link is public to anyone who has its address.

Asagiri does not sign in to, send reports to, or open tunnels through Kiri's
hosted infrastructure by default. The protocol implementation is retained for
people running their own compatible service; this repository does not include
a hosted service or promise compatibility with a third-party service.

For local development, prefer container names and ports bound to loopback:

```sh
docker run --rm -p 127.0.0.1:8080:80 nginx:alpine
```

For problems with Asagiri itself, use this repository's issues. Report security
vulnerabilities through **Security → Report a vulnerability**.
