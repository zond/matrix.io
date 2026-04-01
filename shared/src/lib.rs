pub mod protocol_capnp {
    include!(concat!(env!("OUT_DIR"), "/schema/protocol_capnp.rs"));
}

pub type PlayerId = u64;

pub const PLAYER_SPEED: f64 = 12.5;
pub const VISIBILITY_RADIUS: f64 = 250.0;
pub const STARTING_TERRITORY_RADIUS: f64 = 2.5;
pub const CELL_SIZE: f64 = 30.0;
pub const KILL_DISTANCE: f64 = 0.15;

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Position {
    pub x: f64,
    pub y: f64,
}

// ---------------------------------------------------------------------------
// Protocol data types
// ---------------------------------------------------------------------------

pub struct PlayerStateData {
    pub id: PlayerId,
    pub position: Position,
    pub angle: f64,
    pub color: [u8; 3],
    pub trail: Vec<Position>,
    pub sprite_id: u32,
    pub has_crown: bool,
    pub boost_points: u8,
    pub boost_active: bool,
}

pub struct TerritoryRingData {
    pub player_id: PlayerId,
    pub color: [u8; 3],
    pub points: Vec<Position>,
    pub sprite_id: u32,
}

pub struct LeaderboardEntryData {
    pub name: String,
    pub value: u32,
}

pub struct LeaderboardData {
    pub by_area: Vec<LeaderboardEntryData>,
    pub by_kills: Vec<LeaderboardEntryData>,
}

pub enum ClientMsg {
    /// angle in radians, or f64::NAN to stop
    ChangeDirection(f64),
    Ping(f64),
    SetSprite(u32),
    SetName(String),
    ActivateBoost,
}

pub enum ServerMsg {
    Welcome {
        player_id: PlayerId,
        position: Position,
        angle: f64,
        color: [u8; 3],
    },
    Tick {
        players: Vec<PlayerStateData>,
        board_radius: f64,
    },
    TerritorySnapshot(Vec<TerritoryRingData>),
    PlayerKilled {
        player_id: PlayerId,
        killer_id: Option<PlayerId>,
    },
    Pong(f64),
    Leaderboard(LeaderboardData),
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

pub fn encode_server_msg(msg: &ServerMsg) -> Vec<u8> {
    let mut builder = capnp::message::Builder::new_default();
    {
        let mut root = builder.init_root::<protocol_capnp::server_message::Builder>();
        match msg {
            ServerMsg::Welcome {
                player_id,
                position,
                angle,
                color,
            } => {
                let mut w = root.init_welcome();
                w.set_player_id(*player_id);
                let mut pos = w.reborrow().init_position();
                pos.set_x(position.x);
                pos.set_y(position.y);
                w.set_angle(*angle);
                let mut col = w.reborrow().init_color();
                col.set_r(color[0]);
                col.set_g(color[1]);
                col.set_b(color[2]);
            }
            ServerMsg::Tick { ref players, board_radius } => {
                let mut t = root.init_tick();
                t.set_board_radius(*board_radius);
                let mut list = t.init_players(players.len() as u32);
                for (i, p) in players.iter().enumerate() {
                    let mut pb = list.reborrow().get(i as u32);
                    pb.set_id(p.id);
                    let mut pos = pb.reborrow().init_position();
                    pos.set_x(p.position.x);
                    pos.set_y(p.position.y);
                    pb.set_angle(p.angle);
                    let mut col = pb.reborrow().init_color();
                    col.set_r(p.color[0]);
                    col.set_g(p.color[1]);
                    col.set_b(p.color[2]);
                    pb.set_sprite_id(p.sprite_id);
                    pb.set_has_crown(p.has_crown);
                    pb.set_boost_points(p.boost_points);
                    pb.set_boost_active(p.boost_active);
                    let mut trail = pb.init_trail(p.trail.len() as u32);
                    for (j, t) in p.trail.iter().enumerate() {
                        let mut tb = trail.reborrow().get(j as u32);
                        tb.set_x(t.x);
                        tb.set_y(t.y);
                    }
                }
            }
            ServerMsg::TerritorySnapshot(rings) => {
                let ts = root.init_territory_snapshot();
                let mut list = ts.init_rings(rings.len() as u32);
                for (i, ring) in rings.iter().enumerate() {
                    let mut rb = list.reborrow().get(i as u32);
                    rb.set_player_id(ring.player_id);
                    rb.set_sprite_id(ring.sprite_id);
                    let mut col = rb.reborrow().init_color();
                    col.set_r(ring.color[0]);
                    col.set_g(ring.color[1]);
                    col.set_b(ring.color[2]);
                    let mut pts = rb.init_points(ring.points.len() as u32);
                    for (j, p) in ring.points.iter().enumerate() {
                        let mut pb = pts.reborrow().get(j as u32);
                        pb.set_x(p.x);
                        pb.set_y(p.y);
                    }
                }
            }
            ServerMsg::PlayerKilled {
                player_id,
                killer_id,
            } => {
                let mut pk = root.init_player_killed();
                pk.set_player_id(*player_id);
                pk.set_has_killer(killer_id.is_some());
                pk.set_killer_id(killer_id.unwrap_or(0));
            }
            ServerMsg::Pong(timestamp) => {
                root.set_pong(*timestamp);
            }
            ServerMsg::Leaderboard(lb) => {
                let mut l = root.init_leaderboard();
                let mut ba = l.reborrow().init_by_area(lb.by_area.len() as u32);
                for (i, e) in lb.by_area.iter().enumerate() {
                    let mut eb = ba.reborrow().get(i as u32);
                    eb.set_name(&e.name);
                    eb.set_value(e.value);
                }
                let mut bk = l.init_by_kills(lb.by_kills.len() as u32);
                for (i, e) in lb.by_kills.iter().enumerate() {
                    let mut eb = bk.reborrow().get(i as u32);
                    eb.set_name(&e.name);
                    eb.set_value(e.value);
                }
            }
        }
    }
    let mut output = Vec::new();
    capnp::serialize::write_message(&mut output, &builder).unwrap();
    output
}

pub fn encode_client_msg(msg: &ClientMsg) -> Vec<u8> {
    let mut builder = capnp::message::Builder::new_default();
    {
        let mut root = builder.init_root::<protocol_capnp::client_message::Builder>();
        match msg {
            ClientMsg::ChangeDirection(angle) => {
                root.set_change_direction(*angle);
            }
            ClientMsg::Ping(ts) => {
                root.set_ping(*ts);
            }
            ClientMsg::SetSprite(id) => {
                root.set_set_sprite(*id);
            }
            ClientMsg::SetName(ref name) => {
                root.set_set_name(name);
            }
            ClientMsg::ActivateBoost => {
                root.set_activate_boost(());
            }
        }
    }
    let mut output = Vec::new();
    capnp::serialize::write_message(&mut output, &builder).unwrap();
    output
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

pub fn decode_server_msg(bytes: &[u8]) -> capnp::Result<ServerMsg> {
    let reader =
        capnp::serialize::read_message(&mut &bytes[..], capnp::message::ReaderOptions::new())?;
    let root = reader.get_root::<protocol_capnp::server_message::Reader>()?;

    use protocol_capnp::server_message::Which;
    match root.which()? {
        Which::Welcome(r) => {
            let w = r?;
            let pos = w.get_position()?;
            let col = w.get_color()?;
            Ok(ServerMsg::Welcome {
                player_id: w.get_player_id(),
                position: Position {
                    x: pos.get_x(),
                    y: pos.get_y(),
                },
                angle: w.get_angle(),
                color: [col.get_r(), col.get_g(), col.get_b()],
            })
        }
        Which::Tick(r) => {
            let t = r?;
            let list = t.get_players()?;
            let mut players = Vec::with_capacity(list.len() as usize);
            for p in list.iter() {
                let pos = p.get_position()?;
                let col = p.get_color()?;
                let trail_r = p.get_trail()?;
                let mut trail = Vec::with_capacity(trail_r.len() as usize);
                for pt in trail_r.iter() {
                    trail.push(Position {
                        x: pt.get_x(),
                        y: pt.get_y(),
                    });
                }
                players.push(PlayerStateData {
                    id: p.get_id(),
                    position: Position {
                        x: pos.get_x(),
                        y: pos.get_y(),
                    },
                    angle: p.get_angle(),
                    color: [col.get_r(), col.get_g(), col.get_b()],
                    trail,
                    sprite_id: p.get_sprite_id(),
                    has_crown: p.get_has_crown(),
                    boost_points: p.get_boost_points(),
                    boost_active: p.get_boost_active(),
                });
            }
            let board_radius = t.get_board_radius();
            Ok(ServerMsg::Tick { players, board_radius })
        }
        Which::TerritorySnapshot(r) => {
            let ts = r?;
            let list = ts.get_rings()?;
            let mut rings = Vec::with_capacity(list.len() as usize);
            for ring in list.iter() {
                let col = ring.get_color()?;
                let pts = ring.get_points()?;
                let mut points = Vec::with_capacity(pts.len() as usize);
                for p in pts.iter() {
                    points.push(Position {
                        x: p.get_x(),
                        y: p.get_y(),
                    });
                }
                rings.push(TerritoryRingData {
                    player_id: ring.get_player_id(),
                    color: [col.get_r(), col.get_g(), col.get_b()],
                    points,
                    sprite_id: ring.get_sprite_id(),
                });
            }
            Ok(ServerMsg::TerritorySnapshot(rings))
        }
        Which::PlayerKilled(r) => {
            let pk = r?;
            let killer_id = if pk.get_has_killer() {
                Some(pk.get_killer_id())
            } else {
                None
            };
            Ok(ServerMsg::PlayerKilled {
                player_id: pk.get_player_id(),
                killer_id,
            })
        }
        Which::Pong(ts) => Ok(ServerMsg::Pong(ts)),
        Which::Leaderboard(r) => {
            let lb = r?;
            let ba_r = lb.get_by_area()?;
            let mut by_area = Vec::with_capacity(ba_r.len() as usize);
            for e in ba_r.iter() {
                by_area.push(LeaderboardEntryData {
                    name: e.get_name()?.to_string()?,
                    value: e.get_value(),
                });
            }
            let bk_r = lb.get_by_kills()?;
            let mut by_kills = Vec::with_capacity(bk_r.len() as usize);
            for e in bk_r.iter() {
                by_kills.push(LeaderboardEntryData {
                    name: e.get_name()?.to_string()?,
                    value: e.get_value(),
                });
            }
            Ok(ServerMsg::Leaderboard(LeaderboardData { by_area, by_kills }))
        }
    }
}

pub fn decode_client_msg(bytes: &[u8]) -> capnp::Result<ClientMsg> {
    let reader =
        capnp::serialize::read_message(&mut &bytes[..], capnp::message::ReaderOptions::new())?;
    let root = reader.get_root::<protocol_capnp::client_message::Reader>()?;

    use protocol_capnp::client_message::Which;
    match root.which()? {
        Which::ChangeDirection(angle) => Ok(ClientMsg::ChangeDirection(angle)),
        Which::Ping(ts) => Ok(ClientMsg::Ping(ts)),
        Which::SetSprite(id) => Ok(ClientMsg::SetSprite(id)),
        Which::SetName(name) => Ok(ClientMsg::SetName(name?.to_string()?)),
        Which::ActivateBoost(()) => Ok(ClientMsg::ActivateBoost),
    }
}
