# asagiri-win setup: page ⇄ host protocol

The first-run setup page (`setup.html`, Windows only) talks to asagiri-win the
way the main page does: it sends JSON strings with
`window.webkit.messageHandlers.asagiri.postMessage(JSON.stringify({cmd, ...}))`
(asagiri-win's shim forwards that to WebView2) and receives state with
`window.asagiri.on(event)`.

## Page → host

| cmd | fields | when |
|---|---|---|
| `setup-ready` | | the page has loaded; the host answers with the current `setup` state |
| `setup-install-wsl` | | Welcome → "Install WSL" |
| `setup-restart` | | Restart → "Restart now" |
| `setup-later` | | Restart → "Restart later" (hides the window to the tray) |
| `setup-choose` | `value`: `"asagiri"` or `"ubuntu"` | Choose a Linux → the primary button |
| `setup-switch` | `value`: `"asagiri"` | while Ubuntu downloads → "Use Asagiri Linux instead" |
| `setup-cancel` | | any progress screen → "Cancel" (back to the step's start) |
| `setup-retry` | | error → "Try again" |
| `setup-default` | `value`: bool | Docker tools → "Make Asagiri the default for docker" checkbox |
| `setup-build` | `value`: `"docker"` or `"asagiri"` | Docker tools → Builds (shown only with another engine and Asagiri not made docker's default): "Keep building with Docker" / "Build with Asagiri" |
| `setup-autostart` | `value`: bool | Ready → "Start Asagiri when I sign in" |
| `setup-open` | | Ready → "Open Asagiri" (the main window replaces setup) |
| `setup-quit` | | Welcome → "Quit" |
| `copy` | `value`: text | Ready → "Copy" |
| `open` | `name`: https URL | "What is WSL?" |

## Host → page

One event type, sent whenever anything changes (at most ~10 a second):

```json
{
  "type": "setup",
  "step": "welcome" | "wsl" | "restart" | "choose" | "linux" | "engine" | "tools" | "ready",
  "linux": "asagiri" | "ubuntu" | null,
  "sizes": {"wsl": 367575040, "ubuntu": 418495746, "asagiri": 9000000},
  "existingUbuntu": false,
  "otherDocker": false,
  "otherEngine": null | "Docker Desktop",
  "makeDefault": true,
  "buildAsagiri": true,
  "autostart": true,
  "progress": {"pct": 58, "done": 224395264, "total": 367575040, "rate": 19084083, "eta": 9, "label": "Downloading WSL 3.0.3 from Microsoft"} | null,
  "tasks": [{"label": "Download WSL", "state": "done" | "active" | "todo" | "error", "detail": "214 / 368 MB"}],
  "log": ["asagiri daemon: ready in 33 ms"],
  "error": null | "text",
  "testError": null | "text"
}
```

- `step` picks the screen (variant A of the design canvas): welcome = A1,
  wsl = A2, restart = A3, choose = A4, linux = A5 (asagiri) / A5b (ubuntu),
  engine = A6, tools = A7, ready = A8.
- `progress.done`/`total` are bytes when downloading (show "214 of 368 MB",
  rate as MB/s, eta in seconds); `total` null means an unsized step (show
  the percentage only, or an indeterminate bar when `pct` is null).
- `error`: shown on the current step's screen with "Try again" (`setup-retry`).
- `testError`: the first-run test container that did not run. Setup carries
  on without it (a blocked Docker Hub must not trap a first run); the ready
  screen shows it and points at Settings → Registry mirrors.
- The left rail derives from `step`: steps before it done, it current, after it to do.
