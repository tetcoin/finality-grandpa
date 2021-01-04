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

//! Logic for voting and handling messages within a single round.

use std::{fmt::Debug, sync::Arc};

use futures::{
	channel::mpsc::UnboundedReceiver, future::FusedFuture, stream::Fuse, FutureExt, SinkExt,
	Stream, StreamExt,
};
use log::{debug, trace, warn};

use super::Environment;
use crate::{
	round::{Round, State as RoundState},
	validate_commit,
	weights::VoteWeight,
	BlockNumberOps, Commit, HistoricalVotes, ImportResult, Message, Precommit, Prevote,
	PrimaryPropose, SignedMessage, SignedPrecommit,
};

/// The state of a voting round.
pub(super) enum State<T> {
	Start(T, T),
	Proposed(T, T),
	Prevoted(T),
	Precommitted,
}

impl<T> std::fmt::Debug for State<T> {
	fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
		match self {
			State::Start(..) => write!(f, "Start"),
			State::Proposed(..) => write!(f, "Proposed"),
			State::Prevoted(_) => write!(f, "Prevoted"),
			State::Precommitted => write!(f, "Precommitted"),
		}
	}
}

/// Whether we should vote in the current round (i.e. push votes to the sink.)
enum Voting {
	/// Voting is disabled for the current round.
	No,
	/// Voting is enabled for the current round (prevotes and precommits.)
	Yes,
	/// Voting is enabled for the current round and we are the primary proposer
	/// (we can also push primary propose messages).
	Primary,
}

impl Voting {
	/// Whether the voter should cast round votes (prevotes and precommits.)
	fn is_active(&self) -> bool {
		match self {
			Voting::Yes | Voting::Primary => true,
			_ => false,
		}
	}

	/// Whether the voter is the primary proposer.
	fn is_primary(&self) -> bool {
		match self {
			Voting::Primary => true,
			_ => false,
		}
	}
}

enum PrevoteResult {
	Prevoted,
	Skipped,
	Failed,
}

/// Logic for a voter on a specific round.
pub(super) struct VotingRound<H, N, E>
where
	H: Ord,
	E: Environment<H, N>,
{
	env: Arc<E>,
	voting: Voting,
	incoming: Fuse<E::In>,
	outgoing: E::Out,
	round: Round<E::Id, H, N, E::Signature>,
	state: Option<State<E::Timer>>,
	primary_block: Option<(H, N)>,
	best_finalized: Option<Commit<H, N, E::Signature, E::Id>>,
	last_round_state: Option<RoundState<H, N>>,
	last_round_state_updates: Option<UnboundedReceiver<RoundState<H, N>>>,
}

impl<H, N, E> VotingRound<H, N, E>
where
	H: Clone + Debug + Eq + Ord,
	N: BlockNumberOps + Debug,
	E: Environment<H, N>,
{
	fn handle_vote(
		&mut self,
		vote: SignedMessage<H, N, E::Signature, E::Id>,
	) -> Result<(), E::Error> {
		let SignedMessage {
			message,
			signature,
			id,
		} = vote;

		if !self
			.env
			.is_equal_or_descendent_of(self.round.base().0, message.target().0.clone())
		{
			trace!(target: "afg",
				"Ignoring message targeting {:?} lower than round base {:?}",
				message.target(), self.round.base(),
			);

			return Ok(());
		}

		match message {
			Message::Prevote(prevote) => {
				let import_result = self
					.round
					.import_prevote(&*self.env, prevote, id, signature)?;

				if let Some(equivocation) = import_result.equivocation {
					self.env
						.prevote_equivocation(self.round.number(), equivocation);
				}
			}
			Message::Precommit(precommit) => {
				let import_result = self
					.round
					.import_precommit(&*self.env, precommit, id, signature)?;

				if let Some(equivocation) = import_result.equivocation {
					self.env
						.precommit_equivocation(self.round.number(), equivocation);
				}
			}
			Message::PrimaryPropose(primary) => {
				let primary_id = self.round.primary_voter().0.clone();

				// note that id here refers to the party which has cast the vote
				// and not the id of the party which has received the vote message.
				if id == primary_id {
					self.primary_block = Some((primary.target_hash, primary.target_number));
				}
			}
		}

		Ok(())
	}

	async fn primary_propose(
		&mut self,
		last_round_state: &RoundState<H, N>,
	) -> Result<bool, E::Error> {
		let maybe_estimate = last_round_state.estimate.clone();

		match (maybe_estimate, self.voting.is_primary()) {
			(Some(last_round_estimate), true) => {
				let maybe_finalized = last_round_state.finalized.clone();

				// Last round estimate has not been finalized.
				let should_send_primary =
					maybe_finalized.map_or(true, |f| last_round_estimate.1 > f.1);

				if should_send_primary {
					debug!(target: "afg", "Sending primary block hint for round {}", self.round.number());

					let primary = PrimaryPropose {
						target_hash: last_round_estimate.0,
						target_number: last_round_estimate.1,
					};

					self.env.proposed(self.round.number(), primary.clone())?;
					self.outgoing.send(Message::PrimaryPropose(primary)).await?;

					return Ok(true);
				} else {
					debug!(target: "afg",
						"Last round estimate has been finalized, not sending primary block hint for round {}",
						self.round.number(),
					);
				}
			}
			(None, true) => {
				debug!(target: "afg",
					"Last round estimate does not exist, not sending primary block hint for round {}",
					self.round.number(),
				);
			}
			_ => {}
		}

		Ok(false)
	}

	async fn prevote(
		&mut self,
		prevote_timer_ready: bool,
		last_round_state: &RoundState<H, N>,
	) -> Result<PrevoteResult, E::Error> {
		let should_prevote = prevote_timer_ready || self.round.completable();

		if should_prevote && self.voting.is_active() {
			if let Some(prevote) = self.construct_prevote(last_round_state)? {
				debug!(target: "afg", "Casting prevote for round {}", self.round.number());

				self.round.set_prevoted_index();

				self.env.prevoted(self.round.number(), prevote.clone())?;
				self.outgoing.send(Message::Prevote(prevote)).await?;

				return Ok(PrevoteResult::Prevoted);
			} else {
				// when we can't construct a prevote, we shouldn't
				// precommit.
				self.voting = Voting::No;

				return Ok(PrevoteResult::Failed);
			}
		}

		Ok(PrevoteResult::Skipped)
	}

	async fn precommit(
		&mut self,
		precommit_timer_ready: bool,
		last_round_state: &RoundState<H, N>,
	) -> Result<bool, E::Error> {
		let last_round_estimate = last_round_state
			.estimate
			.clone()
			.expect("Rounds only started when prior round completable; qed");

		let should_precommit = {
			// we wait for the last round's estimate to be equal to or
			// the ancestor of the current round's p-Ghost before precommitting.
			let last_round_estimate_lower_or_equal_to_prevote_ghost = self
				.round
				.state()
				.prevote_ghost
				.as_ref()
				.map_or(false, |p_g| {
					p_g == &last_round_estimate
						|| self
							.env
							.is_equal_or_descendent_of(last_round_estimate.0, p_g.0.clone())
				});

			last_round_estimate_lower_or_equal_to_prevote_ghost
				&& (precommit_timer_ready || self.round.completable())
		};

		if should_precommit && self.voting.is_active() {
			debug!(target: "afg", "Casting precommit for round {}", self.round.number());

			let precommit = self.construct_precommit();
			self.round.set_precommitted_index();

			self.env
				.precommitted(self.round.number(), precommit.clone())?;
			self.outgoing.send(Message::Precommit(precommit)).await?;

			return Ok(true);
		}

		Ok(false)
	}

	// construct a prevote message based on local state.
	fn construct_prevote(
		&self,
		last_round_state: &RoundState<H, N>,
	) -> Result<Option<Prevote<H, N>>, E::Error> {
		let last_round_estimate = last_round_state
			.estimate
			.clone()
			.expect("Rounds only started when prior round completable; qed");

		let find_descendent_of = match self.primary_block {
			None => {
				// vote for best chain containing prior round-estimate.
				last_round_estimate.0
			}
			Some(ref primary_block) => {
				// we will vote for the best chain containing `p_hash` iff
				// the last round's prevote-GHOST included that block and
				// that block is a strict descendent of the last round-estimate that we are
				// aware of.
				let last_prevote_g = last_round_state
					.prevote_ghost
					.clone()
					.expect("Rounds only started when prior round completable; qed");

				// if the blocks are equal, we don't check ancestry.
				if primary_block == &last_prevote_g {
					primary_block.0.clone()
				} else if primary_block.1 >= last_prevote_g.1 {
					last_round_estimate.0
				} else {
					// from this point onwards, the number of the primary-broadcasted
					// block is less than the last prevote-GHOST's number.
					// if the primary block is in the ancestry of p-G we vote for the
					// best chain containing it.
					let &(ref p_hash, p_num) = primary_block;
					match self
						.env
						.ancestry(last_round_estimate.0.clone(), last_prevote_g.0.clone())
					{
						Ok(ancestry) => {
							let to_sub = p_num + N::one();

							let offset: usize = if last_prevote_g.1 < to_sub {
								0
							} else {
								(last_prevote_g.1 - to_sub).as_()
							};

							if ancestry.get(offset).map_or(false, |b| b == p_hash) {
								p_hash.clone()
							} else {
								last_round_estimate.0
							}
						}
						Err(crate::Error::NotDescendent) => {
							// This is only possible in case of massive equivocation
							warn!(target: "afg",
								"Possible case of massive equivocation: \
								last round prevote GHOST: {:?} is not a descendant of last round estimate: {:?}",
								last_prevote_g, last_round_estimate,
							);

							last_round_estimate.0
						}
					}
				}
			}
		};

		let best_chain = self.env.best_chain_containing(find_descendent_of.clone());
		debug_assert!(
			best_chain.is_some(),
			"Previously known block {:?} has disappeared from chain",
			find_descendent_of
		);

		let t = if let Some(target) = best_chain {
			target
		} else {
			// If this block is considered unknown, something has gone wrong.
			// log and handle, but skip casting a vote.
			warn!(target: "afg",
				"Could not cast prevote: previously known block {:?} has disappeared",
				find_descendent_of,
			);
			return Ok(None);
		};

		Ok(Some(Prevote {
			target_hash: t.0,
			target_number: t.1,
		}))
	}

	// construct a precommit message based on local state.
	fn construct_precommit(&self) -> Precommit<H, N> {
		let t = match self.round.state().prevote_ghost {
			Some(target) => target,
			None => self.round.base(),
		};

		Precommit {
			target_hash: t.0,
			target_number: t.1,
		}
	}

	pub async fn run(mut self) -> Result<(), E::Error> {
		let mut last_round_state_updates = match self.last_round_state_updates.take() {
			Some(stream) => stream.boxed_local().fuse(),
			None => futures::stream::pending().boxed_local().fuse(),
		};

		let mut last_round_state = std::mem::take(&mut self.last_round_state);

		macro_rules! handle_inputs {
			($timer:expr) => {{
				futures::select! {
					vote = self.incoming.select_next_some() => {
						self.handle_vote(vote?)?;
					}
					state = last_round_state_updates.select_next_some() => {
						last_round_state = Some(state);
					}
					_ = &mut $timer => {}
				}
				$timer.is_terminated()
			}};
			() => {
				handle_inputs!(futures::future::pending::<()>())
			};
		}

		loop {
			match self.state.take() {
				Some(State::Start(mut prevote_timer, precommit_timer)) => {
					let prevote_timer_ready = handle_inputs!(prevote_timer);

					if let Some(last_round_state) = last_round_state.as_ref() {
						let proposed = self.primary_propose(last_round_state).await?;
						let prevote_result = self.prevote(prevote_timer_ready, last_round_state).await?;

						match prevote_result {
							PrevoteResult::Prevoted => {
								self.state = Some(State::Prevoted(precommit_timer));
							}
							PrevoteResult::Skipped => {
								if proposed {
									self.state = Some(State::Proposed(prevote_timer, precommit_timer));
								} else {
									self.state = Some(State::Start(prevote_timer, precommit_timer));
								}
							}
							PrevoteResult::Failed => {
								// we failed to construct the prevote, so let's make sure we don't
								// try to vote anymore in this round.
								self.state = None;
							}
						}
					} else {
						self.state = Some(State::Start(prevote_timer, precommit_timer));
					}
				}
				Some(State::Proposed(mut prevote_timer, precommit_timer)) => {
					let prevote_timer_ready = handle_inputs!(prevote_timer);

					if let Some(last_round_state) = last_round_state.as_ref() {
						match self.prevote(prevote_timer_ready, last_round_state).await? {
							PrevoteResult::Prevoted => {
								self.state = Some(State::Prevoted(precommit_timer));
							}
							PrevoteResult::Skipped => {
								self.state = Some(State::Proposed(prevote_timer, precommit_timer));
							}
							PrevoteResult::Failed => {
								// we failed to construct the prevote, so let's make sure we don't
								// try to vote anymore in this round.
								self.state = None;
							}
						}
					} else {
						self.state = Some(State::Proposed(prevote_timer, precommit_timer));
					}
				}
				Some(State::Prevoted(mut precommit_timer)) => {
					let precommit_timer_ready = handle_inputs!(precommit_timer);

					if let Some(last_round_state) = last_round_state.as_ref() {
						let precommitted =
							self.precommit(precommit_timer_ready, last_round_state).await?;

						if precommitted {
							self.state = Some(State::Precommitted);
						} else {
							self.state = Some(State::Prevoted(precommit_timer));
						}
					} else {
						self.state = Some(State::Prevoted(precommit_timer));
					}
				}
				Some(State::Precommitted) => {
					handle_inputs!();
					self.state = Some(State::Precommitted);
				}
				None => {
					handle_inputs!();
					self.state = None;
				}
			}

			if self.round.completable() {
				let last_round_estimate_finalized = match last_round_state {
					Some(RoundState {
						estimate: Some((_, last_round_estimate)),
						finalized: Some((_, last_round_finalized)),
						..
					}) => {
						// either it was already finalized in the previous round
						let finalized_in_last_round = last_round_estimate <= last_round_finalized;

						// or it must be finalized in the current round
						let finalized_in_current_round =
							self.round
								.finalized()
								.map_or(false, |(_, current_round_finalized)| {
									last_round_estimate <= *current_round_finalized
								});

						finalized_in_last_round || finalized_in_current_round
					}
					None => {
						// NOTE: when we catch up to a round we complete the round
						// without any last round state. in this case we already started
						// a new round after we caught up so this guard is unneeded.
						true
					}
					_ => false,
				};

				if last_round_estimate_finalized {
					// TODO: return background round
					return Ok(());
				}
			}
		}
	}
}
