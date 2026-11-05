// SPDX-License-Identifier: GPL-3.0-or-later
//! Graph: how your notes link, drawn two ways.
//!
//! * `graph-local` -- the note you are in, and the notes within a few links
//!   of it (either direction: what it links to and what links to it).
//! * `graph-global` -- every note in the vault.
//!
//! Notes are dots, links are lines; bigger dots have more links, colours
//! follow the top folder. Click a dot to light it and its neighbours, drag
//! to move it (it stays put when the graph is laid out again), double-click
//! to open the note (a tag: filter by it). The wheel zooms, middle-drag pans.
//!
//! Keys: `RET` open the lit note, `+` / `-` links deep (local), `t` tags,
//! `o` orphans, `u` unresolved links, `/` filter by folder, `#` filter by
//! tag, `l` layout again, `g` read the vault again, `F` follow the note you
//! are in, `w` save a picture.
//!
//! Settings (init.rhai):
//!
//! ```rhai
//! forge.plugin.config("graph", #{ vault: "~/Notes", depth: 2, follow: true });
//! forge.plugin.grant("graph", #{ directories: ["~/Notes"] });
//! ```
//!
//! Big vaults: the first time, every note is read (a few hundred thousand
//! take a while). The index is then kept in the plugin's cache folder, with
//! the position of each dot, and the global graph opens from that in a
//! fraction of a second without reading the index at all. Notes you save in
//! Forge are folded in as you go; `g` looks for notes changed outside Forge
//! (and it happens by itself when the saved index is older than
//! `rescan_hours`, a day unless you say, 0 for never); the menu's "Read every
//! note again" starts over.
//!
//!   cargo build --release --target wasm32-wasip2
//!   cp target/wasm32-wasip2/release/forge_graph.wasm ~/.config/forge/plugins/graph.wasm

use std::cell::RefCell;

use forge_notes::cache::{self, Header};
use forge_notes::graph::{self, Act, Fit, Graph, Kind, Options, Pointer};
use forge_notes::jsoncanvas::Event as PointerEvent;
use forge_notes::vault::{Index, Note, RawNote, is_note_file};
use forge_plugin_sdk::{
    Buffer, Event, EventKind, MenuItem, bind_in_mode, canvas, command, define_menu_for_mode, files, forge_plugin,
    menu_item, menu_separator, message, modes, on, read_string, run_command, when_idle,
};
use serde_json::Value;


const MODE: &str = "graph-mode";
const FRAME: (f32, f32) = (1200.0, 800.0);

#[derive(Clone)]
struct Config {
    vault: Option<String>,
    depth: usize,
    follow: bool,
    options: Options,
    /// Look for notes changed outside Forge when the saved index is older
    /// than this many hours (0: only when asked).
    rescan_hours: i64,
}

impl Default for Config {
    fn default() -> Self {
        Config { vault: None, depth: 1, follow: false, options: Options::default(), rescan_hours: 24 }
    }
}

fn parse_config(json: &str) -> Result<Config, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("graph: settings: {e}"))?;
    let map = v.as_object().ok_or("graph: settings must be a table")?;
    let mut c = Config::default();
    let flag = |v: &Value, k: &str| v.as_bool().ok_or(format!("graph: {k} must be true or false"));
    for (k, v) in map {
        match k.as_str() {
            "vault" => c.vault = Some(v.as_str().ok_or("graph: vault must be text")?.to_string()),
            "depth" => c.depth = v.as_u64().ok_or("graph: depth must be a number")?.clamp(1, 8) as usize,
            "follow" => c.follow = flag(v, k)?,
            "rescan_hours" => c.rescan_hours = v.as_i64().ok_or("graph: rescan_hours must be a number")?.max(0),
            "tags" => c.options.tags = flag(v, k)?,
            "orphans" => c.options.orphans = flag(v, k)?,
            "unresolved" => c.options.unresolved = flag(v, k)?,
            "max_nodes" => c.options.max_nodes = v.as_u64().ok_or("graph: max_nodes must be a number")? as usize,
            other => return Err(format!("graph: unknown setting {other} (vault, depth, follow, tags, orphans, unresolved, max_nodes, rescan_hours)")),
        }
    }
    Ok(c)
}

#[derive(Clone, Copy, PartialEq)]
enum Which {
    Local,
    Global,
}

impl Which {
    fn canvas(self) -> &'static str {
        match self {
            Which::Local => "graph:local",
            Which::Global => "graph:global",
        }
    }
}

struct View {
    graph: Graph,
    selected: Option<usize>,
    fit: Fit,
    pointer: Pointer,
    /// The note path (vault-relative) a local graph is centred on.
    center: Option<String>,
}

#[derive(Default)]
struct State {
    config: Config,
    root: Option<String>,
    index: Option<Index>,
    local: Option<View>,
    global: Option<View>,
    depth: usize,
    options: Options,
    follow: bool,
    /// While the global graph was opened from its saved picture and the
    /// index hasn't been read: the path of each node.
    lazy_paths: Option<Vec<String>>,
    /// Notes saved since the index file was (their links as written).
    edits: Vec<Note>,
    /// When the index file was saved (milliseconds since 1970; 0 unknown).
    saved_at: i64,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
    static REGISTERED: RefCell<[bool; 2]> = const { RefCell::new([false; 2]) };
}

fn vault_for(path: &str, cfg: &Config) -> String {
    if let Some(v) = &cfg.vault {
        return v.trim_end_matches('/').to_string();
    }
    // The nearest folder above the file holding `.obsidian`, else its own.
    match files::locate_dominating_file(path, ".obsidian") {
        Some(dir) => files::directory_name(&dir),
        None => files::directory_name(&files::name_directory(path).unwrap_or_default()),
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

fn read_note(root: &str, entry: &files::Entry) -> Option<Note> {
    let text = files::read(&entry.path).ok()?;
    let raw = RawNote { path: forge_notes::vault::relative_to(root, &entry.path), text, size: entry.info.size, mtime: entry.info.modified, ctime: entry.info.created };
    Some(Note::parse_light(&raw))
}

/// Every note of the vault, read from disk. Each is parsed as it is read and
/// its text dropped, so a vault of hundreds of thousands of notes never has
/// all its text in memory.
fn read_vault(root: &str) -> Result<Index, String> {
    let mut notes = Vec::new();
    for e in files::list_dir_info(root, true)? {
        if e.path.ends_with('/') || !is_note_file(&e.path) {
            continue;
        }
        notes.extend(read_note(root, &e));
    }
    Ok(Index::from_notes(notes))
}

// ---- What is kept between runs (files::cache_dir: nothing in the vault).

fn fnv(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

/// A file of this vault's cache, or none if the plugin has no cache folder.
fn cache_file(root: &str, name: &str) -> String {
    format!("{}/vault-{:016x}/{name}", files::cache_dir(), fnv(root))
}

fn cache_read(root: &str, name: &str) -> Option<String> {
    files::read(&cache_file(root, name)).ok()
}

fn cache_write(root: &str, name: &str, text: &str) {
    // The cache only makes the next open faster: failing to keep it is not an error.
    let _ = files::write(&cache_file(root, name), text.as_bytes());
}

fn options_key(root: &str, o: &Options) -> String {
    format!("{root}|{}{}{}|{:?}|{:?}|{}", u8::from(o.tags), u8::from(o.orphans), u8::from(o.unresolved), o.folder, o.tag, o.max_nodes)
}

/// Keep the index (and clear the edits that are now part of it).
fn save_index(root: &str, index: &Index) {
    let h = Header { saved: now_ms(), vault: root.to_string() };
    cache_write(root, "index.tsv", &cache::encode_index(index, &h));
    cache_write(root, "stamp.txt", &h.saved.to_string());
    cache_write(root, "edits.tsv", &cache::encode_edits(&[], &h));
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.saved_at = h.saved;
        s.edits.clear();
    });
}

/// When the saved index was written, without reading it (0 if there isn't one).
fn stamp(root: &str) -> i64 {
    let known = STATE.with(|s| s.borrow().saved_at);
    if known != 0 {
        return known;
    }
    cache_read(root, "stamp.txt").and_then(|t| t.trim().parse().ok()).unwrap_or(0)
}

/// The saved index, with the notes saved since, or none if there isn't a good one.
fn load_index(root: &str) -> Option<Index> {
    let (h, mut index) = cache::decode_index(&cache_read(root, "index.tsv")?)?;
    if h.vault != root {
        return None;
    }
    let edits = cache_read(root, "edits.tsv").and_then(|t| cache::decode_edits(&t)).filter(|(eh, _)| eh.saved == h.saved).map(|e| e.1).unwrap_or_default();
    index.update_notes(edits.clone());
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.saved_at = h.saved;
        s.edits = edits;
    });
    Some(index)
}

/// The vault's folder: the settings' `vault`, else the one the note being
/// edited is in. Chosen again when `reread`.
fn ensure_root(reread: bool) -> Result<String, String> {
    let path = Buffer::current().path();
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        if s.root.is_none() || reread {
            let from = path.clone().unwrap_or_default();
            let root = match (&s.config.vault, path.as_deref()) {
                (Some(_), _) | (None, Some(_)) => vault_for(&from, &s.config),
                (None, None) => return Err("Visit a note first, or set the vault: forge.plugin.config(\"graph\", #{ vault: \"~/Notes\" })".to_string()),
            };
            s.root = Some(root);
            s.index = None;
        }
        Ok(s.root.clone().expect("set above"))
    })
}

/// The vault's index: in memory, else from the saved one, else read from the
/// notes (the first time; slow for a big vault, and kept for next time).
fn ensure_index(reread: bool) -> Result<(), String> {
    let root = ensure_root(reread)?;
    if STATE.with(|s| s.borrow().index.is_none()) {
        let index = match load_index(&root) {
            Some(i) => i,
            None => {
                let i = read_vault(&root)?;
                save_index(&root, &i);
                i
            }
        };
        STATE.with(|s| s.borrow_mut().index = Some(index));
    }
    hydrate();
    Ok(())
}

/// A global graph opened from its saved picture knows its notes by path; now
/// the index is here, give each its number.
fn hydrate() {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let s = &mut *s;
        let (Some(paths), Some(index)) = (s.lazy_paths.take(), s.index.as_ref()) else {
            return;
        };
        let at = index.positions_of(&paths);
        if let Some(v) = s.global.as_mut() {
            for (n, pos) in v.graph.nodes.iter_mut().zip(at) {
                // A note that has gone since stays as a dot that opens nothing.
                n.kind = Kind::Note(pos.unwrap_or(usize::MAX));
            }
        }
    });
}

/// Look at the vault for notes changed, added or removed outside Forge, and
/// read only those. Returns (changed, added, removed).
fn rescan() -> Result<(usize, usize, usize), String> {
    ensure_index(false)?;
    let root = STATE.with(|s| s.borrow().root.clone()).ok_or("no vault")?;
    let listing = files::list_dir_info(&root, true)?;
    let Some(old) = STATE.with(|s| s.borrow_mut().index.take()) else { return Ok((0, 0, 0)) };
    let at: std::collections::HashMap<&str, usize> = old.notes.iter().enumerate().map(|(i, n)| (n.path.as_str(), i)).collect();
    let (mut kept, mut fresh, mut changed) = (Vec::new(), Vec::new(), 0);
    for e in listing.iter().filter(|e| !e.path.ends_with('/') && is_note_file(&e.path)) {
        let rel = forge_notes::vault::relative_to(&root, &e.path);
        match at.get(rel.as_str()) {
            Some(&i) if old.notes[i].mtime == e.info.modified && old.notes[i].size == e.info.size => kept.push(i),
            known => {
                if let Some(n) = read_note(&root, e) {
                    changed += usize::from(known.is_some());
                    fresh.push(n);
                }
            }
        }
    }
    let added = fresh.len() - changed;
    let removed = old.notes.len() - kept.len() - changed;
    if fresh.is_empty() && removed == 0 {
        STATE.with(|s| s.borrow_mut().index = Some(old));
        return Ok((0, 0, 0));
    }
    let new = old.merged(&kept, fresh);
    save_index(&root, &new);
    STATE.with(|s| s.borrow_mut().index = Some(new));
    Ok((changed, added, removed))
}

/// A note was saved in Forge: keep its links up to date without a rescan.
fn note_saved(id: u32) {
    let Some(b) = Buffer::from_id(id) else { return };
    let Some(path) = b.path() else { return };
    let Some(root) = STATE.with(|s| s.borrow().root.clone()) else { return };
    let rel = forge_notes::vault::relative_to(&root, &path);
    if rel == path || !is_note_file(&rel) {
        return;
    }
    let (size, mtime) = files::stat(&path).map_or((0, now_ms()), |i| (i.size, i.modified));
    let note = Note::parse_light(&RawNote { path: rel.clone(), text: b.text(), size, mtime, ..Default::default() });
    let fold = STATE.with(|s| {
        let mut s = s.borrow_mut();
        if let Some(ix) = s.index.as_mut() {
            ix.update_note(note.clone());
        }
        s.edits.retain(|n| n.path != rel);
        s.edits.push(note);
        s.edits.len() > 500 && s.index.is_some()
    });
    if fold {
        if let Some(ix) = STATE.with(|s| s.borrow_mut().index.take()) {
            save_index(&root, &ix);
            STATE.with(|s| s.borrow_mut().index = Some(ix));
        }
    } else {
        let edits = STATE.with(|s| s.borrow().edits.clone());
        cache_write(&root, "edits.tsv", &cache::encode_edits(&edits, &Header { saved: stamp(&root), vault: root.clone() }));
    }
}

/// Idle: a saved index that has gone a day (`rescan_hours`) without a look at
/// the disk is checked, in case notes changed outside Forge (sync, git).
fn rescan_tick() {
    let (hours, root, loaded) = STATE.with(|s| {
        let s = s.borrow();
        (s.config.rescan_hours, s.root.clone(), s.index.is_some() || s.lazy_paths.is_some())
    });
    let Some(root) = root else { return };
    let saved = stamp(&root);
    if hours == 0 || !loaded || saved == 0 || now_ms() - saved < hours * 3_600_000 {
        return;
    }
    match rescan() {
        Ok((0, 0, 0)) => {
            // Nothing changed: don't look again until the next period.
            if let (Some(root), Some(ix)) = (STATE.with(|s| s.borrow().root.clone()), STATE.with(|s| s.borrow_mut().index.take())) {
                save_index(&root, &ix);
                STATE.with(|s| s.borrow_mut().index = Some(ix));
            }
        }
        Ok((c, a, r)) => {
            message(&format!("graph: vault checked: {c} changed, {a} new, {r} removed"));
            for w in [Which::Local, Which::Global] {
                if STATE.with(|s| match w {
                    Which::Local => s.borrow().local.is_some(),
                    Which::Global => s.borrow().global.is_some(),
                }) {
                    let _ = rebuild(w, false);
                }
            }
        }
        Err(e) => message(&format!("graph: {e}")),
    }
}

/// Scenes name colour roles (`card`, `line` ...); Forge picks the colours
/// when it draws them, so a theme or light/dark change recolours every
/// picture at once. Nothing to fetch from the theme here.
fn apply_theme() {
    forge_canvas::Palette::set_current(forge_canvas::Palette::tokens());
}


fn draw(which: Which) {
    apply_theme();
    let json = STATE.with(|s| {
        let s = s.borrow();
        let v = match which {
            Which::Local => s.local.as_ref(),
            Which::Global => s.global.as_ref(),
        }?;
        Some(graph::scene_fit(&v.graph, v.selected, FRAME.0, FRAME.1, None, v.fit).to_json())
    });
    if let Some(json) = json {
        if let Err(e) = canvas::show(which.canvas(), &json) {
            message(&format!("graph: {e}"));
        }
    }
}

fn register(which: Which) -> Result<(), String> {
    let i = usize::from(which == Which::Global);
    if REGISTERED.with(|r| std::mem::replace(&mut r.borrow_mut()[i], true)) {
        return Ok(());
    }
    let cmd = format!("graph-event-{}", if i == 0 { "local" } else { "global" });
    command(&cmd, "Handle the pointer on a graph (internal).", move |_| {
        event(which);
        Ok(())
    })?;
    canvas::on_event(which.canvas(), &cmd)
}

/// The global graph from its saved picture, without reading the index: the
/// notes drawn, where they are, and how they link. Only when nothing has
/// changed since it was saved (a note saved in Forge sends it the long way,
/// through the index, which then saves a new picture). Returns whether it did.
fn open_global_saved(focus: bool) -> bool {
    let Ok(root) = ensure_root(false) else { return false };
    let key = STATE.with(|s| options_key(&root, &s.borrow().options));
    if STATE.with(|s| !s.borrow().edits.is_empty()) {
        return false;
    }
    let Some(snap) = cache_read(&root, "global.tsv").and_then(|t| cache::decode_snapshot(&t, &key)) else { return false };
    // Edits kept from an earlier run are in the edits file, not yet in memory.
    if cache_read(&root, "edits.tsv").and_then(|t| cache::decode_edits(&t)).is_some_and(|(_, n)| !n.is_empty()) {
        return false;
    }
    let cache::Snapshot { graph: g, paths } = snap;
    STATE.with(|s| s.borrow_mut().lazy_paths = Some(paths));
    if present(Which::Global, g, None, focus).is_err() {
        STATE.with(|s| s.borrow_mut().lazy_paths = None);
        return false;
    }
    true
}

fn rebuild(which: Which, focus: bool) -> Result<(), String> {
    if which == Which::Global && STATE.with(|s| s.borrow().index.is_none()) && open_global_saved(focus) {
        return Ok(());
    }
    ensure_index(false)?;
    let built: Result<(Graph, Option<String>), String> = STATE.with(|s| {
        let s = s.borrow();
        let index = s.index.as_ref().expect("ensured");
        let root = s.root.clone().unwrap_or_default();
        match which {
            Which::Global => {
                let mut g = graph::global(index, &s.options);
                // Where the notes settled last time, if it is the same notes: no physics.
                let key = options_key(&root, &s.options);
                let reused = cache_read(&root, "global.tsv").is_some_and(|t| cache::apply_layout(&t, &mut g, index, &key));
                if !reused {
                    graph::layout(&mut g, None);
                    if let Some(text) = cache::encode_layout(&g, index, &key, &Header { saved: now_ms(), vault: root.clone() }) {
                        cache_write(&root, "global.tsv", &text);
                    }
                }
                Ok((g, None::<String>))
            }
            Which::Local => {
                let cur = Buffer::current().path();
                // A local graph follows the note being edited; from the
                // graph itself it keeps the one it had.
                let mut path = cur.as_deref().map(|p| forge_notes::vault::relative_to(&root, p)).filter(|r| Some(r.as_str()) != cur.as_deref());
                if path.as_ref().is_none_or(|p| !is_note_file(p)) {
                    path = s.local.as_ref().and_then(|v| v.center.clone());
                }
                let path = path.ok_or_else(|| "Visit a note in the vault first".to_string())?;
                let c = index.find_path(&path).ok_or_else(|| format!("{path} isn't a note in {root}"))?;
                let mut g = graph::local(index, c, s.depth, &s.options);
                graph::layout(&mut g, None);
                Ok((g, Some(path)))
            }
        }
    });
    let (g, center) = built?;
    // Notes saved since the index file was written are folded into it now,
    // so the next open can use the saved picture again.
    if which == Which::Global && STATE.with(|s| !s.borrow().edits.is_empty()) {
        if let (Some(root), Some(ix)) = (STATE.with(|s| s.borrow().root.clone()), STATE.with(|s| s.borrow_mut().index.take())) {
            save_index(&root, &ix);
            STATE.with(|s| s.borrow_mut().index = Some(ix));
        }
    }
    STATE.with(|s| {
        if which == Which::Global {
            s.borrow_mut().lazy_paths = None;
        }
    });
    present(which, g, center, focus)
}

/// Show a graph: fit it, draw it, and (when `focus`) bring up its buffer.
fn present(which: Which, g: Graph, center: Option<String>, focus: bool) -> Result<(), String> {
    let fit = graph::fit(&g, FRAME.0, FRAME.1);
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let selected = g.center;
        let v = View { graph: g, selected, fit, pointer: Pointer::default(), center };
        match which {
            Which::Local => s.local = Some(v),
            Which::Global => s.global = Some(v),
        }
    });
    draw(which);
    register(which)?;
    if focus {
        let name = which.canvas();
        canvas::set_view(name, 0.0, 0.0, 1.0)?;
        let buf = Buffer::find(&format!("*Canvas: {name}*")).ok_or("the graph buffer is missing")?;
        buf.set_major_mode(MODE)?;
        canvas::display(name)?;
        let (n, e, h) = STATE.with(|s| {
            let s = s.borrow();
            let v = match which {
                Which::Local => s.local.as_ref(),
                Which::Global => s.global.as_ref(),
            };
            v.map_or((0, 0, 0), |v| (v.graph.nodes.len(), v.graph.edges.len(), v.graph.hidden))
        });
        let more = match (h, which) {
            (0, _) => String::new(),
            (h, Which::Local) => format!(" (+{h} more not drawn: lower the depth, or open a note with fewer links)"),
            (h, Which::Global) => format!(" (the best connected; {h} more not drawn: filter by folder or tag to see them)"),
        };
        message(&format!("{} graph: {n} notes, {e} links{more}", if which == Which::Local { "Local" } else { "Global" }));
    }
    Ok(())
}

fn which_here() -> Option<Which> {
    let cur = Buffer::current().name();
    match cur.as_str() {
        "*Canvas: graph:local*" => Some(Which::Local),
        "*Canvas: graph:global*" => Some(Which::Global),
        _ => None,
    }
}

thread_local! {
    static LAST_DRAW: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn event(which: Which) {
    let Some(json) = canvas::last_event(which.canvas()) else { return };
    let Ok(ev) = serde_json::from_str::<PointerEvent>(&json) else { return };
    let acts = STATE.with(|s| {
        let mut s = s.borrow_mut();
        let v = match which {
            Which::Local => s.local.as_mut(),
            Which::Global => s.global.as_mut(),
        }?;
        let (g, sel, fit, p) = (&mut v.graph, &mut v.selected, v.fit, &mut v.pointer);
        Some(p.handle(g, sel, fit, &ev))
    });
    // A drag sends events faster than a picture can be drawn; drawing every
    // one makes the dragged dot lag far behind the pointer. Draw about
    // twenty a second, and always the last (the release).
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
    let skip = ev.kind == "drag" && LAST_DRAW.with(|l| now.saturating_sub(l.get()) < 50);
    for a in acts.unwrap_or_default() {
        match a {
            Act::Redraw if skip => {}
            Act::Redraw => {
                LAST_DRAW.with(|l| l.set(now));
                draw(which)
            }
            Act::Open(i) => open_node(which, i),
        }
    }
}

fn open_node(which: Which, i: usize) {
    let target = STATE.with(|s| {
        let s = s.borrow();
        let v = match which {
            Which::Local => s.local.as_ref(),
            Which::Global => s.global.as_ref(),
        }?;
        let n = v.graph.nodes.get(i)?;
        let lazy = if which == Which::Global { s.lazy_paths.as_ref().and_then(|p| p.get(i).cloned()) } else { None };
        Some((n.kind.clone(), n.label.clone(), s.root.clone().unwrap_or_default(), lazy.or_else(|| s.index.as_ref().and_then(|ix| match n.kind {
            Kind::Note(k) => ix.notes.get(k).map(|n| n.path.clone()),
            _ => None,
        }))))
    });
    match target {
        Some((Kind::Note(_), _, root, Some(path))) => match Buffer::visit(&format!("{root}/{path}")) {
            Ok(b) => b.show(),
            Err(e) => message(&format!("{path}: {e}")),
        },
        Some((Kind::Tag, label, ..)) => {
            STATE.with(|s| s.borrow_mut().options.tag = Some(label.trim_start_matches('#').to_string()));
            let _ = rebuild(which, false);
            message(&format!("Showing only notes tagged {label} (# then empty clears)"));
        }
        Some((Kind::Unresolved, label, root, _)) => {
            let path = format!("{root}/{label}.md");
            if let Ok(b) = Buffer::visit(&path) {
                b.show();
                message(&format!("New note {label} (save to create it)"));
            }
        }
        _ => {}
    }
}

fn open_selected() -> Result<(), String> {
    let which = which_here().ok_or("Not in a graph")?;
    let sel = STATE.with(|s| {
        let s = s.borrow();
        match which {
            Which::Local => s.local.as_ref().and_then(|v| v.selected),
            Which::Global => s.global.as_ref().and_then(|v| v.selected),
        }
    });
    match sel {
        Some(i) => open_node(which, i),
        None => message("Click a dot first"),
    }
    Ok(())
}

fn rebuild_here(reread: bool) -> Result<(), String> {
    let which = which_here().ok_or("Not in a graph")?;
    if reread {
        let (c, a, r) = rescan()?;
        message(&if (c, a, r) == (0, 0, 0) { "The vault hasn't changed".to_string() } else { format!("Vault checked: {c} changed, {a} new, {r} removed") });
        // Everything numbered by the old index is stale.
        for w in [Which::Local, Which::Global] {
            if w != which && STATE.with(|s| match w {
                Which::Local => s.borrow().local.is_some(),
                Which::Global => s.borrow().global.is_some(),
            }) {
                rebuild(w, false)?;
            }
        }
    }
    rebuild(which, false)
}

/// Forget what was kept and read every note again.
fn reindex() -> Result<(), String> {
    let root = ensure_root(true)?;
    let index = read_vault(&root)?;
    save_index(&root, &index);
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.index = Some(index);
        s.lazy_paths = None;
    });
    message("Read the whole vault again");
    for w in [Which::Local, Which::Global] {
        if STATE.with(|s| match w {
            Which::Local => s.borrow().local.is_some(),
            Which::Global => s.borrow().global.is_some(),
        }) {
            rebuild(w, false)?;
        }
    }
    Ok(())
}

fn depth(delta: i64) -> Result<(), String> {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.depth = (s.depth as i64 + delta).clamp(1, 8) as usize;
        message(&format!("Local graph: {} link{} deep", s.depth, if s.depth == 1 { "" } else { "s" }));
    });
    rebuild(Which::Local, false)
}

fn toggle(what: &'static str) -> Result<(), String> {
    let state = STATE.with(|s| {
        let mut s = s.borrow_mut();
        let flag = match what {
            "tags" => &mut s.options.tags,
            "orphans" => &mut s.options.orphans,
            _ => &mut s.options.unresolved,
        };
        *flag = !*flag;
        *flag
    });
    message(&format!("{what}: {}", if state { "shown" } else { "hidden" }));
    for w in [Which::Local, Which::Global] {
        if STATE.with(|s| match w {
            Which::Local => s.borrow().local.is_some(),
            Which::Global => s.borrow().global.is_some(),
        }) {
            rebuild(w, false)?;
        }
    }
    Ok(())
}

fn filter(kind: &'static str) -> Result<(), String> {
    let prompt = if kind == "folder" { "Only notes in folder (empty for all): " } else { "Only notes with tag (empty for all): " };
    read_string(prompt, "", move |a| {
        let Some(a) = a else { return };
        let v = (!a.trim().is_empty()).then(|| a.trim().trim_start_matches('#').to_string());
        STATE.with(|s| {
            let mut s = s.borrow_mut();
            if kind == "folder" {
                s.options.folder = v;
            } else {
                s.options.tag = v;
            }
        });
        for w in [Which::Local, Which::Global] {
            let has = STATE.with(|s| match w {
                Which::Local => s.borrow().local.is_some(),
                Which::Global => s.borrow().global.is_some(),
            });
            if has {
                if let Err(e) = rebuild(w, false) {
                    message(&e);
                }
            }
        }
    });
    Ok(())
}

fn relayout() -> Result<(), String> {
    let which = which_here().ok_or("Not in a graph")?;
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let v = match which {
            Which::Local => s.local.as_mut(),
            Which::Global => s.global.as_mut(),
        };
        if let Some(v) = v {
            // Dots you dragged (and the centre) stay; the rest start over.
            for n in v.graph.nodes.iter_mut().filter(|n| !n.pinned) {
                n.x = 0.0;
                n.y = 0.0;
            }
            graph::layout(&mut v.graph, None);
            v.fit = graph::fit(&v.graph, FRAME.0, FRAME.1);
        }
    });
    draw(which);
    Ok(())
}

fn toggle_follow() -> Result<(), String> {
    let on = STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.follow = !s.follow;
        s.follow
    });
    message(if on { "The local graph follows the note you are in" } else { "The local graph stays put" });
    Ok(())
}

/// Idle: if following and the note changed, redraw the local graph.
fn follow_tick() {
    let (on, center, root) = STATE.with(|s| {
        let s = s.borrow();
        (s.follow && s.local.is_some(), s.local.as_ref().and_then(|v| v.center.clone()), s.root.clone())
    });
    if !on {
        return;
    }
    let (Some(root), Some(path)) = (root, Buffer::current().path()) else { return };
    let rel = forge_notes::vault::relative_to(&root, &path);
    if rel == path {
        return;
    }
    let rel = rel.as_str();
    if is_note_file(rel) && center.as_deref() != Some(rel) {
        let _ = rebuild(Which::Local, false);
    }
}

fn init() -> Result<(), String> {
    let cfg = parse_config(&forge_plugin_sdk::config_json())?;
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.depth = cfg.depth;
        s.options = cfg.options.clone();
        s.follow = cfg.follow;
        s.config = cfg;
    });
    modes::define_major_mode(MODE, Some("canvas-mode"), &[])?;

    command("graph-local", "Draw the notes near this one: what it links to and what links to it.", |_| rebuild(Which::Local, true))?;
    command("graph-global", "Draw every note in the vault and how they link.", |_| rebuild(Which::Global, true))?;
    command("graph-refresh", "Look for notes changed outside Forge, and redraw the graph.", |_| rebuild_here(true))?;
    command("graph-reindex", "Read every note of the vault again (the saved index is replaced).", |_| reindex())?;
    command("graph-relayout", "Lay the graph out again (dragged dots stay where you put them).", |_| relayout())?;
    command("graph-open", "Open the note that is lit.", |_| open_selected())?;
    command("graph-deeper", "Local graph: one more link out.", |_| depth(1))?;
    command("graph-shallower", "Local graph: one link fewer.", |_| depth(-1))?;
    command("graph-toggle-tags", "Show tags as dots joined to their notes.", |_| toggle("tags"))?;
    command("graph-toggle-orphans", "Show notes that have no links.", |_| toggle("orphans"))?;
    command("graph-toggle-unresolved", "Show links to notes that don't exist.", |_| toggle("unresolved"))?;
    command("graph-filter-folder", "Only draw notes under a folder.", |_| filter("folder"))?;
    command("graph-filter-tag", "Only draw notes with a tag.", |_| filter("tag"))?;
    command("graph-follow", "Make the local graph follow the note you are in.", |_| toggle_follow())?;
    command("graph-export", "Save the graph as a PNG or SVG picture.", |_| run_command("canvas-export"))?;
    on(EventKind::BufferSaved, |e| {
        if let Event::BufferSaved(id) = e {
            note_saved(*id);
        }
    });
    when_idle("graph-rescan-tick", "Check an old saved index against the disk (graph).", 5.0, rescan_tick)?;
    when_idle("graph-follow-tick", "Move the local graph to the note you are in (graph-follow).", 1.2, follow_tick)?;

    for (keys, cmd) in [
        ("RET", "graph-open"),
        ("+", "graph-deeper"),
        ("=", "graph-deeper"),
        ("-", "graph-shallower"),
        ("t", "graph-toggle-tags"),
        ("o", "graph-toggle-orphans"),
        ("u", "graph-toggle-unresolved"),
        ("/", "graph-filter-folder"),
        ("#", "graph-filter-tag"),
        ("l", "graph-relayout"),
        ("g", "graph-refresh"),
        ("F", "graph-follow"),
        ("w", "graph-export"),
    ] {
        bind_in_mode(MODE, keys, cmd)?;
    }
    let items: Vec<MenuItem> = vec![
        menu_item("Local graph", "graph-local"),
        menu_item("Global graph", "graph-global"),
        menu_separator(),
        menu_item("Open lit note", "graph-open"),
        menu_item("Links deeper", "graph-deeper"),
        menu_item("Links shallower", "graph-shallower"),
        menu_separator(),
        menu_item("Show tags", "graph-toggle-tags"),
        menu_item("Show orphans", "graph-toggle-orphans"),
        menu_item("Show unresolved", "graph-toggle-unresolved"),
        menu_item("Filter by folder…", "graph-filter-folder"),
        menu_item("Filter by tag…", "graph-filter-tag"),
        menu_separator(),
        menu_item("Follow current note", "graph-follow"),
        menu_item("Layout again", "graph-relayout"),
        menu_item("Refresh", "graph-refresh"),
        menu_item("Read every note again", "graph-reindex"),
        menu_item("Save picture…", "graph-export"),
    ];
    define_menu_for_mode("Graph", &items, MODE)?;
    Ok(())
}

forge_plugin!(init);
