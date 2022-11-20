use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use futures_util::{SinkExt, StreamExt, TryFutureExt};
use rand::{seq::SliceRandom, Rng};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;
use tokio_stream::wrappers::UnboundedReceiverStream;
use urlencoding::decode;
use warp::{
    hyper::Method,
    ws::{Message, WebSocket, Ws},
    Filter,
};

struct Bingo {
    answers: HashMap<String, bool>,
    players: HashMap<String, Player>,
    has_started: bool,
    width: usize,
    height: usize,
}

impl Bingo {
    pub fn new(answers: Vec<String>, width: usize, height: usize) -> Self {
        let mut map = HashMap::new();
        answers.iter().for_each(|a| {
            map.insert(a.to_string(), false);
        });
        Bingo {
            answers: map,
            players: HashMap::new(),
            has_started: false,
            width,
            height,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct BingoCreate {
    answers: Vec<String>,
    width: usize,
    height: usize,
}

impl BingoCreate {
    pub fn valid(&self) -> bool {
        self.answers.len() >= self.width
    }
}

struct Player {
    ws: UnboundedSender<Message>,
    grid: Vec<Vec<(String, bool)>>,
    admin: bool,
}

#[tokio::main]
async fn main() {
    let games: Arc<Mutex<HashMap<String, Bingo>>> = Arc::new(Mutex::new(HashMap::new()));
    let games = warp::any().map(move || games.clone());

    let cors = warp::cors()
        .allow_any_origin()
        .allow_headers(vec!["content-type"])
        .allow_methods(vec![Method::GET, Method::POST, Method::OPTIONS]);

    let options = warp::options().map(warp::reply).with(cors.clone());

    let create = warp::path("create")
        .and(warp::post())
        .and(warp::filters::body::json::<BingoCreate>())
        .and(games.clone())
        .map(
            |bg: BingoCreate, games: Arc<Mutex<HashMap<String, Bingo>>>| {
                if !bg.valid() {
                    return "Invalid configuration".to_string();
                }
                let bingo = Bingo::new(bg.answers, bg.width, bg.height);

                let mut games = games.lock().unwrap();

                let mut rng = rand::thread_rng();

                let game_id: String = (0..6)
                    .map(|_| rng.sample(rand::distributions::Alphanumeric) as char)
                    .collect();

                games.insert(game_id.to_string(), bingo);

                game_id
            },
        )
        .with(cors.clone());

    let get_rooms = warp::path("rooms")
        .and(warp::get())
        .and(games.clone())
        .map(|games: Arc<Mutex<HashMap<String, Bingo>>>| {
            let games = games.lock().unwrap();
            serde_json::ser::to_string(&games.keys().collect::<Vec<&String>>()).unwrap()
        })
        .with(cors.clone());

    let join = warp::path("join")
        .and(warp::path::param::<String>())
        .and(warp::path::param::<String>())
        .and(warp::ws())
        .and(games)
        .map(
            |game_id: String, name: String, ws: Ws, games: Arc<Mutex<HashMap<String, Bingo>>>| {
                let name = decode(&name).unwrap().to_string();
                {
                    let games = games.lock().unwrap();
                    let bingo = games.get(&game_id);
                    if let Some(bingo) = bingo {
                        if bingo.has_started {
                            drop(games);
                            panic!("The game {} already started", game_id);
                        } else if bingo.players.contains_key(&name) {
                            drop(games);
                            panic!("The game {} already contains {}", game_id, name);
                        }
                    } else {
                        drop(games);
                        panic!("The game {} doesn't exist", game_id);
                    }
                }
                println!("{} {}", game_id, name);
                ws.on_upgrade(move |websocket| user_connected(websocket, name, game_id, games))
            },
        )
        .with(cors);

    let routes = options.or(join).or(create).or(get_rooms);

    warp::serve(routes).run(([0, 0, 0, 0], 7654)).await;
}

async fn user_connected(
    ws: WebSocket,
    name: String,
    game_id: String,
    games: Arc<Mutex<HashMap<String, Bingo>>>,
) {
    let (mut user_ws_tx, mut user_ws_rx) = ws.split();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut rx = UnboundedReceiverStream::new(rx);
    tokio::task::spawn(async move {
        while let Some(message) = rx.next().await {
            user_ws_tx
                .send(message)
                .unwrap_or_else(|e| {
                    eprintln!("websocket send error: {}", e);
                })
                .await;
        }
    });

    {
        let mut games = games.lock().unwrap();
        let bingo = games.get_mut(&game_id).unwrap();

        let mut rng = rand::thread_rng();

        let mut grid = vec![];

        for _i in 0..bingo.height {
            let mut row: Vec<(String, bool)> = bingo
                .answers
                .keys()
                .collect::<Vec<&String>>()
                .choose_multiple(&mut rng, bingo.width * 8 / 10)
                .map(|e| (e.to_string(), false))
                .collect();
            while row.len() < bingo.width {
                row.insert(rng.gen_range(0..row.len()), ("".to_string(), true));
            }

            grid.push(row);
        }
        println!("{grid:?}");

        let player = Player {
            ws: tx,
            grid: grid.clone(),
            admin: bingo.players.is_empty(),
        };
        let answers = if player.admin {
            Some(bingo.answers.keys().cloned().collect::<Vec<String>>())
        } else {
            None
        };

        player
            .ws
            .send(Message::text(
                serde_json::ser::to_string(&WebSocketServerMessage::Init {
                    players: bingo.players.keys().cloned().collect(),
                    grid: grid
                        .iter()
                        .map(|e| e.iter().map(|r| r.0.clone()).collect())
                        .collect(),
                    answers,
                })
                .unwrap(),
            ))
            .unwrap();

        bingo.players.insert(name.clone(), player);

        broadcast(WebSocketServerMessage::Joined(name.clone()), bingo);
    }

    while let Some(result) = user_ws_rx.next().await {
        let msg = match result {
            Ok(msg) => msg,
            Err(_) => {
                break;
            }
        };
        user_message(&name, &game_id, msg, games.clone()).await;
    }

    user_disconnected(&name, &game_id, games).await;
}

/// The JSON will look like this :
/// ```json
/// {
///     "TickBox": [3, 2]
/// }
/// ```
/// or
/// ```json
/// {
///     "Message": "Weeeeeeeesh"
/// }
/// ```
#[derive(Deserialize)]
enum WebSocketClientMessage {
    TickBox(usize, usize),
    Answer(String),
    Start,
    Message(String),
    Bingo,
}

/// The JSON will look like this :
/// ```json
/// {
///     "Bingo": "Aznogg"
/// }
/// ```
/// or
/// ```json
/// {
///     "Message": ["Aznogg", "Weeeeeeeesh"]
/// }
/// ```
#[derive(Serialize)]
enum WebSocketServerMessage {
    Bingo(String),
    Init {
        players: Vec<String>,
        grid: Vec<Vec<String>>,
        answers: Option<Vec<String>>,
    },
    Answer(String),
    Joined(String),
    Left(String),
    Started,
    Message(String, String),
}

async fn user_message(
    name: &str,
    game_id: &str,
    msg: Message,
    games: Arc<Mutex<HashMap<String, Bingo>>>,
) {
    let msg = if let Ok(s) = msg.to_str() {
        s
    } else {
        return;
    };

    let msg: WebSocketClientMessage = serde_json::de::from_str(msg).unwrap();

    let mut games = games.lock().unwrap();

    let bingo = games.get_mut(game_id).unwrap();

    match msg {
        WebSocketClientMessage::TickBox(x, y) => {
            let player = bingo.players.get_mut(name).unwrap();
            if x < bingo.height && y < bingo.width {
                player.grid[x][y].1 = true;
            }
        }
        WebSocketClientMessage::Bingo => {
            let player = bingo.players.get(name).unwrap();
            for row in &player.grid {
                if row
                    .iter()
                    .all(|e| e.0.is_empty() || (e.1 && *bingo.answers.get(&e.0).unwrap()))
                {
                    broadcast(WebSocketServerMessage::Bingo(name.to_string()), bingo);
                }
            }
        }
        WebSocketClientMessage::Answer(answer_str) => {
            let player = bingo.players.get(name).unwrap();
            if player.admin {
                if let Some(answer) = bingo.answers.get_mut(&answer_str) {
                    *answer = true;
                    broadcast(WebSocketServerMessage::Answer(answer_str), bingo);
                }
            }
        }
        WebSocketClientMessage::Message(msg) => {
            broadcast(
                WebSocketServerMessage::Message(name.to_string(), msg),
                bingo,
            );
        }
        WebSocketClientMessage::Start => {
            bingo.has_started = true;
            broadcast(WebSocketServerMessage::Started, bingo);
        }
    }
}

async fn user_disconnected(name: &str, game_id: &str, games: Arc<Mutex<HashMap<String, Bingo>>>) {
    let mut games = games.lock().unwrap();
    let bingo = games.get_mut(game_id).unwrap();
    bingo.players.remove(name);
    broadcast(WebSocketServerMessage::Left(name.to_string()), bingo);
}

fn broadcast(msg: WebSocketServerMessage, bingo: &Bingo) {
    bingo.players.iter().for_each(|p| {
        p.1.ws
            .send(Message::text(serde_json::ser::to_string(&msg).unwrap()))
            .unwrap();
    });
}
