// A container's log can be selected and copied, without the line numbers.
kiri.on({ type: "status", version: "0.1.4", dev: true, app: "Kiri Dev", ours: "kiri-dev", context: "kiri-dev" });
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
show("containers"); openDetail("a1"); setTab("logs"); render();
kiri.on({ type: "logs", id: "a1", parts: [{ e: false, t: "[INFO] Started worker-1\n" }, { e: true, t: "ERROR: engine failed\n" }] });
t("selecting the log copies its lines, without the numbers", () => {
  const log = document.querySelector(".console.numbered");
  getSelection().selectAllChildren(log);
  eq(getSelection().toString().trim().split("\n"), ["[INFO] Started worker-1", "ERROR: engine failed"], "copied");
});
report(R);
