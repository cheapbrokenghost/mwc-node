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

use crate::chain::{self, SyncState, SyncStatus};
use crate::mwc::sync::sync_manager::SyncManager;
use crate::mwc::sync::sync_utils::SyncRequestResponses;
use crate::p2p;
use crate::util::StopState;
use chrono::Utc;
use mwc_p2p::{Capabilities, Peer};
use std::sync::Arc;
use std::thread;
use std::time;

pub fn run_sync(
	sync_state: Arc<SyncState>,
	peers: Arc<p2p::Peers>,
	chain: Arc<chain::Chain>,
	stop_state: Arc<StopState>,
	sync_manager: Arc<SyncManager>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
	thread::Builder::new()
		.name("sync".to_string())
		.spawn(move || {
			let runner = SyncRunner::new(sync_state, peers, chain, stop_state, sync_manager);
			runner.sync_loop();
		})
}

pub struct SyncRunner {
	sync_state: Arc<SyncState>,
	peers: Arc<p2p::Peers>,
	chain: Arc<chain::Chain>,
	stop_state: Arc<StopState>,
	sync_manager: Arc<SyncManager>,
}

impl SyncRunner {
	fn new(
		sync_state: Arc<SyncState>,
		peers: Arc<p2p::Peers>,
		chain: Arc<chain::Chain>,
		stop_state: Arc<StopState>,
		sync_manager: Arc<SyncManager>,
	) -> SyncRunner {
		SyncRunner {
			sync_state,
			peers,
			chain,
			stop_state,
			sync_manager,
		}
	}

	fn wait_for_min_peers(&self) -> Result<(), chain::Error> {
		let wait_secs = if let SyncStatus::AwaitingPeers = self.sync_state.status() {
			30
		} else {
			3
		};

		let head = self.chain.head()?;

		let mut n = 0;
		const MIN_PEERS: usize = 3;
		loop {
			if self.stop_state.is_stopped() {
				break;
			}
			// Count peers with at least our difficulty.
			let wp = self
				.peers
				.iter()
				.outbound()
				.with_difficulty(|x| x.to_num() > 0 && x >= head.total_difficulty)
				.connected()
				.count();

			debug!(
				"Waiting for at least {} peers to start sync. So far has {}",
				MIN_PEERS, wp
			);

			// exit loop when:
			// * we have more than MIN_PEERS more_or_same_work peers
			// * we are synced already, e.g. mwc was quickly restarted
			// * timeout
			if wp >= MIN_PEERS || n > wait_secs {
				break;
			}
			thread::sleep(time::Duration::from_secs(1));
			n += 1;
		}
		Ok(())
	}

	/// Starts the syncing loop, just spawns two threads that loop forever
	fn sync_loop(&self) {
		// Wait for connections reach at least MIN_PEERS
		self.sync_state.update(SyncStatus::AwaitingPeers);
		if let Err(e) = self.wait_for_min_peers() {
			error!("wait_for_min_peers failed: {:?}", e);
		}

		// Main syncing loop
		let mut last_peer_dump = Utc::now();
		let mut sleep_time = 1000;
		loop {
			if self.stop_state.is_stopped() {
				break;
			}
			// Sync manager request might be relatevely heavy, it is expected that latency is higer then 1 second, so
			// waiting time for 1000ms is reasonable.
			thread::sleep(time::Duration::from_millis(sleep_time));

			self.sync_manager.headers_blocks_request(&self.peers);

			// Onle in a while let's dump the peers. Needed to understand how network is doing
			let now = Utc::now();
			if (now - last_peer_dump).num_seconds() > 60 * 20 {
				last_peer_dump = now;
				let peers: Vec<Arc<Peer>> = self.peers.iter().connected().into_iter().collect();
				info!("Has connected peers: {}", peers.len());
				for p in peers {
					info!(
						"Peer: {:?} {:?} H:{}  Diff:{} Cap: {} BFee: {}",
						p.info.addr,
						p.info.direction,
						p.info.height(),
						p.info.total_difficulty().to_num(),
						p.info.capabilities.bits(),
						p.info.tx_base_fee
					);
				}
			}

			// run each sync stage, each of them deciding whether they're needed
			// except for state sync that only runs if body sync return true (means txhashset is needed)
			let sync_reponse = self.sync_manager.sync_request(&self.peers);
			debug!("sync_manager responsed with {:?}", sync_reponse);

			let prev_state = self.sync_state.status();
			sleep_time = 1000;
			match sync_reponse.response {
				SyncRequestResponses::WaitingForPeers => {
					info!("Waiting for the peers");
					self.sync_state.update(SyncStatus::AwaitingPeers);
					self.peers
						.set_boost_peers_capabilities(sync_reponse.peers_capabilities);
				}
				SyncRequestResponses::Syncing => {
					//debug_assert!(self.sync_state.is_syncing());
					self.peers
						.set_boost_peers_capabilities(sync_reponse.peers_capabilities);
				}
				SyncRequestResponses::HasMoreHeadersToApply => {
					debug!("Has more headers to apply, will continue soon");
					sleep_time = 100;
				}
				SyncRequestResponses::SyncDone => {
					self.sync_state.update(SyncStatus::NoSync);
					// reset the boost mode
					self.peers
						.set_boost_peers_capabilities(Capabilities::UNKNOWN);

					if let Err(e) = self.chain.compact() {
						error!("Compact chain is failed. Error: {}", e);
					}

					for _ in 0..20 {
						if !self.stop_state.is_stopped() {
							thread::sleep(time::Duration::from_secs(1));
							// Processing regular headers/blocks requests.
							// Every second we will fire the requests to headers/blocks from the queue
							// Purpose of that to prevent data requests flooding.
							self.sync_manager.headers_blocks_request(&self.peers);
						}
					}
				}
				_ => debug_assert!(false),
			}

			let new_state = self.sync_state.status();
			if prev_state != new_state {
				info!(
					"Sync status was changed from {:?}  to  {:?}",
					prev_state, new_state
				);
			}
		}
	}
}
