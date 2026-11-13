# Native window regression checks

These checks require the named native desktop. A passing browser suite or
cross-compilation does not count as a pass. Record the source commit, OS,
WebView2 version on Windows, observations, and any failure.

## Windows: close to tray (#168)

1. Open Asagiri, start a container terminal, and enter a command that prints
   a counter once a second. Leave text in a second terminal's input.
2. Record the process tree and private working sets for Asagiri's WebView2
   renderer processes. Close the window using its close button.
3. Confirm the tray remains available and containers keep running. With a
   debugger or WebView2 instrumentation, confirm the controller's
   `IsVisible` is false and `ICoreWebView2_3::IsSuspended` becomes true.
   Memory reclamation is best effort; a fixed megabyte threshold is not
   the acceptance criterion.
4. Reopen from the tray. Confirm the same tabs, terminal history, typed
   input, selected page, and current container state are present.
5. Repeat close/reopen rapidly at least 20 times. After every reopen,
   confirm the page accepts input and updates; a late suspension callback
   must not leave a visible window suspended.
6. Close while the page is first loading, then reopen. Repeat while setup
   completes and while terminal output is streaming. Confirm hidden
   updates do not permanently wake the renderer, and reopening does not
   lose terminal output.
7. Repeat with light, dark, and system theme settings.

## macOS: traffic lights after theme changes (#187)

1. Open a native Asagiri window on a test Mac.
2. Switch repeatedly among light, dark, and system themes. Confirm the
   close/minimize/zoom controls remain centered in the 44-point strip.
3. Repeat after resizing, leaving full screen, and changing focus.
4. Repeat in a right-to-left language and with macOS appearance changes
   while Asagiri follows the system theme.

The pure `window_buttons_stay_centered_in_the_strip_in_both_directions`
test checks the geometry. It does not verify AppKit's layout timing.

## Windows: resume incomplete setup (#92)

Use a disposable Windows account or VM; do not alter a real user's setup.

1. Interrupt setup after the distribution is registered but before the
   engine starts. Preserve the saved `done:false` state and restart Asagiri.
   Confirm it offers/retries the engine step rather than silently exiting.
2. Repeat with the daemon already running. Confirm it resumes tools setup.
3. Repeat with a previous setup error saved. Confirm retry is possible
   and a new failure displays its current cause.
4. Verify reboot-pending setup stays on the restart step until Windows
   actually reboots, then proceeds.
5. Remove the disposable distribution after completing setup. Restart
   Asagiri and confirm distribution selection is offered again.

The `incomplete_setup_resumes_from_persisted_state` unit test checks these
decisions without running WSL. Native WSL recovery remains a separate check.
