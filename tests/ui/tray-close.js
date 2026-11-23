// parity: macos windows linux
// Settings → Appearance → Close the window to the tray: a Linux-only
// setting (Windows and macOS always close to the tray, so it has nothing
// to do there). The native icon, menu and close cannot be tested headless:
// this covers the toggle rendering and the pref round-trip only.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
show("settings");
const row = () => document.querySelector("[data-trayclose]");
t("Close the window to the tray is a Linux-only setting", () => {
  if (LINUX && !row()) throw new Error("toggle missing on Linux");
  if (!LINUX && row()) throw new Error("toggle shown off Linux");
});
if (LINUX) {
  t("Off pressed by default", () => {
    if (document.querySelector('[data-trayclose="0"]')?.getAttribute("aria-pressed") !== "true") throw new Error("Off not pressed");
  });
  t("On sends setpref closeToTray true", () => {
    document.querySelector('[data-trayclose="1"]').click();
    if (!sent.some((m) => m.type === "setpref" && m.name === "closeToTray" && m.value === true)) throw new Error("no setpref true: " + JSON.stringify(sent));
    if (prefs.closeToTray !== true) throw new Error("prefs not updated");
    if (document.querySelector('[data-trayclose="1"]')?.getAttribute("aria-pressed") !== "true") throw new Error("On not pressed after click");
  });
  t("Off sends setpref closeToTray false", () => {
    document.querySelector('[data-trayclose="0"]').click();
    if (!sent.some((m) => m.type === "setpref" && m.name === "closeToTray" && m.value === false)) throw new Error("no setpref false");
    if (prefs.closeToTray !== false) throw new Error("prefs not updated");
  });
  report([...R, "ITEM close to tray toggle"]);
} else {
  report(R);
}
