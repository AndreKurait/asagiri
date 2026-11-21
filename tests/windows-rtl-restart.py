"""Reproduce #140 with a fresh native process for each persisted language."""
import json
import os
from pathlib import Path
import subprocess
import time

import win32api
import win32con
import win32gui
import win32process
from pywinauto import Desktop


def quit_from_tray(process):
    hwnd = win32gui.FindWindow('AsagiriWindow', None)
    assert win32process.GetWindowThreadProcessId(hwnd)[1] == process.pid
    win32gui.PostMessage(hwnd, win32con.WM_APP + 2, 1, win32con.WM_RBUTTONUP)
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        menu = win32gui.FindWindow('#32768', None)
        if menu and win32process.GetWindowThreadProcessId(menu)[1] == process.pid:
            break
        time.sleep(0.05)
    else:
        raise RuntimeError('Native tray menu did not open')
    for key in (win32con.VK_END, win32con.VK_RETURN):
        win32api.keybd_event(key, 0, 0, 0)
        win32api.keybd_event(key, 0, win32con.KEYEVENTF_KEYUP, 0)
    assert process.wait(timeout=45) == 0


def control(window, predicate, label):
    deadline = time.monotonic() + 35
    seen = []
    while time.monotonic() < deadline:
        seen = []
        for item in window.descendants():
            info = item.element_info
            seen.append((info.control_type, info.name, info.automation_id))
            if predicate(info) and item.is_visible() and item.is_enabled():
                return item
        time.sleep(0.4)
    raise AssertionError(f'{label} unavailable. Accessible controls: {seen[:160]}')

prefs = Path(os.environ['LOCALAPPDATA']) / 'Asagiri/ui.json'
prefs.parent.mkdir(exist_ok=True)
prefs.write_text(json.dumps({'lang':'en','langAsked':True,'domainsAsked':True}), encoding='utf-8')
for expected, select in [('en','ar'),('ar','fa'),('fa',None)]:
    translations = {} if expected == 'en' else json.loads((Path(__file__).resolve().parents[1] / f'crates/asagiri/src/ui/lang/{expected}.json').read_text(encoding='utf-8'))
    proc = subprocess.Popen([os.environ['RTL_APP'], '--distro', 'Asagiri-rtl-140', '--pipe', r'\\.\pipe\asagiri-rtl-140'])
    try:
        window = Desktop(backend='uia').window(process=proc.pid, class_name='AsagiriWindow')
        window.wait('visible', timeout=60)
        for label in ['Settings','Appearance']:
            title = translations.get(label,label)
            control(window,lambda i: i.control_type=='Button' and i.name==title,title).invoke()
        picker = control(window, lambda i: i.automation_id=='pref-lang-btn', 'language picker')
        print(f'PASS native {expected} Settings after launch in PID {proc.pid}',flush=True)
        subprocess.run([os.environ['RTL_DOCKER'],'--host','npipe:////./pipe/asagiri-rtl-140','version'],check=True,timeout=90)
        if select:
            picker.invoke()
            option = control(window, lambda i: i.automation_id=='lp-'+select, select)
            option.click_input()
            for _ in range(100):
                if json.loads(prefs.read_text(encoding='utf-8')).get('lang')==select: break
                time.sleep(0.1)
            else: raise AssertionError(f'{select} selection did not persist')
        quit_from_tray(proc)
    finally:
        if proc.poll() is None:
            proc.kill()
            proc.wait()
