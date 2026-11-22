"""Reproduce #140 with a fresh native process for each persisted language."""
import ctypes
from ctypes import wintypes
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
    # Locate the actual Quit command in the localized popup by its native ID.
    user32 = ctypes.windll.user32
    hmenu = win32gui.SendMessage(menu, 0x01E1, 0, 0)  # MN_GETHMENU
    count = user32.GetMenuItemCount(wintypes.HMENU(hmenu))
    for index in range(count):
        if user32.GetMenuItemID(wintypes.HMENU(hmenu), index) == 4:  # ID_QUIT
            break
    else:
        raise RuntimeError('Quit command missing from native tray menu')
    rect = wintypes.RECT()
    if not user32.GetMenuItemRect(None, wintypes.HMENU(hmenu), index, ctypes.byref(rect)):
        raise ctypes.WinError()
    win32api.SetCursorPos(((rect.left + rect.right) // 2, (rect.top + rect.bottom) // 2))
    win32api.mouse_event(win32con.MOUSEEVENTF_LEFTDOWN, 0, 0)
    win32api.mouse_event(win32con.MOUSEEVENTF_LEFTUP, 0, 0)
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
            picker.set_focus()
            picker.type_keys('{DOWN}', set_foreground=True)
            search = control(window, lambda i: i.automation_id == 'lp-q', 'language search')
            search.type_keys({'ar': 'Arabic', 'fa': 'Persian'}[select], set_foreground=True)
            control(window, lambda i: i.automation_id == 'lp-' + select, select)
            search.type_keys('{ENTER}', set_foreground=True)
            for _ in range(100):
                if json.loads(prefs.read_text(encoding='utf-8')).get('lang')==select: break
                time.sleep(0.1)
            else: raise AssertionError(f'{select} selection did not persist')
        quit_from_tray(proc)
    finally:
        if proc.poll() is None:
            proc.kill()
            proc.wait()
