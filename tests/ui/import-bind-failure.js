// A failed container keeps its reason when importdone requests fresh inventory.
{
  const source = "desktop-linux";
  const inventory = {type: "inventory", source, images: [], volumes: [], containers: [{name: "example", image: "example", state: "stopped"}]};
  const error = "cannot import bind source /Users/example/project: No such file or directory";
  ui.imp.sources = [{id: source, label: "Docker Desktop"}];
  ui.imp.source = source;
  asagiri.on(inventory);
  ui.imp.col = "containers";
  openImportModal();
  const metadataNotice = document.querySelector(".imp-metadata-note")?.textContent;
  asagiri.on({type: "importstep", kind: "containers", name: "example", state: "running"});
  asagiri.on({type: "importstep", kind: "containers", name: "example", state: "error", error});
  asagiri.on({type: "importdone", done: 0, failed: 1});
  asagiri.on(inventory);
  const failure = document.querySelector('.imp-dialog .irow .err-text');
  const note = document.querySelector('.imp-dialog .imp-note');
  report([metadataNotice?.includes("extended attributes")
    ? "PASS container import explains snapshot metadata loss" : "FAIL missing snapshot metadata notice",
    failure?.innerText === "Failed" && failure.title === error && note.innerText.includes("1 failed")
    ? "PASS container failure and source path survive inventory refresh" : "FAIL missing import failure details"]);
}
