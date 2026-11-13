// parity: macos windows linux
// Issue 189: a stopped process backend does not measure the resident WSL VM.
const results = [];
const check = (label, fn) => {
  try { fn(); results.push("ITEM " + label); }
  catch (e) { results.push("FAIL " + label + ": " + e.message); }
};
const eq = (actual, expected) => {
  if (actual !== expected) throw new Error(JSON.stringify(actual) + " != " + JSON.stringify(expected));
};
const state = (value) => asagiri.on({ type: "state", state: value });
const vm = (awake) => asagiri.on({ type: "vm", awake, used_mib: 512, target_mib: 1024, ceiling_mib: 6144, running: 0, cpus: 4 });
const row = () => document.getElementById("vm-mem");
state("running");
vm(true);
check("running memory is displayed", () => eq(row().textContent, "512 MB / 6 GB"));
vm(false);
state("asleep");
check("idle memory respects the host backend", () => {
  eq(row().textContent, WIN ? "—" : "0 MB / 6 GB");
  if (WIN) {
    eq(row().title.includes("does not shut down WSL"), true);
    eq(row().title.includes("holds no memory"), false);
  }
});
state("running");
vm(true);
state("stopping");
check("Stop clears stale memory", () => eq(row().textContent, "—"));
state("stopped");
vm(false);
check("stopped memory events do not imply WSL released its memory", () => {
  eq(row().textContent, WIN ? "—" : "0 MB / 6 GB");
  if (WIN) {
    eq(VM, "Engine");
    eq(document.getElementById("statusbar").textContent.includes("Engine off"), true);
    eq(document.getElementById("statusbar").textContent.includes("Linux VM off"), false);
  }
});
vm(true); // A delayed sample from before Stop must not imply WSL reclamation.
check("Windows ignores stale running memory after Stop", () => {
  if (WIN) eq(row().textContent, "—");
});
state("running");
vm(true);
check("Start restores measured memory", () => eq(row().textContent, "512 MB / 6 GB"));
report(results);
