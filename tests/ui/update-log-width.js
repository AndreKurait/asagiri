// window: 320x480
// lang: de
const results = [];
ui.upd = { phase: "available", version: "2.0", current: "1.0", notes: "", snoozing: true };
renderUpdate();
const controls = [...document.querySelectorAll("#updcard [data-upd]")];
const inside = controls.length >= 5 && controls.every(button => {
  const rect = button.getBoundingClientRect();
  return rect.width > 0 && rect.left >= 0 && rect.right <= innerWidth;
});
results.push((inside ? "PASS" : "FAIL") + " localized update controls fit the viewport");
const log = document.createElement("div");
log.className = "dg-log";
log.style.width = "290px";
log.innerHTML = dgRow({level: "warn", part: "daemon", msg: "Diagnostic message", time: new Date().toISOString()});
document.body.append(log);
const message = log.querySelector(".m");
results.push((message.offsetWidth >= log.clientWidth * 0.8 ? "PASS" : "FAIL") + " narrow log gives the message a full row");
report(results);
