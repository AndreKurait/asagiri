"use strict";
const tabs = [...document.querySelectorAll("[data-os]")];
function select(tab) {
  for (const item of tabs) {
    const selected = item === tab;
    item.setAttribute("aria-selected", String(selected));
    item.tabIndex = selected ? 0 : -1;
    document.getElementById(item.dataset.os).hidden = !selected;
  }
}
tabs.forEach((tab, index) => {
  tab.addEventListener("click", () => select(tab));
  tab.addEventListener("keydown", event => {
    if (!["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key)) return;
    event.preventDefault();
    const next = event.key === "Home" ? tabs[0] : event.key === "End" ? tabs[tabs.length - 1] : tabs[(index + 1) % tabs.length];
    select(next);
    next.focus();
  });
});
if (/Linux/.test(navigator.platform)) select(tabs[1]);

const channel = document.getElementById("channel");
const status = document.getElementById("release-status");
let releases;
const releasePage = "https://github.com/AndreKurait/asagiri/releases";
const buildPage = "https://github.com/AndreKurait/asagiri/actions/workflows/release.yml";
function setDownloadStatus(message) {
  // A cached page may predate this status element.
  const element = document.getElementById("download-status");
  if (element) element.textContent = message;
}
function pendingDownloads(message, progress = false) {
  setDownloadStatus(message);
  for (const arch of ["arm64", "x86_64"]) {
    const button = document.getElementById(`download-${arch}`);
    const platform = arch === "arm64" ? "Apple Silicon" : "Intel";
    button.removeAttribute("aria-disabled");
    button.classList.remove("disabled");
    button.href = progress ? buildPage : releasePage;
    button.textContent = progress ? `${platform} · View release progress ↗` : `Check ${platform} downloads ↗`;
  }
}
function render() {
  const name = channel.value;
  const option = name === "latest" ? "" : ` -s -- --channel ${name}`;
  const command = `curl -fsSL https://andrekurait.github.io/asagiri/install.sh | sh${option}`;
  for (const id of ["mac-command", "linux-command"]) document.getElementById(id).textContent = command;
  document.getElementById("image-command").textContent = `${command}${option ? "" : " -s --"} --no-start`;
  pendingDownloads("Checking for published installers…");
  if (!releases) return;
  const release = releases.channels[name];
  if (!release) {
    status.textContent = name === "stable" ? "No stable release published yet" : "No installer published yet";
    pendingDownloads(name === "stable"
      ? "No stable installer is available yet. Try Recommended or check the releases page."
      : "The first installer has not been published. Follow release progress for build status.", name !== "stable");
    return;
  }
  if (!/^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(-dev\.[1-9]\d*)?$/.test(release.version)) {
    status.textContent = "Download metadata could not be verified";
    pendingDownloads("Check the releases page for verified installers.");
    return;
  }
  status.textContent = `${release.version} · ${release.prerelease ? "Preview release" : "Stable release"}`;
  setDownloadStatus("Choose your Mac to download the installer.");
  for (const arch of ["arm64", "x86_64"]) {
    const url = release.downloads?.[`macos-${arch}`]?.url;
    const expected = `https://github.com/AndreKurait/asagiri/releases/download/v${release.version}/Asagiri-${release.version}-${arch}.dmg`;
    if (url !== expected) continue;
    const button = document.getElementById(`download-${arch}`);
    button.href = url;
    button.textContent = `Download for ${arch === "arm64" ? "Apple Silicon" : "Intel"} ↓`;
  }
}
channel.addEventListener("change", render);
render();
fetch("../releases.json", {cache: "no-cache"})
  .then(response => { if (!response.ok) throw new Error("No release index"); return response.json(); })
  .then(index => {
    if (index.repository !== "AndreKurait/asagiri" || !index.channels) throw new Error("Invalid release index");
    releases = index;
    render();
  })
  .catch(() => {
    status.textContent = "Could not check published downloads";
    pendingDownloads("Check the releases page for available installers.");
  });

for (const button of document.querySelectorAll("[data-copy]")) {
  button.addEventListener("click", async () => {
    const code = document.getElementById(button.dataset.copy);
    try {
      await navigator.clipboard.writeText(code.textContent);
      const label = button.textContent;
      button.textContent = "Copied ✓";
      document.getElementById("copy-status").textContent = "Command copied.";
      setTimeout(() => { button.textContent = label; }, 2000);
    } catch {
      const range = document.createRange();
      range.selectNodeContents(code);
      const selection = window.getSelection();
      selection.removeAllRanges();
      selection.addRange(range);
      document.getElementById("copy-status").textContent = "Command selected. Use your copy shortcut.";
    }
  });
}
