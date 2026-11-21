"""Operate the published WebView2 window and verify persisted CLI permissions."""
import json
import os
import subprocess
import time
import urllib.request

from playwright.sync_api import expect, sync_playwright


def status():
    output = subprocess.check_output([os.environ['ASAGIRI_TEST_CLI'], 'mcp', 'status'], timeout=15)
    return json.loads(output)


with sync_playwright() as driver:
    endpoint = 'http://127.0.0.1:19227'
    deadline = time.monotonic() + 100
    while time.monotonic() < deadline:
        try:
            with urllib.request.urlopen(endpoint + '/json/list', timeout=2) as response:
                if any(target.get('url') == 'https://asagiri.invalid/' for target in json.load(response)):
                    break
        except OSError:
            pass
        time.sleep(0.5)
    else:
        raise RuntimeError('Published desktop document did not open in WebView2')
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
    page.locator('[data-view="settings"]').click()
    page.locator('[data-stab="integrations"]').click()

    def control(name, value):
        return page.locator(f'[data-mcp="{name}"][data-value="{int(value)}"]')

    def verify(enabled, changes):
        expect(control('enabled', enabled)).to_have_attribute('aria-pressed', 'true')
        assert status() == {'enabled': enabled, 'allowContainerChanges': changes}
        print(f'PASS native Settings and CLI enabled={enabled} changes={changes}', flush=True)

    verify(False, False)
    expect(control('allowContainerChanges', True)).to_be_disabled()
    control('enabled', True).click()
    verify(True, False)
    control('allowContainerChanges', True).click()
    expect(control('allowContainerChanges', True)).to_have_attribute('aria-pressed', 'true')
    verify(True, True)
    control('enabled', False).click()
    verify(False, False)
    expect(control('allowContainerChanges', True)).to_be_disabled()
