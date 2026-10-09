//! yank — clipboard history for the Fe2O3 suite.
//!
//! Two halves in one binary:
//!
//!   yank --watch     the recorder. Sits on XFixes selection events and
//!                    writes every copy and mouse selection to ~/.yank/hist/, one
//!                    file per entry, newest kept, capped. A copied
//!                    picture is kept as a PNG. Fully event-driven: zero
//!                    wakeups between copies. It records nothing while
//!                    the picker has an entry open in the editor.
//!
//!   yank             the picker. Lists the history newest first; Enter
//!                    owns CLIPBOARD and PRIMARY with the chosen entry
//!                    and leaves ~/.yank/paste as a flag. A picture goes
//!                    to CLIPBOARD alone. `e` opens a text entry in the
//!                    editor first.
//!
//!   yank --paste-into XID   run by the yank-pop wrapper once the
//!                    picker's terminal has closed: refocus XID and send
//!                    one Shift+Insert. glass pastes PRIMARY on that key,
//!                    most other apps CLIPBOARD; owning both makes the
//!                    same keystroke work in either. For a picture the
//!                    key is Ctrl+V.
//!
//! Replaces copyq: no tray, no Qt, no selection-sync helpers to hang.

use crust::{style, Crust, Input, Pane};
use std::io::Write as _;
use std::path::PathBuf;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ClientMessageEvent, ConnectionExt as _, CreateWindowAux, EventMask,
    GetPropertyReply, KeyButMask, PropMode, Property, SelectionNotifyEvent,
    SelectionRequestEvent, WindowClass, SELECTION_NOTIFY_EVENT,
};
use x11rb::wrapper::ConnectionExt as _;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const KEEP: usize = 100; // history entries kept
const MAX_ENTRY: usize = 65536; // bytes; larger copies are not recorded
const KEEP_PICS: usize = 20; // pictures among them: they are big
const MAX_PIC: usize = 16 << 20; // bytes; a bigger picture is not recorded

fn hist_dir() -> PathBuf { yank_dir("hist") }

/// Entries kept for good: the picker's second tab. Same files as the
/// history, copied over with `a`, never trimmed.
fn keep_dir() -> PathBuf { yank_dir("keep") }

fn yank_dir(name: &str) -> PathBuf {
    let d = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".into()))
        .join(".yank").join(name);
    let _ = std::fs::create_dir_all(&d);
    d
}

fn intern(conn: &RustConnection, name: &[u8]) -> Option<Atom> {
    Some(conn.intern_atom(false, name).ok()?.reply().ok()?.atom)
}

// ---------------------------------------------------------------------------
// The recorder
// ---------------------------------------------------------------------------

/// One watcher only. An abstract socket name holds the claim: it
/// vanishes with the process, so there is no stale lock file. Raw libc,
/// as in drain, because std's UnixListener rejects a NUL-prefixed path.
fn claim_single_instance() -> bool {
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return true; // cannot check: run rather than refuse
        }
        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        addr.sun_family = libc::AF_UNIX as u16;
        // One recorder per display: the lock name carries $DISPLAY.
        let name = format!("yank-watch{}", std::env::var("DISPLAY").unwrap_or_default());
        let name = name.as_bytes();
        for (i, b) in name.iter().enumerate() {
            addr.sun_path[i + 1] = *b as libc::c_char; // [0] stays NUL: abstract
        }
        let len = std::mem::size_of::<libc::sa_family_t>() + 1 + name.len();
        libc::bind(fd, &addr as *const _ as *const libc::sockaddr, len as u32) == 0
    }
}

fn watch() {
    if !claim_single_instance() {
        eprintln!("yank: watcher already running");
        return;
    }
    let (conn, screen_num) = match RustConnection::connect(None) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("yank: no X display: {}", e);
            std::process::exit(1);
        }
    };
    let root = conn.setup().roots[screen_num].root;
    let clipboard = intern(&conn, b"CLIPBOARD").expect("atom");
    let primary: Atom = AtomEnum::PRIMARY.into();
    let utf8 = intern(&conn, b"UTF8_STRING").expect("atom");
    let incr = intern(&conn, b"INCR").expect("atom");
    let dest_prop = intern(&conn, b"YANK_DATA").expect("atom");
    let targets = intern(&conn, b"TARGETS").expect("atom");
    let string: Atom = AtomEnum::STRING.into();

    // Hidden 1x1 window that receives the converted selection.
    let win = conn.generate_id().expect("id");
    conn.create_window(
        0, win, root, -1, -1, 1, 1, 0,
        WindowClass::INPUT_OUTPUT, 0,
        &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )
    .expect("window");

    let xf = conn.xfixes_query_version(5, 0);
    if xf.is_err() || xf.unwrap().reply().is_err() {
        eprintln!("yank: XFixes unavailable");
        std::process::exit(1);
    }
    // Subscribe on our own window, as Qt does; frame files the
    // subscription against the window given.
    // Both selections: Ctrl+C lands in CLIPBOARD, a mouse selection in
    // PRIMARY. Two subscriptions, two of frame's sixteen slots.
    for sel in [clipboard, primary] {
        conn.xfixes_select_selection_input(
            win, sel, xfixes::SelectionEventMask::SET_SELECTION_OWNER,
        )
        .expect("select input");
    }
    conn.flush().ok();

    let png = intern(&conn, b"image/png").expect("atom");
    // One request at a time, each answered into the same property.
    let ask = |target: Atom, sel: Atom| {
        let _ = conn.convert_selection(win, sel, target, dest_prop, x11rb::CURRENT_TIME);
        let _ = conn.flush();
    };
    let take = |max: usize| -> Option<GetPropertyReply> {
        conn.get_property(true, win, dest_prop, AtomEnum::ANY, 0, (max / 4) as u32 + 1)
            .ok()?
            .reply()
            .ok()
    };
    // The copy has a picture as well as text: ask for it once the text is in.
    let mut picture_next = false;
    // A big answer comes in pieces (INCR). What is in so far.
    let mut pieces: Option<Pieces> = None;

    let mut last = read_newest().unwrap_or_default();
    // The selection this window currently owns, and its text. A mouse
    // selection is mirrored into CLIPBOARD and a copy into PRIMARY, so
    // Shift+Insert pastes the last thing taken either way, in every app.
    let mut owned: Option<(Atom, String)> = None;
    let mut pending: Atom = clipboard; // which selection the reply is for
    if std::env::var_os("YANK_DEBUG").is_some() {
        eprintln!("yank: root={} win={} clipboard_atom={} dest_prop={}",
                  root, win, clipboard, dest_prop);
        eprintln!("yank: xfixes ext = {:?}",
                  conn.extension_information(xfixes::X11_EXTENSION_NAME));
    }
    loop {
        let ev = match conn.wait_for_event() {
            Ok(e) => e,
            Err(_) => return, // display gone: session over
        };
        if std::env::var_os("YANK_DEBUG").is_some() {
            match &ev {
                Event::PropertyNotify(p) => eprintln!(
                    "yank: PropertyNotify win={} atom={} state={:?}", p.window, p.atom, p.state),
                Event::SelectionNotify(s) => eprintln!(
                    "yank: SelectionNotify prop={} target={}", s.property, s.target),
                Event::XfixesSelectionNotify(x) => eprintln!(
                    "yank: XfixesSelectionNotify owner={} sel={}", x.owner, x.selection),
                other => eprintln!("yank: event {:?}", other),
            }
        }
        match ev {
            Event::XfixesSelectionNotify(x) => {
                if x.owner == win || x.owner == x11rb::NONE {
                    continue; // our own mirror, or a release
                }
                pending = x.selection;
                // A new copy: what was on its way for the last one is dropped.
                pieces = None;
                picture_next = false;
                // A copy can be a picture. Ask what is on offer first.
                // A mouse selection is text, and is asked for as before.
                if x.selection == clipboard {
                    ask(targets, clipboard);
                    continue;
                }
                // A mouse selection is claimed while the button may still
                // be down (Firefox re-claims on every drag step). Wait for
                // the release so one finished selection is stored, not a
                // trail of partial ones. Only spins during a drag.
                if x.selection == primary {
                    for _ in 0..300 {
                        let held = conn.query_pointer(root).ok()
                            .and_then(|c| c.reply().ok())
                            .map(|r| r.mask.intersects(
                                KeyButMask::BUTTON1 | KeyButMask::BUTTON2
                                | KeyButMask::BUTTON3))
                            .unwrap_or(false);
                        if !held { break; }
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                }
                // A new owner: ask for the text. The reply arrives as a
                // SelectionNotify + property on our window.
                ask(utf8, x.selection);
            }
            Event::SelectionNotify(sn) if sn.target == targets => {
                // What the copy is. An owner that will not say is asked
                // for text, as every owner was before pictures.
                let offered: Option<Vec<Atom>> = if sn.property == x11rb::NONE {
                    None
                } else {
                    take(4096).and_then(|p| p.value32().map(|v| v.collect()))
                };
                let (text, picture) = wanted(offered.as_deref(), utf8, png);
                picture_next = text && picture;
                if text {
                    ask(utf8, pending);
                } else if picture {
                    ask(png, pending);
                }
            }
            Event::SelectionNotify(sn) if sn.target == png => {
                if sn.property == x11rb::NONE {
                    continue;
                }
                let Some(prop) = take(MAX_PIC) else { continue };
                if prop.type_ == incr {
                    pieces = Some(Pieces { picture: true, bytes: Vec::new(), keep: true });
                } else if prop.bytes_after == 0 {
                    store_picture(&prop.value);
                }
            }
            Event::SelectionNotify(sn) => {
                // Text. Whatever comes of it, the picture is asked for
                // after, if the copy has one.
                'text: {
                    if sn.property == x11rb::NONE {
                        break 'text; // owner had no UTF8 text
                    }
                    let Some(prop) = take(MAX_ENTRY) else { break 'text };
                    if prop.type_ == incr {
                        // Bigger than is recorded. It is still taken to
                        // its end, or the owner waits for us for good.
                        pieces = Some(Pieces { picture: false, bytes: Vec::new(), keep: false });
                        break 'text;
                    }
                    if !is_text(prop.format, prop.type_, png) {
                        break 'text; // another answer was there; its own notice follows
                    }
                    let text = String::from_utf8_lossy(&prop.value).to_string();
                    let trimmed = text.trim();
                    if trimmed.is_empty() || text.len() > MAX_ENTRY {
                        break 'text;
                    }
                    // Nothing is recorded while the picker has an entry
                    // open in the editor. The copy is still mirrored.
                    let quiet = editing();
                    if text != last && !quiet {
                        store(&text);
                    }
                    let other = if pending == primary { clipboard } else { primary };
                    let _ = conn.set_selection_owner(win, other, x11rb::CURRENT_TIME);
                    let _ = conn.flush();
                    if !quiet {
                        last = text.clone();
                    }
                    owned = Some((other, text));
                }
                if picture_next && pieces.is_none() {
                    picture_next = false;
                    ask(png, pending);
                }
            }
            // The next piece of a big answer. Taking it out of the
            // property is what tells the owner to send one more; an empty
            // piece is the end.
            Event::PropertyNotify(p)
                if pieces.is_some() && p.window == win && p.atom == dest_prop
                    && p.state == Property::NEW_VALUE =>
            {
                let Some(prop) = take(MAX_PIC) else { pieces = None; continue };
                if !prop.value.is_empty() {
                    if let Some(pc) = pieces.as_mut() {
                        if pc.keep && pc.bytes.len() + prop.value.len() <= MAX_PIC {
                            pc.bytes.extend_from_slice(&prop.value);
                        } else {
                            pc.keep = false;
                            pc.bytes = Vec::new();
                        }
                    }
                    continue;
                }
                let Some(done) = pieces.take() else { continue };
                if done.picture {
                    if done.keep {
                        store_picture(&done.bytes);
                    }
                } else if picture_next {
                    picture_next = false;
                    ask(png, pending);
                }
                drop(done);
                give_back();
            }
            Event::SelectionRequest(r) => serve(&conn, &owned, &r, utf8, string, targets),
            Event::SelectionClear(c) => {
                if owned.as_ref().map_or(false, |(sel, _)| *sel == c.selection) {
                    owned = None;
                }
            }
            _ => {}
        }
    }
}

/// A text answer is 8-bit data. Two copies made right after each other
/// (an app that sets both selections, or one twice) are answered by two
/// programs into the one property. What is there when a text is announced
/// can then be the other program's list of formats, 32-bit numbers, or a
/// picture. Read as text, that list became an entry of odd bytes.
fn is_text(format: u8, type_: Atom, png: Atom) -> bool {
    format == 8 && type_ != png
}

/// Answer a SelectionRequest for the selection this window owns:
/// TARGETS, UTF8_STRING or STRING; anything else gets property None.
fn serve(
    conn: &RustConnection, owned: &Option<(Atom, String)>, r: &SelectionRequestEvent,
    utf8: Atom, string: Atom, targets: Atom,
) {
    let prop = if r.property == x11rb::NONE { r.target } else { r.property };
    let mut reply = x11rb::NONE;
    if let Some((sel, text)) = owned {
        if *sel == r.selection {
            if r.target == targets {
                let list = [targets, utf8, string];
                let _ = conn.change_property32(
                    PropMode::REPLACE, r.requestor, prop, AtomEnum::ATOM, &list,
                );
                reply = prop;
            } else if r.target == utf8 || r.target == string {
                let _ = conn.change_property8(
                    PropMode::REPLACE, r.requestor, prop, r.target, text.as_bytes(),
                );
                reply = prop;
            }
        }
    }
    let ev = SelectionNotifyEvent {
        response_type: SELECTION_NOTIFY_EVENT,
        sequence: 0,
        time: r.time,
        requestor: r.requestor,
        selection: r.selection,
        target: r.target,
        property: reply,
    };
    let _ = conn.send_event(false, r.requestor, EventMask::NO_EVENT, ev);
    let _ = conn.flush();
}

/// Hands freed memory back to the system after a big answer. glibc keeps
/// what was freed, and the recorder stayed 12 MB bigger for the rest of
/// the session after one 6 MB picture.
fn give_back() {
    #[cfg(target_env = "gnu")]
    unsafe {
        libc::malloc_trim(0);
    }
}

/// An answer that comes in pieces: a picture to keep, or text too big to
/// record, which is taken and dropped.
struct Pieces {
    picture: bool,
    bytes: Vec<u8>,
    /// False once it has outgrown what is recorded.
    keep: bool,
}

/// What to ask a copy for, from what its owner offers: (text, picture).
/// Both, when it has both: a browser's "copy image" carries the address
/// as text beside the picture. An owner that offers neither, or did not
/// answer, is asked for text, as every owner was before pictures.
fn wanted(offered: Option<&[Atom]>, utf8: Atom, png: Atom) -> (bool, bool) {
    let picture = offered.is_some_and(|o| o.contains(&png));
    let text = offered.is_none_or(|o| o.contains(&utf8)) || !picture;
    (text, picture)
}

/// Microseconds since 1970: the name of an entry, so names sort by age.
fn stamp() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros())
        .unwrap_or(0)
}

fn is_picture(p: &std::path::Path) -> bool {
    p.extension().is_some_and(|e| e == "png")
}

/// The files of a folder, oldest first: a name is a zero-padded time.
fn names_in(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .map(|it| it.flatten().map(|e| e.path()).collect::<Vec<_>>())
        .unwrap_or_default();
    names.sort();
    names
}

/// Which files go to bring a history down to its caps, given its names
/// oldest first: no more than `keep` entries, `pics` of them pictures.
fn surplus(names: &[PathBuf], keep: usize, pics: usize) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let pictures: Vec<&PathBuf> = names.iter().filter(|p| is_picture(p)).collect();
    let extra = pictures.len().saturating_sub(pics);
    out.extend(pictures.into_iter().take(extra).cloned());
    let left: Vec<&PathBuf> = names.iter().filter(|p| !out.contains(p)).collect();
    let extra = left.len().saturating_sub(keep);
    out.extend(left.into_iter().take(extra).cloned());
    out
}

fn trim(dir: &std::path::Path) {
    for p in surplus(&names_in(dir), KEEP, KEEP_PICS) {
        let _ = std::fs::remove_file(p);
    }
}

/// Where the picker keeps an entry while the editor has it open.
fn edit_dir() -> PathBuf {
    hist_dir().with_file_name("edit")
}

/// True while a picker has an entry open in the editor. The recorder
/// records nothing then: scribe copies every piece it deletes, and each
/// of them became an entry.
fn editing() -> bool {
    editing_in(&edit_dir())
}

/// The folder has the picker's process id. A picker that was killed
/// with the editor open leaves its folder behind, and the recording
/// must not stop for good.
fn editing_in(dir: &std::path::Path) -> bool {
    std::fs::read_to_string(dir.join("pid"))
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .is_some_and(|pid| std::path::Path::new(&format!("/proc/{}", pid)).exists())
}

/// The same text or picture again, picked from the history or copied
/// twice: the file it has moves to the top under its new name, and no
/// second one is written. True when it was there.
fn raised(dir: &std::path::Path, picture: bool, data: &[u8], to: &std::path::Path) -> bool {
    for old in names_in(dir).into_iter().filter(|p| is_picture(p) == picture) {
        let same_size = std::fs::metadata(&old).is_ok_and(|m| m.len() == data.len() as u64);
        if same_size && std::fs::read(&old).is_ok_and(|b| b == data) {
            return std::fs::rename(&old, to).is_ok();
        }
    }
    false
}

/// One file per entry, named by microsecond epoch, pruned to KEEP.
fn store(text: &str) {
    store_text_in(&hist_dir(), text);
}

fn store_text_in(dir: &std::path::Path, text: &str) {
    let path = dir.join(format!("{:020}.txt", stamp()));
    if raised(dir, false, text.as_bytes(), &path) {
        return;
    }
    if let Ok(mut f) = std::fs::File::create(&path) {
        let _ = f.write_all(text.as_bytes());
    }
    trim(dir);
}

/// A copied picture, kept as the PNG its owner handed over.
fn store_picture(data: &[u8]) {
    if !editing() {
        store_picture_in(&hist_dir(), data);
    }
}

fn store_picture_in(dir: &std::path::Path, data: &[u8]) {
    if png_size(data).is_none() {
        return; // not a PNG after all
    }
    let path = dir.join(format!("{:020}.png", stamp()));
    if raised(dir, true, data, &path) {
        return;
    }
    if std::fs::write(&path, data).is_ok() {
        trim(dir);
    }
}

/// Width and height of a PNG, from its first 24 bytes.
fn png_size(head: &[u8]) -> Option<(u32, u32)> {
    if head.len() < 24 || &head[..8] != b"\x89PNG\r\n\x1a\n" || &head[12..16] != b"IHDR" {
        return None;
    }
    let n = |at: usize| u32::from_be_bytes([head[at], head[at + 1], head[at + 2], head[at + 3]]);
    Some((n(16), n(20)))
}

/// What an entry is: text, or a picture known by its size.
enum What {
    Text(String),
    Picture { w: u32, h: u32, bytes: u64 },
}

/// The text copied last, which a copy of the same text is checked against.
fn read_newest() -> Option<String> {
    entries_in(&hist_dir()).into_iter().find_map(|(_, w)| match w {
        What::Text(t) => Some(t),
        What::Picture { .. } => None,
    })
}

/// Newest first, by file name (a zero-padded timestamp). Of a picture
/// only the first bytes are read: its size is in them.
fn entries_in(dir: &PathBuf) -> Vec<(PathBuf, What)> {
    let mut names = names_in(dir);
    names.reverse();
    names
        .into_iter()
        .filter_map(|p| {
            if !is_picture(&p) {
                return std::fs::read_to_string(&p).ok().map(|t| (p, What::Text(t)));
            }
            let mut head = [0u8; 24];
            let mut f = std::fs::File::open(&p).ok()?;
            std::io::Read::read_exact(&mut f, &mut head).ok()?;
            let (w, h) = png_size(&head)?;
            let bytes = f.metadata().ok()?.len();
            Some((p, What::Picture { w, h, bytes }))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The picker
// ---------------------------------------------------------------------------

/// A single list line out of an entry: control characters visible,
/// newlines folded to ⏎.
fn preview(t: &str, max: usize) -> String {
    let one: String = t
        .chars()
        .map(|c| if c == '\n' { '⏎' } else if c.is_control() { '·' } else { c })
        .collect();
    let one = one.trim().to_string();
    if one.chars().count() <= max {
        one
    } else {
        let mut s: String = one.chars().take(max - 1).collect();
        s.push('…');
        s
    }
}

/// How long ago an entry was copied, from its file name: "now", "5 min",
/// "3 h", "2 d".
fn ago(path: &std::path::Path, now: u128) -> String {
    let then = path.file_stem()
        .and_then(|s| s.to_str())
        .and_then(|s| s.parse::<u128>().ok())
        .unwrap_or(now);
    let secs = now.saturating_sub(then) / 1_000_000;
    match secs {
        0..=59 => "now".to_string(),
        60..=3599 => format!("{} min", secs / 60),
        3600..=86399 => format!("{} h", secs / 3600),
        _ => format!("{} d", secs / 86400),
    }
}

/// The list line of a picture: there is no text to show, so its size
/// and its age tell one from the next. The picture itself is shown
/// beside the list while the bar is on it.
fn picture_line(path: &std::path::Path, w: u32, h: u32, bytes: u64, now: u128) -> String {
    let size = if bytes >= 1 << 20 {
        format!("{:.1} MB", bytes as f64 / (1 << 20) as f64)
    } else {
        format!("{} kB", bytes.div_ceil(1024))
    };
    format!("picture {}\u{d7}{} \u{b7} {} \u{b7} {}", w, h, size, ago(path, now))
}

/// Own CLIPBOARD with the picture, outliving this process. Not PRIMARY:
/// a terminal pastes that as text.
fn own_picture(path: &std::path::Path) {
    if let Ok(mut child) = std::process::Command::new("setsid")
        .args(["xclip", "-selection", "clipboard", "-t", "image/png", "-i"])
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        let _ = child.wait();
    }
}

/// Own CLIPBOARD and PRIMARY with the text, outliving this process.
fn own_selections(text: &str) {
    for sel in ["clipboard", "primary"] {
        if let Ok(mut child) = std::process::Command::new("setsid")
            .args(["xclip", "-selection", sel])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            if let Some(ref mut si) = child.stdin {
                let _ = si.write_all(text.as_bytes());
            }
            let _ = child.wait();
        }
    }
}

/// What an edit leaves in an entry, from its text before and the text
/// the editor saved. An editor ends a file with a line end. An entry
/// that had none gets none, or a pasted command would run by itself in
/// a shell. None when the entry stays as it was: nothing was changed,
/// or everything was deleted.
fn edited(before: &str, saved: &str) -> Option<String> {
    let after = if before.ends_with('\n') { saved } else { saved.strip_suffix('\n').unwrap_or(saved) };
    (!after.is_empty() && after != before).then(|| after.to_string())
}

/// Open a text entry in the editor: `$EDITOR`, or scribe.
fn edit_entry(path: &std::path::Path, before: &str) {
    let editor = std::env::var("EDITOR").ok().filter(|e| !e.trim().is_empty());
    edit_in(&edit_dir(), editor.as_deref().unwrap_or("scribe"), path, before);
}

/// The editor gets a copy in a folder of its own, and the folder goes
/// when it is done. An editor may leave a backup beside the file it
/// saves, and a backup in the history would be listed as an entry.
/// The folder also tells the recorder to record nothing meanwhile.
fn edit_in(dir: &std::path::Path, editor: &str, path: &std::path::Path, before: &str) {
    let copy = dir.join("entry.txt");
    let mut words = editor.split_whitespace(); // "code --wait" is an editor too
    if let (Some(program), true) = (words.next(), std::fs::create_dir_all(dir).is_ok()) {
        let _ = std::fs::write(dir.join("pid"), std::process::id().to_string());
        if std::fs::write(&copy, before).is_ok() {
            let _ = std::process::Command::new(program).args(words).arg(&copy).status();
            if let Some(after) = std::fs::read_to_string(&copy).ok().and_then(|s| edited(before, &s)) {
                let _ = std::fs::write(path, after);
            }
        }
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// Flag the picker leaves for the wrapper: "Enter was pressed, paste".
fn paste_flag() -> PathBuf {
    hist_dir().parent().map(|p| p.join("paste")).unwrap_or_else(|| "/tmp/yank-paste".into())
}

/// Flag the picker leaves for `--paste-into`: what was taken is a
/// picture, so the key to send is another.
fn picture_flag() -> PathBuf {
    paste_flag().with_file_name("picture")
}

/// The key that pastes. Shift+Insert for text: a terminal pastes
/// PRIMARY on it and most other apps CLIPBOARD. Ctrl+V for a picture:
/// that is the key a browser, an image editor and Claude Code in a
/// terminal take a picture on.
fn paste_key(picture: bool) -> &'static str {
    if picture { "ctrl+v" } else { "shift+Insert" }
}

/// Run by the wrapper after the picker's terminal has closed: focus the
/// target (tile handles the EWMH message), then one Shift+Insert via
/// XTEST so it pastes what the picker left in PRIMARY and CLIPBOARD.
/// Doing this from inside the picker raced tile's refocus of the
/// closing tab, and a detached helper died with the terminal's pty.
fn paste_into(target: u32) {
    let dbg = std::env::var_os("YANK_DEBUG").is_some();
    let key = paste_key(std::fs::remove_file(picture_flag()).is_ok());
    let log = |m: String| {
        if dbg {
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true)
                .open("/tmp/yank-paste.log") { let _ = writeln!(f, "{}", m); }
        }
    };
    std::thread::sleep(std::time::Duration::from_millis(250));
    if let Ok((conn, screen_num)) = RustConnection::connect(None) {
        let root = conn.setup().roots[screen_num].root;
        let focus = |c: &RustConnection| c.get_input_focus().ok()
            .and_then(|k| k.reply().ok()).map(|r| r.focus).unwrap_or(0);
        log(format!("paste_into target={} focus before={}", target, focus(&conn)));
        if let Some(active) = intern(&conn, b"_NET_ACTIVE_WINDOW") {
            let ev = ClientMessageEvent::new(32, target, active, [2u32, 0, 0, 0, 0]);
            let _ = conn.send_event(
                false, root,
                EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
                ev,
            );
            let _ = conn.flush();
        }
        // Send the keystroke only once the target really has focus, so a
        // slow refocus can never paste into some other window. Give tile
        // up to a second; if it never lands, do nothing (the entry stays
        // on the clipboard for a manual paste).
        let mut ok = false;
        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            if focus(&conn) == target {
                ok = true;
                break;
            }
        }
        log(format!("paste_into focus after activate={} ok={}", focus(&conn), ok));
        if !ok {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(120));
    }
    // No --clearmodifiers: under frame it releases the Shift it needs
    // and the keystroke arrives as a bare Insert.
    let st = std::process::Command::new("xdotool")
        .args(["key", key])
        .status();
    log(format!("paste_into xdotool={:?}", st));
}

fn picker() {
    let dirs = [hist_dir(), keep_dir()];
    let mut lists = [entries_in(&dirs[0]), entries_in(&dirs[1])];
    if lists[0].is_empty() && lists[1].is_empty() {
        println!("yank: no history yet (is `yank --watch` running?)");
        std::thread::sleep(std::time::Duration::from_secs(2));
        return;
    }
    Crust::init();
    Crust::set_app_identity("Yank");
    let (cols, rows) = Crust::terminal_size();
    let mut tab = 0usize;      // 0 = history, 1 = kept
    let mut sel = [0usize; 2];
    // The way to show a picture, made the first time the bar is on one,
    // and the place a picture is shown at just now.
    let mut display: Option<glow::Display> = None;
    let mut shown: Option<(u16, u16, u16, u16)> = None;
    loop {
        let w = cols as usize;
        if let (Some(d), Some((x, y, pw, ph))) = (display.as_mut(), shown.take()) {
            d.clear(x, y, pw, ph, cols, rows);
        }
        let mut pane = Pane::new(1, 1, cols, rows, 231, 0);
        pane.wrap = false;
        let mut out = String::new();
        // Two tabs across the top row, the open one lighter. Rows are
        // faintly striped in pairs and the chosen row is blue; every row
        // is padded to the width so the colour reaches the right edge.
        let names = [
            format!(" yank \u{2014} {} entr{}", lists[0].len(), if lists[0].len() == 1 { "y" } else { "ies" }),
            format!(" kept \u{2014} {}", lists[1].len()),
        ];
        for t in 0..2 {
            let width = if t == 0 { w / 2 } else { w - w / 2 };
            let bg = if t == tab { 240 } else { 236 };
            out.push_str(&style::styled(&format!("{:<w$}", names[t], w = width), Some(231), Some(bg), "b"));
        }
        out.push('\n');
        let list = &lists[tab];
        sel[tab] = sel[tab].min(list.len().saturating_sub(1));
        let cur = sel[tab];
        let body = rows.saturating_sub(2) as usize;
        let top = cur.saturating_sub(body.saturating_sub(1));
        if list.is_empty() {
            out.push_str(if tab == 0 { " (no history)" } else { " (nothing kept: press a on a history entry)" });
            out.push('\n');
        }
        // With the bar on a picture the list has the left half, and the
        // picture the right.
        let picture = match list.get(cur) {
            Some((p, What::Picture { .. })) => Some(p.clone()),
            _ => None,
        };
        let room = if picture.is_some() { w / 2 } else { w };
        let now = stamp();
        for (n, (i, (p, what))) in list.iter().enumerate().skip(top).take(body).enumerate() {
            let text = match what {
                What::Text(t) => preview(t, room.saturating_sub(4).max(1)),
                What::Picture { w, h, bytes } => picture_line(p, *w, *h, *bytes, now),
            };
            let line = format!("{:<room$}", format!(" {}", text), room = room);
            if i == cur {
                out.push_str(&style::fb(&line, 231, 18));
            } else if n % 2 == 1 {
                out.push_str(&style::fb(&line, 231, 233));
            } else {
                out.push_str(&line);
            }
            out.push('\n');
        }
        pane.set_text(out.trim_end_matches('\n'));
        pane.refresh();
        // A picture is shown once the bar has rested on it. A key that
        // comes at once, Enter straight after opening or a held j, is
        // not kept waiting while a big picture is read and scaled.
        let mut key = None;
        if let Some(p) = &picture {
            key = Input::getchr_ms(120);
            if key.is_none() {
                let (x, y) = (room as u16 + 3, 3);
                let (pw, ph) = (cols.saturating_sub(x + 1), rows.saturating_sub(y));
                let d = display.get_or_insert_with(glow::Display::new);
                if pw > 0 && ph > 0 && d.show(&p.to_string_lossy(), x, y, pw, ph) {
                    shown = Some((x, y, pw, ph));
                }
            }
        }
        let key = key.or_else(|| Input::getchr(None));
        match key.as_deref() {
            Some("q") | Some("Q") | Some("ESC") => break,
            Some("TAB") | Some("S-TAB") | Some("LEFT") | Some("RIGHT") | Some("h") | Some("l") => tab ^= 1,
            Some("UP") | Some("k") => sel[tab] = sel[tab].saturating_sub(1),
            Some("DOWN") | Some("j") => {
                if sel[tab] + 1 < lists[tab].len() {
                    sel[tab] += 1;
                }
            }
            Some("ENTER") if !lists[tab].is_empty() => {
                let _ = std::fs::remove_file(picture_flag());
                match &lists[tab][cur] {
                    (_, What::Text(text)) => own_selections(text),
                    (path, What::Picture { .. }) => {
                        own_picture(path);
                        let _ = std::fs::write(picture_flag(), b"");
                    }
                }
                // The wrapper that opened this terminal pastes after the
                // terminal has closed; this flag tells it Enter was hit.
                let _ = std::fs::write(paste_flag(), b"");
                break;
            }
            // Keep: the history file copied to keep/ under its own name.
            Some("a") if tab == 0 && !lists[0].is_empty() => {
                let src = &lists[0][cur].0;
                if let Some(name) = src.file_name() {
                    let _ = std::fs::copy(src, dirs[1].join(name));
                }
                lists[1] = entries_in(&dirs[1]);
            }
            // Edit: the entry's text in the editor, then back to the
            // list with the bar still on it, so Enter pastes the new text.
            Some("e") => {
                let entry = match lists[tab].get(cur) {
                    Some((path, What::Text(text))) => Some((path.clone(), text.clone())),
                    _ => None,
                };
                if let Some((path, text)) = entry {
                    Crust::cleanup();
                    edit_entry(&path, &text);
                    Crust::init();
                    Crust::set_app_identity("Yank");
                    lists[tab] = entries_in(&dirs[tab]);
                    // The bar stays on the edited entry, also when an
                    // older recorder put new entries above it meanwhile.
                    if let Some(i) = lists[tab].iter().position(|(p, _)| *p == path) {
                        sel[tab] = i;
                    }
                }
            }
            Some("d") if !lists[tab].is_empty() => {
                let _ = std::fs::remove_file(&lists[tab][cur].0);
                lists[tab] = entries_in(&dirs[tab]);
            }
            _ => {}
        }
    }
    if let Some(d) = display.as_mut() {
        d.clear_all();
    }
    Crust::cleanup();
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("yank — clipboard history (Fe2O3 suite)");
        println!();
        println!("Usage: yank [--watch | --paste-into XID]");
        println!();
        println!("  --watch          record CLIPBOARD and PRIMARY to ~/.yank/hist/, copied pictures too");
        println!("  (no args)        pick an entry: Enter takes it, d deletes, q quits;");
        println!("                   e edits a text entry in $EDITOR (scribe when that is not set);");
        println!("                   Tab switches to the kept tab, a keeps a history entry there");
        println!("  --paste-into XID focus XID and send Shift+Insert, or Ctrl+V for a picture");
        println!("                   (yank-pop runs this after the picker's terminal has closed)");
        return;
    }
    if args.iter().any(|a| a == "-v" || a == "--version") {
        println!("yank {}", VERSION);
        return;
    }
    if args.iter().any(|a| a == "--watch") {
        watch();
        return;
    }
    if let Some(i) = args.iter().position(|a| a == "--paste-into") {
        if let Some(t) = args.get(i + 1).and_then(|v| v.parse::<u32>().ok()) {
            paste_into(t);
        }
        return;
    }
    picker();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The start of a PNG of this size, with some bytes after it.
    fn png(w: u32, h: u32, fill: u8) -> Vec<u8> {
        let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        b.extend_from_slice(&w.to_be_bytes());
        b.extend_from_slice(&h.to_be_bytes());
        b.extend_from_slice(&[fill; 40]);
        b
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("yank-test-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_copy_is_asked_for_what_it_has() {
        let (targets, utf8, png, html) = (1, 2, 3, 4);
        assert_eq!(wanted(Some(&[targets, utf8]), utf8, png), (true, false));
        assert_eq!(wanted(Some(&[targets, png]), utf8, png), (false, true));
        assert_eq!(wanted(Some(&[targets, html, utf8, png]), utf8, png), (true, true),
                   "a browser's copy image: the address and the picture");
        // An owner that offers neither, or will not say: text, as before.
        assert_eq!(wanted(Some(&[targets, html]), utf8, png), (true, false));
        assert_eq!(wanted(None, utf8, png), (true, false));
    }

    #[test]
    fn a_png_tells_its_size_and_other_bytes_do_not() {
        assert_eq!(png_size(&png(1920, 1080, 0)), Some((1920, 1080)));
        assert_eq!(png_size(b"just some text that is long enough to be read"), None);
        assert_eq!(png_size(b"\x89PNG"), None);
    }

    #[test]
    fn a_picture_is_stored_once_and_moves_up_when_copied_again() {
        let d = scratch("store");
        store_picture_in(&d, &png(10, 10, 1));
        store_picture_in(&d, &png(20, 20, 2));
        store_picture_in(&d, b"text that an owner called a picture, long enough");
        assert_eq!(names_in(&d).len(), 2);
        // The first one again: no third file, and it is the newest now.
        store_picture_in(&d, &png(10, 10, 1));
        let list = entries_in(&d);
        assert_eq!(list.len(), 2);
        assert!(matches!(list[0].1, What::Picture { w: 10, h: 10, bytes: 64 }));
        assert!(matches!(list[1].1, What::Picture { w: 20, h: 20, .. }));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_history_lists_text_and_pictures_newest_first() {
        let d = scratch("list");
        std::fs::write(d.join("00000000000000000001.txt"), "first").unwrap();
        std::fs::write(d.join("00000000000000000002.png"), png(640, 480, 7)).unwrap();
        std::fs::write(d.join("00000000000000000003.txt"), "third").unwrap();
        std::fs::write(d.join("00000000000000000004.png"), b"cut short").unwrap();
        let list = entries_in(&d);
        assert_eq!(list.len(), 3, "a broken picture is left out");
        assert!(matches!(&list[0].1, What::Text(t) if t == "third"));
        assert!(matches!(list[1].1, What::Picture { w: 640, h: 480, .. }));
        assert!(matches!(&list[2].1, What::Text(t) if t == "first"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_oldest_go_first_and_pictures_have_a_cap_of_their_own() {
        let name = |n: usize, ext: &str| PathBuf::from(format!("/h/{:020}.{}", n, ext));
        // Six entries, oldest first: three pictures among them.
        let names = vec![name(1, "png"), name(2, "txt"), name(3, "png"),
                         name(4, "txt"), name(5, "png"), name(6, "txt")];
        assert!(surplus(&names, 100, 20).is_empty());
        assert_eq!(surplus(&names, 100, 2), [name(1, "png")]);
        assert_eq!(surplus(&names, 4, 20), [name(1, "png"), name(2, "txt")]);
        assert_eq!(surplus(&names, 3, 1), [name(1, "png"), name(3, "png"), name(2, "txt")]);
    }

    #[test]
    fn a_picture_row_says_its_size_and_age() {
        let now: u128 = 1_790_000_000_000_000;
        let at = |secs: u128| PathBuf::from(format!("/h/{:020}.png", now - secs * 1_000_000));
        assert_eq!(picture_line(&at(5), 1920, 1080, 412_000, now), "picture 1920×1080 · 403 kB · now");
        assert_eq!(picture_line(&at(600), 64, 64, 3 << 20, now), "picture 64×64 · 3.0 MB · 10 min");
        assert_eq!(ago(&at(7200), now), "2 h");
        assert_eq!(ago(&at(3 * 86400), now), "3 d");
        assert_eq!(ago(std::path::Path::new("/h/odd-name.png"), now), "now");
    }

    #[test]
    fn an_edit_gives_an_entry_no_line_end_it_did_not_have() {
        assert_eq!(edited("ls -la", "ls -la /tmp\n").as_deref(), Some("ls -la /tmp"));
        assert_eq!(edited("one\n", "one\ntwo\n").as_deref(), Some("one\ntwo\n"), "it had one, so it keeps one");
        assert_eq!(edited("a", "b\n\n").as_deref(), Some("b\n"), "only the editor's own line end goes");
        assert_eq!(edited("ls -la", "ls -la\n"), None, "saved as it was");
        assert_eq!(edited("ls -la", ""), None, "emptied: the entry stays");
        assert_eq!(edited("ls -la", "\n"), None);
    }

    #[test]
    fn an_entry_is_edited_on_a_copy_that_is_gone_afterwards() {
        let d = scratch("edit");
        let entry = d.join("00000000000000000001.txt");
        std::fs::write(&entry, "ls -la").unwrap();
        // An editor that adds to the line, ends the file with a line end
        // and leaves a backup beside the file it saved.
        let script = d.join("editor.sh");
        std::fs::write(&script, "printf ' /tmp\\n' >> \"$1\"; cp \"$1\" \"$1.bak\"\n").unwrap();
        edit_in(&d.join("edit"), &format!("sh {}", script.display()), &entry, "ls -la");
        assert_eq!(std::fs::read_to_string(&entry).unwrap(), "ls -la /tmp");
        assert!(!d.join("edit").exists(), "the copy and the backup are gone");
        // An editor that is not there changes nothing.
        edit_in(&d.join("edit"), "/no/such/editor", &entry, "ls -la /tmp");
        assert_eq!(std::fs::read_to_string(&entry).unwrap(), "ls -la /tmp");
        assert!(!d.join("edit").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn nothing_is_recorded_while_an_entry_is_in_the_editor() {
        let d = scratch("quiet");
        let dir = d.join("edit");
        assert!(!editing_in(&dir));
        let entry = d.join("00000000000000000001.txt");
        std::fs::write(&entry, "text").unwrap();
        // An editor that notes whose edit it is in, as the recorder would read it.
        let script = d.join("editor.sh");
        std::fs::write(&script, format!("cat \"$(dirname \"$1\")/pid\" > {}/seen\n", d.display())).unwrap();
        edit_in(&dir, &format!("sh {}", script.display()), &entry, "text");
        assert_eq!(std::fs::read_to_string(d.join("seen")).unwrap(), std::process::id().to_string());
        assert!(!editing_in(&dir), "the edit is over");
        // A picker that was killed left its folder. Its process is gone,
        // so the recording goes on.
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("pid"), "4194999").unwrap();
        assert!(!editing_in(&dir));
        std::fs::write(dir.join("pid"), std::process::id().to_string()).unwrap();
        assert!(editing_in(&dir));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_list_of_formats_is_not_stored_as_text() {
        let (utf8, png): (Atom, Atom) = (300, 301);
        assert!(is_text(8, utf8, png));
        assert!(is_text(8, AtomEnum::STRING.into(), png));
        assert!(!is_text(32, AtomEnum::ATOM.into(), png), "the formats on offer, from a second copy");
        assert!(!is_text(8, png, png), "a picture");
        assert!(!is_text(0, 0, png), "nothing there");
    }

    #[test]
    fn the_same_text_again_moves_to_the_top() {
        let d = scratch("again");
        store_text_in(&d, "first");
        store_text_in(&d, "second");
        store_text_in(&d, "first");
        let list = entries_in(&d);
        assert_eq!(list.len(), 2);
        assert!(matches!(&list[0].1, What::Text(t) if t == "first"));
        assert!(matches!(&list[1].1, What::Text(t) if t == "second"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_picture_is_pasted_with_another_key_than_text() {
        assert_eq!(paste_key(false), "shift+Insert");
        assert_eq!(paste_key(true), "ctrl+v");
    }
}
