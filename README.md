# eavesdrop

Records audio locally: **what plays on your computer** (system audio) plus **your microphone**,
mixed into one WAV file. It captures audio at the OS level, so it works with any app that plays
sound, without integrating with it.

> Recording laws differ by country/state. Some require consent from **everyone** who is recorded.
> Check what applies to you before recording other people.

## Download

Grab a single self-contained binary from the
[latest release](https://github.com/tamashy/eavesdrop/releases/latest). Nothing to install, just
run it.

| OS      | File | Run |
|---------|------|-----|
| Windows | `eavesdrop-windows-x86_64.zip` (ARM: `-aarch64`) | unzip, double-click `eavesdrop.exe` (or run it from a terminal for options) |
| macOS   | `eavesdrop-macos-universal.tar.gz` (Apple Silicon + Intel) | `tar xzf eavesdrop-macos-universal.tar.gz && xattr -d com.apple.quarantine eavesdrop; ./eavesdrop` |
| Linux   | `eavesdrop-linux-x86_64.tar.gz` (ARM: `-aarch64`) | `tar xzf eavesdrop-linux-x86_64.tar.gz && ./eavesdrop` |

- **Windows:** no Visual C++ redistributable needed (the C runtime is linked in). Windows
  SmartScreen may warn because the binary isn't signed: *More info → Run anyway*.
- **macOS:** the binary isn't notarized. The `xattr` command clears the "downloaded from the
  internet" flag, so Gatekeeper lets it run. Instead of that, you can right-click → Open once in Finder.
- **Linux:** needs glibc 2.35+ (Ubuntu 22.04, Debian 12, Fedora 36 or newer) and `libasound.so.2`
  (package `libasound2`/`alsa-lib`, present on any desktop install).

Every push to `main` also builds all platforms. The binaries are attached to the run under
*Actions* (artifact `eavesdrop-all-platforms`).

## Build

```sh
task                         # = cargo build --release → target/release/eavesdrop(.exe)
task install                 # copy to ~/.local/bin (override: task install PREFIX=/usr/local)
task run -- --list           # run with arguments
task --list                  # all tasks: build, run, test, lint, fmt, clean, …
```

Needs [Task](https://taskfile.dev) (`brew install go-task`), or use `cargo build --release` directly.
Release builds: push a tag, e.g. `git tag v0.1.0 && git push origin v0.1.0`; the workflow
publishes a GitHub Release with the binaries.

| OS      | How system audio is captured              | Requirements |
|---------|-------------------------------------------|--------------|
| macOS   | Core Audio process tap (system loopback)  | macOS 14.6+. Grant your terminal **Screen & System Audio Recording** + **Microphone** in System Settings › Privacy & Security |
| Windows | WASAPI loopback                           | none |
| Linux   | PulseAudio/PipeWire `*.monitor` source    | `libasound2-dev` + `pkg-config` to build; PulseAudio or `pipewire-pulse` running |

## Use

```sh
eavesdrop                        # open the app; Space starts → ~/Recordings/eavesdrop/eavesdrop_<date>.wav
eavesdrop --now                  # start recording immediately
eavesdrop --list                 # show devices
eavesdrop --mic "AirPods"        # choose microphone (substring of name/id)
eavesdrop --system "Headphones"  # choose output device to capture
eavesdrop --no-mic               # only system audio, no microphone
eavesdrop --keep-tracks          # also keep separate system/mic WAVs
eavesdrop --minutes 60           # auto-stop after 60 min of recorded time
eavesdrop -o ~/Desktop           # output folder
eavesdrop --config ./my.toml     # use another config file
eavesdrop --about "Podcast ep. 3" # note what the recording is about (saved next to it)
eavesdrop --no-tui               # plain one-line output, records immediately (also used when piped)
eavesdrop play                   # open only the player (doesn't open recording devices)
eavesdrop play rec.wav           # … and play this file
eavesdrop recordings             # print the list of recordings: date, length, size, file, about
```

## The app

`eavesdrop` opens one terminal app with two views. **Tab** switches between them, and the top
bar always shows whether you're recording. You can record, stop, listen, and record again
without leaving the app.

### ● Record

It opens in the **READY** state with the level meters already live, so you can check that system
audio and mic come through before pressing Space. The screen shows level meters and the last 60 s
of level for system audio and the mic, recorded time, size so far, and warnings (e.g. no system
audio → permission hint).

| Key                      | Action |
|--------------------------|--------|
| `Space` / `p` / `Enter`  | start recording · pause · resume |
| `s`                      | stop & save, then get ready for the next recording |
| `m`                      | mute/unmute your mic in the recording |
| `e`                      | note what this recording is about (saved with it) |
| `Tab`                    | go to Recordings |
| `q` / `Esc` / `Ctrl+C`   | quit (saves the current recording first, if one was started) |

Saving happens in the background. When it's done, the new file shows up in Recordings, already
selected: press Tab, then Enter, to listen to it. `--minutes` stops & saves the same way.

### ♫ Recordings

Lists the WAV files in the recordings folder, newest first, with what each one is about. Below
the list are the full description of the selected recording and the player. Playback goes to
the system default output device.

| Key                              | Action |
|----------------------------------|--------|
| `↑` `↓` / `j` `k`, `PgUp` `PgDn` | select recording |
| `Enter`                          | play selected |
| `Space` / `p`                    | pause · resume (at the end: play again) |
| `←` `→` / `h` `l`                | back / forward 5 s |
| `Shift+←` `Shift+→`, `[` `]`     | back / forward 30 s |
| `0`–`9`                          | jump to 0 %, 10 %, … 90 % |
| `Home`                           | back to the start |
| `+` / `-`                        | volume up / down (10 % steps, 0–200 %) |
| `e`                              | describe: what the recording is about (multi-line) |
| `r` / `F2`                       | rename (its description is renamed with it) |
| `d` / `Delete`                   | delete: moves it to the Trash / Recycle Bin after you confirm |
| `F5` / `R`                       | refresh the list |
| `Tab` / `Esc`                    | back to Record |
| `q`                              | quit |

You can't start playback while a recording is running, because the recording would capture it.
Pause or stop the recording first. Starting or resuming a recording pauses playback.

In the description editor, `Enter` starts a new line, `Ctrl+S` (or `F2`) saves, and `Esc`
cancels; with unsaved changes, `Esc` asks once more before discarding them. Pasting works,
including several lines at once. If the Trash isn't available (e.g. on some network drives),
delete offers to remove the file permanently instead.

### Descriptions

A description lives in a small text file next to the recording, with the same name:

```
eavesdrop_2026-11-08_10-00-00.wav
eavesdrop_2026-11-08_10-00-00.meta.toml
```

```toml
description = """
Ideas for the onboarding redesign
Try the shorter sign-up flow first"""
```

The WAV file itself is never modified. You can edit, grep or sync the `.meta.toml` files with
other tools. Renaming or deleting a recording in the app does the same to its description.

Paused time is cut out of the file (not filled with silence); system audio and mic stay in sync.

## Config

Optional TOML file at `~/.config/eavesdrop/config.toml` (or `$XDG_CONFIG_HOME/eavesdrop/config.toml`;
Windows: `%APPDATA%\eavesdrop\config.toml`):

```toml
# Folder where recordings are saved. `~` expands to your home directory.
output_dir = "~/Documents/Recordings"
```

`--out` overrides `output_dir`. If neither is set, recordings go to `~/Recordings/eavesdrop`.

Notes
- It captures *all* sound played through the chosen output device, so turn off sounds you
  don't want in the recording (other apps, notifications).
- Works with headphones: the capture is taken digitally from the output, not from the speakers.
- Gaps (when nothing is playing) are filled with silence, so system audio and mic stay in sync.
