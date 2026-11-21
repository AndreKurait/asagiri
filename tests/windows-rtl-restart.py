"""Reproduce #140 with a fresh native process for each persisted language."""
import json
import os
from pathlib import Path
import subprocess
import time
import urllib.request

import win32api
import win32con
import win32gui
import win32process
from playwright.sync_api import expect, sync_playwright


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


with sync_playwright() as driver:
    for expected, select in [('en', 'ar'), ('ar', 'fa'), ('fa', None)]:
        proc = subprocess.Popen([os.environ['RTL_APP'], '--distro', 'Asagiri-rtl-140', '--pipe', r'\\.\pipe\asagiri-rtl-140'])
        try:
            endpoint = 'http://127.0.0.1:19227'
            deadline = time.monotonic() + 120
            last_targets = None
            last_connection_error = None
            while time.monotonic() < deadline:
                assert proc.poll() is None, proc.returncode
                try:
                    with urllib.request.urlopen(endpoint + '/json/list', timeout=2) as response:
                        last_targets = [(t.get('type'), t.get('url', '')[:140]) for t in json.load(response)]
                        if ('page', 'https://asagiri.invalid/') in last_targets:
                            break
                except OSError as error:
                    last_connection_error = str(error)
                time.sleep(0.5)
            else:
                windows = []
                def inspect_window(hwnd, _):
                    if win32process.GetWindowThreadProcessId(hwnd)[1] == proc.pid:
                        windows.append((win32gui.GetClassName(hwnd), win32gui.GetWindowText(hwnd), bool(win32gui.IsWindowVisible(hwnd))))
                    return True
                win32gui.EnumWindows(inspect_window, None)
                print(f'PID {proc.pid} windows={windows}, targets={last_targets}, last connection error={last_connection_error}', flush=True)
                profile = Path(os.environ['LOCALAPPDATA']) / 'Asagiri'
                print('Profile entries:', [entry.name for entry in profile.iterdir()] if profile.exists() else [], flush=True)
                subprocess.run(['tasklist', '/fi', 'imagename eq msedgewebview2.exe'], check=False)
                raise RuntimeError(f'{expected} desktop did not render after launch')
            browser = driver.chromium.connect_over_cdp(endpoint)
            page = next(p for c in browser.contexts for p in c.pages if p.url == 'https://asagiri.invalid/')
            # A fresh desktop can present these first-run dialogs before Settings.
            page.add_locator_handler(page.locator('.langstep'), lambda: page.keyboard.press('Escape'))
            def choose_localhost():
                page.locator('[data-pick="localhost"]').click()
                page.locator('#welcome-go').click()
            page.add_locator_handler(page.locator('#welcome-go'), choose_localhost)
            keep_builder = page.locator('[data-bwpick="docker"]')
            page.add_locator_handler(keep_builder, lambda: keep_builder.click())
            expect(page.locator('html')).to_have_attribute('lang', expected)
            if expected != 'en':
                expect(page.locator('html')).to_have_attribute('dir', 'rtl')
            page.locator('[data-view="settings"]').click()
            page.locator('[data-stab="appearance"]').click()
            expect(page.locator('#pref-lang-btn')).to_be_visible()
            print(f'PASS rendered interactive {expected} Settings in PID {proc.pid}, WebView2 {browser.version}', flush=True)
            subprocess.run([os.environ['RTL_DOCKER'], '--host', 'npipe:////./pipe/asagiri-rtl-140', 'version'], check=True, timeout=90)
            if select:
                page.locator('#pref-lang-btn').click()
                page.locator(f'[data-lang="{select}"]').click()
                expect(page.locator('html')).to_have_attribute('lang', select)
                prefs = Path(os.environ['LOCALAPPDATA']) / 'Asagiri/ui.json'
                for _ in range(100):
                    if prefs.exists() and json.loads(prefs.read_text(encoding='utf-8')).get('lang') == select:
                        break
                    time.sleep(0.1)
                else:
                    raise AssertionError(f'{select} preference did not persist')
            quit_from_tray(proc)
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.wait()
