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

## macOS: editing settings fields (#340)

1. Focus a settings text field. Use temporary text and restore the original
   value before leaving the page. Confirm ⌘A, ⌘C, ⌘V, ⌘X and ⌘Z select,
   copy, paste, cut and undo in the focused field without beeping.
2. Repeat using the Edit menu. Confirm its actions follow the focused
   field and that pasting does not trigger another app command.
3. Change the app language and repeat; the rebuilt Edit menu must retain
   its shortcuts and show the selected language's labels.

Compilation and browser tests do not exercise native shortcut dispatch.

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

## Windows: restore the daemon without undoing Stop (#61, #93)

Use a disposable Windows host and Asagiri distribution. These native checks
remain unverified by the pure state and concurrency tests.

1. Start a container with a restart policy, then terminate only the test
   distribution with `wsl --terminate <test-distribution>`. Confirm the
   Docker API comes back and record whether the container recovers.
2. Press Stop and confirm `/info` reports `AsagiriStopped: true`. Terminate
   the test distribution again. Confirm the daemon becomes reachable,
   `AsagiriStopped` stays true, and containers stay stopped until Start.
3. Repeat step 2 while reopening the window and while starting the app
   with `--background`. A failed initial `/info` must not turn into wake.
4. In the disposable host, use Settings to restart WSL while a working
   bridge disconnects. Confirm one copy/start sequence owns the startup;
   an overlapping caller checks the restored daemon before starting it.
5. Delay daemon startup and press Stop while recovery is pending. Record
   any unavailable-daemon error honestly. If Stop succeeds, recovery must
   not later clear its persisted stopped mark. The window must remain
   responsive while the background recovery attempt waits.
6. After an intentionally failed recovery, restore the test distribution
   and request Start. Confirm explicit Start wakes the engine and a
   later successful Stop remains effective.
7. In the disposable profile, remove its managed `asagiri-build` definition
   while the engine runs, then reopen the window or use `--background`.
   Confirm registration returns and the saved Docker/Asagiri build choice
   is preserved. Repeat while stopped: registration must wait for Start.
   Record any Buildx or daemon error instead of treating it as success.

The `daemon_start` unit tests cover the process-availability/wake boundary,
overlapping startup calls, a Stop during recovery, failed attempts, and a
bounded pending retry. The builder fixture exercises the actual saved-choice
policy on temporary files for running, stopped and unknown engine states.
These tests do not run Buildx or boot WSL, or prove native sleep recovery.

## macOS: first launch and visible errors (#250)

Use a disposable Mac desktop account and record its shell configuration.
These checks have not been performed by the pure filesystem regression suite.

1. Install the app in the test account. Make `.zprofile` a dangling symlink
   whose target directory is absent, then launch the app from Finder.
   Confirm the symlink remains unchanged, the other shell profiles are
   updated, the engine LaunchAgent is created and the window opens.
2. Repeat with a `.zprofile` containing invalid UTF-8 bytes. Confirm those
   bytes remain unchanged and optional shell integration does not prevent
   the window or engine setup.
3. In that disposable account, make the app's command-link directory
   unwritable before first launch. Confirm a visible error identifies
   command-link installation and the log contains `command: app-launch`.
   Confirm Quit dismisses the error; repair the directory and open again.
4. Repeat launch from the Dock and `open -a`. Inspect the app process and
   log; the `open` command's successful dispatch alone is not a test pass.
5. Confirm an existing Docker or OrbStack socket/context remains selected
   unless the user explicitly chooses to switch it.

The reporter's exact launch failure still needs reproduction; the filesystem
tests establish two setup failure cases, not the cause on that machine.

## Windows terminal focus during status refresh (#286)

1. Open a running container's terminal in the Windows desktop app. Type
   `printf '%s\n' AC` without submitting it, then move the caret left once.
2. Leave the terminal focused through three observed container status refreshes.
   Use a second container started or stopped from a separate CLI to make status
   changes visible. Keep the desktop app in the foreground while observing.
3. Without clicking the terminal again, type `B` and press Enter. Confirm the
   command runs once and prints `ABC`. This checks retained focus, caret
   position, and pending input together.
4. Repeat while the terminal is receiving periodic output. Record the source
   commit, Windows and WebView2 versions, and each observed refresh and result.

## macOS Docker context recovery after configuration loss (#98)

1. In a disposable account or private test home, start the Mac daemon on a
   private socket. Use the app's bundled Docker CLI and a temporary
   `DOCKER_CONFIG`. Load an Alpine image into this engine for the final check.
2. Create and select the `asagiri` context pointing to the private socket.
   Confirm `docker ps` works. Move the temporary `DOCKER_CONFIG` directory
   aside. Confirm `docker --context asagiri ps` reports a missing context
   and `docker buildx version` fails in this environment.
3. With the same socket, home and Docker configuration environment, run the
   app bundle's `Contents/MacOS/asagiri ui --start`. Inspect the recreated
   context with `docker context inspect asagiri`. Confirm the endpoint is
   the private socket and run `docker --context asagiri run --rm --network
   none alpine:3.21 echo context-recovered`. Expect `context-recovered`.
4. Repeat with a second live engine selected in the temporary Docker config.
   Create a uniquely named volume on that engine and remove only the
   `asagiri` context before starting the UI. Confirm `docker context show`
   still names the second engine, plain `docker volume ls` includes its
   volume, and `docker --context asagiri volume ls` omits it.
