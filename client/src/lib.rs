use shared::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::f64::consts::TAU;
use std::rc::Rc;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{
    CanvasRenderingContext2d, Document, HtmlCanvasElement, HtmlElement, HtmlImageElement,
    KeyboardEvent, MessageEvent, MouseEvent, TouchEvent, WebSocket, WheelEvent, Window,
};

macro_rules! log {
    ($($t:tt)*) => { web_sys::console::log_1(&format!($($t)*).into()) };
}

// ---------------------------------------------------------------------------
// Territory ring (cached AABB + color)
// ---------------------------------------------------------------------------

struct TerritoryRing {
    color: [u8; 3],
    color_str: String,
    sprite_id: u32,
    points: Vec<Position>,
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
}

impl TerritoryRing {
    fn from_data(d: TerritoryRingData) -> Self {
        let (mut mnx, mut mny) = (f64::MAX, f64::MAX);
        let (mut mxx, mut mxy) = (f64::MIN, f64::MIN);
        for p in &d.points {
            mnx = mnx.min(p.x);
            mny = mny.min(p.y);
            mxx = mxx.max(p.x);
            mxy = mxy.max(p.y);
        }
        TerritoryRing {
            color_str: format!("rgb({},{},{})", d.color[0], d.color[1], d.color[2]),
            color: d.color,
            sprite_id: d.sprite_id,
            points: d.points,
            min_x: mnx,
            min_y: mny,
            max_x: mxx,
            max_y: mxy,
        }
    }
}

// ---------------------------------------------------------------------------
// Remote player
// ---------------------------------------------------------------------------

struct RemotePlayer {
    position: Position,
    angle: f64,
    color: [u8; 3],
    color_fill: String,
    color_trail: String,
    trail: Vec<Position>,
    server_time: f64,
    sprite_id: u32,
    has_crown: bool,
    boost_points: u8,
    boost_active: bool,
}

impl RemotePlayer {
    fn new(pos: Position, angle: f64, color: [u8; 3], sprite_id: u32, now: f64) -> Self {
        RemotePlayer {
            position: pos,
            angle,
            color,
            color_fill: format!("rgb({},{},{})", color[0], color[1], color[2]),
            color_trail: format!("rgba({},{},{},0.8)", color[0], color[1], color[2]),
            trail: Vec::new(),
            server_time: now,
            sprite_id,
            has_crown: false,
            boost_points: 0,
            boost_active: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Game state
// ---------------------------------------------------------------------------

struct GameState {
    player_id: Option<PlayerId>,
    alive: bool,
    connected: bool,

    players: HashMap<PlayerId, RemotePlayer>,
    territory: Vec<TerritoryRing>,

    canvas: HtmlCanvasElement,
    ctx: CanvasRenderingContext2d,
    width: f64,
    height: f64,

    ws: Option<WebSocket>,
    last_tick_time: f64,

    zoom: f64,
    touching: bool,
    mouse_down: bool,
    pinch_dist: Option<f64>,
    pinch_zoom_start: f64,

    /// SVG sprite data (loaded from sprites.json)
    sprites: Vec<String>,
    /// Cached HtmlImageElement per sprite_id
    sprite_images: HashMap<u32, HtmlImageElement>,
    /// Our chosen sprite index
    my_sprite_id: u32,
    /// Total sprites available
    sprite_count: u32,
    /// Player name
    my_name: String,
    /// Leaderboard HTML elements
    lb_area: HtmlElement,
    lb_kills: HtmlElement,
    /// Reusable territory color-batching map (cleared each frame, avoids alloc)
    territory_groups: HashMap<[u8; 3], Vec<usize>>,
    /// Cached territory tile patterns: (sprite_id, color) → CanvasPattern
    territory_patterns: HashMap<(u32, [u8; 3]), web_sys::CanvasPattern>,
    /// Boost display element
    boost_div: HtmlElement,
    /// Current board radius from server
    board_radius: f64,
    /// Minimap canvas (held to keep DOM element alive)
    #[allow(dead_code)]
    minimap: HtmlCanvasElement,
    minimap_ctx: CanvasRenderingContext2d,

    // --- Performance: throttle direction messages ---
    last_direction_sent: f64,

    // --- Performance: throttle minimap rendering ---
    last_minimap_time: f64,

    // --- Performance: cache coordinate string ---
    last_coord_str: String,
    last_coord_pos: (f64, f64),

    // --- Performance: cache leaderboard HTML ---
    last_lb_area_html: String,
    last_lb_kills_html: String,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
    log!("matrix.io client starting");

    let window = web_sys::window().unwrap();
    let document = window.document().unwrap();
    let canvas = create_canvas(&document);
    let ctx = canvas
        .get_context("2d")
        .unwrap()
        .unwrap()
        .dyn_into::<CanvasRenderingContext2d>()
        .unwrap();

    let width = window.inner_width().unwrap().as_f64().unwrap();
    let height = window.inner_height().unwrap().as_f64().unwrap();
    canvas.set_width(width as u32);
    canvas.set_height(height as u32);

    let (lb_area, lb_kills) = create_leaderboards(&document);
    let boost_div = create_boost_display(&document);
    let (minimap, minimap_ctx) = create_minimap(&document);

    // Load name from localStorage
    let my_name = web_sys::window()
        .unwrap()
        .local_storage()
        .ok()
        .flatten()
        .and_then(|s| s.get_item("player_name").ok().flatten())
        .unwrap_or_default();

    let state = Rc::new(RefCell::new(GameState {
        player_id: None,
        alive: false,
        connected: false,
        players: HashMap::new(),
        territory: Vec::new(),
        canvas,
        ctx,
        width,
        height,
        ws: None,
        last_tick_time: 0.0,
        zoom: 1.0,
        touching: false,
        mouse_down: false,
        pinch_dist: None,
        pinch_zoom_start: 1.0,
        sprites: Vec::new(),
        sprite_images: HashMap::new(),
        my_sprite_id: 0,
        sprite_count: 0,
        my_name,
        lb_area,
        lb_kills,
        territory_groups: HashMap::new(),
        territory_patterns: HashMap::new(),
        boost_div,
        board_radius: 50.0,
        minimap,
        minimap_ctx,
        last_direction_sent: 0.0,
        last_minimap_time: 0.0,
        last_coord_str: String::new(),
        last_coord_pos: (f64::MAX, f64::MAX),
        last_lb_area_html: String::new(),
        last_lb_kills_html: String::new(),
    }));

    setup_resize(state.clone(), &window);
    setup_keyboard(state.clone(), &window);
    setup_touch(state.clone(), &window);
    setup_mouse(state.clone(), &window);
    setup_wheel(state.clone(), &window);
    create_hamburger_menu(state.clone(), &document);
    setup_boost_clicks(state.clone());
    load_sprites(state.clone());
    start_render_loop(state, &window);
}

fn create_canvas(document: &Document) -> HtmlCanvasElement {
    let canvas = document
        .create_element("canvas")
        .unwrap()
        .dyn_into::<HtmlCanvasElement>()
        .unwrap();
    let style = canvas.style();
    style.set_property("position", "fixed").unwrap();
    style.set_property("top", "0").unwrap();
    style.set_property("left", "0").unwrap();
    style.set_property("width", "100%").unwrap();
    style.set_property("height", "100%").unwrap();
    style.set_property("background", "#ffffff").unwrap();
    style.set_property("touch-action", "none").unwrap();

    let body = document.body().unwrap();
    body.style().set_property("margin", "0").unwrap();
    body.style().set_property("overflow", "hidden").unwrap();
    body.append_child(&canvas).unwrap();
    canvas
}

// ---------------------------------------------------------------------------
// Leaderboards
// ---------------------------------------------------------------------------

fn create_leaderboards(document: &Document) -> (HtmlElement, HtmlElement) {
    // Container for both lists, stacked vertically
    let container = document.create_element("div").unwrap().dyn_into::<HtmlElement>().unwrap();
    let cs = container.style();
    cs.set_property("position", "fixed").unwrap();
    cs.set_property("top", "12px").unwrap();
    cs.set_property("left", "12px").unwrap();
    cs.set_property("z-index", "50").unwrap();
    cs.set_property("pointer-events", "none").unwrap();

    let make_panel = |doc: &Document| -> HtmlElement {
        let div = doc.create_element("div").unwrap().dyn_into::<HtmlElement>().unwrap();
        let s = div.style();
        s.set_property("background", "rgba(0,0,0,0.5)").unwrap();
        s.set_property("color", "white").unwrap();
        s.set_property("padding", "8px 12px").unwrap();
        s.set_property("border-radius", "6px").unwrap();
        s.set_property("font-family", "monospace").unwrap();
        s.set_property("font-size", "12px").unwrap();
        s.set_property("min-width", "120px").unwrap();
        s.set_property("white-space", "pre").unwrap();
        s.set_property("margin-bottom", "6px").unwrap();
        div
    };

    let lb_area = make_panel(document);
    let lb_kills = make_panel(document);

    container.append_child(&lb_area).unwrap();
    container.append_child(&lb_kills).unwrap();
    document.body().unwrap().append_child(&container).unwrap();

    (lb_area, lb_kills)
}

fn create_minimap(document: &Document) -> (HtmlCanvasElement, CanvasRenderingContext2d) {
    let size = 150u32;
    let canvas = document.create_element("canvas").unwrap().dyn_into::<HtmlCanvasElement>().unwrap();
    canvas.set_width(size);
    canvas.set_height(size);
    let s = canvas.style();
    s.set_property("position", "fixed").unwrap();
    s.set_property("bottom", "12px").unwrap();
    s.set_property("left", "12px").unwrap();
    s.set_property("width", &format!("{}px", size)).unwrap();
    s.set_property("height", &format!("{}px", size)).unwrap();
    s.set_property("border-radius", "50%").unwrap();
    s.set_property("z-index", "50").unwrap();
    s.set_property("pointer-events", "none").unwrap();
    s.set_property("opacity", "0.4").unwrap();
    document.body().unwrap().append_child(&canvas).unwrap();
    let ctx = canvas.get_context("2d").unwrap().unwrap().dyn_into::<CanvasRenderingContext2d>().unwrap();
    (canvas, ctx)
}

fn create_boost_display(document: &Document) -> HtmlElement {
    let div = document.create_element("div").unwrap().dyn_into::<HtmlElement>().unwrap();
    let s = div.style();
    s.set_property("position", "fixed").unwrap();
    s.set_property("bottom", "12px").unwrap();
    s.set_property("right", "12px").unwrap();
    s.set_property("font-size", "36px").unwrap();
    s.set_property("z-index", "60").unwrap();
    s.set_property("cursor", "pointer").unwrap();
    s.set_property("user-select", "none").unwrap();
    s.set_property("touch-action", "auto").unwrap();
    document.body().unwrap().append_child(&div).unwrap();

    // Stop touch events from reaching the game
    let el: web_sys::EventTarget = div.clone().into();
    for evt in &["touchstart", "touchmove", "touchend"] {
        let cb = Closure::<dyn FnMut(TouchEvent)>::new(move |e: TouchEvent| {
            e.stop_propagation();
        });
        el.add_event_listener_with_callback(evt, cb.as_ref().unchecked_ref()).unwrap();
        cb.forget();
    }

    div
}

// ---------------------------------------------------------------------------
// Hamburger menu
// ---------------------------------------------------------------------------

fn create_hamburger_menu(state: Rc<RefCell<GameState>>, document: &Document) {
    // Menu button
    let btn = document.create_element("div").unwrap();
    let btn = btn.dyn_into::<HtmlElement>().unwrap();
    btn.set_inner_html("&#9776;");
    let bs = btn.style();
    bs.set_property("position", "fixed").unwrap();
    bs.set_property("top", "12px").unwrap();
    bs.set_property("right", "12px").unwrap();
    bs.set_property("font-size", "28px").unwrap();
    bs.set_property("color", "black").unwrap();
    bs.set_property("cursor", "pointer").unwrap();
    bs.set_property("z-index", "100").unwrap();
    bs.set_property("user-select", "none").unwrap();
    bs.set_property("touch-action", "auto").unwrap();
    bs.set_property("padding", "8px 12px").unwrap();
    document.body().unwrap().append_child(&btn).unwrap();

    // Dropdown panel (hidden)
    let panel = document.create_element("div").unwrap().dyn_into::<HtmlElement>().unwrap();
    let ps = panel.style();
    ps.set_property("position", "fixed").unwrap();
    ps.set_property("top", "48px").unwrap();
    ps.set_property("right", "12px").unwrap();
    ps.set_property("background", "rgba(0,0,0,0.85)").unwrap();
    ps.set_property("color", "white").unwrap();
    ps.set_property("padding", "0").unwrap();
    ps.set_property("border-radius", "6px").unwrap();
    ps.set_property("font-family", "monospace").unwrap();
    ps.set_property("font-size", "14px").unwrap();
    ps.set_property("z-index", "100").unwrap();
    ps.set_property("display", "none").unwrap();
    ps.set_property("user-select", "none").unwrap();
    ps.set_property("touch-action", "auto").unwrap();
    ps.set_property("overflow", "hidden").unwrap();

    // Menu items
    let item_style = "padding:10px 16px;cursor:pointer;border-bottom:1px solid rgba(255,255,255,0.15)";
    let item_style_last = "padding:10px 16px;cursor:pointer";

    let item_sprite = document.create_element("div").unwrap().dyn_into::<HtmlElement>().unwrap();
    item_sprite.set_inner_html("Regenerate sprite");
    item_sprite.set_attribute("style", item_style).unwrap();
    panel.append_child(&item_sprite).unwrap();

    let item_name = document.create_element("div").unwrap().dyn_into::<HtmlElement>().unwrap();
    item_name.set_inner_html("Set name");
    item_name.set_attribute("style", item_style_last).unwrap();
    panel.append_child(&item_name).unwrap();

    document.body().unwrap().append_child(&panel).unwrap();

    // Stop touch events from reaching the game's window-level handlers
    for el in [btn.clone().into(), panel.clone().into()] {
        let el: web_sys::EventTarget = el;
        for evt in &["touchstart", "touchmove", "touchend"] {
            let cb = Closure::<dyn FnMut(TouchEvent)>::new(move |e: TouchEvent| {
                e.stop_propagation();
            });
            el.add_event_listener_with_callback(evt, cb.as_ref().unchecked_ref())
                .unwrap();
            cb.forget();
        }
    }

    // Toggle panel on button tap/click
    {
        let panel = panel.clone();
        let cb = Closure::<dyn FnMut(MouseEvent)>::new(move |e: MouseEvent| {
            e.stop_propagation();
            let d = panel.style().get_property_value("display").unwrap();
            panel.style().set_property("display", if d == "none" { "block" } else { "none" }).unwrap();
        });
        btn.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref()).unwrap();
        cb.forget();
    }

    // Regenerate sprite
    {
        let state = state.clone();
        let panel = panel.clone();
        let cb = Closure::<dyn FnMut(MouseEvent)>::new(move |e: MouseEvent| {
            e.stop_propagation();
            let mut s = state.borrow_mut();
            if s.sprite_count > 0 {
                let new_id = (js_sys::Math::random() * s.sprite_count as f64) as u32;
                s.my_sprite_id = new_id;
                if let Ok(Some(storage)) = web_sys::window().unwrap().local_storage() {
                    let _ = storage.set_item("sprite_id", &new_id.to_string());
                }
                send_sprite(&s.ws, new_id);
            }
            panel.style().set_property("display", "none").unwrap();
        });
        item_sprite.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref()).unwrap();
        cb.forget();
    }

    // Set name
    {
        let panel = panel.clone();
        let cb = Closure::<dyn FnMut(MouseEvent)>::new(move |e: MouseEvent| {
            e.stop_propagation();
            panel.style().set_property("display", "none").unwrap();
            let win = web_sys::window().unwrap();
            if let Ok(Some(name)) = win.prompt_with_message("Enter your name:") {
                let name = name.trim().to_string();
                if !name.is_empty() {
                    let mut s = state.borrow_mut();
                    s.my_name = name.clone();
                    if let Ok(Some(storage)) = win.local_storage() {
                        let _ = storage.set_item("player_name", &name);
                    }
                    send_name(&s.ws, &name);
                }
            }
        });
        item_name.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref()).unwrap();
        cb.forget();
    }
}

// ---------------------------------------------------------------------------
// Sprite loading
// ---------------------------------------------------------------------------

fn load_sprites(state: Rc<RefCell<GameState>>) {
    let req = web_sys::XmlHttpRequest::new().unwrap();
    req.open_with_async("GET", "sprites.json", true).unwrap();

    let state2 = state.clone();
    let req2 = req.clone();
    let cb = Closure::<dyn FnMut()>::new(move || {
        if req2.status().unwrap() == 200 {
            if let Ok(text) = req2.response_text() {
                if let Some(text) = text {
                    // Parse JSON array of SVG strings via js_sys
                    let val = js_sys::JSON::parse(&text).unwrap();
                    let arr = js_sys::Array::from(&val);
                    let len = arr.length();

                    let mut s = state2.borrow_mut();
                    s.sprite_count = len;
                    s.sprites = (0..len)
                        .map(|i| arr.get(i).as_string().unwrap_or_default())
                        .collect();

                    // Pick sprite from localStorage or random
                    let stored = web_sys::window()
                        .unwrap()
                        .local_storage()
                        .ok()
                        .flatten()
                        .and_then(|st| st.get_item("sprite_id").ok().flatten())
                        .and_then(|v| v.parse::<u32>().ok());

                    let id = match stored {
                        Some(id) if id < len => id,
                        _ => {
                            let id = (js_sys::Math::random() * len as f64) as u32;
                            if let Ok(Some(storage)) =
                                web_sys::window().unwrap().local_storage()
                            {
                                let _ = storage.set_item("sprite_id", &id.to_string());
                            }
                            id
                        }
                    };
                    s.my_sprite_id = id;
                    log!("Loaded {} sprites, using #{}", len, id);

                    // Connect now that sprites are loaded
                    drop(s);
                    connect_ws(state2.clone(), &web_sys::window().unwrap());
                }
            }
        }
    });
    req.set_onload(Some(cb.as_ref().unchecked_ref()));
    cb.forget();
    req.send().unwrap();
}

/// Get or create a cached HtmlImageElement for a sprite.
fn get_sprite_image(s: &mut GameState, sprite_id: u32) -> Option<HtmlImageElement> {
    if let Some(img) = s.sprite_images.get(&sprite_id) {
        return Some(img.clone());
    }
    let svg = s.sprites.get(sprite_id as usize)?;
    let img = HtmlImageElement::new().ok()?;
    let data_uri = format!(
        "data:image/svg+xml;base64,{}",
        base64_encode(svg.as_bytes())
    );
    img.set_src(&data_uri);
    s.sprite_images.insert(sprite_id, img.clone());
    Some(img)
}

fn base64_encode(data: &[u8]) -> String {
    let js_str = js_sys::JsString::from(
        std::str::from_utf8(data).unwrap_or(""),
    );
    let encoded = web_sys::window()
        .unwrap()
        .btoa(&js_str.as_string().unwrap())
        .unwrap_or_default();
    encoded
}

/// Create a tiled territory pattern: semi-transparent sprite tinted with the player's color.
fn get_territory_pattern(
    s: &mut GameState,
    sprite_id: u32,
    color: [u8; 3],
) -> Option<web_sys::CanvasPattern> {
    let key = (sprite_id, color);
    if let Some(pat) = s.territory_patterns.get(&key) {
        return Some(pat.clone());
    }

    let img = s.sprite_images.get(&sprite_id)?;
    if !img.complete() || img.natural_width() == 0 {
        return None;
    }

    // Draw the sprite onto a small offscreen canvas with color tint + transparency
    let doc = web_sys::window().unwrap().document().unwrap();
    let tile_size = 48u32;
    let offscreen = doc
        .create_element("canvas")
        .ok()?
        .dyn_into::<HtmlCanvasElement>()
        .ok()?;
    offscreen.set_width(tile_size);
    offscreen.set_height(tile_size);
    let octx = offscreen
        .get_context("2d")
        .ok()??
        .dyn_into::<CanvasRenderingContext2d>()
        .ok()?;

    // Fill with semi-transparent player color
    octx.set_global_alpha(1.0);
    octx.set_fill_style_str(&format!("rgb({},{},{})", color[0], color[1], color[2]));
    octx.fill_rect(0.0, 0.0, tile_size as f64, tile_size as f64);

    // Draw sprite on top, lighter overlay
    octx.set_global_alpha(0.2);
    let _ = octx.draw_image_with_html_image_element_and_dw_and_dh(
        img,
        2.0,
        2.0,
        (tile_size - 4) as f64,
        (tile_size - 4) as f64,
    );

    // Create repeating pattern
    let pattern = s
        .ctx
        .create_pattern_with_html_canvas_element(&offscreen, "repeat")
        .ok()?;

    if let Some(ref pat) = pattern {
        s.territory_patterns.insert(key, pat.clone());
    }

    pattern
}

// ---------------------------------------------------------------------------
// Resize
// ---------------------------------------------------------------------------

fn setup_resize(state: Rc<RefCell<GameState>>, window: &Window) {
    let cb = Closure::<dyn FnMut()>::new(move || {
        let w = web_sys::window().unwrap();
        let width = w.inner_width().unwrap().as_f64().unwrap();
        let height = w.inner_height().unwrap().as_f64().unwrap();
        let mut s = state.borrow_mut();
        s.canvas.set_width(width as u32);
        s.canvas.set_height(height as u32);
        s.width = width;
        s.height = height;
    });
    window
        .add_event_listener_with_callback("resize", cb.as_ref().unchecked_ref())
        .unwrap();
    cb.forget();
}

// ---------------------------------------------------------------------------
// Keyboard — hold to move, release to stop
// ---------------------------------------------------------------------------

fn is_movement_key(key: &str) -> bool {
    matches!(
        key,
        "ArrowRight" | "ArrowDown" | "ArrowLeft" | "ArrowUp"
            | "d" | "D" | "s" | "S" | "a" | "A" | "w" | "W"
    )
}

fn setup_keyboard(state: Rc<RefCell<GameState>>, window: &Window) {
    use std::f64::consts::{FRAC_PI_2, PI};
    {
        let state = state.clone();
        let cb = Closure::<dyn FnMut(KeyboardEvent)>::new(move |e: KeyboardEvent| {
            let angle: Option<f64> = match e.key().as_str() {
                "ArrowRight" | "d" | "D" => Some(0.0),
                "ArrowDown" | "s" | "S" => Some(FRAC_PI_2),
                "ArrowLeft" | "a" | "A" => Some(PI),
                "ArrowUp" | "w" | "W" => Some(-FRAC_PI_2),
                _ => None,
            };
            if let Some(a) = angle {
                e.prevent_default();
                send_angle(&mut state.borrow_mut(), a);
            }
        });
        window
            .add_event_listener_with_callback("keydown", cb.as_ref().unchecked_ref())
            .unwrap();
        cb.forget();
    }
    {
        let cb = Closure::<dyn FnMut(KeyboardEvent)>::new(move |e: KeyboardEvent| {
            if is_movement_key(&e.key()) {
                send_angle(&mut state.borrow_mut(), f64::NAN);
            }
        });
        window
            .add_event_listener_with_callback("keyup", cb.as_ref().unchecked_ref())
            .unwrap();
        cb.forget();
    }
}

// ---------------------------------------------------------------------------
// Touch — move while touching, stop on release, pinch to zoom
// ---------------------------------------------------------------------------

fn touch_dist(e: &TouchEvent) -> Option<f64> {
    let t = e.touches();
    if t.length() < 2 { return None; }
    let a = t.get(0)?;
    let b = t.get(1)?;
    let dx = (a.client_x() - b.client_x()) as f64;
    let dy = (a.client_y() - b.client_y()) as f64;
    Some((dx * dx + dy * dy).sqrt())
}

fn angle_from_touch(e: &TouchEvent, s: &GameState) -> f64 {
    let t = e.touches().get(0).unwrap();
    let dx = t.client_x() as f64 - s.width / 2.0;
    let dy = t.client_y() as f64 - s.height / 2.0;
    dy.atan2(dx)
}

fn setup_touch(state: Rc<RefCell<GameState>>, window: &Window) {
    {
        let state = state.clone();
        let cb = Closure::<dyn FnMut(TouchEvent)>::new(move |e: TouchEvent| {
            e.prevent_default();
            let n = e.touches().length();
            if n == 1 {
                let mut s = state.borrow_mut();
                if s.pinch_dist.is_none() {
                    s.touching = true;
                    let a = angle_from_touch(&e, &s);
                    send_angle(&mut s, a);
                }
            } else if n >= 2 {
                let mut s = state.borrow_mut();
                if s.touching { send_angle(&mut s, f64::NAN); }
                s.touching = false;
                if let Some(d) = touch_dist(&e) {
                    s.pinch_dist = Some(d);
                    s.pinch_zoom_start = s.zoom;
                }
            }
        });
        window.add_event_listener_with_callback("touchstart", cb.as_ref().unchecked_ref()).unwrap();
        cb.forget();
    }
    {
        let state = state.clone();
        let cb = Closure::<dyn FnMut(TouchEvent)>::new(move |e: TouchEvent| {
            e.prevent_default();
            let n = e.touches().length();
            if n == 1 {
                let mut s = state.borrow_mut();
                if s.touching {
                    let a = angle_from_touch(&e, &s);
                    send_angle(&mut s, a);
                }
            } else if n >= 2 {
                if let Some(nd) = touch_dist(&e) {
                    let mut s = state.borrow_mut();
                    if let Some(sd) = s.pinch_dist {
                        s.zoom = (s.pinch_zoom_start * nd / sd).clamp(0.15, 5.0);
                    }
                }
            }
        });
        window.add_event_listener_with_callback("touchmove", cb.as_ref().unchecked_ref()).unwrap();
        cb.forget();
    }
    {
        let state = state.clone();
        let cb = Closure::<dyn FnMut(TouchEvent)>::new(move |e: TouchEvent| {
            e.prevent_default();
            let n = e.touches().length();
            if n == 0 {
                let mut s = state.borrow_mut();
                send_angle(&mut s, f64::NAN);
                s.touching = false;
                s.pinch_dist = None;
            } else if n == 1 {
                let mut s = state.borrow_mut();
                send_angle(&mut s, f64::NAN);
                s.pinch_dist = None;
                s.touching = false;
            }
        });
        window.add_event_listener_with_callback("touchend", cb.as_ref().unchecked_ref()).unwrap();
        cb.forget();
    }
}

// ---------------------------------------------------------------------------
// Mouse — hold to move, release to stop
// ---------------------------------------------------------------------------

fn setup_mouse(state: Rc<RefCell<GameState>>, window: &Window) {
    {
        let state = state.clone();
        let cb = Closure::<dyn FnMut(MouseEvent)>::new(move |e: MouseEvent| {
            if e.button() != 0 { return; }
            e.prevent_default();
            let mut s = state.borrow_mut();
            s.mouse_down = true;
            let a = (e.client_y() as f64 - s.height / 2.0)
                .atan2(e.client_x() as f64 - s.width / 2.0);
            send_angle(&mut s, a);
        });
        window.add_event_listener_with_callback("mousedown", cb.as_ref().unchecked_ref()).unwrap();
        cb.forget();
    }
    {
        let state = state.clone();
        let cb = Closure::<dyn FnMut(MouseEvent)>::new(move |e: MouseEvent| {
            let mut s = state.borrow_mut();
            if s.mouse_down {
                let a = (e.client_y() as f64 - s.height / 2.0)
                    .atan2(e.client_x() as f64 - s.width / 2.0);
                send_angle(&mut s, a);
            }
        });
        window.add_event_listener_with_callback("mousemove", cb.as_ref().unchecked_ref()).unwrap();
        cb.forget();
    }
    {
        let state = state.clone();
        let cb = Closure::<dyn FnMut(MouseEvent)>::new(move |e: MouseEvent| {
            if e.button() != 0 { return; }
            let mut s = state.borrow_mut();
            if s.mouse_down {
                send_angle(&mut s, f64::NAN);
                s.mouse_down = false;
            }
        });
        window.add_event_listener_with_callback("mouseup", cb.as_ref().unchecked_ref()).unwrap();
        cb.forget();
    }
}

// ---------------------------------------------------------------------------
// Mouse wheel zoom
// ---------------------------------------------------------------------------

fn setup_wheel(state: Rc<RefCell<GameState>>, window: &Window) {
    let cb = Closure::<dyn FnMut(WheelEvent)>::new(move |e: WheelEvent| {
        e.prevent_default();
        let f = if e.delta_y() > 0.0 { 0.9 } else { 1.1 };
        let mut s = state.borrow_mut();
        s.zoom = (s.zoom * f).clamp(0.15, 5.0);
    });
    window.add_event_listener_with_callback("wheel", cb.as_ref().unchecked_ref()).unwrap();
    cb.forget();
}

// ---------------------------------------------------------------------------
// Send helpers
// ---------------------------------------------------------------------------

fn send_angle(s: &mut GameState, angle: f64) {
    // Always send NAN (stop) immediately; throttle direction msgs to 20/sec
    if !angle.is_nan() {
        let now = now_ms();
        if now - s.last_direction_sent < 50.0 {
            return;
        }
        s.last_direction_sent = now;
    }
    send_angle_raw(&s.ws, angle);
}

fn send_angle_raw(ws: &Option<WebSocket>, angle: f64) {
    if let Some(ws) = ws {
        let bytes = encode_client_msg(&ClientMsg::ChangeDirection(angle));
        let arr = js_sys::Uint8Array::from(bytes.as_slice());
        let _ = ws.send_with_array_buffer_view(&arr);
    }
}

fn send_sprite(ws: &Option<WebSocket>, sprite_id: u32) {
    if let Some(ws) = ws {
        let bytes = encode_client_msg(&ClientMsg::SetSprite(sprite_id));
        let arr = js_sys::Uint8Array::from(bytes.as_slice());
        let _ = ws.send_with_array_buffer_view(&arr);
    }
}

fn send_name(ws: &Option<WebSocket>, name: &str) {
    if let Some(ws) = ws {
        let bytes = encode_client_msg(&ClientMsg::SetName(name.to_string()));
        let arr = js_sys::Uint8Array::from(bytes.as_slice());
        let _ = ws.send_with_array_buffer_view(&arr);
    }
}

fn send_boost(ws: &Option<WebSocket>) {
    if let Some(ws) = ws {
        let bytes = encode_client_msg(&ClientMsg::ActivateBoost);
        let arr = js_sys::Uint8Array::from(bytes.as_slice());
        let _ = ws.send_with_array_buffer_view(&arr);
    }
}

fn setup_boost_clicks(state: Rc<RefCell<GameState>>) {
    let s = state.borrow();
    let div = s.boost_div.clone();
    drop(s);

    // Use mousedown (not click) so it works while already holding the mouse
    {
        let state = state.clone();
        let cb = Closure::<dyn FnMut(MouseEvent)>::new(move |e: MouseEvent| {
            e.stop_propagation();
            e.prevent_default();
            send_boost(&state.borrow().ws);
        });
        div.add_event_listener_with_callback("mousedown", cb.as_ref().unchecked_ref())
            .unwrap();
        cb.forget();
    }
    // Also handle touchstart for mobile (in addition to the stopPropagation already set)
    {
        let cb = Closure::<dyn FnMut(TouchEvent)>::new(move |e: TouchEvent| {
            e.stop_propagation();
            e.prevent_default();
            send_boost(&state.borrow().ws);
        });
        div.add_event_listener_with_callback("touchstart", cb.as_ref().unchecked_ref())
            .unwrap();
        cb.forget();
    }
}

// ---------------------------------------------------------------------------
// WebSocket
// ---------------------------------------------------------------------------

fn connect_ws(state: Rc<RefCell<GameState>>, window: &Window) {
    let loc = window.location();
    let proto = if loc.protocol().unwrap() == "https:" { "wss:" } else { "ws:" };
    let url = format!("{}//{}/ws", proto, loc.host().unwrap());
    log!("Connecting to {}", url);

    let ws = WebSocket::new(&url).unwrap();
    ws.set_binary_type(web_sys::BinaryType::Arraybuffer);

    {
        let st = state.clone();
        let cb = Closure::<dyn FnMut()>::new(move || {
            let s = st.borrow();
            s.connected; // just to touch it
            drop(s);
            let mut s = st.borrow_mut();
            s.connected = true;
            // Send our sprite choice and name
            let sid = s.my_sprite_id;
            send_sprite(&s.ws, sid);
            if !s.my_name.is_empty() {
                let name = s.my_name.clone();
                send_name(&s.ws, &name);
            }
        });
        ws.set_onopen(Some(cb.as_ref().unchecked_ref()));
        cb.forget();
    }
    {
        let st = state.clone();
        let cb = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
            if let Ok(buf) = e.data().dyn_into::<js_sys::ArrayBuffer>() {
                let arr = js_sys::Uint8Array::new(&buf);
                handle_msg(&st, &arr.to_vec());
            }
        });
        ws.set_onmessage(Some(cb.as_ref().unchecked_ref()));
        cb.forget();
    }
    {
        let cb = Closure::<dyn FnMut()>::new(|| log!("WebSocket error"));
        ws.set_onerror(Some(cb.as_ref().unchecked_ref()));
        cb.forget();
    }
    {
        let st = state.clone();
        let cb = Closure::<dyn FnMut()>::new(move || {
            {
                let mut s = st.borrow_mut();
                s.connected = false;
                s.alive = false;
                s.player_id = None;
                s.ws = None;
            }
            let st2 = st.clone();
            let w = web_sys::window().unwrap();
            let rc = Closure::<dyn FnMut()>::once(move || {
                connect_ws(st2, &web_sys::window().unwrap());
            });
            w.set_timeout_with_callback_and_timeout_and_arguments_0(
                rc.as_ref().unchecked_ref(), 1000,
            ).unwrap();
            rc.forget();
        });
        ws.set_onclose(Some(cb.as_ref().unchecked_ref()));
        cb.forget();
    }

    state.borrow_mut().ws = Some(ws);
}

fn handle_msg(state: &Rc<RefCell<GameState>>, bytes: &[u8]) {
    let msg = match decode_server_msg(bytes) {
        Ok(m) => m,
        Err(e) => { log!("decode err: {:?}", e); return; }
    };
    let mut s = state.borrow_mut();
    let now = now_ms();

    match msg {
        ServerMsg::Welcome { player_id, position, angle, color } => {
            s.player_id = Some(player_id);
            s.alive = true;
            s.territory.clear();
            s.players.clear();
            let sid = s.my_sprite_id;
            s.players.insert(player_id, RemotePlayer::new(position, angle, color, sid, now));
        }
        ServerMsg::Tick { players, board_radius } => {
            s.board_radius = board_radius;
            s.last_tick_time = now;
            let ids: std::collections::HashSet<PlayerId> = players.iter().map(|p| p.id).collect();
            for ps in players {
                let e = s.players.entry(ps.id).or_insert_with(|| {
                    RemotePlayer::new(ps.position, ps.angle, ps.color, ps.sprite_id, now)
                });
                e.position = ps.position;
                e.angle = ps.angle;
                e.trail = ps.trail;
                e.server_time = now;
                e.sprite_id = ps.sprite_id;
                e.has_crown = ps.has_crown;
                e.boost_points = ps.boost_points;
                e.boost_active = ps.boost_active;
                if e.color != ps.color {
                    e.color = ps.color;
                    e.color_fill = format!("rgb({},{},{})", ps.color[0], ps.color[1], ps.color[2]);
                    e.color_trail = format!("rgba({},{},{},0.8)", ps.color[0], ps.color[1], ps.color[2]);
                }
            }
            if let Some(my) = s.player_id {
                s.players.retain(|id, _| ids.contains(id) || *id == my);
                // Update boost display for local player
                if let Some(me) = s.players.get(&my) {
                    let mut html = String::new();
                    for _ in 0..me.boost_points {
                        html.push_str("\u{26A1}");
                    }
                    if me.boost_active {
                        html.push_str(" \u{1F525}"); // fire = boost active
                    }
                    s.boost_div.set_inner_html(&html);
                }
            }
        }
        ServerMsg::TerritorySnapshot(rings) => {
            s.territory = rings.into_iter().map(TerritoryRing::from_data).collect();
        }
        ServerMsg::PlayerKilled { player_id, .. } => {
            if Some(player_id) == s.player_id { s.alive = false; }
            s.players.remove(&player_id);
        }
        ServerMsg::Pong(_) => {}
        ServerMsg::Leaderboard(lb) => {
            // Update area leaderboard — only touch DOM if content changed
            let mut html = String::from("<b>Top Area</b>\n");
            for (i, e) in lb.by_area.iter().enumerate() {
                html.push_str(&format!(
                    "{}. {} ({})\n",
                    i + 1,
                    if e.name.is_empty() { "???" } else { &e.name },
                    e.value
                ));
            }
            if lb.by_area.is_empty() {
                html.push_str("  ---\n");
            }
            if html != s.last_lb_area_html {
                s.lb_area.set_inner_html(&html);
                s.last_lb_area_html = html;
            }

            // Update kills leaderboard — only touch DOM if content changed
            let mut html = String::from("<b>Top Kills</b>\n");
            for (i, e) in lb.by_kills.iter().enumerate() {
                html.push_str(&format!(
                    "{}. {} ({})\n",
                    i + 1,
                    if e.name.is_empty() { "???" } else { &e.name },
                    e.value
                ));
            }
            if lb.by_kills.is_empty() {
                html.push_str("  ---\n");
            }
            if html != s.last_lb_kills_html {
                s.lb_kills.set_inner_html(&html);
                s.last_lb_kills_html = html;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Render loop
// ---------------------------------------------------------------------------

fn start_render_loop(state: Rc<RefCell<GameState>>, _: &Window) {
    fn schedule(f: Rc<RefCell<Option<Closure<dyn FnMut(f64)>>>>) {
        web_sys::window().unwrap()
            .request_animation_frame(f.borrow().as_ref().unwrap().as_ref().unchecked_ref())
            .unwrap();
    }
    let cb: Rc<RefCell<Option<Closure<dyn FnMut(f64)>>>> = Rc::new(RefCell::new(None));
    let cb2 = cb.clone();
    *cb.borrow_mut() = Some(Closure::<dyn FnMut(f64)>::new(move |ts: f64| {
        render(&mut state.borrow_mut(), ts);
        schedule(cb2.clone());
    }));
    schedule(cb.clone());
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn render(s: &mut GameState, _ts: f64) {
    let now = now_ms();

    // Pre-cache sprite images and territory patterns (needs &mut s)
    {
        let needed: Vec<u32> = s
            .players
            .values()
            .filter(|rp| !s.sprite_images.contains_key(&rp.sprite_id))
            .map(|rp| rp.sprite_id)
            .collect();
        for sid in needed {
            get_sprite_image(s, sid);
        }
        // Pre-cache territory patterns for visible rings (uses sprite_id from territory data)
        let ring_keys: Vec<(u32, [u8; 3])> = s
            .territory
            .iter()
            .filter(|ring| !s.territory_patterns.contains_key(&(ring.sprite_id, ring.color)))
            .map(|ring| (ring.sprite_id, ring.color))
            .collect();
        for (sid, color) in ring_keys {
            // Ensure the sprite image is loaded first
            get_sprite_image(s, sid);
            get_territory_pattern(s, sid, color);
        }
    }

    let ctx = &s.ctx;
    let w = s.width;
    let h = s.height;
    let cs = CELL_SIZE * s.zoom;

    ctx.set_fill_style_str("#ffffff");
    ctx.fill_rect(0.0, 0.0, w, h);

    if !s.connected {
        centered_text(ctx, w, h, "Connecting...");
        return;
    }
    if !s.alive {
        centered_text(ctx, w, h, "You died! Reconnecting...");
        return;
    }

    let (cam_x, cam_y) = match s.player_id.and_then(|id| s.players.get(&id)) {
        Some(me) => extrap(me, now),
        None => (0.0, 0.0),
    };

    let to_sx = |wx: f64| (wx - cam_x) * cs + w / 2.0;
    let to_sy = |wy: f64| (wy - cam_y) * cs + h / 2.0;

    // Grid
    {
        let cx = (w / cs / 2.0).ceil() as i32 + 1;
        let cy = (h / cs / 2.0).ceil() as i32 + 1;
        let ccx = cam_x.floor() as i32;
        let ccy = cam_y.floor() as i32;
        ctx.set_stroke_style_str("rgba(0,0,0,0.04)");
        ctx.set_line_width(0.5);
        ctx.begin_path();
        for x in (ccx - cx)..=(ccx + cx) {
            let sx = to_sx(x as f64);
            ctx.move_to(sx, 0.0);
            ctx.line_to(sx, h);
        }
        for y in (ccy - cy)..=(ccy + cy) {
            let sy = to_sy(y as f64);
            ctx.move_to(0.0, sy);
            ctx.line_to(w, sy);
        }
        ctx.stroke();
    }

    // Board boundary — dark void outside, thick black circle edge
    {
        let bcx = to_sx(0.0);
        let bcy = to_sy(0.0);
        let br = s.board_radius * cs;

        // Draw dark overlay outside the circle using clip inversion
        ctx.save();
        ctx.begin_path();
        ctx.rect(0.0, 0.0, w, h);
        // Cut out the circle (counterclockwise = hole in the rect)
        let _ = ctx.arc_with_anticlockwise(bcx, bcy, br, 0.0, TAU, true);
        ctx.set_fill_style_str("rgba(0,0,0,0.7)");
        ctx.fill();
        ctx.restore();

        // Thick black border ring
        ctx.set_stroke_style_str("black");
        ctx.set_line_width((4.0 * s.zoom).max(2.0));
        ctx.begin_path();
        let _ = ctx.arc(bcx, bcy, br, 0.0, TAU);
        ctx.stroke();
    }

    // Drop shadow settings
    let shadow_offset = (6.0 * s.zoom).max(2.0);
    let shadow_blur = (1.0 * s.zoom).max(0.5);
    let shadows_enabled = s.zoom >= 0.25;

    // Territory — batch by color, AABB cull, pattern fill with drop shadow
    ctx.set_shadow_color(if shadows_enabled { "rgba(0,0,0,0.55)" } else { "transparent" });
    ctx.set_shadow_offset_x(shadow_offset);
    ctx.set_shadow_offset_y(shadow_offset);
    ctx.set_shadow_blur(shadow_blur);
    {
        for v in s.territory_groups.values_mut() {
            v.clear();
        }
        for (i, ring) in s.territory.iter().enumerate() {
            if ring.points.len() < 3 { continue; }
            if to_sx(ring.max_x) < 0.0 || to_sx(ring.min_x) > w
                || to_sy(ring.max_y) < 0.0 || to_sy(ring.min_y) > h { continue; }
            s.territory_groups.entry(ring.color).or_default().push(i);
        }
        for indices in s.territory_groups.values() {
            if indices.is_empty() { continue; }
            let ring = &s.territory[indices[0]];
            let pattern = s.territory_patterns.get(&(ring.sprite_id, ring.color)).cloned();

            if let Some(pat) = pattern {
                ctx.set_fill_style_canvas_pattern(&pat);
            } else {
                ctx.set_fill_style_str(&s.territory[indices[0]].color_str);
            }

            ctx.begin_path();
            for &i in indices {
                let pts = &s.territory[i].points;
                ctx.move_to(to_sx(pts[0].x), to_sy(pts[0].y));
                for p in &pts[1..] { ctx.line_to(to_sx(p.x), to_sy(p.y)); }
                ctx.close_path();
            }
            ctx.fill();
        }
    }

    // Trails — with drop shadow
    ctx.set_shadow_color(if shadows_enabled { "rgba(0,0,0,0.5)" } else { "transparent" });
    ctx.set_shadow_offset_x(shadow_offset * 0.8);
    ctx.set_shadow_offset_y(shadow_offset * 0.8);
    ctx.set_shadow_blur(shadow_blur * 0.8);
    for rp in s.players.values() {
        if rp.trail.len() < 2 { continue; }
        ctx.set_stroke_style_str(&rp.color_trail);
        ctx.set_line_width((cs * 0.15).max(1.0));
        ctx.set_line_cap("round");
        ctx.set_line_join("round");
        ctx.begin_path();
        ctx.move_to(to_sx(rp.trail[0].x), to_sy(rp.trail[0].y));
        for p in &rp.trail[1..] { ctx.line_to(to_sx(p.x), to_sy(p.y)); }
        ctx.stroke();
    }

    // Reset shadow before players (players set their own)
    ctx.set_shadow_color("transparent");

    // Players — collect lightweight render data (no String cloning)
    let player_render: Vec<(f64, f64, f64, u32, bool, bool)> = s
        .players
        .iter()
        .map(|(&id, rp)| {
            let (px, py) = extrap(rp, now);
            (px, py, rp.angle, rp.sprite_id, rp.has_crown, Some(id) == s.player_id)
        })
        .collect();

    for &(px, py, angle, sprite_id, has_crown, _is_me) in &player_render {
        let sx = to_sx(px);
        let sy = to_sy(py);
        let r = cs * 0.4;
        let sprite_size = r * 8.0;

        // Drop shadow for player sprite/circle
        ctx.set_shadow_color(if shadows_enabled { "rgba(0,0,0,0.6)" } else { "transparent" });
        ctx.set_shadow_offset_x(shadow_offset);
        ctx.set_shadow_offset_y(shadow_offset);
        ctx.set_shadow_blur(shadow_blur);

        // Look up cached color strings by sprite_id → player
        let (color_fill, color_trail) = s
            .players
            .values()
            .find(|rp| rp.sprite_id == sprite_id)
            .map(|rp| (rp.color_fill.as_str(), rp.color_trail.as_str()))
            .unwrap_or(("gray", "gray"));

        let drew_sprite = if let Some(img) = s.sprite_images.get(&sprite_id) {
            if img.complete() && img.natural_width() > 0 {
                let _ = ctx.draw_image_with_html_image_element_and_dw_and_dh(
                    img,
                    sx - sprite_size / 2.0,
                    sy - sprite_size / 2.0,
                    sprite_size,
                    sprite_size,
                );
                true
            } else {
                false
            }
        } else {
            false
        };

        if !drew_sprite {
            ctx.set_fill_style_str(color_fill);
            ctx.begin_path();
            let _ = ctx.arc(sx, sy, r, 0.0, TAU);
            ctx.fill();
        }

        // Clear shadow for indicator and crown
        ctx.set_shadow_color("transparent");

        // Direction indicator
        if !angle.is_nan() {
            let len = r * 1.6;
            ctx.set_stroke_style_str(color_trail);
            ctx.set_line_width(2.0);
            ctx.begin_path();
            ctx.move_to(sx, sy);
            ctx.line_to(sx + angle.cos() * len, sy + angle.sin() * len);
            ctx.stroke();
        }

        // Golden crown for territory leader
        if has_crown {
            let crown_size = sprite_size * 0.5;
            ctx.set_font(&format!("{}px serif", crown_size));
            ctx.set_fill_style_str("gold");
            ctx.set_text_align("center");
            ctx.set_text_baseline("bottom");
            let _ = ctx.fill_text("\u{1F451}", sx, sy - sprite_size * 0.4);
        }
    }

    // Coordinates display (bottom center) — only reformat when position changes noticeably
    {
        let (lx, ly) = s.last_coord_pos;
        if (cam_x - lx).abs() > 0.5 || (cam_y - ly).abs() > 0.5 {
            s.last_coord_str = format!("{:.0}, {:.0}", cam_x, cam_y);
            s.last_coord_pos = (cam_x, cam_y);
        }
    }
    ctx.set_font("12px monospace");
    ctx.set_fill_style_str("rgba(0,0,0,0.5)");
    ctx.set_text_align("center");
    ctx.set_text_baseline("bottom");
    let _ = ctx.fill_text(
        &s.last_coord_str,
        w / 2.0,
        h - 8.0,
    );

    // Minimap — throttle to ~20Hz (50ms intervals)
    if now - s.last_minimap_time >= 50.0 {
        s.last_minimap_time = now;
        render_minimap(s, cam_x, cam_y, cs);
    }
}

fn render_minimap(s: &GameState, cam_x: f64, cam_y: f64, cs: f64) {
    let mc = &s.minimap_ctx;
    let size = 150.0;
    let br = s.board_radius;
    if br <= 0.0 {
        return;
    }
    let scale = (size * 0.45) / br; // map board radius to ~45% of minimap size
    let cx = size / 2.0;
    let cy = size / 2.0;

    let to_mx = |wx: f64| cx + wx * scale;
    let to_my = |wy: f64| cy + wy * scale;

    // Clear
    mc.clear_rect(0.0, 0.0, size, size);

    // Board circle background
    mc.set_fill_style_str("rgba(255,255,255,0.8)");
    mc.begin_path();
    let _ = mc.arc(cx, cy, br * scale, 0.0, TAU);
    mc.fill();

    // Board border
    mc.set_stroke_style_str("black");
    mc.set_line_width(1.5);
    mc.begin_path();
    let _ = mc.arc(cx, cy, br * scale, 0.0, TAU);
    mc.stroke();

    // Territory polygons (filled, no detail needed)
    for ring in &s.territory {
        if ring.points.len() < 3 {
            continue;
        }
        mc.set_fill_style_str(&ring.color_str);
        mc.begin_path();
        mc.move_to(to_mx(ring.points[0].x), to_my(ring.points[0].y));
        for p in &ring.points[1..] {
            mc.line_to(to_mx(p.x), to_my(p.y));
        }
        mc.close_path();
        mc.fill();
    }

    // Player dots
    for rp in s.players.values() {
        mc.set_fill_style_str(&rp.color_fill);
        mc.begin_path();
        let _ = mc.arc(to_mx(rp.position.x), to_my(rp.position.y), 2.5, 0.0, TAU);
        mc.fill();
    }

    // Viewport rectangle
    let vw = s.width / cs; // viewport size in world units
    let vh = s.height / cs;
    mc.set_stroke_style_str("rgba(0,0,0,0.6)");
    mc.set_line_width(1.0);
    mc.stroke_rect(
        to_mx(cam_x - vw / 2.0),
        to_my(cam_y - vh / 2.0),
        vw * scale,
        vh * scale,
    );
}

fn centered_text(ctx: &CanvasRenderingContext2d, w: f64, h: f64, text: &str) {
    ctx.set_fill_style_str("black");
    ctx.set_font("24px monospace");
    ctx.set_text_align("center");
    ctx.set_text_baseline("middle");
    let _ = ctx.fill_text(text, w / 2.0, h / 2.0);
}

fn extrap(p: &RemotePlayer, now: f64) -> (f64, f64) {
    if p.angle.is_nan() { return (p.position.x, p.position.y); }
    let dt = ((now - p.server_time) / 1000.0).min(0.15);
    (
        p.position.x + p.angle.cos() * PLAYER_SPEED * dt,
        p.position.y + p.angle.sin() * PLAYER_SPEED * dt,
    )
}

fn now_ms() -> f64 {
    web_sys::window().unwrap().performance().unwrap().now()
}
