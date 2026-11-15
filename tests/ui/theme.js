// Kiri #291: a GTK theme preference must also change the web page, even when
// WebKitGTK's system media query keeps reporting the opposite scheme.
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const root = document.documentElement;
const value = (name) => getComputedStyle(root).getPropertyValue(name).trim();
const check = (pick, light) => {
  setPref("theme", pick);
  const colors = light
    ? { "--bg": "#F3F1F7", "--text": "#161B33", "--lit-hi": "#0F9E6A", "--term-bg": "#FFFFFF", "--term-fg": "#1F2328" }
    : { "--bg": "#161B33", "--text": "#F3F1F7", "--lit-hi": "#A8F5D4", "--term-bg": "#0B0F13", "--term-fg": "#D7DCE1" };
  for (const [name, wanted] of Object.entries(colors))
    if (value(name).toLowerCase() !== wanted.toLowerCase()) throw new Error(`${pick}: ${name} is ${value(name)}, wanted ${wanted}`);
  if (getComputedStyle(root).colorScheme !== (light ? "light" : "dark")) throw new Error(`${pick}: wrong native control scheme`);
  const link = document.createElement("a");
  link.className = "link";
  const expected = document.createElement("a");
  expected.style.color = `var(${light ? "--blue" : "--blue-light"})`;
  document.body.append(link, expected);
  try {
    if (getComputedStyle(link).color !== getComputedStyle(expected).color) throw new Error(`${pick}: link color ignores the preference`);
  } finally { link.remove(); expected.remove(); }
};
t("explicit light preference changes the page and terminal", () => check("light", true));
t("explicit dark preference changes the page and terminal", () => check("dark", false));
t("switching back to light repaints the page", () => check("light", true));
t("system preference follows the media query again", () => check("system", !matchMedia("(prefers-color-scheme: dark)").matches));
report(R);
