"use strict";
const commands = {
  macos: ["cargo xtask app-dev", "Requires Rust and Xcode command line tools."],
  linux: ["cargo xtask linux", "Requires Rust and Docker for the static build."],
  windows: ["cargo xtask windows-dev", "Requires Rust, MSVC build tools, Docker, and WSL2."],
};
const tabs = [...document.querySelectorAll("[data-platform]")];
function select(tab) {
  for (const item of tabs) {
    item.setAttribute("aria-selected", String(item === tab));
    item.tabIndex = item === tab ? 0 : -1;
  }
  const [command, hint] = commands[tab.dataset.platform];
  document.querySelector("#build-command code").textContent =
    `git clone https://github.com/AndreKurait/asagiri.git\ncd asagiri\n${command}`;
  document.querySelector("#build-command").setAttribute("aria-labelledby", tab.id);
  document.querySelector("#build-hint").textContent = hint;
}
tabs.forEach((tab, index) => {
  tab.addEventListener("click", () => select(tab));
  tab.addEventListener("keydown", (event) => {
    const next = event.key === "ArrowRight" ? (index + 1) % tabs.length
      : event.key === "ArrowLeft" ? (index + tabs.length - 1) % tabs.length
      : event.key === "Home" ? 0 : event.key === "End" ? tabs.length - 1 : -1;
    if (next < 0) return;
    event.preventDefault();
    select(tabs[next]);
    tabs[next].focus();
  });
});
document.querySelector("#copy-command").addEventListener("click", async () => {
  try {
    await navigator.clipboard.writeText(document.querySelector("#build-command code").textContent);
    document.querySelector("#copy-status").textContent = "Build commands copied.";
    document.querySelector("#copy-command").textContent = "Copied ✓";
    setTimeout(() => { document.querySelector("#copy-command").textContent = "Copy ↗"; }, 2000);
  } catch {
    const range = document.createRange();
    range.selectNodeContents(document.querySelector("#build-command code"));
    const selection = window.getSelection();
    selection.removeAllRanges();
    selection.addRange(range);
    document.querySelector("#copy-status").textContent = "Commands selected. Use your copy shortcut.";
  }
});
