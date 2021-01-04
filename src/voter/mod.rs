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

use std::fmt::Debug;

use futures::{future::FusedFuture, Future, Sink, Stream};

use crate::{
	BlockNumberOps, Chain, Equivocation, Message, Precommit, Prevote, PrimaryPropose, SignedMessage,
};

mod past_rounds;
mod voting_round;

pub trait Environment<H: Eq, N>: Chain<H, N> {
	type Id: Clone + Debug + Ord;
	type Signature: Clone + Eq;

	type In: Stream<Item = Result<SignedMessage<H, N, Self::Signature, Self::Id>, Self::Error>>
		+ Unpin;

	type Out: Sink<Message<H, N>, Error = Self::Error> + Unpin;

	type Timer: Future<Output = Result<(), Self::Error>> + FusedFuture + Unpin;

	type Error: From<crate::Error>;

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
}
