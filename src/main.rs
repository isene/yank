//! yank — clipboard history for the Fe2O3 suite.
//!
//! Two halves in one binary:
//!
//!   yank --watch     the recorder. Sits on XFixes selection events and
//!                    writes every copy and mouse selection to ~/.yank/hist/, one
//!                    file per entry, newest kept, capped. Fully
//!                    event-driven: zero wakeups between copies.
//!
//!   yank             the picker. Lists the history newest first; Enter
//!                    owns CLIPBOARD and PRIMARY with the chosen entry
//!                    and leaves ~/.yank/paste as a flag.
//!
//!   yank --paste-into XID   run by the yank-pop wrapper once the
//!                    picker's terminal has closed: refocus XID and send
//!                    one Shift+Insert. glass pastes PRIMARY on that key,
//!                    most other apps CLIPBOARD; owning both makes the
//!                    same keystroke work in either.
//!
//! Replaces copyq: no tray, no Qt, no selection-sync helpers to hang.

use crust::{style, Crust, Input, Pane};
use std::io::Write as _;
use std::path::PathBuf;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, KeyButMask, ClientMessageEvent, ConnectionExt as _, CreateWindowAux, EventMask,
    WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const KEEP: usize = 100; // history entries kept
const MAX_ENTRY: usize = 65536; // bytes; larger copies are not recorded

fn hist_dir() -> PathBuf {
    let d = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".into()))
        .join(".yank/hist");
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
        let name = b"yank-watch";
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

    let mut last = read_newest().unwrap_or_default();
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
                let _ = conn.convert_selection(
                    win, x.selection, utf8, dest_prop, x11rb::CURRENT_TIME,
                );
                let _ = conn.flush();
            }
            Event::SelectionNotify(sn) => {
                if sn.property == x11rb::NONE {
                    continue; // owner had no UTF8 text
                }
                let Ok(cookie) = conn.get_property(
                    true, win, dest_prop, AtomEnum::ANY, 0, (MAX_ENTRY / 4) as u32 + 1,
                ) else { continue };
                let Ok(prop) = cookie.reply() else { continue };
                if prop.type_ == incr {
                    continue; // bigger than we record; skip the INCR dance
                }
                let text = String::from_utf8_lossy(&prop.value).to_string();
                let trimmed = text.trim();
                if trimmed.is_empty() || text.len() > MAX_ENTRY || text == last {
                    continue;
                }
                store(&text);
                last = text;
            }
            _ => {}
        }
    }
}

/// One file per entry, named by microsecond epoch, pruned to KEEP.
fn store(text: &str) {
    let dir = hist_dir();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros())
        .unwrap_or(0);
    let path = dir.join(format!("{:020}.txt", now));
    if let Ok(mut f) = std::fs::File::create(&path) {
        let _ = f.write_all(text.as_bytes());
    }
    let mut names: Vec<_> = std::fs::read_dir(&dir)
        .map(|it| it.flatten().map(|e| e.path()).collect::<Vec<_>>())
        .unwrap_or_default();
    names.sort();
    while names.len() > KEEP {
        let _ = std::fs::remove_file(names.remove(0));
    }
}

fn read_newest() -> Option<String> {
    entries().first().map(|(_, t)| t.clone())
}

/// (path, text) newest first.
fn entries() -> Vec<(PathBuf, String)> {
    let mut names: Vec<_> = std::fs::read_dir(hist_dir())
        .map(|it| it.flatten().map(|e| e.path()).collect::<Vec<_>>())
        .unwrap_or_default();
    names.sort();
    names.reverse();
    names
        .into_iter()
        .filter_map(|p| std::fs::read_to_string(&p).ok().map(|t| (p, t)))
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

/// Flag the picker leaves for the wrapper: "Enter was pressed, paste".
fn paste_flag() -> PathBuf {
    hist_dir().parent().map(|p| p.join("paste")).unwrap_or_else(|| "/tmp/yank-paste".into())
}

/// Run by the wrapper after the picker's terminal has closed: focus the
/// target (tile handles the EWMH message), then one Shift+Insert via
/// XTEST so it pastes what the picker left in PRIMARY and CLIPBOARD.
/// Doing this from inside the picker raced tile's refocus of the
/// closing tab, and a detached helper died with the terminal's pty.
fn paste_into(target: u32) {
    let dbg = std::env::var_os("YANK_DEBUG").is_some();
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
        .args(["key", "shift+Insert"])
        .status();
    log(format!("paste_into xdotool={:?}", st));
}

fn picker() {
    let list = entries();
    if list.is_empty() {
        println!("yank: no history yet (is `yank --watch` running?)");
        std::thread::sleep(std::time::Duration::from_secs(2));
        return;
    }
    Crust::init();
    Crust::set_app_identity("Yank");
    let (cols, rows) = Crust::terminal_size();
    let mut sel = 0usize;
    loop {
        let mut pane = Pane::new(1, 1, cols, rows, 231, 0);
        let mut out = String::new();
        out.push_str(&style::styled(
            &format!(" yank — {} entr{} ", list.len(),
                     if list.len() == 1 { "y" } else { "ies" }),
            Some(231), Some(234), "b"));
        out.push('\n');
        let body = rows.saturating_sub(2) as usize;
        let top = sel.saturating_sub(body.saturating_sub(1));
        for (i, (_, t)) in list.iter().enumerate().skip(top).take(body) {
            let line = format!(" {}", preview(t, cols as usize - 4));
            if i == sel {
                out.push_str(&format!("\x1b[48;5;238m{:<w$}\x1b[49m\n", line,
                                      w = cols as usize - 2));
            } else {
                out.push_str(&line);
                out.push('\n');
            }
        }
        pane.set_text(out.trim_end_matches('\n'));
        pane.refresh();
        match Input::getchr(None).as_deref() {
            Some("q") | Some("Q") | Some("ESC") => break,
            Some("UP") | Some("k") => sel = sel.saturating_sub(1),
            Some("DOWN") | Some("j") => {
                if sel + 1 < list.len() {
                    sel += 1;
                }
            }
            Some("ENTER") => {
                let text = list[sel].1.clone();
                own_selections(&text);
                // The wrapper that opened this terminal pastes after the
                // terminal has closed; this flag tells it Enter was hit.
                let _ = std::fs::write(paste_flag(), b"");
                break;
            }
            Some("d") => {
                let _ = std::fs::remove_file(&list[sel].0);
                Crust::cleanup();
                return picker(); // reread, simplest
            }
            _ => {}
        }
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
        println!("  --watch          record CLIPBOARD and PRIMARY to ~/.yank/hist/");
        println!("  (no args)        pick an entry: Enter takes it, d deletes, q quits");
        println!("  --paste-into XID focus XID and send Shift+Insert (yank-pop runs");
        println!("                   this after the picker's terminal has closed)");
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
