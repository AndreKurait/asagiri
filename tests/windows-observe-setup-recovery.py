"""Observe the relaunched desktop advancing to the Docker tools page."""
import sys
import time

from pywinauto import Desktop

window = Desktop(backend="uia").window(process=int(sys.argv[1]), class_name="AsagiriWindow")
window.wait("visible", timeout=90)
deadline = time.monotonic() + 300
while time.monotonic() < deadline:
    if any(item.element_info.automation_id == "mk" and item.is_visible() for item in window.descendants()):
        print("Observed the Docker tools page after relaunch", flush=True)
        break
    time.sleep(1)
else:
    raise RuntimeError("Relaunched setup did not reach the Docker tools page")
