use crate::{
    common::update_progressive_balances_cache::initialize_progressive_balances_cache,
    epoch_cache::{initialize_epoch_cache, PreEpochCache},
    per_epoch_processing::{Delta, Error, ParticipationEpochSummary},
};
use itertools::izip;
use safe_arith::{SafeArith, SafeArithIter};
use std::cmp::{max, min};
use std::collections::BTreeSet;
use types::{
    consts::altair::{
        NUM_FLAG_INDICES, PARTICIPATION_FLAG_WEIGHTS, TIMELY_HEAD_FLAG_INDEX,
        TIMELY_TARGET_FLAG_INDEX, WEIGHT_DENOMINATOR,
    },
    milhouse::Cow,
    ActivationQueue, BeaconState, BeaconStateError, ChainSpec, Epoch, EthSpec, ExitCache, ForkName,
    ParticipationFlags, ProgressiveBalancesCache, Unsigned, Validator,
};

pub struct SinglePassConfig {
    pub inactivity_updates: bool,
    pub rewards_and_penalties: bool,
    pub registry_updates: bool,
    pub slashings: bool,
    pub effective_balance_updates: bool,
}

impl Default for SinglePassConfig {
    fn default() -> SinglePassConfig {
        Self::enable_all()
    }
}

impl SinglePassConfig {
    pub fn enable_all() -> SinglePassConfig {
        Self {
            inactivity_updates: true,
            rewards_and_penalties: true,
            registry_updates: true,
            slashings: true,
            effective_balance_updates: true,
        }
    }

    pub fn disable_all() -> SinglePassConfig {
        SinglePassConfig {
            inactivity_updates: false,
            rewards_and_penalties: false,
            registry_updates: false,
            slashings: false,
            effective_balance_updates: false,
        }
    }
}

/// Values from the state that are immutable throughout epoch processing.
struct StateContext {
    current_epoch: Epoch,
    next_epoch: Epoch,
    is_in_inactivity_leak: bool,
    total_active_balance: u64,
    churn_limit: u64,
    fork_name: ForkName,
}

struct RewardsAndPenaltiesContext {
    unslashed_participating_increments_array: [u64; NUM_FLAG_INDICES],
    active_increments: u64,
}

struct SlashingsContext {
    adjusted_total_slashing_balance: u64,
    target_withdrawable_epoch: Epoch,
}

struct EffectiveBalancesContext {
    downward_threshold: u64,
    upward_threshold: u64,
}

#[derive(Debug, PartialEq, Clone)]
pub struct ValidatorInfo {
    pub index: usize,
    pub effective_balance: u64,
    pub base_reward: u64,
    pub is_eligible: bool,
    pub is_slashed: bool,
    pub is_active_current_epoch: bool,
    pub is_active_previous_epoch: bool,
    // Used for determining rewards.
    pub previous_epoch_participation: ParticipationFlags,
    // Used for updating the progressive balances cache for next epoch.
    pub current_epoch_participation: ParticipationFlags,
}

impl ValidatorInfo {
    #[inline]
    pub fn is_unslashed_participating_index(&self, flag_index: usize) -> Result<bool, Error> {
        Ok(self.is_active_previous_epoch
            && !self.is_slashed
            && self
                .previous_epoch_participation
                .has_flag(flag_index)
                .map_err(|_| Error::InvalidFlagIndex(flag_index))?)
    }
}

pub fn process_epoch_single_pass<E: EthSpec>(
    state: &mut BeaconState<E>,
    spec: &ChainSpec,
    conf: SinglePassConfig,
) -> Result<ParticipationEpochSummary<E>, Error> {
    initialize_epoch_cache(state, spec)?;
    initialize_progressive_balances_cache(state, None, spec)?;
    state.build_exit_cache(spec)?;

    let previous_epoch = state.previous_epoch();
    let current_epoch = state.current_epoch();
    let next_epoch = state.next_epoch()?;
    let is_in_inactivity_leak = state.is_in_inactivity_leak(previous_epoch, spec)?;
    let total_active_balance = state.get_total_active_balance()?;
    let churn_limit = state.get_churn_limit(spec)?;
    let activation_churn_limit = state.get_activation_churn_limit(spec)?;
    let finalized_checkpoint = state.finalized_checkpoint();
    let fork_name = state.fork_name_unchecked();

    let state_ctxt = &StateContext {
        current_epoch,
        next_epoch,
        is_in_inactivity_leak,
        total_active_balance,
        churn_limit,
        fork_name,
    };

    // Contexts that require immutable access to `state`.
    let slashings_ctxt = &SlashingsContext::new(state, state_ctxt, spec)?;
    let mut next_epoch_cache = PreEpochCache::new_for_next_epoch(state)?;

    // Split the state into several disjoint mutable borrows.
    let (
        validators,
        balances,
        previous_epoch_participation,
        current_epoch_participation,
        inactivity_scores,
        progressive_balances,
        exit_cache,
        epoch_cache,
    ) = state.mutable_validator_fields()?;

    let num_validators = validators.len();

    // Take a snapshot of the validators and participation before mutating. This is used for
    // informational purposes (e.g. by the validator monitor).
    let summary = ParticipationEpochSummary::new(
        validators.clone(),
        previous_epoch_participation.clone(),
        current_epoch_participation.clone(),
        previous_epoch,
        current_epoch,
    );

    // Compute shared values required for different parts of epoch processing.
    let rewards_ctxt = &RewardsAndPenaltiesContext::new(progressive_balances, state_ctxt, spec)?;
    let activation_queue = &epoch_cache
        .activation_queue()?
        .get_validators_eligible_for_activation(
            finalized_checkpoint.epoch,
            activation_churn_limit as usize,
        );
    let effective_balances_ctxt = &EffectiveBalancesContext::new(spec)?;

    // Iterate over the validators and related fields in one pass.
    let mut validators_iter = validators.iter_cow();
    let mut balances_iter = balances.iter_cow();
    let mut inactivity_scores_iter = inactivity_scores.iter_cow();

    // Values computed for the next epoch transition.
    let mut next_epoch_total_active_balance = 0;
    let mut next_epoch_activation_queue = ActivationQueue::default();

    for (index, &previous_epoch_participation, &current_epoch_participation) in izip!(
        0..num_validators,
        previous_epoch_participation.iter(),
        current_epoch_participation.iter(),
    ) {
        let (_, mut validator) = validators_iter
            .next_cow()
            .ok_or(BeaconStateError::UnknownValidator(index))?;
        let (_, mut balance) = balances_iter
            .next_cow()
            .ok_or(BeaconStateError::UnknownValidator(index))?;
        let (_, mut inactivity_score) = inactivity_scores_iter
            .next_cow()
            .ok_or(BeaconStateError::UnknownValidator(index))?;

        let is_active_current_epoch = validator.is_active_at(current_epoch);
        let is_active_previous_epoch = validator.is_active_at(previous_epoch);
        let is_eligible = is_active_previous_epoch
            || (validator.slashed()
                && previous_epoch.safe_add(1)? < validator.withdrawable_epoch());

        let base_reward = if is_eligible {
            epoch_cache.get_base_reward(index)?
        } else {
            0
        };

        let validator_info = &ValidatorInfo {
            index,
            effective_balance: validator.effective_balance(),
            base_reward,
            is_eligible,
            is_slashed: validator.slashed(),
            is_active_current_epoch,
            is_active_previous_epoch,
            previous_epoch_participation,
            current_epoch_participation,
        };

        if current_epoch != E::genesis_epoch() {
            // `process_inactivity_updates`
            if conf.inactivity_updates {
                process_single_inactivity_update(
                    &mut inactivity_score,
                    validator_info,
                    state_ctxt,
                    spec,
                )?;
            }

            // `process_rewards_and_penalties`
            if conf.rewards_and_penalties {
                process_single_reward_and_penalty(
                    &mut balance,
                    &inactivity_score,
                    validator_info,
                    rewards_ctxt,
                    state_ctxt,
                    spec,
                )?;
            }
        }

        // `process_registry_updates`
        if conf.registry_updates {
            process_single_registry_update(
                &mut validator,
                validator_info,
                exit_cache,
                activation_queue,
                &mut next_epoch_activation_queue,
                state_ctxt,
                spec,
            )?;
        }

        // `process_slashings`
        if conf.slashings {
            process_single_slashing(&mut balance, &validator, slashings_ctxt, state_ctxt, spec)?;
        }

        // `process_effective_balance_updates`
        if conf.effective_balance_updates {
            process_single_effective_balance_update(
                *balance,
                &mut validator,
                validator_info,
                &mut next_epoch_total_active_balance,
                &mut next_epoch_cache,
                progressive_balances,
                effective_balances_ctxt,
                state_ctxt,
                spec,
            )?;
        }
    }

    if conf.effective_balance_updates {
        state.set_total_active_balance(next_epoch, next_epoch_total_active_balance);
        *state.epoch_cache_mut() = next_epoch_cache.into_epoch_cache(
            next_epoch_total_active_balance,
            next_epoch_activation_queue,
            spec,
        )?;
    }

    Ok(summary)
}

fn process_single_inactivity_update(
    inactivity_score: &mut Cow<u64>,
    validator_info: &ValidatorInfo,
    state_ctxt: &StateContext,
    spec: &ChainSpec,
) -> Result<(), Error> {
    if !validator_info.is_eligible {
        return Ok(());
    }

    // Increase inactivity score of inactive validators
    if validator_info.is_unslashed_participating_index(TIMELY_TARGET_FLAG_INDEX)? {
        // Avoid mutating when the inactivity score is 0 and can't go any lower -- the common
        // case.
        if **inactivity_score == 0 {
            return Ok(());
        }
        inactivity_score.make_mut()?.safe_sub_assign(1)?;
    } else {
        inactivity_score
            .make_mut()?
            .safe_add_assign(spec.inactivity_score_bias)?;
    }

    // Decrease the score of all validators for forgiveness when not during a leak
    if !state_ctxt.is_in_inactivity_leak {
        let deduction = min(spec.inactivity_score_recovery_rate, **inactivity_score);
        inactivity_score.make_mut()?.safe_sub_assign(deduction)?;
    }

    Ok(())
}

fn process_single_reward_and_penalty(
    balance: &mut Cow<u64>,
    inactivity_score: &u64,
    validator_info: &ValidatorInfo,
    rewards_ctxt: &RewardsAndPenaltiesContext,
    state_ctxt: &StateContext,
    spec: &ChainSpec,
) -> Result<(), Error> {
    if !validator_info.is_eligible {
        return Ok(());
    }

    let mut delta = Delta::default();
    for flag_index in 0..NUM_FLAG_INDICES {
        get_flag_index_delta(
            &mut delta,
            validator_info,
            flag_index,
            rewards_ctxt,
            state_ctxt,
        )?;
    }
    get_inactivity_penalty_delta(
        &mut delta,
        validator_info,
        inactivity_score,
        state_ctxt,
        spec,
    )?;

    if delta.rewards != 0 || delta.penalties != 0 {
        let balance = balance.make_mut()?;
        balance.safe_add_assign(delta.rewards)?;
        *balance = balance.saturating_sub(delta.penalties);
    }

    Ok(())
}

fn get_flag_index_delta(
    delta: &mut Delta,
    validator_info: &ValidatorInfo,
    flag_index: usize,
    rewards_ctxt: &RewardsAndPenaltiesContext,
    state_ctxt: &StateContext,
) -> Result<(), Error> {
    let base_reward = validator_info.base_reward;
    let weight = get_flag_weight(flag_index)?;
    let unslashed_participating_increments =
        rewards_ctxt.get_unslashed_participating_increments(flag_index)?;

    if validator_info.is_unslashed_participating_index(flag_index)? {
        if !state_ctxt.is_in_inactivity_leak {
            let reward_numerator = base_reward
                .safe_mul(weight)?
                .safe_mul(unslashed_participating_increments)?;
            delta.reward(
                reward_numerator.safe_div(
                    rewards_ctxt
                        .active_increments
                        .safe_mul(WEIGHT_DENOMINATOR)?,
                )?,
            )?;
        }
    } else if flag_index != TIMELY_HEAD_FLAG_INDEX {
        delta.penalize(base_reward.safe_mul(weight)?.safe_div(WEIGHT_DENOMINATOR)?)?;
    }
    Ok(())
}

/// Get the weight for a `flag_index` from the constant list of all weights.
fn get_flag_weight(flag_index: usize) -> Result<u64, Error> {
    PARTICIPATION_FLAG_WEIGHTS
        .get(flag_index)
        .copied()
        .ok_or(Error::InvalidFlagIndex(flag_index))
}

fn get_inactivity_penalty_delta(
    delta: &mut Delta,
    validator_info: &ValidatorInfo,
    inactivity_score: &u64,
    state_ctxt: &StateContext,
    spec: &ChainSpec,
) -> Result<(), Error> {
    if !validator_info.is_unslashed_participating_index(TIMELY_TARGET_FLAG_INDEX)? {
        let penalty_numerator = validator_info
            .effective_balance
            .safe_mul(*inactivity_score)?;
        let penalty_denominator = spec
            .inactivity_score_bias
            .safe_mul(spec.inactivity_penalty_quotient_for_fork(state_ctxt.fork_name))?;
        delta.penalize(penalty_numerator.safe_div(penalty_denominator)?)?;
    }
    Ok(())
}

impl RewardsAndPenaltiesContext {
    fn new(
        progressive_balances: &ProgressiveBalancesCache,
        state_ctxt: &StateContext,
        spec: &ChainSpec,
    ) -> Result<Self, Error> {
        let mut unslashed_participating_increments_array = [0; NUM_FLAG_INDICES];
        for flag_index in 0..NUM_FLAG_INDICES {
            let unslashed_participating_balance =
                progressive_balances.previous_epoch_flag_attesting_balance(flag_index)?;
            let unslashed_participating_increments =
                unslashed_participating_balance.safe_div(spec.effective_balance_increment)?;

            *unslashed_participating_increments_array
                .get_mut(flag_index)
                .ok_or(Error::InvalidFlagIndex(flag_index))? = unslashed_participating_increments;
        }
        let active_increments = state_ctxt
            .total_active_balance
            .safe_div(spec.effective_balance_increment)?;

        Ok(Self {
            unslashed_participating_increments_array,
            active_increments,
        })
    }

    fn get_unslashed_participating_increments(&self, flag_index: usize) -> Result<u64, Error> {
        self.unslashed_participating_increments_array
            .get(flag_index)
            .copied()
            .ok_or(Error::InvalidFlagIndex(flag_index))
    }
}

fn process_single_registry_update(
    validator: &mut Cow<Validator>,
    validator_info: &ValidatorInfo,
    exit_cache: &mut ExitCache,
    activation_queue: &BTreeSet<usize>,
    next_epoch_activation_queue: &mut ActivationQueue,
    state_ctxt: &StateContext,
    spec: &ChainSpec,
) -> Result<(), Error> {
    let current_epoch = state_ctxt.current_epoch;

    if validator.is_eligible_for_activation_queue(spec) {
        validator.make_mut()?.mutable.activation_eligibility_epoch = current_epoch.safe_add(1)?;
    }

    if validator.is_active_at(current_epoch)
        && validator.effective_balance() <= spec.ejection_balance
    {
        initiate_validator_exit(validator, exit_cache, state_ctxt, spec)?;
    }

    if activation_queue.contains(&validator_info.index) {
        validator.make_mut()?.mutable.activation_epoch =
            spec.compute_activation_exit_epoch(current_epoch)?;
    }

    // Caching: add to speculative activation queue for next epoch.
    next_epoch_activation_queue.add_if_could_be_eligible_for_activation(
        validator_info.index,
        validator,
        state_ctxt.next_epoch,
        spec,
    );

    Ok(())
}

fn initiate_validator_exit(
    validator: &mut Cow<Validator>,
    exit_cache: &mut ExitCache,
    state_ctxt: &StateContext,
    spec: &ChainSpec,
) -> Result<(), Error> {
    // Return if the validator already initiated exit
    if validator.exit_epoch() != spec.far_future_epoch {
        return Ok(());
    }

    // Compute exit queue epoch
    let delayed_epoch = spec.compute_activation_exit_epoch(state_ctxt.current_epoch)?;
    let mut exit_queue_epoch = exit_cache
        .max_epoch()?
        .map_or(delayed_epoch, |epoch| max(epoch, delayed_epoch));
    let exit_queue_churn = exit_cache.get_churn_at(exit_queue_epoch)?;

    if exit_queue_churn >= state_ctxt.churn_limit {
        exit_queue_epoch.safe_add_assign(1)?;
    }

    let validator = validator.make_mut()?;
    validator.mutable.exit_epoch = exit_queue_epoch;
    validator.mutable.withdrawable_epoch =
        exit_queue_epoch.safe_add(spec.min_validator_withdrawability_delay)?;

    exit_cache.record_validator_exit(exit_queue_epoch)?;
    Ok(())
}

impl SlashingsContext {
    fn new<E: EthSpec>(
        state: &BeaconState<E>,
        state_ctxt: &StateContext,
        spec: &ChainSpec,
    ) -> Result<Self, Error> {
        let sum_slashings = state.get_all_slashings().iter().copied().safe_sum()?;
        let adjusted_total_slashing_balance = min(
            sum_slashings.safe_mul(spec.proportional_slashing_multiplier_for_state(state))?,
            state_ctxt.total_active_balance,
        );

        let target_withdrawable_epoch = state_ctxt
            .current_epoch
            .safe_add(E::EpochsPerSlashingsVector::to_u64().safe_div(2)?)?;

        Ok(Self {
            adjusted_total_slashing_balance,
            target_withdrawable_epoch,
        })
    }
}

fn process_single_slashing(
    balance: &mut Cow<u64>,
    validator: &Validator,
    slashings_ctxt: &SlashingsContext,
    state_ctxt: &StateContext,
    spec: &ChainSpec,
) -> Result<(), Error> {
    if validator.slashed()
        && slashings_ctxt.target_withdrawable_epoch == validator.withdrawable_epoch()
    {
        let increment = spec.effective_balance_increment;
        let penalty_numerator = validator
            .effective_balance()
            .safe_div(increment)?
            .safe_mul(slashings_ctxt.adjusted_total_slashing_balance)?;
        let penalty = penalty_numerator
            .safe_div(state_ctxt.total_active_balance)?
            .safe_mul(increment)?;

        *balance.make_mut()? = balance.saturating_sub(penalty);
    }
    Ok(())
}

impl EffectiveBalancesContext {
    fn new(spec: &ChainSpec) -> Result<Self, Error> {
        let hysteresis_increment = spec
            .effective_balance_increment
            .safe_div(spec.hysteresis_quotient)?;
        let downward_threshold =
            hysteresis_increment.safe_mul(spec.hysteresis_downward_multiplier)?;
        let upward_threshold = hysteresis_increment.safe_mul(spec.hysteresis_upward_multiplier)?;

        Ok(Self {
            downward_threshold,
            upward_threshold,
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn process_single_effective_balance_update(
    balance: u64,
    validator: &mut Cow<Validator>,
    validator_info: &ValidatorInfo,
    next_epoch_total_active_balance: &mut u64,
    next_epoch_cache: &mut PreEpochCache,
    progressive_balances: &mut ProgressiveBalancesCache,
    eb_ctxt: &EffectiveBalancesContext,
    state_ctxt: &StateContext,
    spec: &ChainSpec,
) -> Result<(), Error> {
    let old_effective_balance = validator.effective_balance();
    let new_effective_balance = if balance.safe_add(eb_ctxt.downward_threshold)?
        < validator.effective_balance()
        || validator
            .effective_balance()
            .safe_add(eb_ctxt.upward_threshold)?
            < balance
    {
        min(
            balance.safe_sub(balance.safe_rem(spec.effective_balance_increment)?)?,
            spec.max_effective_balance,
        )
    } else {
        validator.effective_balance()
    };

    if validator.is_active_at(state_ctxt.next_epoch) {
        next_epoch_total_active_balance.safe_add_assign(new_effective_balance)?;
    }

    if new_effective_balance != old_effective_balance {
        validator.make_mut()?.mutable.effective_balance = new_effective_balance;

        // Update progressive balances cache for the *current* epoch, which will soon become the
        // previous epoch once the epoch transition completes.
        progressive_balances.on_effective_balance_change(
            validator.slashed(),
            validator_info.current_epoch_participation,
            old_effective_balance,
            new_effective_balance,
        )?;
    }

    // Caching: update next epoch effective balances.
    next_epoch_cache.push_effective_balance(new_effective_balance);

    Ok(())
}