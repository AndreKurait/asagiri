// parity: macos windows linux
// Permissions remain usable while the engine is stopped, fail closed on a
// settings error, and require a second explicit choice for container changes.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const config = { mcpServers: { asagiri: { command: "/opt/Asagiri/asagiri", args: ["mcp"] } } };
const saved = (enabled, allowContainerChanges) => asagiri.on({ type: "integrations", permissions: { enabled, allowContainerChanges }, config });
const control = (name, value) => document.querySelector(`[data-mcp="${name}"][data-value="${value}"]`);
const mutations = () => sent.filter((message) => message.type === "setintegrations");
const items = [];
asagiri.on({ type: "state", state: "stopped" });
prefs.tab = "integrations";
ui.integrations = null;
sent.length = 0;
show("settings");
t("opening Settings fetches local permissions even with the engine stopped", () => {
  if (!sent.some((message) => message.type === "integrations")) throw new Error("permissions not requested");
  if (mutations().length) throw new Error("opening settings changed permissions");
});
saved(false, false);
if (document.querySelector('[data-stab="integrations"]')) items.push("AI integrations settings tab");
if (control("enabled", 1)) items.push("MCP access permission");
if (control("allowContainerChanges", 1)) items.push("separate container change permission");
if (document.querySelector("[data-mcpcopy]")) items.push("copy MCP connection configuration");
t("disabled MCP cannot grant changes", () => {
  if (control("enabled", 0).getAttribute("aria-pressed") !== "true") throw new Error("MCP is not off");
  if (!control("allowContainerChanges", 1).disabled) throw new Error("changes can be granted before access");
  sent.length = 0;
  control("allowContainerChanges", 1).click();
  if (mutations().length) throw new Error("disabled button saved permissions");
});
t("enabling access grants only reads and waits for the host", () => {
  sent.length = 0;
  control("enabled", 1).click();
  if (JSON.stringify(mutations()[0]?.value) !== JSON.stringify({ enabled: true, allowContainerChanges: false })) throw new Error(JSON.stringify(sent));
  if (![...document.querySelectorAll("[data-mcp]")].every((button) => button.disabled)) throw new Error("a second save can race the first");
  if (control("enabled", 0).getAttribute("aria-pressed") !== "true") throw new Error("unsaved permission displayed as enabled");
  saved(true, false);
  if (control("allowContainerChanges", 1).disabled) throw new Error("host response did not release controls");
});
t("changes require their own choice and disabling access revokes both", () => {
  sent.length = 0;
  control("allowContainerChanges", 1).click();
  if (JSON.stringify(mutations()[0]?.value) !== JSON.stringify({ enabled: true, allowContainerChanges: true })) throw new Error(JSON.stringify(sent));
  saved(true, true);
  sent.length = 0;
  control("enabled", 0).click();
  if (JSON.stringify(mutations()[0]?.value) !== JSON.stringify({ enabled: false, allowContainerChanges: false })) throw new Error(JSON.stringify(sent));
  saved(false, false);
});
t("connection configuration copies exact JSON", () => {
  sent.length = 0;
  document.querySelector("[data-mcpcopy]").click();
  const value = sent.find((message) => message.type === "copy")?.name;
  if (JSON.stringify(JSON.parse(value)) !== JSON.stringify(config)) throw new Error("different configuration");
});
t("permission errors and command paths are displayed as text", () => {
  const untrusted = '<img src=x onerror="window.badMcp=true">';
  asagiri.on({ type: "integrations", permissions: { enabled: false, allowContainerChanges: false }, config: { command: untrusted } });
  if (document.querySelector("#content .pane img") || window.badMcp) throw new Error("command path became HTML");
  asagiri.on({ type: "integrations", error: untrusted });
  if (document.querySelector("[data-mcp]") || document.querySelector("#content .pane img") || window.badMcp) throw new Error("error exposed controls or became HTML");
  if (!document.querySelector("#content .pane").textContent.includes(untrusted)) throw new Error("error was hidden");
});
for (const language of ["zh-Hans", "ar"]) {
  t("permission descriptions are translated in " + language, () => {
    saved(false, false);
    prefs.tab = "appearance"; show("settings");
    const selector = document.getElementById("pref-lang");
    selector.value = language;
    selector.dispatchEvent(new Event("change"));
    prefs.tab = "integrations"; show("settings");
    const untranslated = english(document.querySelector("#content .pane"));
    if (untranslated.length) throw new Error(untranslated.join(", "));
  });
}
report([...R, ...items.map((item) => "ITEM " + item)]);
