use std::net::{SocketAddr, ToSocketAddrs};
use std::collections::HashMap;
use std::hash::Hasher;
use std::net::UdpSocket;
use std::io::Read;
use std::fmt;
use std::os::unix::io::AsRawFd;

use time::{Duration, SteadyTime};
use epoll;

use super::{ethernet, udpmessage};
use super::tapdev::TapDevice;


#[derive(Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct Mac(pub [u8; 6]);

impl fmt::Debug for Mac {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        write!(formatter, "{:x}:{:x}:{:x}:{:x}:{:x}:{:x}",
            self.0[0], self.0[1], self.0[2], self.0[3], self.0[4], self.0[5])
    }
}


pub type Token = u64;

#[derive(Debug)]
pub enum Error {
    ParseError(&'static str),
    WrongToken(Token),
    SocketError(&'static str),
    TapdevError(&'static str),
}


struct PeerList {
    timeout: Duration,
    peers: HashMap<SocketAddr, SteadyTime>
}

impl PeerList {
    fn new(timeout: Duration) -> PeerList {
        PeerList{peers: HashMap::new(), timeout: timeout}
    }

    fn timeout(&mut self) {
        let now = SteadyTime::now();
        let mut del: Vec<SocketAddr> = Vec::new();
        for (&addr, &timeout) in &self.peers {
            if timeout < now {
                del.push(addr);
            }
        }
        for addr in del {
            debug!("Forgot peer: {:?}", addr);
            self.peers.remove(&addr);
        }
    }

    fn contains(&mut self, addr: &SocketAddr) -> bool {
        self.peers.contains_key(addr)
    }

    fn add(&mut self, addr: &SocketAddr) {
        if self.peers.insert(*addr, SteadyTime::now()+self.timeout).is_none() {
            info!("New peer: {:?}", addr);
        }
    }

    fn as_vec(&self) -> Vec<SocketAddr> {
        self.peers.keys().map(|addr| *addr).collect()
    }

    fn remove(&mut self, addr: &SocketAddr) {
        if self.peers.remove(&addr).is_some() {
            info!("Removed peer: {:?}", addr);
        }
    }
}


#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct MacTableKey {
    mac: Mac,
    vlan: u16
}

struct MacTableValue {
    address: SocketAddr,
    timeout: SteadyTime
}

struct MacTable {
    table: HashMap<MacTableKey, MacTableValue>,
    timeout: Duration
}

impl MacTable {
    fn new(timeout: Duration) -> MacTable {
        MacTable{table: HashMap::new(), timeout: timeout}
    }

    fn timeout(&mut self) {
        let now = SteadyTime::now();
        let mut del: Vec<MacTableKey> = Vec::new();
        for (&key, val) in &self.table {
            if val.timeout < now {
                del.push(key);
            }
        }
        for key in del {
            info!("Forgot mac: {:?} (vlan {})", key.mac, key.vlan);
            self.table.remove(&key);
        }
    }

    fn learn(&mut self, mac: &Mac, vlan: u16, addr: &SocketAddr) {
       let key = MacTableKey{mac: *mac, vlan: vlan};
       let value = MacTableValue{address: *addr, timeout: SteadyTime::now()+self.timeout};
       if self.table.insert(key, value).is_none() {
           info!("Learned mac: {:?} (vlan {}) => {}", mac, vlan, addr);
       }
    }

    fn lookup(&self, mac: &Mac, vlan: u16) -> Option<SocketAddr> {
       let key = MacTableKey{mac: *mac, vlan: vlan};
       match self.table.get(&key) {
           Some(value) => Some(value.address),
           None => None
       }
    }
}

pub struct EthCloud {
    peers: PeerList,
    mactable: MacTable,
    socket: UdpSocket,
    tapdev: TapDevice,
    token: Token,
    next_peerlist: SteadyTime,
    update_freq: Duration,
    buffer_out: [u8; 64*1024],
    last_housekeep: SteadyTime,
}

impl EthCloud {
    pub fn new(device: &str, listen: String, token: Token, mac_timeout: Duration, peer_timeout: Duration) -> Self {
        let socket = match UdpSocket::bind(&listen as &str) {
            Ok(socket) => socket,
            _ => panic!("Failed to open socket")
        };
        let tapdev = match TapDevice::new(device) {
            Ok(tapdev) => tapdev,
            _ => panic!("Failed to open tap device")
        };
        info!("Opened tap device {}", tapdev.ifname());
        EthCloud{
            peers: PeerList::new(peer_timeout),
            mactable: MacTable::new(mac_timeout),
            socket: socket,
            tapdev: tapdev,
            token: token,
            next_peerlist: SteadyTime::now(),
            update_freq: peer_timeout/2,
            buffer_out: [0; 64*1024],
            last_housekeep: SteadyTime::now()
        }
    }

    fn send_msg<A: ToSocketAddrs + fmt::Display>(&mut self, addr: A, msg: &udpmessage::Message) -> Result<(), Error> {
        debug!("Sending {:?} to {}", msg, addr);
        let size = udpmessage::encode(self.token, msg, &mut self.buffer_out);
        match self.socket.send_to(&self.buffer_out[..size], addr) {
            Ok(written) if written == size => Ok(()),
            Ok(_) => Err(Error::SocketError("Sent out truncated packet")),
            Err(e) => {
                error!("Failed to send via network {:?}", e);
                Err(Error::SocketError("IOError when sending"))
            }
        }
    }

    pub fn connect<A: ToSocketAddrs + fmt::Display>(&mut self, addr: A) -> Result<(), Error> {
        info!("Connecting to {}", addr);
        self.send_msg(addr, &udpmessage::Message::GetPeers)
    }

    fn housekeep(&mut self) -> Result<(), Error> {
        debug!("Running housekeeping...");
        //self.cache.clear();
        self.peers.timeout();
        self.mactable.timeout();
        if self.next_peerlist <= SteadyTime::now() {
            debug!("Send peer list to all peers");
            let peers = self.peers.as_vec();
            let msg = udpmessage::Message::Peers(peers.clone());
            for addr in &peers {
                try!(self.send_msg(addr, &msg));
            }
            self.next_peerlist = SteadyTime::now() + self.update_freq;
        }
        Ok(())
    }

    fn handle_ethernet_frame(&mut self, frame: ethernet::Frame) -> Result<(), Error> {
        debug!("Read ethernet frame from tap {:?}", frame);
        match self.mactable.lookup(frame.dst, frame.vlan) {
            Some(addr) => {
                debug!("Found destination for {:?} (vlan {}) => {}", frame.dst, frame.vlan, addr);
                try!(self.send_msg(addr, &udpmessage::Message::Frame(frame)))
            },
            None => {
                debug!("No destination for {:?} (vlan {}) found, broadcasting", frame.dst, frame.vlan);
                let msg = udpmessage::Message::Frame(frame);
                for addr in &self.peers.as_vec() {
                    try!(self.send_msg(addr, &msg));
                }
            }
        }
        Ok(())
    }

    fn handle_net_message(&mut self, peer: SocketAddr, token: Token, msg: udpmessage::Message) -> Result<(), Error> {
        if token != self.token {
            info!("Ignoring message from {} with wrong token {}", peer, token);
            return Err(Error::WrongToken(token));
        }
        debug!("Recieved {:?} from {}", msg, peer);
        match msg {
            udpmessage::Message::Frame(frame) => {
                let size = ethernet::encode(&frame, &mut self.buffer_out);
                debug!("Writing ethernet frame to tap: {:?}", frame);
                match self.tapdev.write(&self.buffer_out[..size]) {
                    Ok(()) => (),
                    Err(e) => {
                        error!("Failed to send via tap device {:?}", e);
                        return Err(Error::TapdevError("Failed to write to tap device"));
                    }
                }
                self.peers.add(&peer);
                self.mactable.learn(frame.src, frame.vlan, &peer);
            },
            udpmessage::Message::Peers(peers) => {
                self.peers.add(&peer);
                for p in &peers {
                    if ! self.peers.contains(p) {
                        try!(self.connect(p));
                    }
                }
            },
            udpmessage::Message::GetPeers => {
                self.peers.add(&peer);
                let peers = self.peers.as_vec();
                try!(self.send_msg(peer, &udpmessage::Message::Peers(peers)));
            },
            udpmessage::Message::Close => {
                self.peers.remove(&peer);
            }
        }
        Ok(())
    }

    pub fn run(&mut self) {
        let epoll_handle = epoll::create1(0).expect("Failed to create epoll handle");
        let socket_fd = self.socket.as_raw_fd();
        let tapdev_fd = self.tapdev.as_raw_fd();
        let mut socket_event = epoll::EpollEvent{events: epoll::util::event_type::EPOLLIN, data: 0};
        let mut tapdev_event = epoll::EpollEvent{events: epoll::util::event_type::EPOLLIN, data: 1};
        epoll::ctl(epoll_handle, epoll::util::ctl_op::ADD, socket_fd, &mut socket_event).expect("Failed to add socket to epoll handle");
        epoll::ctl(epoll_handle, epoll::util::ctl_op::ADD, tapdev_fd, &mut tapdev_event).expect("Failed to add tapdev to epoll handle");
        let mut events = [epoll::EpollEvent{events: 0, data: 0}; 2];
        let mut buffer = [0; 64*1024];
        loop {
            let count = epoll::wait(epoll_handle, &mut events, 1000).expect("Epoll wait failed");
            // Process events
            for i in 0..count {
                match &events[i as usize].data {
                    &0 => match self.socket.recv_from(&mut buffer) {
                        Ok((size, src)) => {
                            match udpmessage::decode(&buffer[..size]).and_then(|(token, msg)| self.handle_net_message(src, token, msg)) {
                                Ok(_) => (),
                                Err(e) => error!("Error: {:?}", e)
                            }
                        },
                        Err(_error) => panic!("Failed to read from network socket")
                    },
                    &1 => match self.tapdev.read(&mut buffer) {
                        Ok(size) => {
                            match ethernet::decode(&mut buffer[..size]).and_then(|frame| self.handle_ethernet_frame(frame)) {
                                Ok(_) => (),
                                Err(e) => error!("Error: {:?}", e)
                            }
                        },
                        Err(_error) => panic!("Failed to read from tap device")
                    },
                    _ => unreachable!()
                }
            }
            // Do the housekeeping
            if self.last_housekeep < SteadyTime::now() + Duration::seconds(1) {
                match self.housekeep() {
                    Ok(_) => (),
                    Err(e) => error!("Error: {:?}", e)
                }
                self.last_housekeep = SteadyTime::now()
            }
        }
    }
}