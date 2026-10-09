# yank

![Rust](https://img.shields.io/badge/language-Rust-orange) ![Unlicense](https://img.shields.io/badge/license-Unlicense-green) [![Fe2O3](https://img.shields.io/badge/suite-Fe%E2%82%82O%E2%82%83-b7410e)](https://github.com/isene/fe2o3)

Clipboard history that pastes back where you were.

<img src="img/yank.svg" align="left" width="150" height="150">

Two halves in one binary. A recorder sits on X selection events
(CLIPBOARD and PRIMARY, so Ctrl+C and mouse selections both count)
and writes every copy to `~/.yank/hist/`: text, and copied pictures too.

It also keeps the two in step: a mouse selection becomes the CLIPBOARD
too, and a copy becomes PRIMARY, so `Shift+Insert` pastes the last
thing taken in every app.

A picker lists them newest first; Enter puts the one you chose into the
window you came from. No tray icon, no daemon that polls, nothing
running between copies.

<br clear="left"/>

## Usage

```sh
yank --watch           # the recorder, once per session (autostart it)
yank                   # the picker
yank --paste-into XID  # focus XID and paste (run by yank-pop)
```

The picker has two tabs. The left one is the history. The right one, `kept`,
has the entries you want for good: press `a` on a history entry to copy it
there, and it stays until you delete it (`~/.yank/keep/`). `Tab`, `←` `→` or
`h` `l` switch tabs.

In the picker: `↑ ↓` or `j k` select, `Enter` pastes, `d` deletes the
entry, `q` quits.

`e` opens a text entry in your editor: `$EDITOR`, or
[scribe](https://github.com/isene/scribe) when that is not set. Save and
quit, and you are back in the list with the bar on the changed entry, so
`Enter` pastes it. An entry you empty stays as it was.

## Pictures

A copied picture is kept as a PNG: a screenshot, "copy image" in a
browser, a selection in an image editor. Its row says `picture`, its
size and its age, and the picture is shown beside the list while the
bar is on it.

`Enter` puts it back on the clipboard and pastes it with `Ctrl+V`, the
key a browser, an image editor and Claude Code take a picture on.

- A copy that has both text and a picture, such as a browser's "copy
  image" with the address, is kept as two entries.
- The history keeps 20 pictures. Press `a` on one you want for good.
- A picture over 16 MB is not kept.
- The same picture copied again moves to the top. It is not stored twice.

The intended way in: a key that opens the picker in a terminal and
remembers the window that had focus. With tile:

```
exec /home/you/bin/yank --watch
bind Mod4+v exec /home/you/bin/yank-pop
```

where `yank-pop` is the two-line script in this repo's `bin/`.

## How the paste works

Enter makes yank own both CLIPBOARD and PRIMARY with the entry, then a
detached helper refocuses the target window (an EWMH
`_NET_ACTIVE_WINDOW` message, which tile honours) and sends one
Shift+Insert through XTEST.

Terminals paste PRIMARY on that key, most other X apps CLIPBOARD; owning
both makes the same keystroke work in either. The helper runs after the
picker's own terminal has closed, so the keystroke never lands in the
picker.

A picture goes to CLIPBOARD alone, since a terminal pastes PRIMARY as
text, and the key sent is `Ctrl+V`.

## Why not copyq

copyq worked until it did not: its selection-sync helpers hang under a
terminal that owns selections itself, pile up by the dozen, and its
process swarm filled the X server's XFixes subscription table so no
other clipboard tool could subscribe. yank is one process at idle,
woken only by XFixes when a selection changes.

## Battery

The recorder blocks in `wait_for_event`. A copy costs one event, two
requests (what the copy has, then the text or the picture) and one file
write. A mouse selection costs one request, as it is always text.
Idle: zero.

## Requirements

Linux, X11 with XFixes, `xclip` (to hold the selections after the
picker exits) and `xdotool` (for the paste keystroke). A picture is
shown in real pixels where the terminal shows images (glass, kitty),
and in coloured blocks elsewhere.

## Install

```sh
cargo build --release
ln -s "$PWD/target/release/yank" ~/bin/yank
cp bin/yank-pop ~/bin/
```

## License

Public domain (Unlicense).
