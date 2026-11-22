// parity: macos windows linux
// A named share uses the same container and sign-in flow as a normal share.
asagiri.on({ type: "status", version: "0.2.1", share: true, app: "Asagiri", ours: "asagiri", context: "asagiri" });
const results = [];
const test = (name, fn) => {
  try { fn(); results.push("ITEM " + name); }
  catch (e) { results.push("FAIL " + name + ": " + e.message); }
};
const eq = (a, b) => {
  if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(JSON.stringify(a) + " != " + JSON.stringify(b));
};
const port = { PublicPort: 8080, PrivatePort: 80, Type: "tcp" };
const container = (id, ports) => ({
  Id: id, Names: ["/" + id], Image: "nginx:alpine", State: "running", Status: "Up 1 min",
  Created: Date.now() / 1000 - 60, Ports: ports, Labels: {},
});
ui.containers.push(container("demo", [port]), container("demo-ports", []), container("demo-auto", [port]));
show("containers");
const pickName = (id) => {
  document.querySelector(`[data-more="${id}"]`).click();
  document.querySelector(".menu [data-share-name]").click();
  return $("sh-name").querySelector("input");
};
const share = () => sent.filter((x) => x.type === "share").map((x) => [x.name, x.value, x.action]);

test("the name stays selected through sign-in and review", () => {
  sh.account = { signedIn: false };
  const field = pickName("demo");
  eq(document.activeElement === field, true);
  sent.length = 0;
  field.value = "demo.example.com";
  field.form.requestSubmit();
  eq(field.validity.patternMismatch, true);
  eq(share(), []);
  field.value = "Demo";
  field.form.requestSubmit();
  eq(!!$("sh-signin"), true);
  eq(share(), []);
  asagiri.on({ type: "shareaccount", signedIn: true, login: "demo" });
  eq(share(), [["demo", "demo", "reserve"]]);
  asagiri.on({ type: "share", id: 90, source: "demo", name: "demo", state: "connecting" });
  asagiri.on({ type: "share", id: 90, namePending: true });
  asagiri.on({ type: "share", id: 90, state: "live", url: "https://demo-4k2x.share.example.org" });
  const card = $("sh-modal");
  eq(card.querySelector(".sh-linkbox span").textContent, "demo-4k2x.share.example.org");
  eq(card.querySelector(".sh-popnote").textContent.includes("waiting for review"), true);
  asagiri.on({ type: "share", id: 90, state: "live", url: "https://demo.share.example.org" });
  eq(card.querySelector(".sh-linkbox span").textContent, "demo.share.example.org");
  eq(!!card.querySelector(".sh-popnote"), false);
  card.querySelector("[data-close]").click();
  document.querySelector('[data-more="demo"]').click();
  eq(!!document.querySelector(".menu [data-share-name]"), false);
  closeMenu();
});

test("a named share publishes its port first and can retry with the same name", () => {
  const field = pickName("demo-ports");
  sent.length = 0;
  field.value = "demo-web";
  field.form.requestSubmit();
  eq(sent.filter((x) => x.type === "listening").map((x) => x.id), ["demo-ports"]);
  eq(share(), []);
  asagiri.on({ type: "listening", id: "demo-ports", name: "", ports: [{ container_port: 80, host_port: 8080 }], offer: false });
  eq(sent.filter((x) => x.type === "ports").map((x) => x.id), ["demo-ports"]);
  eq(share(), []);
  asagiri.on({ type: "lists", containers: ui.containers.map((c) => c.Id === "demo-ports" ? container(c.Id, [port]) : c), images: [], volumes: [], networks: [] });
  eq(share(), [["demo-ports", "demo-web", "reserve"]]);
  asagiri.on({ type: "share", id: 91, source: "demo-ports", name: "demo-web", namePending: false, state: "ended", reason: "No network", error: true });
  sent.length = 0;
  document.querySelector('[data-shretry="demo-ports"]').click();
  eq(share(), [["demo-ports", "demo-web", "reserve"]]);
});

test("dismissing the name picker leaves the one-click share alone", () => {
  const field = pickName("demo-auto");
  sent.length = 0;
  field.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
  eq(!!$("sh-name"), false);
  eq(share(), []);
  document.querySelector('[data-sharec="demo-auto"]').click();
  eq(share(), [["demo-auto", "", undefined]]);
});

report(results);
