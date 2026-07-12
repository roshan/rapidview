---
name: verify
description: Build, launch, and visually verify Rapid View (native macOS GUI) against a fixture — window capture, clicks, and keystrokes from the CLI.
---

# Verifying Rapid View changes

Rapid View is a native AppKit app running on this Mac — verification is
launching it against a fixture and screenshotting the real window.

## Launch

```sh
cargo run -p rapid-view -- fixtures/sample.csv   # or .json / .xml
```

Run it in the background; stderr prints `loaded <path> (<bytes>, <lines>,
<Format>)` when the parse lands. Panics from GUI launches go to
`/tmp/rapid-view-panic.log` (stderr is /dev/null under Finder, and objc2
debug builds panic on ObjC encoding mismatches — check there first).

Note: a CLI launch loads the file **twice** (argv loop + AppKit's
`application:openFile:` both fire) and opens two tabs. Known quirk, not a
regression; Finder launches are fine.

## Capture

```sh
osascript -e 'tell application "System Events" to tell (first process whose name contains "rapid") to get {position, size} of front window'
screencapture -x -R<x>,<y>,<w>,<h> out.png
```

Window may be on the secondary display → negative Y is normal and works
with `-R`.

## Drive

- Clicks: `cliclick c:<x>,=<y>` (`=` prefix required for negative
  coordinates). **The first click on a non-key window only activates it**
  — it never reaches the DocView. Click twice, verify via the breadcrumb.
- Keystrokes: focus is contested if Roshan is at the machine. Always
  front the app and send keys in ONE osascript block:

  ```applescript
  tell application "System Events"
    set frontmost of (first process whose name contains "rapid") to true
    delay 0.3
    keystroke "p" using command down
  end tell
  ```

  Never split "activate" and "keystroke" across separate commands — the
  user's browser will steal focus in between and eat the keys (⌘P in a
  browser = print dialog).

## What to eyeball

- Breadcrumb = path expression for last click (jq / XPath / xsv).
- ⌘P toggle: Prettify↔Original (JSON/XML), Table↔Original (CSV).
- ⌘F search: type query, Enter — orange highlight + breadcrumb follows
  the current match.
- Kill when done: `pkill -f "target/debug/rapid-view"`.
