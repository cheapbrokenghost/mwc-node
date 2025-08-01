// Copyright 2019 The Grin Developers
// Copyright 2024 The MWC Developers
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::types::PeerAddr::Onion;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::chain;
use crate::chain::txhashset::BitmapChunk;
use crate::handshake::Handshake;
use crate::mwc_core::core;
use crate::mwc_core::core::hash::Hash;
use crate::mwc_core::core::{OutputIdentifier, Segment, SegmentIdentifier, TxKernel};
use crate::mwc_core::global;
use crate::mwc_core::pow::Difficulty;
use crate::peer::Peer;
use crate::peers::Peers;
use crate::store::PeerStore;
use crate::types::{
	Capabilities, ChainAdapter, Error, NetAdapter, P2PConfig, PeerAddr, PeerInfo, ReasonForBan,
	TxHashSetRead,
};
use crate::util::secp::pedersen::RangeProof;
use crate::util::StopState;
use crate::PeerAddr::Ip;
use mwc_chain::txhashset::Segmenter;
use mwc_chain::SyncState;

const INITIAL_SOCKET_READ_TIMEOUT: Duration = Duration::from_millis(5000);
const INITIAL_SOCKET_WRITE_TIMEOUT: Duration = Duration::from_millis(5000);

/// P2P server implementation, handling bootstrapping to find and connect to
/// peers, receiving connections from other peers and keep track of all of them.
#[derive(Clone)]
pub struct Server {
	pub config: P2PConfig,
	pub socks_port: u16,
	capabilities: Capabilities,
	handshake: Arc<Handshake>,
	pub peers: Arc<Peers>,
	sync_state: Arc<SyncState>,
	stop_state: Arc<StopState>,
	pub self_onion_address: Option<String>,
}

// TODO TLS
impl Server {
	/// Creates a new idle p2p server with no peers
	pub fn new(
		db_root: &str,
		capabilities: Capabilities,
		config: P2PConfig,
		adapter: Arc<dyn ChainAdapter>,
		genesis: Hash,
		sync_state: Arc<SyncState>,
		stop_state: Arc<StopState>,
		socks_port: u16,
		onion_address: Option<String>,
	) -> Result<Server, Error> {
		Ok(Server {
			config: config.clone(),
			capabilities,
			handshake: Arc::new(Handshake::new(
				genesis,
				config.clone(),
				onion_address.clone(),
			)),
			peers: Arc::new(Peers::new(
				PeerStore::new(db_root)?,
				adapter,
				config,
				stop_state.clone(),
			)),
			sync_state,
			stop_state,
			socks_port,
			self_onion_address: onion_address,
		})
	}

	/// Starts a new TCP server and listen to incoming connections. This is a
	/// blocking call until the TCP server stops.
	pub fn listen(&self) -> Result<(), Error> {
		// start TCP listener and handle incoming connections
		let addr = SocketAddr::new(self.config.host, self.config.port);
		let listener = TcpListener::bind(addr)?;
		listener.set_nonblocking(true)?;

		let sleep_time = Duration::from_millis(5);
		loop {
			// Pause peer ingress connection request. Only for tests.
			if self.stop_state.is_paused() {
				thread::sleep(Duration::from_secs(1));
				continue;
			}

			match listener.accept() {
				Ok((stream, peer_addr)) => {
					// We want out TCP stream to be in blocking mode.
					// The TCP listener is in nonblocking mode so we *must* explicitly
					// move the accepted TCP stream into blocking mode (or all kinds of
					// bad things can and will happen).
					// A nonblocking TCP listener will accept nonblocking TCP streams which
					// we do not want.
					stream.set_nonblocking(false)?;
					// Note, those timeouts will be overwritten during handshake.
					let _ = stream.set_read_timeout(Some(INITIAL_SOCKET_READ_TIMEOUT));
					let _ = stream.set_write_timeout(Some(INITIAL_SOCKET_WRITE_TIMEOUT));

					let mut peer_addr = PeerAddr::Ip(peer_addr);

					// attempt to see if it an ipv4-mapped ipv6
					// if yes convert to ipv4
					match peer_addr {
						PeerAddr::Ip(socket_addr) => {
							if socket_addr.is_ipv6() {
								if let IpAddr::V6(ipv6) = socket_addr.ip() {
									if let Some(ipv4) = ipv6.to_ipv4() {
										peer_addr = PeerAddr::Ip(SocketAddr::V4(SocketAddrV4::new(
											ipv4,
											socket_addr.port(),
										)))
									}
								}
							}
						}
						_ => {}
					}

					if self.check_undesirable(&stream) {
						// Shutdown the incoming TCP connection if it is not desired
						if let Err(e) = stream.shutdown(Shutdown::Both) {
							debug!("Error shutting down conn: {:?}", e);
						}
						continue;
					}
					match self.handle_new_peer(stream) {
						Err(Error::ConnectionClose(err)) => {
							debug!("shutting down, ignoring a new peer, {}", err)
						}
						Err(e) => {
							debug!("Error accepting peer {}: {:?}", peer_addr.to_string(), e);
							let _ = self.peers.add_banned(peer_addr, ReasonForBan::BadHandshake);
						}
						Ok(_) => {}
					}
				}
				Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
					// nothing to do, will retry in next iteration
				}
				Err(e) => {
					debug!("Couldn't establish new client connection: {:?}", e);
				}
			}
			if self.stop_state.is_stopped() {
				break;
			}
			thread::sleep(sleep_time);
		}
		Ok(())
	}

	/// Asks the server to connect to a new peer. Directly returns the peer if
	/// we're already connected to the provided address.
	pub fn connect(&self, addr: &PeerAddr) -> Result<Arc<Peer>, Error> {
		if self.stop_state.is_stopped() {
			return Err(Error::ConnectionClose(String::from("node is stopping")));
		}

		if Peer::is_denied(&self.config, addr) {
			debug!("connect_peer: peer {:?} denied, not connecting.", addr);
			return Err(Error::ConnectionClose(String::from(
				"Peer is denied because it is in config black list",
			)));
		}

		let max_allowed_connections =
			self.config.peer_max_inbound_count() + self.config.peer_max_outbound_count(true) + 10;
		if self.peers.get_number_connected_peers() > max_allowed_connections as usize {
			return Err(Error::ConnectionClose(String::from(
				"Too many established connections...",
			)));
		}

		if global::is_production_mode() {
			let hs = self.handshake.clone();
			let addrs = hs.addrs.read();
			if addrs.contains(addr) {
				debug!("connect: ignore connecting to PeerWithSelf, addr: {}", addr);
				return Err(Error::PeerWithSelf);
			}
		}

		// check if the onion address is self
		if global::is_production_mode() && self.self_onion_address.is_some() {
			match addr {
				Onion(address) => {
					if self.self_onion_address.as_ref().unwrap() == address {
						debug!("error trying to connect with self: {}", address);
						return Err(Error::PeerWithSelf);
					}
					debug!("not self, connecting to {}", address);
				}
				Ip(_) => {
					if addr.is_loopback() {
						debug!("error trying to connect with self: {:?}", addr);
						return Err(Error::PeerWithSelf);
					}
				}
			}
		}

		if let Some(p) = self.peers.get_connected_peer(addr) {
			// if we're already connected to the addr, just return the peer
			trace!("connect_peer: already connected {}", addr);
			return Ok(p);
		}

		trace!(
			"connect_peer: on {}:{}. connecting to {}",
			self.config.host,
			self.config.port,
			addr
		);

		let peer_addr;
		let self_addr;

		let stream = match addr.clone() {
			PeerAddr::Ip(address) => {
				// we do this, not a good solution, but for now, we'll use it. Other side usually detects with ip.
				self_addr = PeerAddr::Ip(SocketAddr::new(self.config.host, self.config.port));
				if self.socks_port != 0 {
					peer_addr = Some(PeerAddr::Ip(address));
					let proxy_addr =
						SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), self.socks_port);
					let socks5_stream_ref =
						tor_stream::TorStream::connect_with_address(proxy_addr, address);
					match socks5_stream_ref {
						Ok(socks5_stream) => socks5_stream.unwrap(),
						Err(e) => {
							return Err(Error::Connection(e));
						}
					}
				} else {
					peer_addr = Some(PeerAddr::Ip(address));
					TcpStream::connect_timeout(&address, Duration::from_secs(10))?
				}
			}
			PeerAddr::Onion(onion_address) => {
				if self.socks_port != 0 {
					self_addr = PeerAddr::Onion(
						self.self_onion_address
							.as_ref()
							.unwrap_or(&"unknown".to_string())
							.to_string(),
					);
					peer_addr = Some(PeerAddr::Onion(onion_address.clone()));
					let proxy_addr =
						SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), self.socks_port);
					let onion_target: socks::TargetAddr =
						socks::TargetAddr::Domain(onion_address, 80);
					let socks5_stream_ref =
						tor_stream::TorStream::connect_with_address(proxy_addr, onion_target);
					match socks5_stream_ref {
						Ok(socks5_stream) => socks5_stream.unwrap(),
						Err(e) => {
							return Err(Error::Connection(e));
						}
					}
				} else {
					// can't connect to this because we don't have a socks proxy.
					return Err(Error::ConnectionClose(format!(
						"Failed connect to Tor address {} because Tor socks is not configured",
						onion_address
					)));
				}
			}
		};

		match Ok(stream) {
			Ok(stream) => {
				let total_diff = self.peers.total_difficulty()?;

				let peer = Peer::connect(
					stream,
					self.capabilities,
					total_diff,
					self_addr,
					&self.handshake,
					self.peers.clone(),
					peer_addr,
					self.sync_state.clone(),
					(*self).clone(),
				)?;
				let peer = Arc::new(peer);
				self.peers.add_connected(peer.clone())?;
				Ok(peer)
			}
			Err(e) => {
				trace!(
					"connect_peer: on {}:{}. Could not connect to {}: {:?}",
					self.config.host,
					self.config.port,
					addr,
					e
				);
				Err(Error::Connection(e))
			}
		}
	}

	fn handle_new_peer(&self, stream: TcpStream) -> Result<(), Error> {
		if self.stop_state.is_stopped() {
			return Err(Error::ConnectionClose(String::from("Server is stopping")));
		}

		let max_allowed_connections =
			self.config.peer_max_inbound_count() + self.config.peer_max_outbound_count(true) + 10;
		if self.peers.get_number_connected_peers() > max_allowed_connections as usize {
			return Err(Error::ConnectionClose(String::from(
				"Too many established connections...",
			)));
		}

		let total_diff = self.peers.total_difficulty()?;

		// accept the peer and add it to the server map
		let peer = Peer::accept(
			stream,
			self.capabilities,
			total_diff,
			&self.handshake,
			self.peers.clone(),
			self.sync_state.clone(),
			self.clone(),
		)?;
		// if we are using TOR, it will be the local addressed because it comes from the proxy
		// Will still need to save all the peers and renameit after peer will share the TOR address
		self.peers.add_connected(Arc::new(peer))?;
		Ok(())
	}

	/// Checks whether there's any reason we don't want to accept an incoming peer
	/// connection. There can be a few of them:
	/// 1. Accepting the peer connection would exceed the configured maximum allowed
	/// inbound peer count. Note that seed nodes may wish to increase the default
	/// value for PEER_LISTENER_BUFFER_COUNT to help with network bootstrapping.
	/// A default buffer of 8 peers is allowed to help with network growth.
	/// 2. The peer has been previously banned and the ban period hasn't
	/// expired yet.
	/// 3. We're already connected to a peer at the same IP. While there are
	/// many reasons multiple peers can legitimately share identical IP
	/// addresses (NAT), network distribution is improved if they choose
	/// different sets of peers themselves. In addition, it prevent potential
	/// duplicate connections, malicious or not.
	fn check_undesirable(&self, stream: &TcpStream) -> bool {
		if self.peers.iter().inbound().connected().count() as u32
			>= self.config.peer_max_inbound_count() + self.config.peer_listener_buffer_count()
		{
			debug!("Accepting new connection will exceed peer limit, refusing connection.");
			return true;
		}
		if let Ok(peer_addr) = stream.peer_addr() {
			let peer_addr = PeerAddr::Ip(peer_addr.clone());
			if self.peers.is_banned(&peer_addr) {
				debug!("Peer {} banned, refusing connection.", peer_addr);
				return true;
			}
			// The call to is_known() can fail due to contention on the peers map.
			// If it fails we want to default to refusing the connection.
			match self.peers.is_known(&peer_addr) {
				Ok(true) => {
					debug!("Peer {} already known, refusing connection.", peer_addr);
					return true;
				}
				Err(_) => {
					error!(
						"Peer {} is_known check failed, refusing connection.",
						peer_addr
					);
					return true;
				}
				_ => (),
			}
		}
		false
	}

	/// Check if server in syning state
	pub fn is_syncing(&self) -> bool {
		self.sync_state.is_syncing()
	}

	pub fn stop(&self) {
		self.stop_state.stop();
		self.peers.stop();
	}

	/// Pause means: stop all the current peers connection, only for tests.
	/// Note:
	/// 1. must pause the 'seed' thread also, to avoid the new egress peer connection
	/// 2. must pause the 'p2p-server' thread also, to avoid the new ingress peer connection.
	pub fn pause(&self) {
		self.peers.stop();
	}
}

/// A no-op network adapter used for testing.
pub struct DummyAdapter {}

impl ChainAdapter for DummyAdapter {
	fn total_difficulty(&self) -> Result<Difficulty, chain::Error> {
		Ok(Difficulty::min())
	}
	fn total_height(&self) -> Result<u64, chain::Error> {
		Ok(0)
	}
	fn get_transaction(&self, _h: Hash) -> Option<core::Transaction> {
		None
	}

	fn tx_kernel_received(&self, _h: Hash, _peer_info: &PeerInfo) -> Result<bool, chain::Error> {
		Ok(true)
	}
	fn transaction_received(
		&self,
		_: core::Transaction,
		_stem: bool,
	) -> Result<bool, chain::Error> {
		Ok(true)
	}
	fn compact_block_received(
		&self,
		_cb: core::CompactBlock,
		_peer_info: &PeerInfo,
	) -> Result<bool, chain::Error> {
		Ok(true)
	}
	fn header_received(
		&self,
		_bh: core::BlockHeader,
		_peer_info: &PeerInfo,
	) -> Result<bool, chain::Error> {
		Ok(true)
	}
	fn block_received(
		&self,
		_: core::Block,
		_: &PeerInfo,
		_: chain::Options,
	) -> Result<bool, chain::Error> {
		Ok(true)
	}
	fn headers_received(
		&self,
		_: &[core::BlockHeader],
		_remaining: u64,
		_: &PeerInfo,
	) -> Result<(), chain::Error> {
		Ok(())
	}

	fn header_locator(&self) -> Result<Vec<Hash>, chain::Error> {
		Ok(Vec::new())
	}

	fn locate_headers(&self, _: &[Hash]) -> Result<Vec<core::BlockHeader>, chain::Error> {
		Ok(vec![])
	}
	fn get_block(&self, _: Hash, _: &PeerInfo) -> Option<core::Block> {
		None
	}
	fn txhashset_read(&self, _h: Hash) -> Option<TxHashSetRead> {
		unimplemented!()
	}

	fn txhashset_archive_header(&self) -> Result<core::BlockHeader, chain::Error> {
		unimplemented!()
	}

	fn get_tmp_dir(&self) -> PathBuf {
		unimplemented!()
	}

	fn get_tmpfile_pathname(&self, _tmpfile_name: String) -> PathBuf {
		unimplemented!()
	}

	fn prepare_segmenter(&self) -> Result<Segmenter, chain::Error> {
		unimplemented!()
	}

	fn get_kernel_segment(
		&self,
		_hash: Hash,
		_id: SegmentIdentifier,
	) -> Result<Segment<TxKernel>, chain::Error> {
		unimplemented!()
	}

	fn get_bitmap_segment(
		&self,
		_hash: Hash,
		_id: SegmentIdentifier,
	) -> Result<Segment<BitmapChunk>, chain::Error> {
		unimplemented!()
	}

	fn get_output_segment(
		&self,
		_hash: Hash,
		_id: SegmentIdentifier,
	) -> Result<Segment<OutputIdentifier>, chain::Error> {
		unimplemented!()
	}

	fn get_rangeproof_segment(
		&self,
		_hash: Hash,
		_id: SegmentIdentifier,
	) -> Result<Segment<RangeProof>, chain::Error> {
		unimplemented!()
	}

	fn receive_bitmap_segment(
		&self,
		_peer: &PeerAddr,
		_archive_header_hash: Hash,
		_segment: Segment<BitmapChunk>,
	) -> Result<(), chain::Error> {
		unimplemented!()
	}

	fn receive_output_segment(
		&self,
		_peer: &PeerAddr,
		_archive_header_hash: Hash,
		_segment: Segment<OutputIdentifier>,
	) -> Result<(), chain::Error> {
		unimplemented!()
	}

	fn receive_rangeproof_segment(
		&self,
		_peer: &PeerAddr,
		_archive_header_hash: Hash,
		_segment: Segment<RangeProof>,
	) -> Result<(), chain::Error> {
		unimplemented!()
	}

	fn receive_kernel_segment(
		&self,
		_peer: &PeerAddr,
		_archive_header_hash: Hash,
		_segment: Segment<TxKernel>,
	) -> Result<(), chain::Error> {
		unimplemented!()
	}

	fn recieve_pibd_status(
		&self,
		_peer: &PeerAddr,
		_header_hash: Hash,
		_header_height: u64,
		_output_bitmap_root: Hash,
	) -> Result<(), chain::Error> {
		Ok(())
	}

	fn recieve_another_archive_header(
		&self,
		_peer: &PeerAddr,
		_header_hash: Hash,
		_header_height: u64,
	) -> Result<(), chain::Error> {
		Ok(())
	}

	fn receive_headers_hash_response(
		&self,
		_peer: &PeerAddr,
		_archive_height: u64,
		_headers_hash_root: Hash,
	) -> Result<(), chain::Error> {
		Ok(())
	}

	fn get_header_hashes_segment(
		&self,
		_header_hashes_root: Hash,
		_id: SegmentIdentifier,
	) -> Result<Segment<Hash>, chain::Error> {
		unimplemented!()
	}

	fn receive_header_hashes_segment(
		&self,
		_peer: &PeerAddr,
		_header_hashes_root: Hash,
		_segment: Segment<Hash>,
	) -> Result<(), chain::Error> {
		Ok(())
	}

	fn peer_difficulty(&self, _: &PeerAddr, _: Difficulty, _: u64) {}
}

impl NetAdapter for DummyAdapter {
	fn find_peer_addrs(&self, _: Capabilities) -> Vec<PeerAddr> {
		vec![]
	}
	fn peer_addrs_received(&self, _: Vec<PeerAddr>) {}
	fn is_banned(&self, _: &PeerAddr) -> bool {
		false
	}

	fn ban_peer(&self, _addr: &PeerAddr, _ban_reason: ReasonForBan, _message: &str) {}
}
