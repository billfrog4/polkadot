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

use super::{Trait, Module, Store};
use crate::configuration::{self, HostConfiguration};
use sp_std::prelude::*;
use sp_std::collections::{btree_map::BTreeMap, vec_deque::VecDeque};
use frame_support::{StorageMap, StorageValue, weights::Weight, traits::Get};
use primitives::v1::{Id as ParaId, UpwardMessage};
use codec::Decode;

impl<T: Trait> Module<T> {
	pub(super) fn outgoing_para_cleanup_ump(outgoing_para: ParaId) {
		<Self as Store>::RelayDispatchQueueSize::remove(&outgoing_para);
		<Self as Store>::RelayDispatchQueues::remove(&outgoing_para);
		<Self as Store>::NeedsDispatch::mutate(|v| {
			if let Ok(i) = v.binary_search(&outgoing_para) {
				v.remove(i);
			}
		});
		<Self as Store>::NextDispatchRoundStartWith::mutate(|v| {
			*v = v.filter(|p| *p == outgoing_para)
		});
	}

	/// Check that all the upward messages sent by a candidate pass the acceptance criteria. Returns
	/// false, if any of the messages doesn't pass.
	pub(crate) fn check_upward_messages(
		config: &HostConfiguration<T::BlockNumber>,
		para: ParaId,
		upward_messages: &[UpwardMessage],
	) -> bool {
		if upward_messages.len() as u32 > config.max_upward_message_num_per_candidate {
			return false;
		}

		let (mut para_queue_count, mut para_queue_size) =
			<Self as Store>::RelayDispatchQueueSize::get(&para);

		for msg in upward_messages {
			para_queue_count += 1;
			para_queue_size += msg.len() as u32;
		}

		// make sure that the queue is not overfilled.
		// we do it here only once since returning false invalidates the whole relay-chain block.
		if para_queue_count > config.max_upward_queue_count
			|| para_queue_size > config.max_upward_queue_size
		{
			return false;
		}

		true
	}

	/// Enacts all the upward messages sent by a candidate.
	pub(crate) fn enact_upward_messages(para: ParaId, upward_messages: &[UpwardMessage]) -> Weight {
		let mut weight = 0;

		if !upward_messages.is_empty() {
			let (extra_cnt, extra_size) = upward_messages
				.iter()
				.fold((0, 0), |(cnt, size), d| (cnt + 1, size + d.len() as u32));

			<Self as Store>::RelayDispatchQueues::mutate(&para, |v| {
				v.extend(upward_messages.iter().cloned())
			});

			<Self as Store>::RelayDispatchQueueSize::mutate(
				&para,
				|(ref mut cnt, ref mut size)| {
					*cnt += extra_cnt;
					*size += extra_size;
				},
			);

			<Self as Store>::NeedsDispatch::mutate(|v| {
				if let Err(i) = v.binary_search(&para) {
					v.insert(i, para);
				}
			});

			weight += T::DbWeight::get().reads_writes(3, 3);
		}

		weight
	}

	/// Devote some time into dispatching pending upward messages.
	pub(crate) fn process_pending_upward_messages() {
		let mut weight = 0;

		let mut queue_cache: BTreeMap<ParaId, VecDeque<UpwardMessage>> = BTreeMap::new();

		let mut needs_dispatch: Vec<ParaId> = <Self as Store>::NeedsDispatch::get();
		let start_with = <Self as Store>::NextDispatchRoundStartWith::get();

		let config = <configuration::Module<T>>::config();

		let mut idx = match start_with {
			Some(para) => match needs_dispatch.binary_search(&para) {
				Ok(found_idx) => found_idx,
				// well, that's weird, since the `NextDispatchRoundStartWith` is supposed to be reset.
				// let's select 0 as the starting index as a safe bet.
				Err(_supposed_idx) => 0,
			},
			None => 0,
		};

		loop {
			// find the next dispatchee
			let dispatchee = match needs_dispatch.get(idx) {
				Some(para) => {
					// update the index now. It may be used to set `NextDispatchRoundStartWith`.
					idx = (idx + 1) % needs_dispatch.len();
					*para
				}
				None => {
					// no pending upward queues need processing at the moment.
					break;
				}
			};

			if weight >= config.preferred_dispatchable_upward_messages_step_weight {
				// Then check whether we've reached or overshoot the
				// preferred weight for the dispatching stage.
				//
				// if so - bail.
				break;
			}

			// deuque the next message from the queue of the dispatchee
			let queue = queue_cache
				.entry(dispatchee)
				.or_insert_with(|| <Self as Store>::RelayDispatchQueues::get(&dispatchee));
			match queue.pop_front() {
				Some(upward_msg) => {
					// process the upward message
					match self::xcm::Xcm::decode(&mut &upward_msg[..]) {
						Ok(xcm) => {
							if self::xcm::estimate_weight(&xcm)
								<= config.dispatchable_upward_message_critical_weight
							{
								weight += match self::xcm::execute(xcm) {
									Ok(w) => w,
									Err(w) => w,
								};
							}
						}
						Err(_) => {}
					}
				}
				None => {}
			}

			if queue.is_empty() {
				// the queue is empty - this para doesn't need attention anymore.
				match needs_dispatch.binary_search(&dispatchee) {
					Ok(i) => {
						let _ = needs_dispatch.remove(i);
					}
					Err(_) => {
						// the invariant is we dispatch only queues that present in the
						// `needs_dispatch` in the first place.
						//
						// that should not be harmful though.
						debug_assert!(false);
					}
				}
			}
		}

		let next_one = needs_dispatch.get(idx).cloned();
		<Self as Store>::NextDispatchRoundStartWith::set(next_one);
		<Self as Store>::NeedsDispatch::put(needs_dispatch);
	}
}

mod xcm {
	//! A plug for the time being until we have XCM merged.
	use frame_support::weights::Weight;
	use codec::{Encode, Decode};

	#[derive(Clone, Eq, PartialEq, Encode, Decode)]
	pub enum Xcm {}

	// we expect the following functions to be satisfied by an implementation of the XcmExecute
	// trait.

	pub fn execute(xcm: Xcm) -> Result<Weight, Weight> {
		match xcm {}
	}

	pub fn estimate_weight(xcm: &Xcm) -> Weight {
		match *xcm {}
	}
}

#[cfg(test)]
mod tests {
	use crate::router::tests::default_genesis_config;
	use crate::mock::{Router, new_test_ext};

	#[test]
	fn ump_dispatch_empty() {
		new_test_ext(default_genesis_config()).execute_with(|| {
			// make sure that the case with empty queues is handled properly
			Router::process_pending_upward_messages();
		});
	}
}
