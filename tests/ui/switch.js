// Switching docker to Asagiri: the strip, the dialog, choosing what to bring,
// and Switch back. Run with scripts/ui-test.py.
const inv=(big)=>({type:"inventory",source:"orbstack",images:[{name:"grafana/grafana:latest",size:big?"40GB":"712MB",have:false},{name:"redis:7",size:"138MB",have:false},{name:"nginx:alpine",size:"52MB",have:true}],volumes:[{name:"pgdata",anonymous:false,have:false}],containers:[]});
const reset=(big=false)=>{sent.length=0;ctx.card=null;ctx.pending=null;document.querySelectorAll(".scrim").forEach(x=>x.remove());if(impModal)closeImportModal();ui.imp.running=false;for(const k of Object.keys(ui.imp.picked))ui.imp.picked[k].clear();
  asagiri.on({type:"status",version:"0.1.4",dev:true,app:"Asagiri Dev",ours:"asagiri-dev",context:"orbstack"});
  asagiri.on({type:"importsources",sources:[{id:"orbstack",label:"OrbStack",running:true}]});asagiri.on(inv(big));sent.length=0;};
const click=(sel)=>{const el=document.querySelector(sel);if(!el)throw new Error("no "+sel);el.click();};
const used=()=>sent.filter(x=>x.type==="usecontext");
const confirmSwitch=()=>{asagiri.on({...ui.status,context:ui.status.ours});asagiri.on({type:"usedcontext",name:ui.status.ours});};
const R=[];const t=(name,fn)=>{try{fn();R.push("PASS "+name)}catch(e){R.push("FAIL "+name+": "+e.message)}};
const eq=(a,b,m)=>{if(JSON.stringify(a)!==JSON.stringify(b))throw new Error(m+": got "+JSON.stringify(a)+" want "+JSON.stringify(b))};
show("containers");
t("strip and status pill offer the switch",()=>{reset();render();eq(!!document.querySelector("#impstrip [data-ctxswitch]"),true,"strip");eq(!!document.querySelector("#statusbar [data-ctxswitch]"),true,"pill");});
t("Windows Docker environment overrides disable ineffective switches", () => {
  for (const contextOverride of ["DOCKER_CONTEXT", "DOCKER_HOST"]) {
    for (const context of ["orbstack", null]) {
      reset();
      asagiri.on({...ui.status, context, contextOverride});
      render();
      eq(document.querySelector("#statusbar [data-ctxswitch], #statusbar [data-ctxrepair], #impstrip [data-ctxswitch]"), null, "no switch or repair");
      eq(document.querySelector("#statusbar").textContent.includes(contextOverride), true, "names the override");
      ctxDialog();
      eq(document.querySelector("#ctx-dlg"), null, "no switch dialog");
    }
  }
});
t("Not now switches nothing",()=>{reset();ctxDialog();click("#ctx-dlg [data-close]");eq(used().length,0,"usecontext");eq(!!document.getElementById("ctx-dlg"),false,"dialog closed");});
t("Choose what to bring does not switch",()=>{reset();ctxDialog();click("[data-ctxchoose]");eq(used().length,0,"usecontext");eq(!!impModal,true,"import window open");eq(impModal.querySelector("[data-impgo]").textContent.startsWith("Switch and import"),true,"button says Switch and import");});
t("closing the chooser switches nothing",()=>{reset();ctxDialog();click("[data-ctxchoose]");closeImportModal();eq(used().length,0,"usecontext");eq(ctx.pending,null,"pending cleared");eq(ctx.card,null,"no card");});
t("importing from the chooser switches, after the import starts",()=>{reset();ctxDialog();click("[data-ctxchoose]");impModal.querySelector("[data-impgo]").click();eq(sent.map(x=>x.type).filter(x=>x==="import"||x==="usecontext"),["import","usecontext"],"order");eq(used()[0].name,"asagiri-dev","target");eq(ctx.card.total>0,true,"card counts");});
t("Switch with the box ticked brings every image",()=>{reset();ctxDialog();eq(document.getElementById("ctx-bring").checked,true,"ticked when small");click("[data-ctxgo]");const im=sent.find(x=>x.type==="import");eq(im.value.images,["grafana/grafana:latest","redis:7"],"images");eq(used()[0].name,"asagiri-dev","switched");});
t("Switch with the box unticked only switches",()=>{reset();ctxDialog();document.getElementById("ctx-bring").checked=false;click("[data-ctxgo]");eq(sent.some(x=>x.type==="import"),false,"no import");eq(used().length,1,"switched");eq(ctx.card.total,0,"card without import");});
t("a big import is not ticked for you",()=>{reset(true);ctxDialog();eq(document.getElementById("ctx-bring").checked,false,"unticked over 10 GB");});
t("progress counts each finished item",()=>{reset();ctxDialog();click("[data-ctxgo]");confirmSwitch();asagiri.on({type:"importstep",kind:"images",name:"redis:7",state:"done"});eq(ctx.card.done,1,"done");eq(document.getElementById("ctxcard").textContent.includes("1 of 2"),true,"card shows 1 of 2");});
t("Switch back restores the previous engine",()=>{reset();ctxDialog();click("[data-ctxgo]");confirmSwitch();sent.length=0;click("#ctxcard [data-ctxback]");eq(used()[0].name,"orbstack","back to orbstack");eq(ctx.card,null,"card gone");});
t("switch success waits for the effective context",()=>{reset();ctxDialog();click("[data-ctxgo]");eq(document.getElementById("ctxcard").hidden,true,"awaiting host");asagiri.on({type:"usedcontext",name:ui.status.ours});eq(ctx.card,null,"override still selects another context");const feedback=document.querySelector("#toasts .toast:last-child .txt")?.textContent || "";eq(["asagiri-dev","orbstack","DOCKER_CONTEXT"].every(word=>feedback.includes(word)),true,"names both contexts and the setting to check");});
t("an owned name on a custom endpoint never confirms an engine switch",()=>{
  reset(); ctxDialog(); click("[data-ctxgo]");
  asagiri.on({...ui.status,context:ui.status.ours,customEndpoint:true});
  asagiri.on({type:"usedcontext",name:ui.status.ours});
  eq(ctx.card,null,"no switch confirmation");
  eq(document.querySelector("#statusbar .ctx-ok"),null,"no owned engine claim");
  eq(document.querySelector("#statusbar .ctx-warn").textContent.includes("Custom endpoint"),true,"custom endpoint visible");
});
t("failed switches dismiss the pending success card",()=>{reset();ctxDialog();click("[data-ctxgo]");asagiri.on({type:"usedcontext",name:ui.status.ours,error:"could not save"});eq(ctx.card,null,"failed");});
t("return preference changes only after Docker confirms the switch",()=>{
  reset(); setPref("ctxBack","desktop-linux");
  ctxDialog(); click("[data-ctxgo]");
  eq(prefs.ctxBack,"desktop-linux","preserved while pending");
  asagiri.on({type:"usedcontext",name:ui.status.ours,error:"could not save"});
  eq(prefs.ctxBack,"desktop-linux","preserved after error");
  ctxDialog(); click("[data-ctxgo]"); confirmSwitch();
  eq(prefs.ctxBack,"orbstack","confirmed return context");
});
t("failed context probe reports an unknown selection",()=>{reset();ctxDialog();click("[data-ctxgo]");asagiri.on({...ui.status,context:null});asagiri.on({type:"usedcontext",name:ui.status.ours});eq(ctx.card,null,"no success card");eq(document.querySelector("#statusbar [data-ctxswitch]"),null,"no invented engine to switch from");eq(document.querySelector("#statusbar .ctx-ok"),null,"no confirmed engine");eq(document.querySelector("#toasts .toast:last-child .txt")?.textContent.includes("couldn't verify"),true,"probe failure explained");});
t("unknown context can select the owned context without importing or inventing a previous engine",()=>{
  reset(); asagiri.on({...ui.status,context:null});
  click("#statusbar [data-ctxrepair]"); eq(used()[0].name,ui.status.ours,"owned context requested");
  confirmSwitch(); eq(ctx.card,null,"no switch-back card");
  eq(!!document.querySelector("#statusbar .ctx-ok"),true,"confirmed by host status");
});
t("repair ignores repeated clicks until Docker replies and permits retry after failure",()=>{
  reset(); asagiri.on({...ui.status,context:null});
  const repair = "#statusbar [data-ctxrepair]";
  click(repair); click(repair);
  eq(used().length,1,"one pending repair");
  asagiri.on({type:"usedcontext",name:ui.status.ours,error:"save failed"});
  click(repair); eq(used().length,2,"retry submitted");
  confirmSwitch(); eq(ctx.card,null,"retry confirmed");
});
report(R);
