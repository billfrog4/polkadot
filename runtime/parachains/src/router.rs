// Copyright 2020 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! The router module is responsible for handling messaging.
//!
//! The core of the messaging is checking and processing messages sent out by the candidates,
//! routing the messages at their destinations and informing the parachains about the incoming
//! messages.

use crate::{configuration, initializer};
use sp_std::prelude::*;
use frame_support::{decl_error, decl_module, decl_storage, weights::Weight};
use sp_std::collections::vec_deque::VecDeque;
use primitives::v1::{
	Id as ParaId, InboundDownwardMessage, Hash, UpwardMessage, HrmpChannelId, InboundHrmpMessage,
};

mod dmp;
mod hrmp;
mod ump;

use hrmp::{HrmpOpenChannelRequest, HrmpChannel};

pub trait Trait: frame_system::Trait + configuration::Trait {}

decl_storage! {
	trait Store for Module<T: Trait> as Router {
		/// Paras that are to be cleaned up at the end of the session.
		/// The entries are sorted ascending by the para id.
		OutgoingParas: Vec<ParaId>;

		/*
		 * Downward Message Passing (DMP)
		 *
		 * Storage layout required for implementation of DMP.
		 */

		/// The downward messages addressed for a certain para.
		DownwardMessageQueues: map hasher(twox_64_concat) ParaId => Vec<InboundDownwardMessage<T::BlockNumber>>;
		/// A mapping that stores the downward message queue MQC head for each para.
		///
		/// Each link in this chain has a form:
		/// `(prev_head, B, H(M))`, where
		/// - `prev_head`: is the previous head hash or zero if none.
		/// - `B`: is the relay-chain block number in which a message was appended.
		/// - `H(M)`: is the hash of the message being appended.
		DownwardMessageQueueHeads: map hasher(twox_64_concat) ParaId => Hash;

		/*
		 * Upward Message Passing (UMP)
		 *
		 * Storage layout required for UMP, specifically dispatchable upward messages.
		 */

		/// Dispatchable objects ready to be dispatched onto the relay chain. The messages are processed in FIFO order.
		RelayDispatchQueues: map hasher(twox_64_concat) ParaId => VecDeque<UpwardMessage>;
		/// Size of the dispatch queues. Caches sizes of the queues in `RelayDispatchQueue`.
		/// First item in the tuple is the count of messages and second
		/// is the total length (in bytes) of the message payloads.
		RelayDispatchQueueSize: map hasher(twox_64_concat) ParaId => (u32, u32);
		/// The ordered list of `ParaId`s that have a `RelayDispatchQueue` entry.
		NeedsDispatch: Vec<ParaId>;
		/// This is the para that gets will get dispatched first during the next upward dispatchable queue
		/// execution round.
		NextDispatchRoundStartWith: Option<ParaId>;

		/*
		 * Horizontally Relay-routed Message Passing (HRMP)
		 *
		 * HRMP related storage layout
		 */

		/// The set of pending HRMP open channel requests.
		///
		/// The set is accompanied by a list for iteration.
		///
		/// Invariant:
		/// - There are no channels that exists in list but not in the set and vice versa.
		HrmpOpenChannelRequests: map hasher(twox_64_concat) HrmpChannelId => Option<HrmpOpenChannelRequest>;
		HrmpOpenChannelRequestsList: Vec<HrmpChannelId>;

		/// This mapping tracks how many open channel requests are inititated by a given sender para.
		/// Invariant: `HrmpOpenChannelRequests` should contain the same number of items that has `(X, _)`
		/// as the number of `HrmpOpenChannelRequestCount` for `X`.
		HrmpOpenChannelRequestCount: map hasher(twox_64_concat) ParaId => u32;
		/// This mapping tracks how many open channel requests were accepted by a given recipient para.
		/// Invariant: `HrmpOpenChannelRequests` should contain the same number of items `(_, X)` with
		/// `confirmed` set to true, as the number of `HrmpAcceptedChannelRequestCount` for `X`.
		HrmpAcceptedChannelRequestCount: map hasher(twox_64_concat) ParaId => u32;

		/// A set of pending HRMP close channel requests that are going to be closed during the session change.
		/// Used for checking if a given channel is registered for closure.
		///
		/// The set is accompanied by a list for iteration.
		///
		/// Invariant:
		/// - There are no channels that exists in list but not in the set and vice versa.
		HrmpCloseChannelRequests: map hasher(twox_64_concat) HrmpChannelId => Option<()>;
		HrmpCloseChannelRequestsList: Vec<HrmpChannelId>;

		/// The HRMP watermark associated with each para.
		HrmpWatermarks: map hasher(twox_64_concat) ParaId => Option<T::BlockNumber>;
		/// HRMP channel data associated with each para.
		HrmpChannels: map hasher(twox_64_concat) HrmpChannelId => Option<HrmpChannel>;
		/// The indexes that map all senders to their recievers and vise versa.
		/// Invariants:
		/// - for each ingress index entry for `P` each item `I` in the index should present in `HrmpChannels` as `(I, P)`.
		/// - for each egress index entry for `P` each item `E` in the index should present in `HrmpChannels` as `(P, E)`.
		/// - there should be no other dangling channels in `HrmpChannels`.
		HrmpIngressChannelsIndex: map hasher(twox_64_concat) ParaId => Vec<ParaId>;
		HrmpEgressChannelsIndex: map hasher(twox_64_concat) ParaId => Vec<ParaId>;
		/// Storage for the messages for each channel.
		/// Invariant: cannot be non-empty if the corresponding channel in `HrmpChannels` is `None`.
		HrmpChannelContents: map hasher(twox_64_concat) HrmpChannelId => Vec<InboundHrmpMessage<T::BlockNumber>>;
		/// Maintains a mapping that can be used to answer the question:
		/// What paras sent a message at the given block number for a given reciever.
		/// Invariant: The para ids vector is never empty.
		HrmpChannelDigests: map hasher(twox_64_concat) ParaId => Vec<(T::BlockNumber, Vec<ParaId>)>;
	}
}

decl_error! {
	pub enum Error for Module<T: Trait> { }
}

decl_module! {
	/// The router module.
	pub struct Module<T: Trait> for enum Call where origin: <T as frame_system::Trait>::Origin {
		type Error = Error<T>;
	}
}

impl<T: Trait> Module<T> {
	/// Block initialization logic, called by initializer.
	pub(crate) fn initializer_initialize(_now: T::BlockNumber) -> Weight {
		0
	}

	/// Block finalization logic, called by initializer.
	pub(crate) fn initializer_finalize() {}

	/// Called by the initializer to note that a new session has started.
	pub(crate) fn initializer_on_new_session(
		_notification: &initializer::SessionChangeNotification<T::BlockNumber>,
	) {
		let outgoing = OutgoingParas::take();
		for outgoing_para in outgoing {
			// DMP
			<Self as Store>::DownwardMessageQueues::remove(&outgoing_para);
			<Self as Store>::DownwardMessageQueueHeads::remove(&outgoing_para);

			// UMP
			Self::outgoing_para_cleanup_ump(outgoing_para);
		}
	}

	/// Schedule a para to be cleaned up at the start of the next session.
	pub fn schedule_para_cleanup(id: ParaId) {
		OutgoingParas::mutate(|v| {
			if let Err(i) = v.binary_search(&id) {
				v.insert(i, id);
			}
		});
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use primitives::v1::BlockNumber;
	use frame_support::traits::{OnFinalize, OnInitialize};

	use crate::mock::{System, Router, GenesisConfig as MockGenesisConfig};

	pub(crate) fn run_to_block(to: BlockNumber, new_session: Option<Vec<BlockNumber>>) {
		while System::block_number() < to {
			let b = System::block_number();
			Router::initializer_finalize();
			System::on_finalize(b);

			System::on_initialize(b + 1);
			System::set_block_number(b + 1);

			if new_session.as_ref().map_or(false, |v| v.contains(&(b + 1))) {
				Router::initializer_on_new_session(&Default::default());
			}
			Router::initializer_initialize(b + 1);
		}
	}

	pub(crate) fn default_genesis_config() -> MockGenesisConfig {
		MockGenesisConfig {
			configuration: crate::configuration::GenesisConfig {
				config: crate::configuration::HostConfiguration {
					critical_downward_message_size: 1024,
					..Default::default()
				},
			},
			..Default::default()
		}
	}
}
