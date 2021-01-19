// Copyright 2018-2019 Parity Technologies (UK) Ltd
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

//! A voter in GRANDPA. This transitions between rounds and casts votes.
//!
//! Voters rely on some external context to function:
//!   - setting timers to cast votes.
//!   - incoming vote streams.
//!   - providing voter weights.
//!   - getting the local voter id.
//!
//!  The local voter id is used to check whether to cast votes for a given
//!  round. If no local id is defined or if it's not part of the voter set then
//!  votes will not be pushed to the sink. The protocol state machine still
//!  transitions state as if the votes had been pushed out.

use std::{fmt::Debug, sync::Arc};

use futures::{future::FusedFuture, Future, Sink, Stream};

use crate::{
	round::State as RoundState, BlockNumberOps, Chain, Commit, Equivocation, Message, Precommit,
	Prevote, PrimaryPropose, SignedMessage, VoterSet,
};

mod past_rounds;
mod voting_round;

use voting_round::VotingRound;

/// Data necessary to participate in a round.
pub struct RoundData<Id, Timer, Input, Output> {
	/// Local voter id (if any.)
	pub voter_id: Option<Id>,
	/// Timer before prevotes can be cast. This should be Start + 2T
	/// where T is the gossip time estimate.
	pub prevote_timer: Timer,
	/// Timer before precommits can be cast. This should be Start + 4T
	pub precommit_timer: Timer,
	/// Incoming messages.
	pub incoming: Input,
	/// Outgoing messages.
	pub outgoing: Output,
}

pub trait Environment<H: Eq, N>: Chain<H, N> {
	type Id: Clone + Debug + Ord;
	type Signature: Clone + Eq;

	type In: Stream<Item = Result<SignedMessage<H, N, Self::Signature, Self::Id>, Self::Error>>
		+ Unpin;

	type Out: Sink<Message<H, N>, Error = Self::Error> + Unpin;

	type Timer: Future<Output = Result<(), Self::Error>> + FusedFuture + Unpin;

	type Error: From<crate::Error>;

	fn round_data(&self, round: u64) -> RoundData<Self::Id, Self::Timer, Self::In, Self::Out>;

	/// Note that an equivocation in prevotes has occurred.
	fn prevote_equivocation(
		&self,
		round: u64,
		equivocation: Equivocation<Self::Id, Prevote<H, N>, Self::Signature>,
	);

	/// Note that an equivocation in precommits has occurred.
	fn precommit_equivocation(
		&self,
		round: u64,
		equivocation: Equivocation<Self::Id, Precommit<H, N>, Self::Signature>,
	);

	/// Note that we've done a primary proposal in the given round.
	fn proposed(&self, round: u64, propose: PrimaryPropose<H, N>) -> Result<(), Self::Error>;

	/// Note that we have prevoted in the given round.
	fn prevoted(&self, round: u64, prevote: Prevote<H, N>) -> Result<(), Self::Error>;

	/// Note that we have precommitted in the given round.
	fn precommitted(&self, round: u64, precommit: Precommit<H, N>) -> Result<(), Self::Error>;

	// /// Note that a round is completed. This is called when a round has been
	// /// voted in and the next round can start. The round may continue to be run
	// /// in the background until _concluded_.
	// /// Should return an error when something fatal occurs.
	// fn completed(
	// 	&self,
	// 	round: u64,
	// 	state: RoundState<H, N>,
	// 	base: (H, N),
	// 	votes: &HistoricalVotes<H, N, Self::Signature, Self::Id>,
	// ) -> Result<(), Self::Error>;

	// /// Note that a round has concluded. This is called when a round has been
	// /// `completed` and additionally, the round's estimate has been finalized.
	// ///
	// /// There may be more votes than when `completed`, and it is the responsibility
	// /// of the `Environment` implementation to deduplicate. However, the caller guarantees
	// /// that the votes passed to `completed` for this round are a prefix of the votes passed here.
	// fn concluded(
	// 	&self,
	// 	round: u64,
	// 	state: RoundState<H, N>,
	// 	base: (H, N),
	// 	votes: &HistoricalVotes<H, N, Self::Signature, Self::Id>,
	// ) -> Result<(), Self::Error>;

	fn finalize_block(
		&self,
		hash: H,
		number: N,
		round: u64,
		commit: Commit<H, N, Self::Signature, Self::Id>,
	) -> Result<(), Self::Error>;
}

struct Voter<H, N, E>
where
	H: Ord,
	E: Environment<H, N>,
{
	env: Arc<E>,
	voters: VoterSet<E::Id>,
	best_round: VotingRound<H, N, E>,
}

impl<H, N, E> Voter<H, N, E>
where
	H: Clone + Debug + Eq + Ord + Send,
	N: BlockNumberOps + Debug + Send,
	E: Environment<H, N>,
{
	pub fn new(
		env: Arc<E>,
		voters: VoterSet<E::Id>,
		last_round_number: u64,
		last_round_votes: Vec<SignedMessage<H, N, E::Signature, E::Id>>,
		last_round_base: (H, N),
		last_finalized: (H, N),
	) -> Self {
		let best_round = VotingRound::new(
			env.clone(),
			last_round_number + 1,
			voters.clone(),
			last_finalized,
			Some(RoundState::genesis(last_round_base)),
			None,
		);

		Voter {
			env,
			voters,
			best_round,
		}
	}

	pub async fn run(self) -> Result<(), E::Error> {
		self.best_round.run().await
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::testing::{
		self,
		chain::GENESIS_HASH,
		environment::{Environment, Id, Signature},
	};
	use futures::{executor::LocalPool, prelude::*, task::SpawnExt};

	#[test]
	fn talking_to_myself() {
		let _ = env_logger::try_init();

		let local_id = Id(5);
		let voters = VoterSet::new(std::iter::once((local_id, 100))).unwrap();

		let (network, routing_task) = testing::environment::make_network();

		// let global_comms = network.make_global_comms();
		let env = Arc::new(Environment::new(network, local_id));

		// initialize chain
		let last_finalized = env.with_chain(|chain| {
			chain.push_blocks(GENESIS_HASH, &["A", "B", "C", "D", "E"]);
			chain.last_finalized()
		});

		// run voter in background. scheduling it to shut down at the end.
		let finalized = env.finalized_stream();
		let voter = Voter::new(
			env.clone(),
			voters,
			0,
			Vec::new(),
			last_finalized,
			last_finalized,
		);

		let mut pool = LocalPool::new();
		pool.spawner()
			.spawn(voter.run().map(|v| v.expect("Error voting")))
			.unwrap();

		pool.spawner().spawn(routing_task).unwrap();

		// wait for the best block to finalize.
		pool.run_until(
			finalized
				.take_while(|&(_, n, _)| future::ready(n < 6))
				.for_each(|_| future::ready(())),
		)
	}

	#[test]
	fn finalizing_at_fault_threshold() {
		let _ = env_logger::try_init();

		// 10 voters
		let voters = VoterSet::new((0..10).map(|i| (Id(i), 1))).expect("nonempty");

		let (network, routing_task) = testing::environment::make_network();
		let mut pool = LocalPool::new();

		// 3 voters offline.
		let finalized_streams = (0..7)
			.map(|i| {
				let local_id = Id(i);
				// initialize chain
				let env = Arc::new(Environment::new(network.clone(), local_id));
				let last_finalized = env.with_chain(|chain| {
					chain.push_blocks(GENESIS_HASH, &["A", "B", "C", "D", "E"]);
					chain.last_finalized()
				});

				// run voter in background. scheduling it to shut down at the end.
				let finalized = env.finalized_stream();
				let voter = Voter::new(
					env.clone(),
					voters.clone(),
					0,
					Vec::new(),
					last_finalized,
					last_finalized,
				);

				pool.spawner()
					.spawn(voter.run().map(|v| v.expect("Error voting")))
					.unwrap();

				// wait for the best block to be finalized by all honest voters
				finalized
					.take_while(|&(_, n, _)| future::ready(n < 6))
					.for_each(|_| future::ready(()))
			})
			.collect::<Vec<_>>();

		pool.spawner().spawn(routing_task.map(|_| ())).unwrap();

		pool.run_until(future::join_all(finalized_streams.into_iter()));
	}
}
