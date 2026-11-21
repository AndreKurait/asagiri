"""Drive the published first-run distro choice on a disposable Windows runner."""
import sys
import time
from pywinauto import Desktop

window = Desktop(backend='uia').window(process=int(sys.argv[1]), class_name='AsagiriWindow')
window.wait('visible', timeout=90)
for _ in range(60):
    controls = window.descendants()
    for item in controls:
        if item.element_info.automation_id == 'chooseBtn' and item.is_enabled():
            print('Selecting:', item.window_text(), flush=True)
            item.invoke()
            sys.exit(0)
    time.sleep(1)
raise RuntimeError([(item.element_info.automation_id, item.window_text()) for item in controls][:100])
