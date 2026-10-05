// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The page.
//!
//! It looks at what the buckets hold — scenes, the packs each is cut into,
//! the camera and the tiles a pack carries, the source tiles under that
//! camera — and renders a scene: its frames split between workers
//! (contiguous slices, the farm's rule), each worker walking the packs its
//! slice crosses, and their encoded frames joined into one mp4.
//!
//! It knows no key: scenes and packs come from the API's repositories, and a
//! pack is only ever a URL built from what they handed back.
//!
//! One piece of state, on the one thread a page has. Every handler borrows
//! it for as long as it takes to read or change a field and never across an
//! `await`: what an async step needs it takes with it, and it checks, when
//! it comes back, that it is still the step anyone is waiting for.

use std::cell::RefCell;
use std::rc::Rc;

use futures_channel::oneshot;
use futures_util::future::{join_all, try_join_all};
use js_sys::{Array, Uint8Array};
use tuile_film::{slice, CameraSample, TileInfo};
use tuile_mp4::{Codec, Muxer, ParameterSets};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{spawn_local, JsFuture};
use web_sys::{
    Blob, BlobPropertyBag, CanvasRenderingContext2d, Document, Element, Event, HtmlAnchorElement,
    HtmlCanvasElement, HtmlElement, HtmlInputElement, HtmlMediaElement, HtmlOptionElement,
    HtmlProgressElement, HtmlSelectElement, ImageBitmap, MessageEvent, MouseEvent, Response, Url,
    UrlSearchParams, Window, Worker, WorkerOptions, WorkerType,
};

use crate::encode::browser_config;
use crate::js::{get, now, number, object, string, text};
use crate::source::BLOCK_BYTES;
use crate::worker::PackView;

// ------------------------------------------------------------------ the API

#[derive(serde::Deserialize)]
struct Projects {
    projects: Vec<ProjectInfo>,
    tiles: Option<String>,
}

#[derive(serde::Deserialize)]
struct ProjectInfo {
    name: String,
    store: String,
    layout: String,
}

#[derive(serde::Deserialize)]
struct Summary {
    id: String,
    packs: usize,
    bytes: f64,
}

#[derive(serde::Deserialize, Clone)]
struct Chunk {
    key: String,
    first: u32,
    last: u32,
    bytes: f64,
    scene: Option<String>,
}

#[derive(serde::Deserialize)]
struct Unreadable {
    key: String,
    bytes: f64,
    why: String,
}

#[derive(serde::Deserialize)]
struct Scene {
    id: String,
    chunks: Vec<Chunk>,
    unreadable: Vec<Unreadable>,
    others: Vec<serde_json::Value>,
}

#[derive(serde::Deserialize, Clone)]
struct Layer {
    name: String,
    grid: String,
    content_type: String,
}

// ---------------------------------------------------------------- the state

/// What a render was asked of, frozen at the click. The page goes on to
/// other packs; a render and the film it leaves keep naming the one they
/// are of.
#[derive(Clone)]
struct Job {
    project: String,
    scene: String,
    /// The packs it reads, as named within the scene.
    packs: Vec<String>,
}

/// What the address asked for when the page loaded, used once.
#[derive(Default)]
struct Wanted {
    scene: Option<String>,
    pack: Option<String>,
    frame: Option<u32>,
}

/// How a track's points are placed on its canvas, to find one under a click.
#[derive(Clone, Copy)]
struct TrackMap {
    lon0: f64,
    lat0: f64,
    k: f64,
    scale: f64,
    ox: f64,
    oy: f64,
    height: f64,
}

impl TrackMap {
    fn x(&self, lon: f64) -> f64 {
        self.ox + (lon - self.lon0) * self.k * self.scale
    }

    fn y(&self, lat: f64) -> f64 {
        self.height - self.oy - (lat - self.lat0) * self.scale
    }
}

#[derive(Default)]
struct Page {
    api: String,
    project: Option<String>,
    scene: Option<Rc<Scene>>,
    /// The pack on screen, and which of the scene's chunks it is.
    view: Option<(Rc<PackView>, usize)>,
    path: Rc<Vec<CameraSample>>,
    layers: Vec<Layer>,
    /// Counters that name the latest request of each kind: a reply to an
    /// earlier one finds the number moved on and stops.
    opening: u32,
    listing: u32,
    tiles_turn: u32,
    source_turn: u32,
    /// The camera the source tiles were last asked under.
    source_at: Option<CameraSample>,
    track: Option<TrackMap>,
    job: Option<Job>,
    rendering: bool,
    /// How each worker of the render in hand is told it is over: what a
    /// stop sends through, to end the wait on them.
    stoppers: Vec<Stopper>,
    workers_typed: bool,
    wanted: Wanted,
}

/// The end of a worker's slice, to be said once: by the worker when it is
/// done or has failed, or by a stop.
type Stopper = Rc<RefCell<Option<oneshot::Sender<Result<Slice, String>>>>>;

/// What a stopped render ends with, in place of an error.
const STOPPED: &str = "Rendu arrêté.";

thread_local! {
    static PAGE: RefCell<Page> = RefCell::new(Page::default());
}

/// Reads or changes the page's state. Never held across an `await`.
fn page<T>(f: impl FnOnce(&mut Page) -> T) -> T {
    PAGE.with(|p| f(&mut p.borrow_mut()))
}

// ------------------------------------------------------------------ the DOM

fn window() -> Window {
    js_sys::global().unchecked_into()
}

fn document() -> Document {
    window().document().unwrap_throw()
}

fn el(id: &str) -> Element {
    document().get_element_by_id(id).unwrap_throw()
}

fn html(id: &str) -> HtmlElement {
    el(id).unchecked_into()
}

fn input(id: &str) -> HtmlInputElement {
    el(id).unchecked_into()
}

fn select(id: &str) -> HtmlSelectElement {
    el(id).unchecked_into()
}

fn value_of(id: &str) -> f64 {
    input(id).value().parse().unwrap_or(0.0)
}

fn set_text(id: &str, text: &str) {
    el(id).set_text_content(Some(text));
}

fn show(id: &str, shown: bool) {
    html(id).set_hidden(!shown);
}

fn create(tag: &str) -> Element {
    document().create_element(tag).unwrap_throw()
}

/// A node holding text, and nothing an API's reply could turn into markup.
fn cell(tag: &str, text: &str) -> Element {
    let node = create(tag);
    node.set_text_content(Some(text));
    node
}

fn row(cells: &[&str]) -> Element {
    let tr = create("tr");
    for text in cells {
        let _ = tr.append_child(&cell("td", text));
    }
    tr
}

/// The render's line: it stays on the pack being rendered whatever else is
/// looked at meanwhile.
fn status(text: &str, kind: &str) {
    set_text("status", text);
    el("status").set_class_name(kind);
}

/// The line about the pack on screen.
fn note(text: &str, kind: &str) {
    set_text("encoder-note", text);
    el("encoder-note").set_class_name(&format!("note {kind}"));
}

/// Calls `f` on each `event` of `target`, for as long as the page lives.
fn on(target: &web_sys::EventTarget, event: &str, f: impl FnMut(Event) + 'static) {
    let handler = Closure::<dyn FnMut(Event)>::new(f);
    let _ = target.add_event_listener_with_callback(event, handler.as_ref().unchecked_ref());
    handler.forget();
}

fn css(name: &str) -> String {
    let root = document().document_element().unwrap_throw();
    window()
        .get_computed_style(&root)
        .ok()
        .flatten()
        .and_then(|style| style.get_property_value(name).ok())
        .map(|v| v.trim().to_string())
        .unwrap_or_default()
}

fn query() -> UrlSearchParams {
    let search = window().location().search().unwrap_or_default();
    UrlSearchParams::new_with_str(&search).unwrap_throw()
}

fn megabytes(bytes: f64) -> String {
    if bytes >= 1e9 {
        format!("{:.2} Go", bytes / 1e9)
    } else {
        format!("{:.0} Mo", bytes / 1e6)
    }
}

/// How a French reader groups thousands.
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push('\u{202f}');
        }
        out.push(c);
    }
    out
}

fn plural(n: usize) -> &'static str {
    if n > 1 {
        "s"
    } else {
        ""
    }
}

// ----------------------------------------------------------------- fetching

async fn fetch(url: &str) -> Result<Response, String> {
    let response: Response = JsFuture::from(window().fetch_with_str(url))
        .await
        .map_err(text)?
        .unchecked_into();
    Ok(response)
}

async fn get_json<T: serde::de::DeserializeOwned>(path: &str) -> Result<T, String> {
    let api = page(|p| p.api.clone());
    let response = fetch(&format!("{api}{path}")).await?;
    let body = JsFuture::from(response.text().map_err(text)?)
        .await
        .map_err(text)?
        .as_string()
        .unwrap_or_default();
    if !response.ok() {
        return Err(format!("{path} : HTTP {} — {body}", response.status()));
    }
    serde_json::from_str(&body).map_err(|e| format!("{path} : {e}"))
}

/// A path segment, percent-encoded: a project's name or a layer's.
fn encoded(text: &str) -> String {
    String::from(js_sys::encode_uri_component(text))
}

/// A key's segments, each percent-encoded, its slashes kept.
fn encoded_key(key: &str) -> String {
    key.split('/').map(encoded).collect::<Vec<_>>().join("/")
}

/// A pack's block URL: it is read in the API's fixed blocks, each a URL of
/// its own, and `{block}` is where the reader puts a block's number.
fn pack_url(api: &str, project: &str, key: &str) -> String {
    format!(
        "{api}/p/{}/b{}/{{block}}/{}",
        encoded(project),
        BLOCK_BYTES >> 20,
        encoded_key(key)
    )
}

// -------------------------------------------------------------------- links

/// The address of a view: a project, a scene, and when given a pack and a
/// frame.
fn address(project: &str, scene: &str, pack: Option<&str>, frame: Option<u32>) -> String {
    let q = UrlSearchParams::new().unwrap_throw();
    if let Some(api) = query().get("api") {
        q.set("api", &api);
    }
    q.set("project", project);
    q.set("film", scene);
    if let Some(pack) = pack {
        q.set("pack", pack);
    }
    if let Some(frame) = frame {
        q.set("frame", &frame.to_string());
    }
    let path = window().location().pathname().unwrap_or_default();
    format!("{path}?{}", String::from(q.to_string()))
}

/// Keeps the address bar on what is on screen, so a view can be reopened
/// later, or sent to someone, as it is.
fn remember(frame: u32) {
    let Some((project, scene, pack)) = page(|p| {
        let (scene, (_, chunk)) = (p.scene.clone()?, p.view.as_ref()?);
        let key = &scene.chunks.get(*chunk)?.key;
        Some((
            p.project.clone()?,
            scene.id.clone(),
            key[scene.id.len() + 1..].to_string(),
        ))
    }) else {
        return;
    };
    let url = address(&project, &scene, Some(&pack), Some(frame));
    if let Ok(history) = window().history() {
        let _ = history.replace_state_with_url(&JsValue::NULL, "", Some(&url));
    }
    el("permalink")
        .unchecked_into::<HtmlAnchorElement>()
        .set_href(&url);
    show("permalink", true);
}

/// A link that reopens the pack a job is of.
fn link_to(job: &Job) -> Element {
    let a: HtmlAnchorElement = create("a").unchecked_into();
    let pack = (job.packs.len() == 1).then(|| job.packs[0].as_str());
    a.set_href(&address(&job.project, &job.scene, pack, None));
    a.set_text_content(Some(&match pack {
        Some(pack) => format!("{} / {} / {pack}", job.project, job.scene),
        None => format!(
            "{} / {} ({} packs)",
            job.project,
            job.scene,
            job.packs.len()
        ),
    }));
    a.unchecked_into()
}

/// Sets a line to: some words, the link to a job's pack, some more words.
fn cite(id: &str, before: &str, job: &Job, after: &str) {
    let line = el(id);
    line.set_text_content(Some(before));
    let _ = line.append_child(&link_to(job));
    let _ = line.append_with_str_1(after);
}

// ----------------------------------------------------------------- projects

async fn start() {
    let listed: Result<Projects, String> = get_json("/projects").await;
    let projects = match listed {
        Ok(p) => p,
        Err(e) => {
            set_text("films-note", &format!("API indisponible : {e}"));
            el("films-note").set_class_name("note bad");
            return;
        }
    };
    el("projects").set_inner_html("");
    for project in &projects.projects {
        let button = cell("button", &project.name);
        let _ = button.set_attribute("title", &format!("{} — {}", project.store, project.layout));
        let _ = button.set_attribute("role", "tab");
        let name = project.name.clone();
        on(&button, "click", move |_| {
            spawn_local(open_project(name.clone()))
        });
        let _ = el("projects").append_child(&button);
    }
    if projects.tiles.is_some() {
        if let Ok(layers) = get_json::<Vec<Layer>>("/tiles/catalog").await {
            let layers: Vec<Layer> = layers
                .into_iter()
                .filter(|l| !l.name.ends_with(".absent"))
                .collect();
            let choice = select("layer");
            choice.set_inner_html("");
            for layer in &layers {
                let option: HtmlOptionElement = create("option").unchecked_into();
                option.set_value(&layer.name);
                option.set_text(&format!("{} — {}", layer.name, layer.content_type));
                let _ = choice.append_child(&option);
            }
            if let Some(imagery) = layers.iter().find(|l| l.content_type.starts_with("image/")) {
                choice.set_value(&imagery.name);
            }
            page(|p| p.layers = layers);
        }
    }
    // ?project=…&film=…&pack=…&frame=… opens straight onto that view.
    let asked = query().get("project");
    let first = projects
        .projects
        .iter()
        .find(|p| Some(&p.name) == asked.as_ref())
        .or(projects.projects.first());
    if let Some(project) = first {
        open_project(project.name.clone()).await;
    }
    let wanted = page(|p| p.wanted.scene.clone());
    match wanted.and_then(|id| find_row("films", &id)) {
        Some(row) => row.unchecked_into::<HtmlElement>().click(),
        None => page(|p| p.wanted = Wanted::default()),
    }
}

/// The row of a table whose first cell says `text`.
fn find_row(table: &str, text: &str) -> Option<Element> {
    let rows = el(table).query_selector_all("tbody tr").ok()?;
    (0..rows.length())
        .filter_map(|i| rows.item(i)?.dyn_into::<Element>().ok())
        .find(|row| {
            row.first_element_child()
                .and_then(|c| c.text_content())
                .as_deref()
                == Some(text)
        })
}

/// Marks one row of a table as the chosen one.
fn choose(rows: &[Element], chosen: &Element) {
    for row in rows {
        let _ = row.class_list().toggle_with_force("on", row == chosen);
    }
}

async fn open_project(name: String) {
    // Leaving what was on screen: its view goes, and anything still being
    // opened for it is disowned. A render in hand, and the film the last one
    // left, are not the screen's: they stay.
    let turn = page(|p| {
        p.opening += 1;
        p.listing += 1;
        p.view = None;
        p.path = Rc::default();
        p.scene = None;
        p.project = Some(name.clone());
        p.listing
    });
    for panel in ["film-panel", "camera-panel", "tiles-panel"] {
        show(panel, false);
    }
    show("pack-panel", page(|p| p.job.is_some()));
    let tabs = el("projects").children();
    for i in 0..tabs.length() {
        if let Some(tab) = tabs.item(i) {
            let chosen = tab.text_content().as_deref() == Some(name.as_str());
            let _ = tab.set_attribute("aria-selected", if chosen { "true" } else { "false" });
        }
    }
    let body = el("films")
        .query_selector("tbody")
        .ok()
        .flatten()
        .unwrap_throw();
    body.set_inner_html("");
    el("films-note").set_class_name("note");
    set_text("films-note", "Lecture des packs du bucket…");
    let listed: Result<Vec<Summary>, String> =
        get_json(&format!("/p/{}/films", encoded(&name))).await;
    if page(|p| p.listing) != turn {
        return;
    }
    let scenes = match listed {
        Ok(s) => s,
        Err(e) => {
            set_text("films-note", &e);
            el("films-note").set_class_name("note bad");
            return;
        }
    };
    set_text(
        "films-note",
        &format!(
            "{} scènes à rendre, {} de packs. Rien ici n'est déjà rendu : ce sont les packs cuits, que cette page rend.",
            scenes.len(),
            megabytes(scenes.iter().map(|s| s.bytes).sum()),
        ),
    );
    let rows: Rc<RefCell<Vec<Element>>> = Rc::default();
    for scene in scenes {
        let tr = row(&[&scene.id, &scene.packs.to_string(), &megabytes(scene.bytes)]);
        tr.set_class_name("pick");
        let (rows_in, this, id) = (rows.clone(), tr.clone(), scene.id.clone());
        on(&tr, "click", move |_| {
            choose(&rows_in.borrow(), &this);
            spawn_local(open_scene(id.clone()));
        });
        let _ = body.append_child(&tr);
        rows.borrow_mut().push(tr);
    }
}

// -------------------------------------------------------------------- scene

async fn open_scene(id: String) {
    show("film-panel", true);
    set_text("film-title", &id);
    el("film-note").set_class_name("note");
    set_text(
        "film-note",
        "Lecture de la scène (la plage de chaque pack est lue dans sa table)…",
    );
    let body = el("chunks")
        .query_selector("tbody")
        .ok()
        .flatten()
        .unwrap_throw();
    body.set_inner_html("");
    for panel in ["camera-panel", "tiles-panel"] {
        show(panel, false);
    }
    show("pack-panel", page(|p| p.job.is_some()));
    let (turn, project) = page(|p| {
        p.opening += 1;
        p.listing += 1;
        p.view = None;
        p.path = Rc::default();
        p.scene = None;
        (p.listing, p.project.clone().unwrap_or_default())
    });
    let opened: Result<Scene, String> = get_json(&format!(
        "/p/{}/films/{}",
        encoded(&project),
        encoded_key(&id)
    ))
    .await;
    if page(|p| p.listing) != turn {
        return;
    }
    let scene = match opened {
        Ok(s) => Rc::new(s),
        Err(e) => {
            set_text("film-note", &e);
            el("film-note").set_class_name("note bad");
            return;
        }
    };
    page(|p| p.scene = Some(scene.clone()));
    let within = scene.id.len() + 1;

    for u in &scene.unreadable {
        let tr = create("tr");
        let _ = tr.append_child(&cell("td", &u.key[within..]));
        let why = cell(
            "td",
            &format!("illisible — {} ({})", u.why, megabytes(u.bytes)),
        );
        let _ = why.set_attribute("colspan", "3");
        why.set_class_name("bad");
        let _ = why.set_attribute("style", "text-align:left");
        let _ = tr.append_child(&why);
        let _ = body.append_child(&tr);
    }
    let (Some(first), Some(last)) = (scene.chunks.first(), scene.chunks.last()) else {
        set_text(
            "film-note",
            &format!(
                "Aucun pack lisible par ce build dans cette scène : {} illisible{}.",
                scene.unreadable.len(),
                plural(scene.unreadable.len()),
            ),
        );
        el("film-note").set_class_name("note bad");
        return;
    };
    let (first, last) = (first.first, last.last);
    let gaps = scene
        .chunks
        .windows(2)
        .filter(|w| w[1].first != w[0].last + 1)
        .count();
    set_text(
        "film-note",
        &format!(
            "{} pack{}, frames {first}–{last}{}, {} autres objets dans le dossier. Cliquer un pack pour voir sa caméra et ses tuiles.",
            scene.chunks.len(),
            plural(scene.chunks.len()),
            if gaps > 0 { format!(", {gaps} trou{} entre packs", plural(gaps)) } else { String::new() },
            scene.others.len(),
        ),
    );
    let rows: Rc<RefCell<Vec<Element>>> = Rc::default();
    for (i, chunk) in scene.chunks.iter().enumerate() {
        let tr = row(&[
            &chunk.key[within..],
            &format!("{}–{}", chunk.first, chunk.last),
            &megabytes(chunk.bytes),
            chunk.scene.as_deref().unwrap_or("—"),
        ]);
        tr.set_class_name("pick");
        let (rows_in, this) = (rows.clone(), tr.clone());
        on(&tr, "click", move |_| {
            choose(&rows_in.borrow(), &this);
            spawn_local(open_chunk(i, None));
        });
        let _ = body.append_child(&tr);
        rows.borrow_mut().push(tr);
    }

    show("pack-panel", true);
    for id in ["first", "last"] {
        input(id).set_min(&first.to_string());
        input(id).set_max(&last.to_string());
    }
    input("first").set_value(&first.to_string());
    input("last").set_value(&last.to_string());
    // The pack and frame the address named, the first time; the first pack
    // otherwise.
    let wanted = page(|p| std::mem::take(&mut p.wanted));
    let named = scene
        .chunks
        .iter()
        .position(|c| Some(&c.key[within..]) == wanted.pack.as_deref());
    let at = named.unwrap_or(0);
    note("Ouverture du pack…", "");
    let rows = rows.borrow();
    choose(&rows, &rows[at]);
    rows[at].scroll_into_view_with_bool(false);
    spawn_local(open_chunk(at, named.and(wanted.frame)));
}

// ------------------------------------------------------- a chunk: its pack

async fn open_chunk(index: usize, frame: Option<u32>) {
    // From here nothing reaches the previous pack's view: it is dropped from
    // the page, and freed when the last read still holding it lets go.
    let Some((turn, scene, url)) = page(|p| {
        p.opening += 1;
        p.view = None;
        p.path = Rc::default();
        let scene = p.scene.clone()?;
        let url = pack_url(&p.api, p.project.as_deref()?, &scene.chunks.get(index)?.key);
        Some((p.opening, scene, url))
    }) else {
        return;
    };
    el("go")
        .unchecked_into::<web_sys::HtmlButtonElement>()
        .set_disabled(true);
    show("camera-panel", true);
    let name = &scene.chunks[index].key[scene.id.len() + 1..];
    set_text("camera-title", &format!("Caméra — {name}"));
    set_text("cam-at", "Lecture de la table du pack…");
    let opened = PackView::open(JsValue::from_str(&url)).await;
    // Another pack was asked for while this one was being read: it wins.
    if page(|p| p.opening) != turn {
        return;
    }
    let view = match opened {
        Ok(v) => Rc::new(v),
        Err(e) => {
            set_text("cam-at", &format!("Pack illisible : {}", text(e)));
            return;
        }
    };
    let path = Rc::new(view.samples(3000));
    page(|p| {
        p.view = Some((view.clone(), index));
        p.path = path;
    });
    input("cam-frame").set_min(&view.first().to_string());
    input("cam-frame").set_max(&view.last().to_string());
    offer_scales(turn).await;
    if page(|p| p.opening) != turn {
        return;
    }
    select_frame(f64::from(frame.unwrap_or(view.first()))).await;
}

fn nearest(path: &[CameraSample], frame: u32) -> Option<CameraSample> {
    path.iter().min_by_key(|s| s.frame.abs_diff(frame)).copied()
}

async fn select_frame(frame: f64) {
    let Some((view, path)) = page(|p| Some((p.view.clone()?.0, p.path.clone()))) else {
        return;
    };
    let frame = (frame.round() as u32).clamp(view.first(), view.last());
    let Some(at) = nearest(&path, frame) else {
        return;
    };
    input("cam-frame").set_value(&frame.to_string());
    remember(frame);
    set_text(
        "cam-at",
        &format!(
            "lon {:.5}°, lat {:.5}°, {:.0} m, cap {:.0}°, inclinaison {:.1}°, champ {:.0}°",
            at.lon_deg, at.lat_deg, at.height_m, at.heading_deg, at.pitch_deg, at.fovy_deg
        ),
    );
    draw_track(&path, &at);
    draw_profile(&path, &at);
    futures_util::future::join(show_pack_tiles(view, frame), show_source_tiles(Some(at))).await;
}

// -------------------------------------------------------------------- plots

const PAD: f64 = 28.0;

fn canvas(id: &str) -> (HtmlCanvasElement, CanvasRenderingContext2d) {
    let canvas: HtmlCanvasElement = el(id).unchecked_into();
    let context = canvas
        .get_context("2d")
        .ok()
        .flatten()
        .unwrap_throw()
        .unchecked_into();
    (canvas, context)
}

fn stroke_path(ctx: &CanvasRenderingContext2d, points: impl Iterator<Item = (f64, f64)>) {
    ctx.begin_path();
    for (i, (x, y)) in points.enumerate() {
        if i == 0 {
            ctx.move_to(x, y);
        } else {
            ctx.line_to(x, y);
        }
    }
    ctx.stroke();
}

fn dot(ctx: &CanvasRenderingContext2d, x: f64, y: f64, colour: &str, radius: f64) {
    ctx.set_fill_style_str(colour);
    ctx.begin_path();
    let _ = ctx.arc(x, y, radius, 0.0, std::f64::consts::TAU);
    ctx.fill();
}

fn label(ctx: &CanvasRenderingContext2d, text: &str, x: f64, y: f64, align: &str) {
    ctx.set_text_align(align);
    let _ = ctx.fill_text(text, x, y);
    ctx.set_text_align("left");
}

/// The ground track, north up, a metre the same length on both axes.
fn draw_track(path: &[CameraSample], at: &CameraSample) {
    let (canvas, ctx) = canvas("track");
    let (w, h) = (f64::from(canvas.width()), f64::from(canvas.height()));
    ctx.clear_rect(0.0, 0.0, w, h);
    let (Some(start), Some(end)) = (path.first(), path.last()) else {
        return;
    };
    let fold = |f: fn(f64, f64) -> f64, of: fn(&CameraSample) -> f64| {
        path.iter().map(of).fold(of(start), f)
    };
    let (lon0, lon1) = (fold(f64::min, |s| s.lon_deg), fold(f64::max, |s| s.lon_deg));
    let (lat0, lat1) = (fold(f64::min, |s| s.lat_deg), fold(f64::max, |s| s.lat_deg));
    // A degree of longitude is shorter than one of latitude by cos(lat).
    let k = ((lat0 + lat1) / 2.0).to_radians().cos();
    let (span_x, span_y) = (((lon1 - lon0) * k).max(1e-6), (lat1 - lat0).max(1e-6));
    let scale = ((w - 2.0 * PAD) / span_x).min((h - 2.0 * PAD) / span_y);
    let map = TrackMap {
        lon0,
        lat0,
        k,
        scale,
        ox: (w - span_x * scale) / 2.0,
        oy: (h - span_y * scale) / 2.0,
        height: h,
    };
    page(|p| p.track = Some(map));

    ctx.set_line_width(2.0);
    ctx.set_line_join("round");
    ctx.set_stroke_style_str(&css("--track"));
    stroke_path(
        &ctx,
        path.iter().map(|s| (map.x(s.lon_deg), map.y(s.lat_deg))),
    );
    dot(
        &ctx,
        map.x(start.lon_deg),
        map.y(start.lat_deg),
        &css("--start"),
        6.0,
    );
    dot(
        &ctx,
        map.x(end.lon_deg),
        map.y(end.lat_deg),
        &css("--end"),
        6.0,
    );
    // The selected frame, with where it looks.
    let (x, y, heading) = (
        map.x(at.lon_deg),
        map.y(at.lat_deg),
        at.heading_deg.to_radians(),
    );
    ctx.set_stroke_style_str(&css("--fg"));
    stroke_path(
        &ctx,
        [(x, y), (x + 26.0 * heading.sin(), y - 26.0 * heading.cos())].into_iter(),
    );
    dot(&ctx, x, y, &css("--fg"), 5.0);

    ctx.set_fill_style_str(&css("--muted"));
    ctx.set_font("12px ui-sans-serif, system-ui");
    let km = (span_x.max(span_y)) * 111.32;
    let digits = if km > 20.0 { 0 } else { 2 };
    label(&ctx, &format!("{km:.digits$} km"), PAD, h - 8.0, "left");
}

/// Height (solid) and pitch (dashed) against frames.
fn draw_profile(path: &[CameraSample], at: &CameraSample) {
    let (canvas, ctx) = canvas("profile");
    let (w, h) = (f64::from(canvas.width()), f64::from(canvas.height()));
    ctx.clear_rect(0.0, 0.0, w, h);
    let (Some(start), Some(end)) = (path.first(), path.last()) else {
        return;
    };
    let (f0, f1) = (
        f64::from(start.frame),
        f64::from(end.frame.max(start.frame + 1)),
    );
    let low = path
        .iter()
        .map(|s| s.height_m)
        .fold(f64::INFINITY, f64::min);
    let high = path
        .iter()
        .map(|s| s.height_m)
        .fold(f64::NEG_INFINITY, f64::max);
    let x = |frame: u32| PAD + (f64::from(frame) - f0) / (f1 - f0) * (w - 2.0 * PAD);
    let y_height = |v: f64| h - PAD - (v - low) / (high - low).max(1e-6) * (h - 2.0 * PAD);
    // −90° at the bottom, the horizon at the top.
    let y_pitch = |v: f64| h - PAD - (v + 90.0) / 90.0 * (h - 2.0 * PAD);

    ctx.set_line_width(1.0);
    ctx.set_stroke_style_str(&css("--line"));
    ctx.stroke_rect(PAD, PAD, w - 2.0 * PAD, h - 2.0 * PAD);
    ctx.set_line_width(2.0);
    ctx.set_stroke_style_str(&css("--track"));
    let _ = ctx.set_line_dash(&Array::new());
    stroke_path(
        &ctx,
        path.iter().map(|s| (x(s.frame), y_height(s.height_m))),
    );
    ctx.set_stroke_style_str(&css("--end"));
    let _ = ctx.set_line_dash(&Array::of2(&JsValue::from(6), &JsValue::from(5)));
    stroke_path(
        &ctx,
        path.iter().map(|s| (x(s.frame), y_pitch(s.pitch_deg))),
    );
    let _ = ctx.set_line_dash(&Array::new());
    ctx.set_line_width(1.0);
    ctx.set_stroke_style_str(&css("--fg"));
    stroke_path(
        &ctx,
        [(x(at.frame), PAD), (x(at.frame), h - PAD)].into_iter(),
    );

    ctx.set_fill_style_str(&css("--muted"));
    ctx.set_font("12px ui-sans-serif, system-ui");
    label(&ctx, &format!("{high:.0} m"), PAD + 4.0, PAD + 14.0, "left");
    label(
        &ctx,
        &format!("{low:.0} m"),
        PAD + 4.0,
        h - PAD - 6.0,
        "left",
    );
    label(&ctx, "0°", w - PAD - 4.0, PAD + 14.0, "right");
    label(&ctx, "−90°", w - PAD - 4.0, h - PAD - 6.0, "right");
    label(
        &ctx,
        &format!("frame {}", start.frame),
        PAD,
        h - 8.0,
        "left",
    );
    label(&ctx, &end.frame.to_string(), w - PAD, h - 8.0, "right");
}

/// Where a click fell on a canvas, in the canvas's own pixels.
fn click_at(event: &Event) -> Option<(f64, f64, HtmlCanvasElement)> {
    let mouse = event.dyn_ref::<MouseEvent>()?;
    let canvas: HtmlCanvasElement = event.target()?.dyn_into().ok()?;
    let rect = canvas.get_bounding_client_rect();
    let x = (f64::from(mouse.client_x()) - rect.left()) * f64::from(canvas.width()) / rect.width();
    let y = (f64::from(mouse.client_y()) - rect.top()) * f64::from(canvas.height()) / rect.height();
    Some((x, y, canvas))
}

// -------------------------------------------------------------------- tiles

/// How many of a frame's tiles are shown.
const SHOWN: usize = 24;

fn revoke_images(parent: &Element) {
    if let Ok(images) = parent.query_selector_all("img") {
        for i in 0..images.length() {
            if let Some(src) = images
                .item(i)
                .and_then(|n| n.dyn_into::<Element>().ok()?.get_attribute("src"))
            {
                let _ = Url::revoke_object_url(&src);
            }
        }
    }
}

/// An `<img>` showing these bytes.
fn image_of(bytes: &[u8], kind: &str) -> Option<Element> {
    let bag = BlobPropertyBag::new();
    bag.set_type(kind);
    let parts = Array::of1(&Uint8Array::from(bytes));
    let blob = Blob::new_with_u8_array_sequence_and_options(&parts, &bag).ok()?;
    let img = create("img");
    img.set_attribute("src", &Url::create_object_url_with_blob(&blob).ok()?)
        .ok()?;
    Some(img)
}

fn prepend(parent: &Element, child: &Element) {
    let _ = parent.insert_before(child, parent.first_child().as_ref());
}

fn prepend_text(parent: &Element, text: &str) {
    let _ = parent.insert_before(
        &document().create_text_node(text),
        parent.first_child().as_ref(),
    );
}

/// The tiles the pack draws at a frame, the first of them with their
/// imagery. It holds the pack's view for as long as its textures are being
/// read, whatever the page has moved on to.
async fn show_pack_tiles(view: Rc<PackView>, frame: u32) {
    let turn = page(|p| {
        p.tiles_turn += 1;
        p.tiles_turn
    });
    show("tiles-panel", true);
    set_text("tiles-title", &format!("Tuiles — frame {frame}"));
    let tiles: Vec<TileInfo> = view.tiles_of(frame).unwrap_or_default();
    let triangles: u64 = tiles.iter().map(|t| u64::from(t.triangles)).sum();
    let imagery: f64 = tiles.iter().map(|t| f64::from(t.texture_bytes)).sum();
    set_text(
        "pack-tiles-note",
        &format!(
            "Dans le pack : {} tuiles dessinées, {} triangles, {} d'imagerie. Les {} premières :",
            tiles.len(),
            grouped(triangles),
            megabytes(imagery),
            SHOWN.min(tiles.len()),
        ),
    );
    let gallery = el("pack-tiles");
    revoke_images(&gallery);
    gallery.set_inner_html("");
    let shown = tiles.iter().take(SHOWN).enumerate().map(|(i, tile)| {
        let figure = create("figure");
        let _ = figure.set_attribute(
            "title",
            &format!(
                "tuile {}, drapé {:016x}, {} triangles, {:.0} ko",
                tile.id,
                tile.drape,
                tile.triangles,
                f64::from(tile.texture_bytes) / 1e3
            ),
        );
        let _ = figure.append_child(&cell("figcaption", &format!("{} tri.", tile.triangles)));
        let _ = gallery.append_child(&figure);
        let (view, textured) = (view.clone(), tile.texture_bytes > 0);
        async move {
            if !textured {
                prepend_text(&figure, "sans texture");
                return;
            }
            // Superseded before it started: nothing to read.
            if page(|p| p.tiles_turn) != turn {
                return;
            }
            match view.texture(frame, i as u32).await {
                Ok(png) if page(|p| p.tiles_turn) == turn => {
                    if let Some(img) = image_of(&png, "image/png") {
                        prepend(&figure, &img);
                    }
                }
                Ok(_) => {}
                Err(_) => prepend_text(&figure, "illisible"),
            }
        }
    });
    join_all(shown).await;
}

/// Column and row of the tile holding a point, in a layer's own grid, and
/// which way rows are counted.
fn tile_at(grid: &str, z: u32, lon: f64, lat: f64) -> (i64, i64, i64) {
    let n = f64::from(1u32 << z.min(30));
    if grid == "web-mercator" {
        let s = lat.to_radians().tan().asinh();
        (
            ((lon + 180.0) / 360.0 * n).floor() as i64,
            ((1.0 - s / std::f64::consts::PI) / 2.0 * n).floor() as i64,
            1,
        )
    } else {
        // Geographic: two columns per row at level 0, rows from the south.
        (
            ((lon + 180.0) / 180.0 * n).floor() as i64,
            ((lat + 90.0) / 180.0 * n).floor() as i64,
            -1,
        )
    }
}

/// The source tiles around the point under a camera, read as stored.
async fn show_source_tiles(at: Option<CameraSample>) {
    let Some((turn, at, layer, api)) = page(|p| {
        p.source_at = at.or(p.source_at);
        p.source_turn += 1;
        let name = select("layer").value();
        let layer = p.layers.iter().find(|l| l.name == name)?.clone();
        Some((p.source_turn, p.source_at?, layer, p.api.clone()))
    }) else {
        if page(|p| p.layers.is_empty()) {
            set_text("tiles-note", "Pas de store de tuiles configuré.");
        }
        return;
    };
    let z = value_of("tz") as u32;
    let (x, y, dy) = tile_at(&layer.grid, z, at.lon_deg, at.lat_deg);
    set_text(
        "tiles-note",
        &format!(
            "Store source, sous la caméra : {} niveau {z}, autour de {x}/{y} (grille {}). Lu tel que stocké, sans appel à la source.",
            layer.name, layer.grid
        ),
    );
    let grid = el("tiles");
    revoke_images(&grid);
    grid.set_inner_html("");
    let mut cells = Vec::new();
    for j in -1..=1i64 {
        for i in -1..=1i64 {
            cells.push((x + i, y + j * dy));
        }
    }
    let shown = cells.into_iter().map(|(x, y)| {
        let cell_el = create("div");
        let _ = cell_el.append_child(&cell("span", &format!("{z}/{x}/{y}")));
        let _ = grid.append_child(&cell_el);
        let url = format!("{api}/tiles/{}/{z}/{x}/{y}", encoded(&layer.name));
        let kind = layer.content_type.clone();
        async move {
            let Ok(response) = fetch(&url).await else {
                return;
            };
            if page(|p| p.source_turn) != turn {
                return;
            }
            if response.status() == 404 {
                prepend_text(&cell_el, "absente du store");
                return;
            }
            if !response.ok() {
                prepend_text(&cell_el, &format!("HTTP {}", response.status()));
                return;
            }
            let Some(bytes) = body_of(&response).await else {
                return;
            };
            if kind.starts_with("image/") {
                if let Some(img) = image_of(&bytes, &kind) {
                    prepend(&cell_el, &img);
                }
            } else {
                let short = kind.rsplit('/').next().unwrap_or_default();
                prepend_text(
                    &cell_el,
                    &format!("{:.1} ko — {short}", bytes.len() as f64 / 1e3),
                );
            }
        }
    });
    join_all(shown).await;
}

async fn body_of(response: &Response) -> Option<Vec<u8>> {
    let buffer = JsFuture::from(response.array_buffer().ok()?).await.ok()?;
    Some(Uint8Array::new(&buffer).to_vec())
}

// ------------------------------------------------------------------- render

/// A size the film may be rendered at: a multiple of 8, as an encoder and
/// the I420 conversion want.
fn even8(x: f64) -> u32 {
    (((x / 8.0).round() as u32) * 8).max(8)
}

/// How many workers a film is split between, from what the browser says of
/// the machine's logical cores. The two encoders are limited by different
/// things:
///
/// - rav1e is the CPU's: each worker encodes on its own core, so every core
///   but one — kept for the page and the browser's GPU process — is a worker;
/// - the browser's encoder is fast and all workers share one GPU, so past a
///   few they queue behind each other: half the cores, four at most.
///
/// A browser may report fewer cores than there are, never more.
fn suggested_workers(soft: bool) -> u32 {
    let cores = window().navigator().hardware_concurrency().max(1.0) as u32;
    if soft {
        (cores.saturating_sub(1)).clamp(1, 16)
    } else {
        (cores.div_ceil(2)).clamp(1, 4)
    }
}

fn options() -> Vec<HtmlOptionElement> {
    let all = select("scale").options();
    (0..all.length())
        .filter_map(|i| all.item(i)?.dyn_into().ok())
        .collect()
}

fn chosen_is_soft() -> bool {
    let choice = select("scale");
    options()
        .get(choice.selected_index().max(0) as usize)
        .and_then(|o| o.get_attribute("data-encoder"))
        .as_deref()
        == Some("rav1e")
}

/// Says, for each size, which encoder it will get. Encoding is progressive:
/// the browser's H.264 encoder when it has one for the size, and otherwise
/// rav1e in wasm — any size, but seconds per frame rather than milliseconds.
/// The pack's own viewport stays the default.
async fn offer_scales(turn: u32) {
    // The pack's size is read once: the page may be on another pack, or
    // none, by the time the browser has answered.
    let Some(view) = page(|p| Some(p.view.clone()?.0)) else {
        return;
    };
    let (fps, bitrate) = (value_of("fps") as u32, value_of("mbps") * 1e6);
    for option in options() {
        let scale: f64 = option.value().parse().unwrap_or(1.0);
        let (w, h) = (
            even8(f64::from(view.width()) * scale),
            even8(f64::from(view.height()) * scale),
        );
        let browser = browser_config(w, h, fps, bitrate).await.is_some();
        if page(|p| p.opening) != turn {
            return;
        }
        let _ = option.set_attribute("data-encoder", if browser { "browser" } else { "rav1e" });
        option.set_text(&format!(
            "{w}×{h}{} — {}",
            if scale == 1.0 {
                " (viewport du pack)"
            } else {
                ""
            },
            if browser {
                "H.264 du navigateur"
            } else {
                "AV1 logiciel, lent"
            },
        ));
    }
    select("scale").set_value("1");
    el("go")
        .unchecked_into::<web_sys::HtmlButtonElement>()
        .set_disabled(page(|p| p.rendering));
    describe_encoder();
}

fn describe_encoder() {
    let Some((view, scene, typed)) =
        page(|p| Some((p.view.clone()?.0, p.scene.clone()?, p.workers_typed)))
    else {
        return;
    };
    let soft = chosen_is_soft();
    let cores = window().navigator().hardware_concurrency();
    // A number typed in the field is the user's and is left alone.
    if !typed {
        input("workers").set_value(&suggested_workers(soft).to_string());
    }
    input("workers").set_title(&format!(
        "{cores} cœurs logiques déclarés par le navigateur ; {} workers proposés pour cet encodeur",
        suggested_workers(soft)
    ));
    let about = format!(
        "{} : viewport {}×{}, table de {:.2} Mo lue pour ce pack ({} tuiles).",
        scene.id,
        view.width(),
        view.height(),
        f64::from(view.table_bytes()) / 1e6,
        view.tiles()
    );
    if soft {
        note(
            &format!("{about} Le navigateur n'a pas d'encodeur H.264 à cette taille : repli sur rav1e (AV1 en WebAssembly), de l'ordre de la seconde par image et par worker. Une taille plus petite passe par l'encodeur du navigateur."),
            "",
        );
    } else {
        note(&about, "good");
    }
}

/// What a slice of the film has to read: each pack it crosses, with the
/// frames of that pack the slice wants.
fn crossing(
    scene: &Scene,
    api: &str,
    project: &str,
    first: u32,
    last: u32,
) -> Vec<(String, u32, u32)> {
    scene
        .chunks
        .iter()
        .filter(|c| c.last >= first && c.first <= last)
        .map(|c| {
            (
                pack_url(api, project, &c.key),
                first.max(c.first),
                last.min(c.last),
            )
        })
        .collect()
}

#[derive(Default, Clone, Copy)]
struct Totals {
    fetch: f64,
    unpack: f64,
    decode: f64,
    upload: f64,
    record: f64,
    next: f64,
    encode: f64,
    bytes: f64,
    requests: f64,
    frames: u32,
}

fn render_stats(totals: &Totals, wall: f64) {
    let body = el("stats")
        .query_selector("tbody")
        .ok()
        .flatten()
        .unwrap_throw();
    body.set_inner_html("");
    let frames = f64::from(totals.frames.max(1));
    let rows = [
        ("Lecture des blocs", totals.fetch),
        ("Dépaquetage (LZ4)", totals.unpack),
        ("Décodage PNG (navigateur)", totals.decode),
        ("Upload GPU", totals.upload),
        ("Enregistrement + soumission", totals.record),
        ("next() complet", totals.next),
        ("Encodage", totals.encode),
    ];
    for (name, ms) in rows {
        let _ = body.append_child(&row(&[
            name,
            &format!("{:.2} ms", ms / frames),
            &format!("{:.2} s", ms / 1000.0),
        ]));
    }
    let _ = body.append_child(&row(&[
        "Octets lus",
        &format!("{:.2} Mo", totals.bytes / 1e6 / frames),
        &format!(
            "{:.1} Mo en {:.0} lectures",
            totals.bytes / 1e6,
            totals.requests
        ),
    ]));
    let last = row(&[
        "Mur",
        &format!("{:.1} ms", wall * 1000.0 / frames),
        &format!(
            "{wall:.2} s — {:.1} images/s",
            f64::from(totals.frames) / wall.max(1e-9)
        ),
    ]);
    last.set_class_name("total");
    let _ = body.append_child(&last);
}

/// A block, in megabytes, for saying how much is fetched ahead.
const BLOCK_MB: f64 = (BLOCK_BYTES >> 20) as f64;

/// One worker's slice, encoded, as it came back.
struct Slice {
    codec: String,
    record: Vec<u8>,
    chunks: Vec<(u32, bool, Vec<u8>)>,
}

fn slice_of(done: &JsValue) -> Slice {
    Slice {
        codec: string(done, "codec"),
        record: Uint8Array::new(&get(done, "record")).to_vec(),
        chunks: Array::from(&get(done, "chunks"))
            .iter()
            .map(|c| {
                (
                    number(&c, "index") as u32,
                    get(&c, "key").as_bool().unwrap_or(false),
                    Uint8Array::new(&get(&c, "data")).to_vec(),
                )
            })
            .collect(),
    }
}

/// The codec a configuration record is for: an `av1C` starts with its marker
/// bit set, an `avcC` with its version, 1.
fn codec_of(record: &[u8]) -> Result<Codec, String> {
    match record.first() {
        Some(0x81) => Ok(Codec::Av1(record.to_vec())),
        _ => Ok(Codec::Avc(
            ParameterSets::from_avcc(record).map_err(|e| e.to_string())?,
        )),
    }
}

/// Joins the slices into one mp4, refusing any whose encoder disagreed.
fn join(slices: &[Slice], width: u32, height: u32, fps: u32) -> Result<(Vec<u8>, u64), String> {
    let first = slices.first().ok_or("aucune tranche")?;
    let mut muxer = Muxer::with(width as u16, height as u16, fps, codec_of(&first.record)?)
        .map_err(|e| e.to_string())?;
    for slice in slices {
        muxer
            .check(&codec_of(&slice.record)?)
            .map_err(|e| e.to_string())?;
        for (index, key, data) in &slice.chunks {
            muxer
                .push(u64::from(*index), data.clone(), *key)
                .map_err(|e| e.to_string())?;
        }
    }
    let frames = muxer.frames();
    Ok((muxer.finish().map_err(|e| e.to_string())?, frames))
}

/// Ends the render in hand: every wait on a worker is answered now, and the
/// render, finding its workers over, terminates them — whatever each was in
/// the middle of — and gives the page back.
fn stop() {
    for stopper in page(|p| std::mem::take(&mut p.stoppers)) {
        if let Some(tx) = stopper.borrow_mut().take() {
            let _ = tx.send(Err(STOPPED.to_string()));
        }
    }
}

async fn render() {
    let Some((view, scene, project, api)) = page(|p| {
        if p.rendering {
            return None;
        }
        Some((
            p.view.clone()?.0,
            p.scene.clone()?,
            p.project.clone()?,
            p.api.clone(),
        ))
    }) else {
        return;
    };
    let (first, last) = (value_of("first") as u32, value_of("last") as u32);
    let (fps, bitrate) = (value_of("fps") as u32, value_of("mbps") * 1e6);
    let scale: f64 = select("scale").value().parse().unwrap_or(1.0);
    let supersample = select("ss").value().parse().unwrap_or(1u32);
    let (width, height) = (
        even8(f64::from(view.width()) * scale),
        even8(f64::from(view.height()) * scale),
    );
    // A film with a hole between two packs cannot be one contiguous mp4.
    let covered = crossing(&scene, &api, &project, first, last);
    let frames: u32 = covered.iter().map(|(_, a, b)| b - a + 1).sum();
    if last < first || frames != last - first + 1 {
        status(
            &format!(
                "Les frames {first}–{last} ne sont pas toutes dans un pack ({frames} sur {}).",
                (last + 1).saturating_sub(first)
            ),
            "bad",
        );
        return;
    }
    let parts = slice(first, last, (value_of("workers") as u32).max(1));
    let encoder = if chosen_is_soft() {
        "AV1 (rav1e)"
    } else {
        "H.264"
    };
    let within = scene.id.len() + 1;
    let job = Job {
        project: project.clone(),
        scene: scene.id.clone(),
        packs: scene
            .chunks
            .iter()
            .filter(|c| c.last >= first && c.first <= last)
            .map(|c| c.key[within..].to_string())
            .collect(),
    };
    let about = format!(
        "frames {first}–{last}, {width}×{height}, {} éch./pixel, {fps} images/s, {encoder}, {} workers",
        supersample * supersample,
        parts.len()
    );
    cite("job", "Rendu de ", &job, &format!(" — {about}"));
    show("job", true);
    page(|p| {
        p.job = Some(job.clone());
        p.rendering = true;
    });
    let go: web_sys::HtmlButtonElement = el("go").unchecked_into();
    go.set_disabled(true);
    show("stop", true);
    show("result", false);
    let progress: HtmlProgressElement = el("progress").unchecked_into();
    progress.set_value(0.0);
    progress.set_max(f64::from(frames));
    let previews = el("previews");
    previews.set_inner_html("");
    let contexts: Vec<(HtmlCanvasElement, CanvasRenderingContext2d)> = parts
        .iter()
        .map(|_| {
            let c: HtmlCanvasElement = create("canvas").unchecked_into();
            c.set_width(480);
            c.set_height((480.0 * f64::from(height) / f64::from(width)).round() as u32);
            let _ = previews.append_child(&c);
            let context = c
                .get_context("2d")
                .ok()
                .flatten()
                .unwrap_throw()
                .unchecked_into();
            (c, context)
        })
        .collect();

    let totals = Rc::new(RefCell::new(Totals::default()));
    // Per worker: blocks fetched ahead, of how many, and their bytes.
    let ahead = Rc::new(RefCell::new(vec![(0.0f64, 0.0f64, 0.0f64); parts.len()]));
    let started = now();
    status("Préchargement des blocs…", "");

    // Each worker with its two handlers, which live exactly as long as it.
    type Handled = (
        Worker,
        Closure<dyn FnMut(MessageEvent)>,
        Closure<dyn FnMut(Event)>,
    );
    let mut workers: Vec<Handled> = Vec::new();
    let mut waits = Vec::new();
    for (id, (a, b)) in parts.iter().copied().enumerate() {
        let options = WorkerOptions::new();
        options.set_type(WorkerType::Module);
        let worker = match Worker::new_with_options("./film-worker.js", &options) {
            Ok(w) => w,
            Err(e) => {
                status(&format!("worker {id} : {}", text(e)), "bad");
                // The workers already started have nothing to render for.
                for (started, ..) in &workers {
                    started.terminate();
                }
                page(|p| {
                    p.rendering = false;
                    p.stoppers.clear();
                });
                show("stop", false);
                go.set_disabled(false);
                return;
            }
        };
        let (tx, rx) = oneshot::channel::<Result<Slice, String>>();
        let tx: Stopper = Rc::new(RefCell::new(Some(tx)));
        page(|p| p.stoppers.push(tx.clone()));
        let (totals_in, ahead_in, tx_in) = (totals.clone(), ahead.clone(), tx.clone());
        let (context, progress_in) = (contexts[id].clone(), progress.clone());
        let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
            let data = event.data();
            match string(&data, "type").as_str() {
                "preload" => {
                    ahead_in.borrow_mut()[id] = (
                        number(&data, "done"),
                        number(&data, "total"),
                        number(&data, "bytes"),
                    );
                    if totals_in.borrow().frames == 0 {
                        let (done, total) = ahead_in
                            .borrow()
                            .iter()
                            .fold((0.0, 0.0), |(d, t), a| (d + a.0, t + a.1));
                        status(
                            &format!(
                                "Préchargement dans le cache du navigateur : {done:.0} blocs sur {total:.0} ({:.0} Mo sur {:.0})…",
                                done * BLOCK_MB,
                                total * BLOCK_MB
                            ),
                            "",
                        );
                    }
                }
                "frame" => {
                    let mut t = totals_in.borrow_mut();
                    if t.frames == 0 {
                        status("Rendu…", "");
                    }
                    t.fetch += number(&data, "fetch");
                    t.unpack += number(&data, "unpack");
                    t.decode += number(&data, "decode");
                    t.upload += number(&data, "upload");
                    t.record += number(&data, "record");
                    t.next += number(&data, "next");
                    t.encode += number(&data, "encode");
                    t.bytes += number(&data, "fetchedBytes");
                    t.requests += number(&data, "requests");
                    t.frames += 1;
                    progress_in.set_value(f64::from(t.frames));
                    if let Ok(bitmap) = get(&data, "preview").dyn_into::<ImageBitmap>() {
                        let (canvas, ctx) = &context;
                        let _ = ctx.draw_image_with_image_bitmap_and_dw_and_dh(
                            &bitmap,
                            0.0,
                            0.0,
                            f64::from(canvas.width()),
                            f64::from(canvas.height()),
                        );
                        bitmap.close();
                    }
                    if t.frames % 4 == 0 {
                        render_stats(&t, (now() - started) / 1000.0);
                    }
                }
                kind @ ("done" | "error") => {
                    if let Some(tx) = tx_in.borrow_mut().take() {
                        let _ = tx.send(match kind {
                            "done" => Ok(slice_of(&data)),
                            _ => Err(format!(
                                "worker {id} (frames {a}–{b}) : {}",
                                string(&data, "message")
                            )),
                        });
                    }
                }
                _ => {}
            }
        });
        worker.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        let tx_err = tx.clone();
        let on_error = Closure::<dyn FnMut(Event)>::new(move |event: Event| {
            if let Some(tx) = tx_err.borrow_mut().take() {
                let _ = tx.send(Err(format!("worker {id} : {}", string(&event, "message"))));
            }
        });
        worker.set_onerror(Some(on_error.as_ref().unchecked_ref()));
        let packs: Array = crossing(&scene, &api, &project, a, b)
            .into_iter()
            .map(|(source, first, last)| {
                JsValue::from(object(&[
                    ("source", source.into()),
                    ("first", first.into()),
                    ("last", last.into()),
                ]))
            })
            .collect();
        let order = object(&[
            ("id", (id as u32).into()),
            ("packs", packs.into()),
            ("filmFirst", first.into()),
            ("width", width.into()),
            ("height", height.into()),
            ("supersample", supersample.into()),
            ("fps", fps.into()),
            ("bitrate", bitrate.into()),
        ]);
        let _ = worker.post_message(&order);
        waits.push(async move {
            rx.await
                .unwrap_or_else(|_| Err(format!("worker {id} : interrompu")))
        });
        // The handlers live exactly as long as the worker they are on.
        workers.push((worker, on_message, on_error));
    }

    let results = try_join_all(waits).await;
    for (worker, ..) in &workers {
        worker.terminate();
    }
    drop(workers);
    let wall = (now() - started) / 1000.0;
    let finish = |go: &web_sys::HtmlButtonElement| {
        page(|p| {
            p.rendering = false;
            p.stoppers.clear();
        });
        show("stop", false);
        go.set_disabled(page(|p| p.view.is_none()));
    };
    let slices = match results {
        Ok(s) => s,
        Err(e) => {
            // A stop is not a failure, and is not shown as one.
            status(&e, if e == STOPPED { "" } else { "bad" });
            finish(&go);
            return;
        }
    };
    render_stats(&totals.borrow(), wall);
    let loaded: f64 = ahead.borrow().iter().map(|a| a.2).sum();

    match join(&slices, width, height, fps) {
        Ok((mp4, count)) => {
            let bag = BlobPropertyBag::new();
            bag.set_type("video/mp4");
            let parts = Array::of1(&Uint8Array::from(mp4.as_slice()));
            let url = Blob::new_with_u8_array_sequence_and_options(&parts, &bag)
                .ok()
                .and_then(|blob| Url::create_object_url_with_blob(&blob).ok())
                .unwrap_or_default();
            el("video")
                .unchecked_into::<HtmlMediaElement>()
                .set_src(&url);
            let download: HtmlAnchorElement = el("download").unchecked_into();
            download.set_href(&url);
            download.set_download(&format!(
                "{}-{}-{first}-{last}-{width}x{height}-ss{supersample}.mp4",
                job.project,
                job.scene.replace('/', "-")
            ));
            set_text(
                "summary",
                &format!(
                    "{count} images, {:.1} Mo, {}",
                    mp4.len() as f64 / 1e6,
                    slices[0].codec
                ),
            );
            // The film says what it is of: the pack, by a link that reopens it.
            cite(
                "video-source",
                "Généré depuis ",
                &job,
                &format!(" — {about}."),
            );
            show("result", true);
            status(
                &format!(
                    "Terminé : {count} images en {wall:.1} s ({:.1} images/s), {:.0} Mo préchargés.",
                    count as f64 / wall.max(1e-9),
                    loaded / 1e6
                ),
                "good",
            );
        }
        Err(e) => status(&format!("Assemblage refusé : {e}"), "bad"),
    }
    finish(&go);
}

// -------------------------------------------------------------------- start

/// Makes this document the bench: reads the address, wires the controls,
/// lists the projects.
#[wasm_bindgen]
pub fn start_page() {
    console_error_panic_hook::set_once();
    let q = query();
    let origin = window().location().origin().unwrap_or_default();
    page(|p| {
        // The API: this page's own origin, or ?api=.
        p.api = format!("{}/api", q.get("api").unwrap_or(origin));
        p.wanted = Wanted {
            scene: q.get("film"),
            pack: q.get("pack"),
            frame: q.get("frame").and_then(|f| f.parse().ok()),
        };
    });

    on(&el("cam-frame"), "change", |_| {
        spawn_local(select_frame(value_of("cam-frame")))
    });
    on(&el("track"), "click", |event| {
        let Some((x, y, _)) = click_at(&event) else {
            return;
        };
        let hit = page(|p| {
            let map = p.track?;
            p.path
                .iter()
                .min_by(|a, b| {
                    let d = |s: &CameraSample| (map.x(s.lon_deg) - x).hypot(map.y(s.lat_deg) - y);
                    d(a).total_cmp(&d(b))
                })
                .map(|s| s.frame)
        });
        if let Some(frame) = hit {
            spawn_local(select_frame(f64::from(frame)));
        }
    });
    on(&el("profile"), "click", |event| {
        let Some((x, _, canvas)) = click_at(&event) else {
            return;
        };
        let t = ((x - PAD) / (f64::from(canvas.width()) - 2.0 * PAD)).clamp(0.0, 1.0);
        let frame = page(|p| {
            let (a, b) = (p.path.first()?.frame, p.path.last()?.frame);
            Some(f64::from(a) + t * f64::from(b - a))
        });
        if let Some(frame) = frame {
            spawn_local(select_frame(frame));
        }
    });
    for id in ["layer", "tz"] {
        on(&el(id), "change", |_| spawn_local(show_source_tiles(None)));
    }
    on(&el("scale"), "change", |_| describe_encoder());
    on(&el("workers"), "input", |_| {
        page(|p| p.workers_typed = true)
    });
    on(&el("go"), "click", |_| spawn_local(render()));
    on(&el("stop"), "click", |_| stop());

    if get(&window().navigator(), "gpu").is_undefined() {
        status(
            "Ce navigateur n'expose pas WebGPU : consultation seule.",
            "bad",
        );
    }
    spawn_local(start());
}
