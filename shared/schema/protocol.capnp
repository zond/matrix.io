@0xd9030154e8a5d937;

struct Position {
    x @0 :Float64;
    y @1 :Float64;
}

struct Color {
    r @0 :UInt8;
    g @1 :UInt8;
    b @2 :UInt8;
}

struct PlayerState {
    id @0 :UInt64;
    position @1 :Position;
    angle @2 :Float64;
    color @3 :Color;
    trail @4 :List(Position);
    spriteId @5 :UInt32;
    hasCrown @6 :Bool;
}

struct TerritoryRing {
    playerId @0 :UInt64;
    color @1 :Color;
    points @2 :List(Position);
    spriteId @3 :UInt32;
}

struct LeaderboardEntry {
    name @0 :Text;
    value @1 :UInt32;
}

struct Leaderboard {
    byArea @0 :List(LeaderboardEntry);
    byKills @1 :List(LeaderboardEntry);
}

struct ClientMessage {
    union {
        changeDirection @0 :Float64;
        ping @1 :Float64;
        setSprite @2 :UInt32;
        setName @3 :Text;
    }
}

struct ServerMessage {
    union {
        welcome @0 :Welcome;
        tick @1 :Tick;
        territorySnapshot @2 :TerritorySnapshot;
        playerKilled @3 :PlayerKilled;
        pong @4 :Float64;
        leaderboard @5 :Leaderboard;
    }
}

struct Welcome {
    playerId @0 :UInt64;
    position @1 :Position;
    angle @2 :Float64;
    color @3 :Color;
}

struct Tick {
    players @0 :List(PlayerState);
}

struct TerritorySnapshot {
    rings @0 :List(TerritoryRing);
}

struct PlayerKilled {
    playerId @0 :UInt64;
    killerId @1 :UInt64;
    hasKiller @2 :Bool;
}
