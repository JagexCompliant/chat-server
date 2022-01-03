extern crate tokio;
#[macro_use]
extern crate futures;
extern crate bytes;

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};
use tokio_util::codec::{Framed, LinesCodec, LinesCodecError};

use futures::{ SinkExt, Stream, StreamExt};
use std::collections::HashMap;


use std::error::Error;


use std::{env, io};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use futures::stream::PollNext;


#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {

    let state = Arc::new(Mutex::new(Shared::new()));
    let addr = env::args().nth(1).unwrap_or("127.0.0.0.1".to_string());
    let mut listener = TcpListener::bind(&addr).await?;


    println!("server is running on {}", addr);

    loop{
        let (stream, addr) = listener.accept().await?;

        let state = Arc::clone(&state);

        tokio::spawn(async move{
            if let Err(e) = process(state, stream, addr).await{
                println!("Error occured; error = {:?}", e)
            }
        });
    }
}

type Tx = mpsc::UnboundedSender<String>;
type Rx = mpsc::UnboundedReceiver<String>;

#[derive(Clone)]
struct ConnectRecord{
    addr: SocketAddr,
    username: String,
    curr_room: String,
    tx: Tx
}

#[derive(Clone)]
struct Room{
    name: String,
    participants: Vec<ConnectRecord>,
}

impl Room{

    fn connected_users(&self) -> Vec<String>{
        self.participants.iter()
            .map(|x| x.username.clone())
            .collect()
    }

    async fn broadcast(&mut self, sender: SocketAddr, message: &str){
        for peer in self.participants.iter_mut() {
            if peer.addr != sender {
                let _ = peer.tx.send(message.into());
            }
        }
    }
}
#[derive(Debug)]
enum ClientError{
    RoomExists(String),
    RoomDoesNotExist(String),
    AlreadyJoined,
    Disconnected,
}

struct Shared{
    peers: HashMap<SocketAddr, ConnectRecord>,
    rooms: HashMap<String, Arc<Mutex<Room>>>,
    peers_to_room: HashMap<SocketAddr, String>
}

impl Shared {
    fn new() -> Self {
        let mut rooms = HashMap::new();
        rooms.insert("lobby".to_string(), Arc::new(Mutex::new(Room { name: "Lobby".to_string(), participants: vec!() })));
        Shared {
            peers: HashMap::new(),
            rooms,
            peers_to_room: HashMap::new(),
        }
    }
    fn connected_users(&self) -> Vec<String> {
        self.peers.values()
            .map(|x| x.username.clone())
            .collect()
    }
    fn rooms(&self) -> Vec<String> {
        self.rooms.keys().cloned().collect()
    }

    fn create_room(&mut self, name: &str) -> Result<(), ClientError> {
        if self.rooms.contains_key(name) {
            Err(ClientError::RoomExists(name.to_string()))
        } else {
            self.rooms.insert(name.to_string(), Arc::new(Mutex::new(
                Room { name: name.to_string(), participants: vec!() })));
            Ok(())
        }
    }
    async fn join_room(&mut self, name: &str, sender: SocketAddr) -> Result<Arc<Mutex<Room>>, ClientError> {
        let c_rec = self.peers.get(&sender)
            .ok_or_else(|| ClientError::Disconnected)?;
        if c_rec.curr_room == *name {
            return Err(ClientError::AlreadyJoined);
        }
        {
            let old_opt =
                self.peers_to_room.get(&c_rec.addr).and_then(|x| self.rooms.get(x));
            if old_opt.is_some() {
                let mut old_room = old_opt.unwrap().lock().await;
                println!("{} leaving {}", c_rec.username, old_room.name);
                old_room.participants.retain(|room_rec| {
                    room_rec.addr != c_rec.addr
                });
                println!("Participants in {}: {}", old_room.name,
                         old_room.participants.iter().map(|x| x.username.clone()).collect::<Vec<_>>().join(", "));
            }
        }
        let room_arc = self.rooms.get_mut(name)
            .ok_or_else(|| ClientError::RoomDoesNotExist(name.to_string()))?;
        let mut room = room_arc.lock().await;

        println!("{} joining {}", c_rec.username, room.name);
        room.participants.push(c_rec.clone());
        self.peers_to_room.insert(c_rec.addr, room.name.clone());

        println!("{} joining {}", room.name, room.participants.iter().map(|x| x.username.clone()).collect::<Vec<_>>().join(", "));

        Ok(Arc::clone(room_arc))
    }

    async fn leave_room(&mut self, sender: SocketAddr) -> Result<Arc<Mutex<Room>>, ClientError> {
        self.join_room("lobby", sender).await
    }
    async fn  _global_broadcast(&mut self, sender: SocketAddr, message: &str){
        for peer in self .peers.iter_mut(){
            if *peer.0 != sender {
                let _ = peer.1.tx.send(message.into());
            }
        }
    }
}

struct Peer{
    lines: Framed<TcpStream, LinesCodec>,
    rx: Rx,
}

impl Peer{
    async fn new(
        state: Arc<Mutex<Shared>>,
        username: &str,
        lines: Framed<TcpStream, LinesCodec>,
    ) -> io::Result<Peer>{
        let addr = lines.get_ref().peer_addr()?;
        let (tx, rx) = mpsc::unbounded_channel();
        state.lock().await.peers.insert(addr, ConnectRecord{addr, username: username.to_string(), curr_room: "none".to_string(), tx});
        Ok(Peer {lines, rx})
    }
}
#[derive(Debug)]
enum Command{
    WhoAll,
    WhoRoom,
    JoinRoom(String),
    CreateRoom(String),
    ListRooms,
    LeaveRoom,
    Help,
    Unknown
}
impl Command {
    fn from(s: &str) -> Command {
        match s {
            "\\whoAll" => Command::WhoAll,
            "\\gwho" => Command::WhoAll,
            "\\who" => Command::WhoRoom,
            "\\leave" => Command::LeaveRoom,
            "\\rooms" => Command::ListRooms,
            "\\help" => Command::Help,
            x if x.starts_with("\\create ")
            => Command::CreateRoom(x.replace("\\create ", "").trim().to_string()),
            y if y.starts_with("\\join ")
            => Command::JoinRoom(y.replace("\\join ", "").trim().to_string()),
            _ => Command::Unknown,
        }
    }
}

#[derive(Debug)]
enum Message{
    Broadcast(String),
    Received(String),
    Command(String),
}
impl Stream for Peer {
    type Item = Result<Message, LinesCodecError>;


    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Poll::Ready(Some(v)) = Rx::poll_next_unpin(&mut self.rx, cx) {
            return Poll::Ready(Some(Ok(Message::Received(v))));
        }
        let result: Option<_> = futures::ready!(self.lines.poll_next_unpin(cx));

        Poll::Ready(match result {
            Some(Ok(message)) => {
                if message.starts_with('\\') {
                    Some(Ok(Message::Command(message)))
                } else {
                    Some(Ok(Message::Broadcast(message)))
                }
            },
            Some(Err(e)) => Some(Err(e)),
            None => None,
        })
    }
}

async fn process(
    state: Arc<Mutex<Shared>>,
    stream: TcpStream,
    addr: SocketAddr
)-> Result<(), Box<dyn Error>> {
    let mut lines = Framed::new(stream, LinesCodec::new());

    lines.send(String::from("Please enter your username:")).await?;
    let username = match lines.next().await {
        Some(Ok(line)) => line,
        _ => {
            println!("Failed to get username from {}", addr);
            return Ok(());
        }
    };

    let mut peer = Peer::new(state.clone(), &username, lines).await?;

    let mut room: Arc<Mutex<Room>>;
    let num_users: usize;

    {
        let mut state = state.lock().await;
        room = state.join_room("lobby", addr).await.unwrap();
        num_users = state.connected_users().len();
    }
    {
        let mut room_access = room.lock().await;
        let msg = format!("{} has joined the chat", username);
        println!("{}", msg);
        let announce = room_access.broadcast(addr, &msg);
        let msg = format!("welcome this many niggers {} connected.", num_users - 1);
        let welcome = peer.lines.send(msg);

        join!(announce, welcome).1?;
    }
    while let Some(result) = peer.next().await {
        match result {
            Ok(Message::Received(msg)) => {
                peer.lines.send(msg).await?;
            },
            Ok(Message::Broadcast(msg)) => {
                broadcast_room(addr, &username, &msg, &room).await;
            },
            Ok(Message::Command(msg)) => {
                match Command::from(&msg.clone()) {
                    Command::WhoAll => {
                        who_all(&state, &mut peer).await?;
                    },
                    Command::WhoRoom => {
                        who_room(&room, &mut peer).await?;
                    },
                    Command::JoinRoom(new_room) => {
                        room = join_room(addr, &state, &mut peer, &new_room, room).await?;
                    }
                    Command::LeaveRoom => {
                        room = leave_room(addr, &state, &mut peer, room).await?;
                    }
                    Command::CreateRoom(name) => {
                        create_room(&state, &mut peer, &name).await?;
                    }
                    Command::ListRooms => {
                        list_rooms(&state, &mut peer).await?;
                    }
                    Command::Help => {
                        print_help(&mut peer).await?;
                    }
                    Command::Unknown => {
                        let msg = format!(">> unrecognized command{}", msg);
                        peer.lines.send(msg).await?;
                    },
                }
            }
            Err(e) => {
                println!("This Nigger {}; causing the nigger error = {:?}", username, e);
            },
        }
    }
    {
        let mut state = state.lock().await;
        let mut room_access = room.lock().await;
        state.peers.remove(&addr);
        let msg = format!("{} has left the nigga room", username);
        println!("{}", msg);
        room_access.broadcast(addr, &msg).await;
    }
    Ok(())
}

async fn broadcast_room(addr: SocketAddr, username: &str, msg: &str, room: &Arc<Mutex<Room>>){
    let mut room_access = room.lock().await;
    let msg = format!("{}: {}", username, msg);
    room_access.broadcast(addr, &msg).await;

}

async fn who_all(state: &Arc<Mutex<Shared>>, peer: &mut Peer)-> Result<(), Box<dyn Error>>{
    let state = state.lock().await;
    let msg = format!(">> online now: {}", state.connected_users().join(", "));
    peer.lines.send(msg).await?;
    Ok(())
}
async fn who_room(room: &Arc<Mutex<Room>>, peer: &mut Peer)-> Result<(), Box<dyn Error>>{
    let room_access = room.lock().await;
    let msg = format!(">> in {}: {}", room_access.name, room_access.connected_users().join(","));
    peer.lines.send(msg).await?;
    Ok(())
}
async fn list_rooms(state: &Arc<Mutex<Shared>>, peer: &mut Peer) -> Result<(), Box<dyn Error>>{
    let state = state.lock().await;

    let msg = format!(">> {}", state.rooms().join(", "));
    peer.lines.send(msg).await?;
    Ok(())
}
async fn join_room(addr: SocketAddr, state: &Arc<Mutex<Shared>>, peer: &mut Peer, new_room: &str, curr_room: Arc<Mutex<Room>>) -> Result<Arc<Mutex<Room>>, Box<dyn Error>>{
    let mut state = state.lock().await;
    match state.join_room(&new_room, addr).await{
        Ok(room) => {
            let msg = format!(">> joined {}", new_room);
            peer.lines.send(msg).await?;
            Ok(room)
        },
        Err(e) => {
            let msg = match e {
                ClientError::RoomDoesNotExist(room) => format!("Nigger {}error: No nigger space", room),
                ClientError::RoomExists(_) => "Nigger error: Niggers already exist".to_string(),
                ClientError::Disconnected =>"We kissing our niggers goodnight".to_string(),
                ClientError::AlreadyJoined => format!("Nigga you already in your nigger ass room {}", new_room),
            };
            peer.lines.send(msg).await?;
            Ok(curr_room)
        }
    }
}
async fn leave_room(addr: SocketAddr, state: &Arc<Mutex<Shared>>, peer: &mut Peer, curr_room: Arc<Mutex<Room>>) -> Result<Arc<Mutex<Room>>, Box<dyn Error>>{
    let mut state = state.lock().await;
    match state.leave_room(addr).await {
        Ok(room) => {
            peer.lines.send(">> joined the hood".to_string()).await?;
            Ok(room)
        },
        Err(e) =>{
            let msg = match e {
                ClientError::RoomDoesNotExist(_) => "Nigger error: No nigger space".to_string(),
                ClientError::RoomExists(_) => "Nigger error: Niggers already exist".to_string(),
                ClientError::Disconnected =>"We kissing our niggers goodnight".to_string(),
                ClientError::AlreadyJoined => "Nigga you already in your nigger ass lobby ".to_string(),
            };
            peer.lines.send(msg).await?;
            Ok(curr_room)
        }
    }
}
async fn create_room(state: &Arc<Mutex<Shared>>, peer: &mut Peer, name: &str)->
Result<(), Box<dyn Error>>{
    let mut state = state.lock().await;
    match state.create_room(name){
        Err(e)=>{
            let msg = match e{
                ClientError::RoomDoesNotExist(_) => "Nigger error: No nigger space".to_string(),
                ClientError::RoomExists(_) => "Nigger error: Niggers already exist".to_string(),
                ClientError::Disconnected =>"We kissing our niggers goodnight".to_string(),
                ClientError::AlreadyJoined => "Nigga you already in your nigger ass lobby".to_string(),
            };
            peer.lines.send(msg).await?;
            Ok(())
        }
        Ok(_) =>{
            let msg = format!(">> created room named '{}'; use \\join {} to join", name, name);
            peer.lines.send(msg).await?;
            Ok(())
        }
    }
}
async fn print_help(peer: &mut Peer) -> Result<(), Box<dyn Error>>{
    peer.lines.send(">> type and enter to chat\n
                          >> \\Help          - show this message \n\
                          >> \\rooms         - list rooms avail \n\
                          >> \\create [room] - create a new room \n\
                          >> \\join [room]   - join an available room \n\
                          >> \\leave         - leave the room, return to the lobby \n\
                          >> \\who           - see who is in the current room \n\
                          >> \\gwho          - see who is online".to_string()).await?;
    Ok(())
}







