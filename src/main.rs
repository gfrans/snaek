#![feature(destructuring_assignment)]
#![feature(or_patterns)]
#![feature(duration_saturating_ops)]
#![feature(bindings_after_at)]

// XXX what is the convention for spacing within {}?
use std::time::{Duration, Instant};
use std::io::{self, stdout, Stdout, stdin, Stdin, Read, Write};
use termion::raw::IntoRawMode;
use termion::clear;
use std::thread::{self, sleep};
use std::sync::{Arc, Mutex};
use std::process::exit;
use rand::Rng;
use std::char::from_u32;
use std::collections::HashMap;
use std::net::{ UdpSocket, ToSocketAddrs, SocketAddr };
use serde::{Serialize, Deserialize};
use std::str::{ self, FromStr };
use std::fmt::Debug;
use clap::{ App, Arg };

// struct representing a player
#[derive(Debug)]
struct Snaek {
    name: String,
    segments: Vec<(usize, usize)>,
    symbol: char,
    score: usize,
    last_input: char,
    alive: bool,
    // XXX add field for disconnected/connected/idle?
    // XXX add field for latest vote
    // XXX add field for Instant last heard from; check duration
}

// enum for packet types and their contents
#[derive(Debug, Serialize, Deserialize)]
enum Packet {
    Join(String),
    Move(char),
    Vote(char),
	Reply(NetResult),
	State(Vec<Vec<char>>),
}

// enum for network-related erors
// XXX use result to indicate success/failure
#[derive(Debug, Serialize, Deserialize)]
enum NetResult {
    NameTooLong, // client should catch this
    NameInUse,
    NameEmpty, // client should catch this
    NotJoined, // client should catch this
    FailedToJoin,
    Joined,
}

// fn to return a random emoji character
//
// currently only selects from a few ranges w/ printable, square characters
fn random_emoji() -> char {
    let mut rng = rand::thread_rng();

    match rng.gen_range(0..3) {
        0 => from_u32(rng.gen_range(0x1f600..=0x1f64f)).unwrap(),
        1 => from_u32(rng.gen_range(0x1f910..=0x1f93a)).unwrap(),
        2 => from_u32(rng.gen_range(0x1f442..=0x1f4f7)).unwrap(),
        _ => ' ', // can never happen... is there a way to avoid this arm?
    }
}

// fn to monitor UDP socket for packets from a certain connection
// XXX make this return an iterator that returns packets; when it's exahusted, done processing -- too fancy; KISS
// * take the connection from main socket thread
// * deserialize a Packet and return it
fn get_packet(sock: &UdpSocket) -> Option<(SocketAddr, Packet)> {
    // buffer to hold packet data
    // XXX this may need to be MUCH bigger
    let mut data = [0; 10_000];

    // receive client packet; socket should be non-blocking
    match sock.recv_from(&mut data) {
        Ok((num_bytes, src_addr)) => {
            println!("received {:?} bytes from {:?}\r", num_bytes, src_addr);

            // deserialize client packet
            let deserialized: Packet = serde_json::from_str(str::from_utf8(&data[..num_bytes]).unwrap()).unwrap();
            println!("deserialized: {:?}\r", deserialized);

            // return the src and packet contents
            Some((src_addr, deserialized))
        },
        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
            println!("couldn't recv data\r");
            None
        },
        Err(e) => panic!("encountered IO error: {}", e),
    }
}

// This should happen in the main thread (or another func):
// * monitor the connection for votes moves and joins
// * update last_input and last_vote according to packet
// * set timeout on the recv; if it hits, set disconnected field and exit thread
// * main thread will examine each player
//   * if disconnected, remove from player hash and continue
//   * if alive, process last_input and last_vote for each player
//   * send updates to each player after state is updated (new thread?)
// XXX fn to monitor for join packets
// * copy socket from main thread
// * peek packets looking for joins
//   * if it's a join, process and remove it
//   * if it's not, leave it be
// XXX are packets in a proper queue (if player thread doesn't read, does it block others?)
// XXX update timeout for each player that send a packet
// XXX somehow a 2-seg snake can collide with itself when spamming inputs
//   * it's because last_input is updated multiple times per tick
//   * 1st time, they turn perpendicular to travel
//   * 2nd time, they turn back into themselves
//   * need to use location of current head; prevent them from turning into their old head

// eventually world would be a struct
// * vec of vecs for rendering objects in the world
// * collection of player structs
// * collection of other world objects
// * methods for moving each player
// * methods for checking for collisions
fn init_world(v: &mut Vec<Vec<char>>, w: usize, h: usize, players: &mut HashMap<SocketAddr, Snaek>) -> () {
    let mut rng = rand::thread_rng();

    // reset the world array
    v.clear();

    // fill the world with empty squares
    v.reserve(h as usize);
    for _ in 0..h {
        let mut row_vec = Vec::with_capacity(w as usize);

        for _ in 0..w {
            row_vec.push('⬜');
        }

        v.push(row_vec);
    }

    // create a starting position for each player
    'snakes: for (_, snaek) in players.iter_mut() {
        // need to reset the player positions
        snaek.segments.clear();

        // find an empty square
        'search: loop {
            let start_x = rng.gen_range(0..w);
            let start_y = rng.gen_range(0..h);

            if v[start_y][start_x] != '⬜'{
                continue;
            }

            // assign starting position to player
            snaek.segments.push((start_x, start_y));

            // set player symbol in world
            v[start_y][start_x] = snaek.symbol;

            // RISE FROM UR GRAEV, SNAEK!
            snaek.alive = true;

            break 'search;
        }
    }

    // add fruits to the world
    let mut fruit_count = 0;

    // XXX fruit count should depend on player count
    loop {
        if fruit_count == 10 {
            break;
        }

        let fruit_x = rng.gen_range(0..w);
        let fruit_y = rng.gen_range(0..h);

        if v[fruit_y][fruit_x] != '⬜' {
            continue;
        }

        v[fruit_y][fruit_x] = '🍎';
        fruit_count += 1;
    }
}

// get input from user
// * read raw input (no need to hit enter)
// * run in a separate thread and only store the last value
// * send each input character to the server
// XXX limit number of characters sent per tick?
fn get_input(s: UdpSocket) -> () {
    // iterate over bytes
    for b in stdin().lock().bytes() {
        // read bytes as chars
        let c = b.unwrap() as char;

        // create a move packet for the input
        let packet = Packet::Move(c);
        let mut serialized = serde_json::to_string(&packet).unwrap();

        // send the packet to the server
        s.send(serialized.as_bytes()).expect("couldn't send input");

        // end the input thread if 'q' is read
        if c == 'q' {
			exit(0);
            //return;
        }
    }
}

fn kill_snaek(v: &mut Vec<Vec<char>>, player: &mut Snaek) -> () {
    for (x,y) in &player.segments {
        v[*y][*x] = '💩';
    }

    player.alive = false;
}

// spawn fruit, move snaekes
// * spawn more fruit if the fruit count is too low
// * need to track this in world state
fn update_state(v: &mut Vec<Vec<char>>, players: &mut HashMap<SocketAddr, Snaek>) -> () {
    // hash to store new snake head positions
    let mut heads: HashMap<(usize, usize), Vec<SocketAddr>> = HashMap::new();

    // iterate over the snakes
    for (k, p) in players.iter_mut() {
        // don't update state if the snaek is daed
        if !p.alive {
            continue;
        }
       
        // get old head coordinates
        let (old_x, old_y) = p.segments.last().unwrap();

        // new coord is next grid square in new direction
        // if next square is OOB, snaek is dead
        let (new_x, new_y, alive) = match p.last_input {
            'w' if *old_y > 0 => (*old_x, *old_y-1, true),
            'a' if *old_x > 0 => (*old_x-1, *old_y, true),
            's' if *old_y < (v.len()-1) => (*old_x, *old_y+1, true),
            'd' if *old_x < (v[0].len()-1) => (*old_x+1, *old_y, true),
            ' ' => (*old_x, *old_y, true),
            _ => (*old_x, *old_y, false),
        };

        // kill the snake if it hit a wall
        if !alive {
            kill_snaek(v, p);
            continue;
        }

        // add postion to heads hash
        // * if it's already present, add to existing list
        // * otherwise, create a new list with just this snaek
        match heads.get_mut(&(new_x, new_y)) {
            Some(list) => list.push(*k),
            _ => {
                heads.insert((new_x, new_y), vec![*k]);
            }
        };
    }

    // iterate over heads
    // if a list has more than 1 entry, kill dem snaekes
    // otherwise, check if the snaek hit an apple or other item
    for ((x, y), list) in heads.iter() {
        match list.len() {
            1 => {
                // move the snaek
                //
                // check for collisions
                // XXX w/ multiple players, need to compare all heads after all moves
                let p = players.get_mut(list.first().unwrap()).unwrap();
                match v[*y][*x] {
                    '🍎' => {
                        p.segments.insert(0, p.segments[0]);
                        p.score += 1;
                    },
                    '⬜' => (),
                    _ => {
                        if p.last_input != ' ' {
                            kill_snaek(v, p);
                        }
                        continue;
                    },
                }
                
                // push next square in new direction into segments
                p.segments.push((*x, *y));

                // remove tail segment and save it
                let (old_x, old_y) = p.segments.remove(0);

                // use new coord to set world vector to player symbol
                v[*y][*x] = p.symbol;

                // use coords from removed element to set world vector to '.'
                v[old_y][old_x] = '⬜';
            },
            _ => { 
                // kill dem snaekes 
                for snaek in list {
                    kill_snaek(v, players.get_mut(&snaek).unwrap());
                }
            },
        }
    }
}

// send updated world state to all players
fn notify_players(v: &Vec<Vec<char>>, p: &HashMap<SocketAddr, Snaek>, s: &UdpSocket) -> () {
	// serialize the world state
    let mut serialized = serde_json::to_string(&Packet::State(v.to_vec())).unwrap();
	println!("serialized world size: {}", serialized.len());

    // iterate over the snaekes
    for (address, _) in p.iter() {
		// send the snaek the current world state
		s.send_to(serialized.as_bytes(), address);
	}
}

// print game world; could be an instance method
// print snaek scores as well, or is this another method?
fn print_world(v: &Vec<Vec<char>>) -> () {
    write!(stdout(), "{}", clear::All);
    for row in v {
        for col in row {
            print!("{}", col);
        }
        println!("\r");
    };
}

fn main() {
    let args = App::new("snaek")
        .version("0.1")
        .about("play with my snaek!")
        .arg(Arg::with_name("server")
             .short("s")
             .long("server")
             .help("runs as snaek server (default: client)")
             .takes_value(false)
             .required(false))
        .arg(Arg::with_name("address")
             .short("a")
             .long("address")
             .help("address to connect to/host from (default: localhost)")
             .takes_value(true)
             .required(true))
        .arg(Arg::with_name("port")
             .short("p")
             .long("port")
             .help("port to connect to/host on (default: random)")
             .takes_value(true)
             .required(true))
        .get_matches();

    let mut address = args.value_of("address").unwrap().to_string();
    let mut port = args.value_of("port").unwrap().to_string();

    // append the port to the address
    address.push(':');
    address.push_str(&port);

    // client mode
    if !args.is_present("server") {
        // this seems to work; not sure why result needs to be saved
        let _stdout = stdout().into_raw_mode().unwrap();

        // join the server
        let packet = Packet::Join(String::from("scrubonics"));
        let serialized = serde_json::to_string(&packet).unwrap();

        // create UDP socket to represent client
		println!("binding the client socket");
        let sock = UdpSocket::bind("192.168.100.201:6969").expect("couldn't bind client");

        // bind client socket to server socket
		println!("connecting to server: {}", address);
        sock.connect(&address).expect("couldn't connect to server");

        // send client join packet
		println!("sending the server a join");
        sock.send(serialized.as_bytes()).expect("couldn't send data");

        // ensure the join succeeded
		// XXX set a timeout on this socket
        match get_packet(&sock) {
        	Some((_, Packet::Reply(NetResult::Joined))) => {
				println!("joined {}\r", address);
			},
			Some((_, Packet::Reply(r))) => {
				println!("failed to join {}: {:?}\r", address, r);
				exit(-1);
			},
			_ => {
				println!("failed to join for an unknown reason");
				exit(-1);
			},
		}

        // start the input thread
		let sock_copy = sock.try_clone().expect("couldn't clone the client socket");
        let handler = thread::spawn(move || {
            get_input(sock_copy);
        });

		// client loop
		'client: loop {
			let start = Instant::now();

            // deserialize server packet
            match get_packet(&sock) {
				// process game world packet and print it
				Some((_, Packet::State(v))) => {
					print_world(&v);
				},
				_ => (),
			}

			// XXX need a better way to quit client/server
			//if dir == 'q' {
			//	// CLIENT-ONLY
			//	handler.join().unwrap();
			//	break 'game;
			//}

			println!("loop time: {:?}\r", start.elapsed());
		}

		exit(0);
    }

    // server mode
    let height = 40;
    let width = 40;
    let tick_ms = 100;
    let mut world: Vec<Vec<char>> = Vec::with_capacity(height);

    // XXX this will have to be Arc<Mutex<HashMap>>
    // or, just handle new connections in the main thread
    // keep it simple for now
    // use SocketAddr of each player as the key
    let mut players = HashMap::new();

    // create a UDP socket to receive packets using configured addr/port
    let mut srv_sock = UdpSocket::bind(address).expect("couldn't bind server");
    srv_sock.set_nonblocking(true).expect("couldn't set socket to non-blocking mode");

    // flag indicating game should be reset
    let mut reset = true;

    // game loop
    'game: loop {
        let start = Instant::now();

        if reset {
            // pass game world to init_world()
            // this should eventually populate the world w/ items and players
            // at some point, should pick a random starting direction that won't put them in the wall
            init_world(&mut world, width, height, &mut players);
            reset = false;
        }

        // process all packets added to the queue since the last tick
        'packet: loop {
            // deserialize client packet
            let received = get_packet(&srv_sock);

            // process received packet
            match received {
                Some((from, Packet::Join(name))) => {
                    // XXX send a Result instead of a naked NetResult
                    //let mut reply: Result<NetResult, NetResult>;
                    players.insert(from, Snaek { name: name.to_owned(),
                                                  segments: Vec::new(),
                                                  symbol: random_emoji(),
                                                  score: 0,
                                                  last_input: ' ',
                                                  alive: false, } );
                    //reply = Ok(NetResult::Joined);
                    //serialized = serde_json::to_string(&reply).expect("failed to serialize reply");
                    let reply = serde_json::to_string(&Packet::Reply(NetResult::Joined)).expect("failed to serialize reply");
                    srv_sock.send_to(reply.as_bytes(), from).expect("couldn't send reply");
                    reset = true;
                },
                Some((from, Packet::Move(dir))) => {
                    let player = players.get_mut(&from);

                    match player {
                        Some(p) => {
                            // new direction is match on c
                            p.last_input = match dir {
                                'w' if p.last_input != 's' => 'w',
                                'a' if p.last_input != 'd' => 'a',
                                's' if p.last_input != 'w' => 's',
                                'd' if p.last_input != 'a' => 'd',
                                _ => p.last_input,
                            };
                        },
                        _ => continue 'packet,
                    }
                },
                Some((_, _)) => continue 'packet,
                _ => break 'packet,
            }; // this makes the overall match have () type, which is required (since there's no assignment)?
        }
        // update world and player structures based on next moves
		// XXX need to prune players that time-out
        update_state(&mut world, &mut players);

        // iterate over all players, send notification of state, scores, and new world
		notify_players(&world, &players, &srv_sock);

        println!("loop time: {:?}\r", start.elapsed());

        // delay upto one tick
        // XXX add a debug print when loop time exceeds tick time
        thread::sleep(Duration::from_millis(tick_ms).saturating_sub(start.elapsed()));
    }
}
