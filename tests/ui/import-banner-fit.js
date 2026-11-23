// lang: az
// window: 760x480
show("containers");
const source = "desktop-linux";
asagiri.on({ type: "importsources", sources: [{ id: source, label: "Docker Desktop", running: true }] });
const banner = document.querySelector("#impstrip");
const results = [];
for (const count of [1, 2]) {
  const volumes = Array.from({ length: count }, (_, i) => ({ name: `volume-${i}` }));
  asagiri.on({ type: "inventory", source, volumes });
  for (const [selector, label] of [[".is-text", `${count} verilənlər həcmi`], ["[data-impreview]", "Nəzərdən keçir"]]) {
    const element = banner.querySelector(selector);
    const range = document.createRange();
    range.selectNodeContents(element);
    const box = element.getBoundingClientRect();
    const strip = banner.getBoundingClientRect();
    const visible = [...range.getClientRects()].every(rect =>
      rect.left >= Math.max(0, box.left) - 1 && rect.right <= Math.min(innerWidth, box.right) + 1 &&
      rect.top >= strip.top - 1 && rect.bottom <= strip.bottom + 1);
    const fits = !banner.hidden && box.width > 0 && visible && element.textContent.includes(label);
    results.push(`${fits ? "PASS" : "FAIL"} import banner fits ${label} with ${count} volume(s)`);
  }
}
report(results);
