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

//! Syncing of the chain with the rest of the network

mod block_headers_request_cache;
mod body_sync;
mod header_hashes_sync;
mod header_sync;
mod orphans_sync;
mod state_sync;
pub mod sync_manager;
mod sync_peers;
mod sync_utils;
mod syncer;

pub use header_sync::get_locator_heights;

pub use self::syncer::run_sync;
