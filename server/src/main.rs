use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
    routing::get,
    Router,
};
use futures::{SinkExt, StreamExt};
use geo::{
    Area, BooleanOps, BoundingRect, Contains, ConvexHull, Intersects, Simplify,
};
use rand::Rng;
use shared::*;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tower_http::{cors::CorsLayer, services::ServeDir};

const COLORS: [[u8; 3]; 12] = [
    [231, 76, 60],
    [52, 152, 219],
    [46, 204, 113],
    [241, 196, 15],
    [155, 89, 182],
    [230, 126, 34],
    [26, 188, 156],
    [236, 240, 241],
    [52, 73, 94],
    [192, 57, 43],
    [39, 174, 96],
    [142, 68, 173],
];

/// Cell size for the spatial hash: 2x VISIBILITY_RADIUS.
const SPATIAL_CELL_SIZE: f64 = 500.0;

/// Simplification tolerance applied after polygon union/difference.
const SIMPLIFY_EPSILON: f64 = 0.3;

type Tx = mpsc::UnboundedSender<Vec<u8>>;

#[derive(Clone)]
struct AppState {
    event_tx: mpsc::UnboundedSender<GameEvent>,
}

enum GameEvent {
    Connect {
        tx: Tx,
        resp: oneshot::Sender<PlayerId>,
    },
    Disconnect {
        player_id: PlayerId,
    },
    Input {
        player_id: PlayerId,
        msg: ClientMsg,
    },
}

struct Player {
    id: PlayerId,
    position: Position,
    /// Current heading in radians. NAN = stopped.
    angle: f64,
    color: [u8; 3],
    territory: geo::MultiPolygon<f64>,
    /// Trail polyline recorded while outside territory.
    trail: Vec<Position>,
    /// Whether the player was inside their territory last tick.
    in_territory: bool,
    tx: Tx,
    alive: bool,
    /// Cached AABB for the trail (min_x, min_y, max_x, max_y).
    trail_aabb: Option<(f64, f64, f64, f64)>,
    sprite_id: u32,
    name: String,
    kill_count: u32,
    boost_points: u8,
    boost_until: Option<std::time::Instant>,
    /// Last territory version this player was sent.
    last_territory_version: u64,
}

// ---------------------------------------------------------------------------
// Spatial hash
// ---------------------------------------------------------------------------

struct SpatialHash {
    cells: HashMap<(i32, i32), Vec<PlayerId>>,
    cell_size: f64,
}

impl SpatialHash {
    fn new(cell_size: f64) -> Self {
        SpatialHash {
            cells: HashMap::new(),
            cell_size,
        }
    }

    fn clear(&mut self) {
        self.cells.clear();
    }

    fn cell_key(&self, x: f64, y: f64) -> (i32, i32) {
        (
            (x / self.cell_size).floor() as i32,
            (y / self.cell_size).floor() as i32,
        )
    }

    fn insert(&mut self, id: PlayerId, pos: Position) {
        let key = self.cell_key(pos.x, pos.y);
        self.cells.entry(key).or_default().push(id);
    }

    /// Returns an iterator over player IDs in the 3x3 grid of cells around `pos`.
    fn nearby(&self, pos: Position) -> impl Iterator<Item = PlayerId> + '_ {
        let (cx, cy) = self.cell_key(pos.x, pos.y);
        (-1..=1).flat_map(move |dx| {
            (-1..=1).flat_map(move |dy| {
                self.cells
                    .get(&(cx + dx, cy + dy))
                    .map(|v| v.as_slice())
                    .unwrap_or(&[])
                    .iter()
                    .copied()
            })
        })
    }
}

// ---------------------------------------------------------------------------
// Trail AABB helpers
// ---------------------------------------------------------------------------

impl Player {
    /// Recompute the trail AABB from the current trail points plus the live position.
    fn update_trail_aabb(&mut self) {
        if self.trail.is_empty() {
            self.trail_aabb = None;
            return;
        }
        let mut min_x = f64::MAX;
        let mut min_y = f64::MAX;
        let mut max_x = f64::MIN;
        let mut max_y = f64::MIN;
        for p in &self.trail {
            min_x = min_x.min(p.x);
            min_y = min_y.min(p.y);
            max_x = max_x.max(p.x);
            max_y = max_y.max(p.y);
        }
        // Include live endpoint
        min_x = min_x.min(self.position.x);
        min_y = min_y.min(self.position.y);
        max_x = max_x.max(self.position.x);
        max_y = max_y.max(self.position.y);
        self.trail_aabb = Some((min_x, min_y, max_x, max_y));
    }
}

struct Game {
    players: HashMap<PlayerId, Player>,
    next_id: PlayerId,
    tick_count: u64,
    spatial_hash: SpatialHash,
    /// Monotonic version counter incremented on any territory change.
    territory_version: u64,
    /// Current board radius (smoothly interpolated).
    board_radius: f64,
    /// Target board radius based on player count.
    target_board_radius: f64,
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    tokio::spawn(game_loop(Game::new(), event_rx));

    let state = AppState { event_tx };
    let app = Router::new()
        .route("/ws", get(ws_handler))
        .with_state(state)
        .layer(CorsLayer::permissive())
        .fallback_service(ServeDir::new("web"));

    let port = std::env::var("PORT").unwrap_or_else(|_| "3000".to_string());
    let addr = format!("0.0.0.0:{}", port);
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    println!("Server listening on http://localhost:{}", port);
    axum::serve(listener, app).await.unwrap();
}

// ---------------------------------------------------------------------------
// WebSocket
// ---------------------------------------------------------------------------

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: AppState) {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (resp_tx, resp_rx) = oneshot::channel();

    if state
        .event_tx
        .send(GameEvent::Connect { tx, resp: resp_tx })
        .is_err()
    {
        return;
    }
    let player_id = match resp_rx.await {
        Ok(id) => id,
        Err(_) => return,
    };

    let event_tx = state.event_tx.clone();

    let mut send_task = tokio::spawn(async move {
        while let Some(bytes) = rx.recv().await {
            if ws_tx.send(Message::Binary(bytes.into())).await.is_err() {
                break;
            }
        }
    });

    let event_tx2 = event_tx.clone();
    let mut recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = ws_rx.next().await {
            match msg {
                Message::Binary(bytes) => {
                    if let Ok(m) = decode_client_msg(&bytes) {
                        let _ = event_tx2.send(GameEvent::Input {
                            player_id,
                            msg: m,
                        });
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    });

    tokio::select! {
        _ = &mut send_task => recv_task.abort(),
        _ = &mut recv_task => send_task.abort(),
    }
    let _ = event_tx.send(GameEvent::Disconnect { player_id });
}

// ---------------------------------------------------------------------------
// Game loop
// ---------------------------------------------------------------------------

async fn game_loop(mut game: Game, mut event_rx: mpsc::UnboundedReceiver<GameEvent>) {
    let mut interval = tokio::time::interval(Duration::from_millis(50));
    loop {
        interval.tick().await;
        while let Ok(ev) = event_rx.try_recv() {
            game.handle_event(ev);
        }
        game.update(0.05);
        game.broadcast();
    }
}

// ---------------------------------------------------------------------------
// Game
// ---------------------------------------------------------------------------

impl Game {
    fn new() -> Self {
        Game {
            players: HashMap::new(),
            next_id: 1,
            tick_count: 0,
            spatial_hash: SpatialHash::new(SPATIAL_CELL_SIZE),
            territory_version: 0,
            board_radius: 50.0,
            target_board_radius: 50.0,
        }
    }

    /// Rebuild the spatial hash from current player positions.
    fn rebuild_spatial_hash(&mut self) {
        self.spatial_hash.clear();
        for p in self.players.values() {
            if p.alive {
                self.spatial_hash.insert(p.id, p.position);
            }
        }
    }

    fn handle_event(&mut self, event: GameEvent) {
        match event {
            GameEvent::Connect { tx, resp } => {
                let id = self.next_id;
                self.next_id += 1;
                let color = COLORS[(id as usize) % COLORS.len()];
                let position = self.find_spawn_position();
                let territory = rect_polygon(
                    position.x - STARTING_TERRITORY_RADIUS,
                    position.y - STARTING_TERRITORY_RADIUS,
                    position.x + STARTING_TERRITORY_RADIUS,
                    position.y + STARTING_TERRITORY_RADIUS,
                );
                let spawn_multi = geo::MultiPolygon::new(vec![territory.clone()]);

                // Steal this area from any existing players
                for other in self.players.values_mut() {
                    if let Ok(diff) = std::panic::catch_unwind(
                        std::panic::AssertUnwindSafe(|| {
                            other.territory.difference(&spawn_multi)
                        }),
                    ) {
                        other.territory = diff.simplify(&SIMPLIFY_EPSILON);
                    }
                }

                self.territory_version += 1;
                self.players.insert(
                    id,
                    Player {
                        id,
                        position,
                        angle: f64::NAN, // stopped
                        color,
                        territory: geo::MultiPolygon::new(vec![territory]),
                        trail: Vec::new(),
                        in_territory: true,
                        tx,
                        alive: true,
                        trail_aabb: None,
                        sprite_id: 0,
                        name: format!("Player {}", id),
                        kill_count: 0,
                        boost_points: 0,
                        boost_until: None,
                        last_territory_version: 0,
                    },
                );

                let welcome = ServerMsg::Welcome {
                    player_id: id,
                    position,
                    angle: f64::NAN,
                    color,
                };
                self.send_to(id, &welcome);
                self.send_territory_snapshot(id);
                // Notify nearby players so they see the new territory
                self.broadcast_territory_near(id);
                // Send leaderboard immediately so the new player sees it
                let area_map: HashMap<PlayerId, f64> = self
                    .players
                    .values()
                    .filter(|p| p.alive)
                    .map(|p| (p.id, p.territory.unsigned_area()))
                    .collect();
                let lb = self.compute_leaderboard(&area_map);
                self.send_to(id, &lb);
                let _ = resp.send(id);
            }

            GameEvent::Disconnect { player_id } => {
                // Get position before removing so we can notify nearby players
                let pos = self.players.get(&player_id).map(|p| p.position);
                self.players.remove(&player_id);
                self.territory_version += 1;
                // Notify nearby players so the territory disappears
                if let Some(pos) = pos {
                    let nearby: Vec<PlayerId> = self
                        .players
                        .values()
                        .filter(|p| {
                            p.alive
                                && (p.position.x - pos.x).abs() <= VISIBILITY_RADIUS * 1.5
                                && (p.position.y - pos.y).abs() <= VISIBILITY_RADIUS * 1.5
                        })
                        .map(|p| p.id)
                        .collect();
                    for nid in nearby {
                        self.send_territory_snapshot(nid);
                    }
                }
            }

            GameEvent::Input { player_id, msg } => match msg {
                ClientMsg::ChangeDirection(angle) => {
                    if let Some(p) = self.players.get_mut(&player_id) {
                        if p.alive {
                            // Record trail corner before turning (if trail active)
                            // Only if moved enough from last point to avoid wobbly false crossings
                            if !p.in_territory && !p.trail.is_empty() && !p.angle.is_nan() {
                                let last = p.trail[p.trail.len() - 1];
                                let dx = p.position.x - last.x;
                                let dy = p.position.y - last.y;
                                if dx * dx + dy * dy > 0.25 {
                                    // > 0.5 units from last point
                                    p.trail.push(p.position);
                                    p.update_trail_aabb();
                                }
                            }
                            p.angle = angle;
                        }
                    }
                }
                ClientMsg::Ping(ts) => {
                    self.send_to(player_id, &ServerMsg::Pong(ts));
                }
                ClientMsg::SetSprite(id) => {
                    if let Some(p) = self.players.get_mut(&player_id) {
                        p.sprite_id = id;
                    }
                }
                ClientMsg::SetName(name) => {
                    if let Some(p) = self.players.get_mut(&player_id) {
                        p.name = name.chars().take(20).collect();
                    }
                }
                ClientMsg::ActivateBoost => {
                    if let Some(p) = self.players.get_mut(&player_id) {
                        if p.boost_points > 0 {
                            p.boost_points -= 1;
                            p.boost_until =
                                Some(std::time::Instant::now() + std::time::Duration::from_secs(5));
                        }
                    }
                }
            },
        }
    }

    fn update(&mut self, dt: f64) {
        self.tick_count += 1;

        // Update board radius: 30 * sqrt(player_count), min 50
        let alive = self.players.values().filter(|p| p.alive).count().max(1);
        self.target_board_radius = (30.0 * (alive as f64).sqrt()).max(50.0);
        self.board_radius += (self.target_board_radius - self.board_radius) * 0.02;

        // Rebuild spatial hash at the start of each tick
        self.rebuild_spatial_hash();

        let ids: Vec<PlayerId> = self.players.keys().copied().collect();

        // Compute speed multipliers: +2.2% per player ranked below you on either list
        let speed_mult: HashMap<PlayerId, f64> = {
            let alive_count = self.players.values().filter(|p| p.alive).count();
            if alive_count <= 1 {
                ids.iter().map(|&id| (id, 1.0)).collect()
            } else {
                // Rank by area (descending)
                let mut by_area: Vec<(PlayerId, f64)> = self
                    .players
                    .values()
                    .filter(|p| p.alive)
                    .map(|p| (p.id, p.territory.unsigned_area()))
                    .collect();
                by_area.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));

                // Rank by kills (descending)
                let mut by_kills: Vec<(PlayerId, u32)> = self
                    .players
                    .values()
                    .filter(|p| p.alive)
                    .map(|p| (p.id, p.kill_count))
                    .collect();
                by_kills.sort_by(|a, b| b.1.cmp(&a.1));

                let area_rank: HashMap<PlayerId, usize> = by_area
                    .iter()
                    .enumerate()
                    .map(|(i, (id, _))| (*id, i))
                    .collect();
                let kill_rank: HashMap<PlayerId, usize> = by_kills
                    .iter()
                    .enumerate()
                    .map(|(i, (id, _))| (*id, i))
                    .collect();

                let n = alive_count;
                ids.iter()
                    .map(|&id| {
                        let ar = area_rank.get(&id).copied().unwrap_or(n - 1);
                        let kr = kill_rank.get(&id).copied().unwrap_or(n - 1);
                        // Players below you = (n-1) - rank for each list
                        let below_area = (n - 1).saturating_sub(ar);
                        let below_kills = (n - 1).saturating_sub(kr);
                        // Count unique players below you on either list
                        let below_total = below_area + below_kills;
                        // 1.022^below_total — multiplicative per player below
                        let mult = 1.022_f64.powi(below_total as i32);
                        (id, mult)
                    })
                    .collect()
            }
        };

        // Move players, track old positions for boundary crossing & self-kill
        let mut old_positions: HashMap<PlayerId, Position> = HashMap::new();
        for &id in &ids {
            let p = match self.players.get_mut(&id) {
                Some(p) if p.alive && !p.angle.is_nan() => p,
                _ => continue,
            };
            let mut mult = speed_mult.get(&id).copied().unwrap_or(1.0);
            // Active boost: +50% speed
            if let Some(until) = p.boost_until {
                if std::time::Instant::now() < until {
                    mult *= 1.5;
                } else {
                    p.boost_until = None;
                }
            }
            old_positions.insert(id, p.position);
            p.position.x += p.angle.cos() * PLAYER_SPEED * mult * dt;
            p.position.y += p.angle.sin() * PLAYER_SPEED * mult * dt;
            if !p.in_territory && !p.trail.is_empty() {
                p.update_trail_aabb();
            }
        }

        // Territory transitions — use exact boundary crossing points
        let mut captures = Vec::new();
        for &id in &ids {
            let p = match self.players.get_mut(&id) {
                Some(p) if p.alive => p,
                _ => continue,
            };
            let pt = geo::Point::new(p.position.x, p.position.y);
            let now_in = p.territory.contains(&pt);
            let old_pos = old_positions.get(&id).copied();

            if p.in_territory && !now_in {
                // Exiting territory — find exact crossing point
                let exit_point = old_pos
                    .and_then(|old| boundary_crossing(old, p.position, &p.territory))
                    .unwrap_or(p.position);
                p.in_territory = false;
                p.trail.clear();
                p.trail.push(exit_point);
                p.update_trail_aabb();
            } else if !p.in_territory && now_in && p.trail.len() >= 2 {
                // Returning to territory — find exact crossing point
                let entry_point = old_pos
                    .and_then(|old| boundary_crossing(old, p.position, &p.territory))
                    .unwrap_or(p.position);
                p.trail.push(entry_point);
                p.in_territory = true;
                p.trail_aabb = None;
                captures.push(id);
            } else if !p.in_territory && now_in {
                p.in_territory = true;
                p.trail.clear();
                p.trail_aabb = None;
            }
        }

        // Pre-compute trail LineStrings and AABBs for kill detection
        let mut trail_lines: HashMap<PlayerId, geo::LineString<f64>> = HashMap::new();
        let mut trail_aabbs: HashMap<PlayerId, (f64, f64, f64, f64)> = HashMap::new();
        for (&pid, p) in &self.players {
            if !p.alive || p.trail.len() < 2 {
                continue;
            }
            let mut pts: Vec<geo::Coord<f64>> = Vec::with_capacity(p.trail.len() + 1);
            let mut min_x = f64::MAX;
            let mut min_y = f64::MAX;
            let mut max_x = f64::MIN;
            let mut max_y = f64::MIN;
            for tp in &p.trail {
                let c = geo::Coord { x: tp.x, y: tp.y };
                min_x = min_x.min(c.x);
                min_y = min_y.min(c.y);
                max_x = max_x.max(c.x);
                max_y = max_y.max(c.y);
                pts.push(c);
            }
            let live = geo::Coord { x: p.position.x, y: p.position.y };
            min_x = min_x.min(live.x);
            min_y = min_y.min(live.y);
            max_x = max_x.max(live.x);
            max_y = max_y.max(live.y);
            pts.push(live);
            trail_lines.insert(pid, geo::LineString::new(pts));
            trail_aabbs.insert(pid, (min_x, min_y, max_x, max_y));
        }

        // Kill detection: check trails (both other players' and own)
        let mut kills: Vec<(PlayerId, Option<PlayerId>)> = Vec::new();
        for &id in &ids {
            let p = &self.players[&id];
            if !p.alive {
                continue;
            }
            let px = p.position.x;
            let py = p.position.y;

            // Check own trail: did the movement segment cross any earlier trail segment?
            // Skip the last 2 trail segments (adjacent to current position).
            if let Some(&old_pos) = old_positions.get(&id) {
                if !p.in_territory && p.trail.len() >= 3 {
                    let new_pos = p.position;
                    let check_end = p.trail.len().saturating_sub(2);
                    let mut self_killed = false;
                    for i in 0..check_end.saturating_sub(1) {
                        if segments_cross(old_pos, new_pos, p.trail[i], p.trail[i + 1]) {
                            self_killed = true;
                            break;
                        }
                    }
                    if self_killed {
                        kills.push((id, None));
                        continue;
                    }
                }
            }

            // Check if movement segment crosses any other player's trail
            if let Some(&old_pos) = old_positions.get(&id) {
                let new_pos = p.position;
                for &oid in &ids {
                    if oid == id {
                        continue;
                    }
                    let other = match self.players.get(&oid) {
                        Some(o) if o.alive && o.trail.len() >= 2 => o,
                        _ => continue,
                    };

                    // AABB pre-check
                    if let Some(&(min_x, min_y, max_x, max_y)) = trail_aabbs.get(&oid) {
                        let mx = px.min(old_pos.x);
                        let my = py.min(old_pos.y);
                        let xx = px.max(old_pos.x);
                        let xy = py.max(old_pos.y);
                        if xx < min_x || mx > max_x || xy < min_y || my > max_y {
                            continue;
                        }
                    }

                    // Check movement segment against all trail segments + live endpoint
                    let trail = &other.trail;
                    let mut crossed = false;
                    for i in 0..trail.len() - 1 {
                        if segments_cross(old_pos, new_pos, trail[i], trail[i + 1]) {
                            crossed = true;
                            break;
                        }
                    }
                    // Also check last trail point → current position (live segment)
                    if !crossed {
                        if let Some(&last) = trail.last() {
                            if segments_cross(old_pos, new_pos, last, other.position) {
                                crossed = true;
                            }
                        }
                    }
                    if crossed {
                        kills.push((oid, Some(id)));
                    }
                }
            }
        }

        for id in captures {
            self.capture_territory(id);
        }

        let mut killed = HashSet::new();
        for (victim, killer) in kills {
            if killed.insert(victim) {
                self.kill_player(victim, killer);
            }
        }

        // Clamp players to inside the board boundary (slide along edge)
        let br = self.board_radius;
        for p in self.players.values_mut() {
            if !p.alive {
                continue;
            }
            let dist = (p.position.x * p.position.x + p.position.y * p.position.y).sqrt();
            if dist > br && dist > 0.0 {
                p.position.x = p.position.x / dist * br;
                p.position.y = p.position.y / dist * br;
            }
        }
    }

    // -- capture -----------------------------------------------------------

    fn capture_territory(&mut self, player_id: PlayerId) {
        let (trail, old_territory) = {
            let p = match self.players.get(&player_id) {
                Some(p) if p.alive && p.trail.len() >= 2 => p,
                _ => {
                    if let Some(p) = self.players.get_mut(&player_id) {
                        p.trail.clear();
                        p.trail_aabb = None;
                    }
                    return;
                }
            };
            (p.trail.clone(), p.territory.clone())
        };

        let trail_start = trail[0];
        let trail_end = trail[trail.len() - 1];

        // Build the capture polygon by walking the territory boundary from
        // the re-entry point back to the exit point, then following the trail.
        // This shares an edge with the territory, making the union robust.
        let capture_multi =
            match build_capture_polygon(&trail, &old_territory, trail_start, trail_end) {
                Some(m) => m,
                None => {
                    if let Some(p) = self.players.get_mut(&player_id) {
                        p.trail.clear();
                        p.trail_aabb = None;
                    }
                    return;
                }
            };

        // Union with existing territory
        let new_territory = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            old_territory.union(&capture_multi).simplify(&SIMPLIFY_EPSILON)
        })) {
            Ok(t) => t,
            Err(_) => {
                // Fallback: convex hull of everything
                let mut pts: Vec<geo::Point<f64>> = trail
                    .iter()
                    .map(|p| geo::Point::new(p.x, p.y))
                    .collect();
                for poly in old_territory.iter() {
                    for c in poly.exterior().coords() {
                        pts.push(geo::Point::new(c.x, c.y));
                    }
                }
                geo::MultiPolygon::new(vec![
                    geo::MultiPoint::new(pts).convex_hull(),
                ])
            }
        };

        // Steal territory from other players
        let other_ids: Vec<PlayerId> = self
            .players
            .keys()
            .filter(|&&id| id != player_id)
            .copied()
            .collect();
        for oid in other_ids {
            if let Some(other) = self.players.get_mut(&oid) {
                if let Ok(diff) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    other.territory.difference(&capture_multi)
                })) {
                    other.territory = diff.simplify(&SIMPLIFY_EPSILON);
                }
            }
        }

        // Kill enemies inside the captured area or left stranded
        let enemies_to_kill: Vec<PlayerId> = self
            .players
            .values()
            .filter(|p| p.id != player_id && p.alive)
            .filter(|p| {
                let pt = geo::Point::new(p.position.x, p.position.y);
                capture_multi.contains(&pt)
                    || (new_territory.contains(&pt) && !p.territory.contains(&pt))
            })
            .map(|p| p.id)
            .collect();

        let p = self.players.get_mut(&player_id).unwrap();
        p.territory = new_territory;
        p.trail.clear();
        p.trail_aabb = None;

        self.territory_version += 1;
        self.broadcast_territory_near(player_id);

        for eid in enemies_to_kill {
            self.kill_player(eid, Some(player_id));
        }
    }

    // -- kills -------------------------------------------------------------

    fn kill_player(&mut self, victim_id: PlayerId, killer_id: Option<PlayerId>) {
        let victim_pos = match self.players.get(&victim_id) {
            Some(p) => p.position,
            None => return,
        };
        // Credit the killer with a kill and a boost point (max 3)
        if let Some(kid) = killer_id {
            if let Some(killer) = self.players.get_mut(&kid) {
                killer.kill_count += 1;
                if killer.boost_points < 3 {
                    killer.boost_points += 1;
                }
            }
        }
        let msg = ServerMsg::PlayerKilled {
            player_id: victim_id,
            killer_id,
        };

        // Use spatial hash to find nearby players instead of iterating all
        let nearby: Vec<PlayerId> = self
            .spatial_hash
            .nearby(victim_pos)
            .filter(|&id| {
                if let Some(p) = self.players.get(&id) {
                    p.alive
                        && (p.position.x - victim_pos.x).abs() <= VISIBILITY_RADIUS
                        && (p.position.y - victim_pos.y).abs() <= VISIBILITY_RADIUS
                } else {
                    false
                }
            })
            .collect::<Vec<_>>();
        // Deduplicate (a player could appear in multiple cells, though unlikely
        // with cell_size >> player spread; this is defensive)
        let nearby: Vec<PlayerId> = {
            let mut set = HashSet::new();
            nearby.into_iter().filter(|id| set.insert(*id)).collect()
        };

        for &id in &nearby {
            self.send_to(id, &msg);
        }
        self.players.remove(&victim_id);
        self.territory_version += 1;
        for id in nearby {
            if id != victim_id {
                self.send_territory_snapshot(id);
            }
        }
    }

    // -- spawning ----------------------------------------------------------

    fn find_spawn_position(&self) -> Position {
        if self.players.is_empty() {
            return Position { x: 0.0, y: 0.0 };
        }

        let mut min_x = f64::MAX;
        let mut max_x = f64::MIN;
        let mut min_y = f64::MAX;
        let mut max_y = f64::MIN;
        for p in self.players.values() {
            if let Some(bbox) = p.territory.bounding_rect() {
                min_x = min_x.min(bbox.min().x);
                max_x = max_x.max(bbox.max().x);
                min_y = min_y.min(bbox.min().y);
                max_y = max_y.max(bbox.max().y);
            }
        }

        let mut rng = rand::thread_rng();
        let cx = (min_x + max_x) / 2.0;
        let cy = (min_y + max_y) / 2.0;
        let search = ((max_x - min_x + max_y - min_y) / 2.0 + 20.0).max(20.0);
        let r = STARTING_TERRITORY_RADIUS;

        let max_r = self.board_radius - r - 1.0;
        // Try to find a clear spot first
        for _ in 0..200 {
            let x = cx + rng.gen_range(-search..=search);
            let y = cy + rng.gen_range(-search..=search);
            if x * x + y * y > max_r * max_r {
                continue;
            }
            let spawn = rect_polygon(x - r, y - r, x + r, y + r);
            let clear = self
                .players
                .values()
                .all(|p| !p.territory.intersects(&spawn));
            if clear {
                return Position { x, y };
            }
        }

        // Fallback: random position inside the board (will steal territory on spawn)
        let angle = rng.gen_range(0.0..std::f64::consts::TAU);
        let dist = rng.gen_range(0.0..(max_r * 0.8));
        Position {
            x: angle.cos() * dist,
            y: angle.sin() * dist,
        }
    }

    // -- broadcasting ------------------------------------------------------

    fn broadcast(&mut self) {
        let send_territory = self.tick_count % 200 == 0;

        // Optimization 2: Pre-compute areas once for crown + leaderboard
        let area_map: HashMap<PlayerId, f64> = self
            .players
            .values()
            .filter(|p| p.alive)
            .map(|p| (p.id, p.territory.unsigned_area()))
            .collect();

        // Crown holder = #1 on the area list (same sort: area desc, then lowest ID)
        let crown_holder: Option<PlayerId> = {
            let mut sorted: Vec<_> = area_map.iter().collect();
            sorted.sort_by(|a, b| {
                b.1.partial_cmp(a.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(a.0.cmp(b.0))
            });
            sorted.first().map(|(&id, _)| id)
        };

        let viewers: Vec<(PlayerId, Position)> = self
            .players
            .values()
            .filter(|p| p.alive)
            .map(|p| (p.id, p.position))
            .collect();

        for (vid, vpos) in &viewers {
            let nearby_ids: HashSet<PlayerId> = self.spatial_hash.nearby(*vpos).collect();

            let visible: Vec<PlayerStateData> = self
                .players
                .values()
                .filter(|p| {
                    p.alive
                        && nearby_ids.contains(&p.id)
                        && (p.position.x - vpos.x).abs() <= VISIBILITY_RADIUS
                        && (p.position.y - vpos.y).abs() <= VISIBILITY_RADIUS
                })
                .map(|p| {
                    // Optimization 6: Avoid trail clone when possible
                    let trail = if p.in_territory || p.trail.is_empty() {
                        // In territory or no trail — send empty, no clone needed
                        Vec::new()
                    } else {
                        // Outside territory — clone and append live endpoint
                        let mut t = p.trail.clone();
                        t.push(p.position);
                        t
                    };
                    PlayerStateData {
                        id: p.id,
                        position: p.position,
                        angle: p.angle,
                        color: p.color,
                        trail,
                        sprite_id: p.sprite_id,
                        has_crown: crown_holder == Some(p.id),
                        boost_points: p.boost_points,
                        boost_active: p.boost_until.map_or(false, |u| std::time::Instant::now() < u),
                    }
                })
                .collect();

            self.send_to(
                *vid,
                &ServerMsg::Tick {
                    players: visible,
                    board_radius: self.board_radius,
                },
            );
            if send_territory {
                self.send_territory_snapshot(*vid);
            }
        }

        // Send leaderboard every 2 seconds
        if self.tick_count % 40 == 0 {
            // Optimization 3: encode leaderboard once, send same bytes to all
            let leaderboard = self.compute_leaderboard(&area_map);
            let leaderboard_bytes = encode_server_msg(&leaderboard);
            for p in self.players.values() {
                let _ = p.tx.send(leaderboard_bytes.clone());
            }
        }
    }

    fn compute_leaderboard(&self, area_map: &HashMap<PlayerId, f64>) -> ServerMsg {
        // Top 10 by territory area — use pre-computed areas
        let mut by_area: Vec<(&Player, f64)> = self
            .players
            .values()
            .filter(|p| p.alive)
            .filter_map(|p| area_map.get(&p.id).map(|&a| (p, a)))
            .collect();
        by_area.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.id.cmp(&b.0.id)));
        let by_area: Vec<LeaderboardEntryData> = by_area
            .iter()
            .take(10)
            .map(|(p, area)| LeaderboardEntryData {
                name: p.name.clone(),
                value: *area as u32,
            })
            .collect();

        // Top 10 by kills
        let mut by_kills: Vec<&Player> = self
            .players
            .values()
            .filter(|p| p.alive && p.kill_count > 0)
            .collect();
        by_kills.sort_by(|a, b| b.kill_count.cmp(&a.kill_count));
        let by_kills: Vec<LeaderboardEntryData> = by_kills
            .iter()
            .take(10)
            .map(|p| LeaderboardEntryData {
                name: p.name.clone(),
                value: p.kill_count,
            })
            .collect();

        ServerMsg::Leaderboard(LeaderboardData { by_area, by_kills })
    }

    fn broadcast_territory_near(&mut self, center_id: PlayerId) {
        let cpos = match self.players.get(&center_id) {
            Some(p) => p.position,
            None => return,
        };

        // Use spatial hash to find candidates, then refine with 1.5x visibility radius
        let candidates: HashSet<PlayerId> = self.spatial_hash.nearby(cpos).collect();
        let nearby: Vec<PlayerId> = self
            .players
            .values()
            .filter(|p| {
                p.alive
                    && candidates.contains(&p.id)
                    && (p.position.x - cpos.x).abs() <= VISIBILITY_RADIUS * 1.5
                    && (p.position.y - cpos.y).abs() <= VISIBILITY_RADIUS * 1.5
            })
            .map(|p| p.id)
            .collect();
        for id in nearby {
            self.send_territory_snapshot(id);
        }
    }

    fn send_territory_snapshot(&mut self, viewer_id: PlayerId) {
        let current_version = self.territory_version;
        let vpos = match self.players.get(&viewer_id) {
            Some(p) => {
                // Skip if the player already has the latest territory version
                if p.last_territory_version == current_version {
                    return;
                }
                p.position
            }
            None => return,
        };
        let vis = VISIBILITY_RADIUS;

        // Territory polygons can extend far from the player position, so we
        // cannot rely on spatial-hash proximity alone. The per-polygon AABB
        // check below is the real filter. (The spatial hash is used by callers
        // like broadcast() and broadcast_territory_near() to limit *which
        // viewers* receive snapshots.)
        let mut rings = Vec::new();

        for p in self.players.values() {
            if !p.alive {
                continue;
            }
            for poly in p.territory.iter() {
                if let Some(bbox) = poly.bounding_rect() {
                    if bbox.max().x < vpos.x - vis
                        || bbox.min().x > vpos.x + vis
                        || bbox.max().y < vpos.y - vis
                        || bbox.min().y > vpos.y + vis
                    {
                        continue;
                    }
                }
                let points: Vec<Position> = poly
                    .exterior()
                    .coords()
                    .map(|c| Position { x: c.x, y: c.y })
                    .collect();
                rings.push(TerritoryRingData {
                    player_id: p.id,
                    color: p.color,
                    points,
                    sprite_id: p.sprite_id,
                });
            }
        }

        self.send_to(viewer_id, &ServerMsg::TerritorySnapshot(rings));
        // Update the player's last seen territory version
        if let Some(p) = self.players.get_mut(&viewer_id) {
            p.last_territory_version = current_version;
        }
    }

    fn send_to(&self, id: PlayerId, msg: &ServerMsg) {
        if let Some(p) = self.players.get(&id) {
            let _ = p.tx.send(encode_server_msg(msg));
        }
    }
}

fn rect_polygon(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> geo::Polygon<f64> {
    geo::Polygon::new(
        geo::LineString::from(vec![
            (min_x, min_y),
            (max_x, min_y),
            (max_x, max_y),
            (min_x, max_y),
            (min_x, min_y),
        ]),
        vec![],
    )
}

// ---------------------------------------------------------------------------
// Capture polygon: trail + boundary walk
// ---------------------------------------------------------------------------

/// Build a capture polygon from the trail and a walk along the territory
/// boundary from the re-entry point back to the exit point.
fn build_capture_polygon(
    trail: &[Position],
    territory: &geo::MultiPolygon<f64>,
    trail_start: Position,
    trail_end: Position,
) -> Option<geo::MultiPolygon<f64>> {
    // Find which exterior ring the trail exits/enters, and the nearest vertices
    let (ring_coords, idx_start, idx_end) =
        find_ring_and_vertices(territory, trail_start, trail_end)?;

    let n = ring_coords.len();
    if n < 3 {
        return None;
    }

    // Walk the boundary both directions from idx_end back to idx_start
    let walk_fwd = walk_ring(&ring_coords, idx_end, idx_start, true);
    let walk_bwd = walk_ring(&ring_coords, idx_end, idx_start, false);

    // Pick the shorter walk (the one that goes around the "small" side)
    let boundary = if walk_fwd.len() <= walk_bwd.len() {
        walk_fwd
    } else {
        walk_bwd
    };

    // Build closed polygon: trail path (with exact boundary crossing points)
    // → boundary walk back → close.
    let mut coords: Vec<geo::Coord<f64>> = trail
        .iter()
        .map(|p| geo::Coord { x: p.x, y: p.y })
        .collect();

    // Append boundary walk from idx_end back to idx_start
    for c in boundary.iter() {
        coords.push(*c);
    }

    // Close the ring
    if let Some(&first) = coords.first() {
        coords.push(first);
    }

    if coords.len() < 4 {
        return None;
    }

    let poly = geo::Polygon::new(geo::LineString::new(coords), vec![]);
    Some(geo::MultiPolygon::new(vec![poly]))
}

/// Find the exterior ring closest to both points and return its vertices
/// (without closing duplicate) plus the nearest vertex indices.
fn find_ring_and_vertices(
    territory: &geo::MultiPolygon<f64>,
    start: Position,
    end: Position,
) -> Option<(Vec<geo::Coord<f64>>, usize, usize)> {
    let mut best: Option<(Vec<geo::Coord<f64>>, usize, usize, f64)> = None;

    for poly in territory.iter() {
        let ring = poly.exterior();
        // Determine coord count without collecting: iterate once to count,
        // then work with the coords directly via indexing on LineString.
        let total_len = ring.0.len();
        // Exclude closing duplicate
        let n = if total_len > 1 && ring.0.first() == ring.0.last() {
            total_len - 1
        } else {
            total_len
        };
        if n < 3 {
            continue;
        }
        let coords = &ring.0[..n];

        let is = nearest_vertex_idx(coords, start);
        let ie = nearest_vertex_idx(coords, end);

        let ds = dist_sq(coords[is], start);
        let de = dist_sq(coords[ie], end);
        let total = ds + de;

        if best.as_ref().map_or(true, |b| total < b.3) {
            best = Some((coords.to_vec(), is, ie, total));
        }

        // Early break: if both vertices are within 1.0 distance, this ring
        // is close enough — no need to check remaining rings.
        if ds < 1.0 && de < 1.0 {
            break;
        }
    }

    best.map(|(c, is, ie, _)| (c, is, ie))
}

fn nearest_vertex_idx(coords: &[geo::Coord<f64>], p: Position) -> usize {
    coords
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            let da = (a.x - p.x) * (a.x - p.x) + (a.y - p.y) * (a.y - p.y);
            let db = (b.x - p.x) * (b.x - p.x) + (b.y - p.y) * (b.y - p.y);
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(i, _)| i)
        .unwrap_or(0)
}

fn dist_sq(c: geo::Coord<f64>, p: Position) -> f64 {
    (c.x - p.x) * (c.x - p.x) + (c.y - p.y) * (c.y - p.y)
}

/// Walk along ring vertices from `from` to `to` (inclusive both ends).
fn walk_ring(
    coords: &[geo::Coord<f64>],
    from: usize,
    to: usize,
    forward: bool,
) -> Vec<geo::Coord<f64>> {
    let n = coords.len();
    let mut result = Vec::new();
    let mut i = from;
    loop {
        result.push(coords[i]);
        if i == to {
            break;
        }
        i = if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        };
        // Safety: prevent infinite loop if from == to
        if result.len() > n + 1 {
            break;
        }
    }
    result
}

/// Find the exact point where segment (from→to) crosses the territory boundary.
fn boundary_crossing(
    from: Position,
    to: Position,
    territory: &geo::MultiPolygon<f64>,
) -> Option<Position> {
    let mut best: Option<(Position, f64)> = None;
    for poly in territory.iter() {
        let ring = poly.exterior();
        let mut prev: Option<&geo::Coord<f64>> = None;
        for c in ring.coords() {
            if let Some(p) = prev {
                let b1 = Position { x: p.x, y: p.y };
                let b2 = Position { x: c.x, y: c.y };
                if let Some((pt, t)) = seg_intersection(from, to, b1, b2) {
                    // Early exit: intersection very close to `from`
                    if t < 0.01 {
                        return Some(pt);
                    }
                    if best.as_ref().map_or(true, |b| t < b.1) {
                        best = Some((pt, t));
                    }
                }
            }
            prev = Some(c);
        }
    }
    best.map(|(pt, _)| pt)
}

/// Returns intersection point and parameter t along (a1→a2).
fn seg_intersection(
    a1: Position,
    a2: Position,
    b1: Position,
    b2: Position,
) -> Option<(Position, f64)> {
    let d1x = a2.x - a1.x;
    let d1y = a2.y - a1.y;
    let d2x = b2.x - b1.x;
    let d2y = b2.y - b1.y;
    let denom = d1x * d2y - d1y * d2x;
    if denom.abs() < 1e-10 {
        return None;
    }
    let t = ((b1.x - a1.x) * d2y - (b1.y - a1.y) * d2x) / denom;
    let u = ((b1.x - a1.x) * d1y - (b1.y - a1.y) * d1x) / denom;
    if t >= 0.0 && t <= 1.0 && u >= 0.0 && u <= 1.0 {
        Some((
            Position {
                x: a1.x + t * d1x,
                y: a1.y + t * d1y,
            },
            t,
        ))
    } else {
        None
    }
}

/// True if segment (a1→a2) properly crosses segment (b1→b2).
/// Uses strict interior intersection — shared endpoints don't count.
fn segments_cross(a1: Position, a2: Position, b1: Position, b2: Position) -> bool {
    let d1x = a2.x - a1.x;
    let d1y = a2.y - a1.y;
    let d2x = b2.x - b1.x;
    let d2y = b2.y - b1.y;
    let denom = d1x * d2y - d1y * d2x;
    if denom.abs() < 1e-10 {
        return false; // parallel
    }
    let t = ((b1.x - a1.x) * d2y - (b1.y - a1.y) * d2x) / denom;
    let u = ((b1.x - a1.x) * d1y - (b1.y - a1.y) * d1x) / denom;
    // Strict interior: exclude endpoints to avoid false positives at corners
    t > 1e-6 && t < 1.0 - 1e-6 && u > 1e-6 && u < 1.0 - 1e-6
}

