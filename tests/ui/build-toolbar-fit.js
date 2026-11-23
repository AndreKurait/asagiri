// lang: az
// window: 760x480
show("builds");
const build = { id: "fixture", builder: "default", status: "Completed", name: "demo" };
const step = { name: "[1/1] RUN echo example", log: "sample-output-123" };
asagiri.on({ type: "builds", builds: [build] });
asagiri.on({ ...build, type: "build", steps: [step] });

const button = document.querySelector("[data-blall]");
const label = document.createRange();
label.selectNodeContents(button.lastChild);
const text = label.getBoundingClientRect();
const box = button.getBoundingClientRect();
const fits = button.textContent.trim() === "Hamısını genişləndir" && text.width > 0 &&
  text.left >= Math.max(0, box.left) - 1 && text.right <= Math.min(innerWidth, box.right) + 1;
const wasCollapsed = !document.body.innerText.includes(step.log);
button.click();
const expanded = wasCollapsed && document.body.innerText.includes(step.log);
report([
  `${fits ? "PASS" : "FAIL"} localized Expand all label fits the narrow window`,
  `${expanded ? "PASS" : "FAIL"} expanding the step reveals its output`,
]);
