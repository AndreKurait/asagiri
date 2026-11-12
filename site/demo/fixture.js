// The app's native bridge is replaced with an in-memory demonstration.
// There is no container engine or network service behind this page.
// Keep the translation responder assembled with the page.
const demoLanguageBridge = window.webkit.messageHandlers.asagiri.postMessage;
// Native hosts change the webview's color scheme. In the browser, apply the
// same existing CSS rules when a visitor chooses a theme.
const demoLightRules = [...document.styleSheets].flatMap((sheet) => [...sheet.cssRules])
  .filter((rule) => rule.type === CSSRule.MEDIA_RULE && rule.conditionText === "(prefers-color-scheme: light)");
function demoTheme(theme) {
  for (const rule of demoLightRules)
    rule.media.mediaText = theme === "system" ? "(prefers-color-scheme: light)" : theme === "light" ? "all" : "not all";
  themeBtn();
}
const demoNow = Math.floor(Date.now() / 1000);
const demoContainers = [
  ["a1", "web", "nginx:alpine", 8080, 80, "morning"],
  ["a2", "api", "node:22-alpine", 3000, 3000, "morning"],
  ["a3", "postgres", "postgres:17-alpine", 5432, 5432, "morning"],
  ["a4", "cache", "redis:7-alpine", 6379, 6379, "morning"],
  ["a5", "worker", "python:3.13-alpine", 0, 0, "jobs"],
  ["a6", "scheduler", "alpine:3.21", 0, 0, "jobs"],
].map(([Id, name, Image, PublicPort, PrivatePort, project], i) => ({
  Id, Names: ["/" + name], Image, State: i === 5 ? "exited" : "running",
  Status: i === 5 ? "Exited (0) 12 minutes ago" : "Up 2 hours",
  Created: demoNow - 7800 - i * 120,
  Ports: PublicPort ? [{ PublicPort, PrivatePort, Type: "tcp", IP: "127.0.0.1" }] : [],
  Labels: { "com.docker.compose.project": project, "com.docker.compose.service": name },
}));
const demoImages = demoContainers.map((c, i) => ({
  Id: "sha256:" + String(i + 1).repeat(64), RepoTags: [c.Image], Created: demoNow - 86000 * (i + 1),
  Size: [48200000, 157000000, 275000000, 41400000, 61200000, 8120000][i],
  Containers: 1, Architecture: "arm64", Os: "linux", Labels: {},
}));
const demoVolumes = ["morning_postgres", "morning_cache", "jobs_data"].map((Name) => ({
  Name, Driver: "local", Scope: "local", CreatedAt: new Date((demoNow - 86400) * 1000).toISOString(),
  Mountpoint: "/var/lib/asagiri/volumes/" + Name + "/_data",
  Labels: { "com.docker.compose.project": Name.split("_")[0] }, Options: {},
  UsageData: { Size: 21400000, RefCount: 1 },
}));
const demoNetworks = ["morning_default", "jobs_default", "bridge"].map((Name, i) => ({
  Id: String(i + 1).repeat(64), Name, Driver: "bridge", Scope: "local", Internal: false,
  IPAM: { Config: [{ Subnet: `172.${20 + i}.0.0/16`, Gateway: `172.${20 + i}.0.1` }] },
  Labels: {}, Containers: {},
}));
const demoLists = () => asagiri.on({ type: "lists", containers: demoContainers, images: demoImages, volumes: demoVolumes, networks: demoNetworks });
let demoMirrors = [];
function demoInspect(id) {
  const c = demoContainers.find((item) => item.Id === id);
  if (!c) return;
  asagiri.on({ type: "inspect", id, info: {
    Id: c.Id, Name: c.Names[0], Image: demoImages.find((image) => image.RepoTags.includes(c.Image))?.Id,
    Created: new Date(c.Created * 1000).toISOString(),
    Config: { Image: c.Image, Hostname: c.Names[0].slice(1), WorkingDir: "/app", Env: ["APP_ENV=development", "TZ=UTC"], Cmd: ["serve"], Labels: c.Labels },
    State: { Running: c.State === "running", Status: c.State, ExitCode: 0, Pid: 214, StartedAt: new Date((demoNow - 7200) * 1000).toISOString() },
    HostConfig: { Memory: 0, NanoCpus: 0, NetworkMode: "morning_default", RestartPolicy: { Name: "unless-stopped" } },
    NetworkSettings: { Networks: { morning_default: { IPAddress: "172.20.0.2", Gateway: "172.20.0.1" } },
      Ports: Object.fromEntries(c.Ports.map((p) => [`${p.PrivatePort}/tcp`, [{ HostIp: "127.0.0.1", HostPort: String(p.PublicPort) }]])) },
    Mounts: [],
  } });
}
function demoCommand(message) {
  const { cmd, id, action } = message;
  if (cmd === "container") {
    const index = demoContainers.findIndex((c) => c.Id === id);
    if (index < 0) return;
    if (action === "delete" || action === "remove") demoContainers.splice(index, 1);
    else if (["start", "stop", "restart"].includes(action)) {
      const running = action !== "stop";
      Object.assign(demoContainers[index], { State: running ? "running" : "exited", Status: running ? "Up a few seconds" : "Exited (0) just now" });
    }
    demoLists(); demoInspect(id); return;
  }
  if (cmd === "inspect") return demoInspect(id);
  if (cmd === "lists") return demoLists();
  if (cmd === "logs") {
    asagiri.on({ type: "logs", id, parts: [{ e: false, t: [
      "09:00:00  Starting development service",
      "09:00:00  Configuration loaded",
      "09:00:01  Listening on 0.0.0.0:80",
      "09:00:03  GET /health 200 1ms",
      "09:00:14  GET /api/projects 200 12ms",
      "09:01:02  GET / 200 3ms",
      "09:01:15  Ready for your next idea.",
    ].join("\n") + "\n" }] });
    return;
  }
  if (cmd === "imagelayers") return asagiri.on({ type: "imagelayers", name: message.name, layers: [], history: [], platforms: ["linux/arm64", "linux/amd64"] });
  if (cmd === "prefs") return;
  if (cmd === "pref" || cmd === "setpref") {
    if (message.name === "theme") demoTheme(message.value);
    return;
  }
  if (cmd === "langwords") return demoLanguageBridge(JSON.stringify(message));
  if (cmd === "shareaccount") return asagiri.on({ type: "shareaccount", configured: false, signedIn: false });
  if (cmd === "importsources") return asagiri.on({ type: "importsources", sources: [] });
  if (cmd === "routerports") return asagiri.on({ type: "routerports",
    http: { port: 80, default: 80, serving: true, env: null },
    https: { port: 443, default: 443, serving: true, env: null } });
  if (cmd === "resources") return asagiri.on({ type: "resources", cpus: 4, memory_mib: 8192,
    running_cpus: 4, running_memory_mib: 8192, awake: true, overridden: false, max_cpus: 12, max_memory_mib: 16384 });
  if (cmd === "buildwith") return asagiri.on({ type: "buildwith", with: "asagiri", engine: null,
    ask: false, taken: false, ready: true, builder: "asagiri-build", context: "asagiri" });
  if (cmd === "mirrors") return asagiri.on({ type: "mirrors", mirrors: demoMirrors, applies_to: "docker.io" });
  if (cmd === "setmirrors") {
    demoMirrors = Array.isArray(message.value?.mirrors) ? message.value.mirrors.filter((m) => typeof m === "string") : [];
    return asagiri.on({ type: "mirrors", mirrors: demoMirrors, applies_to: "docker.io" });
  }
  if (cmd === "openurl" || cmd === "url" || cmd === "open") return toast("ok", "Links open in the installed app.");
  if (["stop", "start"].includes(cmd)) return asagiri.on({ type: "state", state: cmd === "start" ? "running" : "stopped" });
  // Navigation, preferences and inspectors work normally. Native operations
  // outside this fixture get an explicit response instead of pretending to run.
  if (!["stats", "stats-stop", "logs-stop", "fonts", "domains", "usage", "needs", "tools", "about", "proxy", "window", "focus", "ready", "updatecheck", "langwords"].includes(cmd))
    toast("ok", "This is sample data. Build Asagiri to use this action with real containers.");
}
window.webkit = { messageHandlers: { asagiri: { postMessage: (raw) => {
  try { const message = JSON.parse(raw); queueMicrotask(() => demoCommand(message)); } catch { /* Ignore malformed bridge messages. */ }
} } } };
document.body.classList.add("demo");
asagiri.on({ type: "status", version: "source", dev: false, share: true, app: "Asagiri", ours: "asagiri", context: "asagiri" });
asagiri.on({ type: "state", state: "running" });
asagiri.on({ type: "prefs", prefs: { theme: "dark", accent: "blue" } });
demoTheme("dark");
asagiri.on({ type: "shareaccount", configured: false, signedIn: false });
asagiri.on({ type: "updatestate", state: "source" });
asagiri.on({ type: "vm", awake: true, running: 5, cpus: 4, cpu_pct: 2.4, used_mib: 386, target_mib: 512, ceiling_mib: 8192, stall_pct: 0, oom_kills: 0 });
demoLists();
function demoView({ view, theme }) {
  if (["containers", "images", "volumes", "networks", "settings", "share"].includes(view)) show(view);
  if (["light", "dark"].includes(theme)) {
    asagiri.on({ type: "prefs", prefs: { theme } });
    demoTheme(theme);
  }
}
window.addEventListener("message", (event) => {
  if (event.source === parent && event.origin === location.origin && event.data?.type === "asagiri-demo") demoView(event.data);
});
demoView(Object.fromEntries(new URLSearchParams(location.search)));
window.ASAGIRI_DEMO_READY = true;
