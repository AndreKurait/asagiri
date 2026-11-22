// A container's Terminal tab runs one command at a time, and each reply lands
// under its own command (#338).
asagiri.on({ type: "status", version: "0.1.4", dev: true, app: "Asagiri Dev", ours: "asagiri-dev", context: "asagiri-dev" });
const R = [];
const t = (name, fn) => { try { fn(); R.push("PASS " + name); } catch (e) { R.push("FAIL " + name + ": " + e.message); } };
const eq = (a, b, m) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(m + ": got " + JSON.stringify(a) + " want " + JSON.stringify(b)); };
show("containers"); openDetail("a1"); setTab("terminal"); render();
const execs = () => sent.filter((s) => s.type === "exec" && s.path !== ui.detail.term.probed);
const type = (cmd) => { $("term-in").value = cmd; $("term-form").requestSubmit(); };
const bodies = () => [...$("term-out").children].map((b) => b.querySelector(".body").textContent);
t("a second command waits in the box until the first one ends", () => {
  type("sleep 5; echo first");
  type("echo second");
  eq(execs().length, 1, "commands sent");
  eq($("term-in").value, "echo second", "typed text kept");
  asagiri.on({ type: "exec", id: "a1", cmd: execs()[0].path, code: 0, output: "first\n" });
  type("echo second");
  eq(execs().length, 2, "commands sent after the first ended");
  asagiri.on({ type: "exec", id: "a1", cmd: execs()[1].path, code: 0, output: "second\n" });
  eq(bodies(), ["first", "second"], "outputs");
});
t("clear lets go of a command that never ends, and its late reply goes nowhere", () => {
  type("psql -U postgres");
  const stuck = execs().pop().path;
  type("clear");
  type("ls");
  eq(execs().pop().path !== stuck, true, "ls not sent");
  asagiri.on({ type: "exec", id: "a1", cmd: stuck, code: -1, output: "timed out\n" });
  eq(bodies(), ["running…"], "the late reply took the ls block");
});
t("a cleared command cannot complete a repeated command or change its prompt", () => {
  type("clear");
  type("pwd");
  const old = execs().pop().path;
  type("clear");
  type("pwd");
  const current = execs().pop().path;
  asagiri.on({ type: "exec", id: "a1", cmd: old, code: 0, output: "old\n\x1eroot\x1fhost\x1f/stale" });
  eq(bodies(), ["running…"], "old reply claimed current command");
  eq(ui.detail.term.cwd === "/stale", false, "old reply changed prompt");
  asagiri.on({ type: "exec", id: "a1", cmd: current, code: 0, output: "current\n" });
  eq(bodies(), ["current"], "matching reply");
});
report(R);
