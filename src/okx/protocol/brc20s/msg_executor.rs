use crate::okx::{
  datastore::{
    balance::{
      convert_amount_with_decimal, convert_amount_without_decimal,
      convert_pledged_tick_with_decimal, convert_pledged_tick_without_decimal, get_raw_brc20_tick,
      get_stake_dec, get_user_common_balance, stake_is_exist,
    },
    brc20s::{
      Balance, DeployPoolEvent, DeployTickEvent, DepositEvent, Event, InscribeTransferEvent,
      MintEvent, PassiveWithdrawEvent, Pid, PoolInfo, Receipt, StakeInfo, Tick, TickId, TickInfo,
      TransferEvent, TransferInfo, TransferableAsset, UserInfo, WithdrawEvent,
    },
    ScriptKey,
  },
  protocol::{
    brc20s::{
      hash::caculate_tick_id,
      operation::Operation,
      params::{BIGDECIMAL_TEN, MAX_DECIMAL_WIDTH},
      version, BRC20SError, Deploy, Error, Message, Mint, Num, PassiveUnStake, Stake, Transfer,
      UnStake,
    },
    utils, BlockContext,
  },
  reward,
};

use crate::okx::datastore::brc20;
use crate::okx::datastore::brc20s;
use crate::okx::datastore::brc20s::PledgedTick;
use crate::okx::datastore::ord;
use crate::{InscriptionId, Result, SatPoint};
use anyhow::anyhow;
use bigdecimal::num_bigint::Sign;
use bitcoin::{Network, Txid};
use std::cmp;
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq)]
pub struct ExecutionMessage {
  pub(crate) txid: Txid,
  pub(crate) inscription_id: InscriptionId,
  pub(crate) inscription_number: i64,
  pub(crate) commit_input_satpoint: Option<SatPoint>,
  pub(crate) old_satpoint: SatPoint,
  pub(crate) new_satpoint: SatPoint,
  pub(crate) commit_from: Option<ScriptKey>,
  pub(crate) from: ScriptKey,
  pub(crate) to: Option<ScriptKey>,
  pub(crate) op: Operation,
}

impl ExecutionMessage {
  pub fn from_message<O: ord::DataStoreReadOnly>(
    ord_store: &O,
    msg: &Message,
    network: Network,
  ) -> Result<Self> {
    Ok(Self {
      txid: msg.txid,
      inscription_id: msg.inscription_id,
      inscription_number: utils::get_inscription_number_by_id(msg.inscription_id, ord_store)?,
      commit_input_satpoint: msg.commit_input_satpoint,
      old_satpoint: msg.old_satpoint,
      new_satpoint: msg
        .new_satpoint
        .ok_or(anyhow!("new satpoint cannot be None"))?,
      commit_from: msg
        .commit_input_satpoint
        .map(|satpoint| utils::get_script_key_on_satpoint(satpoint, ord_store, network))
        .transpose()?,
      from: utils::get_script_key_on_satpoint(msg.old_satpoint, ord_store, network)?,
      to: if msg.sat_in_outputs {
        Some(utils::get_script_key_on_satpoint(
          msg.new_satpoint.unwrap(),
          ord_store,
          network,
        )?)
      } else {
        None
      },
      op: msg.op.clone(),
    })
  }
}

pub fn execute<'a, M: brc20::DataStoreReadWrite, N: brc20s::DataStoreReadWrite>(
  context: BlockContext,
  config: version::Config,
  brc20_store: &'a M,
  brc20s_store: &'a N,
  msg: &ExecutionMessage,
) -> Result<Option<Receipt>> {
  log::debug!("BRC20S execute message: {:?}", msg);
  let mut is_save_receipt = true;
  let event = match &msg.op {
    Operation::Deploy(deploy) => process_deploy(
      context,
      config.clone(),
      brc20_store,
      brc20s_store,
      msg,
      deploy.clone(),
    ),
    Operation::Stake(stake) => process_stake(
      context,
      config.clone(),
      brc20_store,
      brc20s_store,
      msg,
      stake.clone(),
    )
    .map(|event| vec![event]),
    Operation::UnStake(unstake) => process_unstake(
      context,
      config.clone(),
      brc20_store,
      brc20s_store,
      msg,
      unstake.clone(),
    )
    .map(|event| vec![event]),
    Operation::PassiveUnStake(passive_unstake) => {
      let events = process_passive_unstake(
        context,
        config,
        brc20_store,
        brc20s_store,
        msg,
        passive_unstake.clone(),
      );
      match &events {
        Ok(events) => {
          if events.is_empty() {
            is_save_receipt = false
          }
        }
        Err(e) => {
          log::debug!("execute passive failed: {:?}", e.to_string());
          is_save_receipt = false
        }
      };
      events
    }
    Operation::Mint(mint) => process_mint(
      context,
      config.clone(),
      brc20_store,
      brc20s_store,
      msg,
      mint.clone(),
    )
    .map(|event| vec![event]),
    Operation::InscribeTransfer(transfer) => process_inscribe_transfer(
      context,
      config.clone(),
      brc20_store,
      brc20s_store,
      msg,
      transfer.clone(),
    )
    .map(|event| vec![event]),
    Operation::Transfer(_) => {
      process_transfer(context, config, brc20_store, brc20s_store, msg).map(|event| vec![event])
    }
  };

  if !is_save_receipt {
    return Ok(None);
  }

  let receipt = Receipt {
    inscription_id: msg.inscription_id,
    inscription_number: msg.inscription_number,
    old_satpoint: msg.old_satpoint,
    new_satpoint: msg.new_satpoint,
    from: msg.from.clone(),
    to: msg.to.clone().map_or(msg.from.clone(), |v| v),
    op: msg.op.op_type(),
    result: match event {
      Ok(event) => Ok(event),
      Err(Error::BRC20SError(e)) => Err(e),
      Err(e) => return Err(anyhow!("BRC20S execute exception: {e}")),
    },
  };

  log::debug!("BRC20S message receipt: {:?}", receipt);
  brc20s_store
    .add_transaction_receipt(&msg.txid, &receipt)
    .map_err(|e| anyhow!("failed to set transaction receipts to state! error: {e}"))?;
  Ok(Some(receipt))
}

pub fn process_deploy<'a, M: brc20::DataStoreReadWrite, N: brc20s::DataStoreReadWrite>(
  context: BlockContext,
  config: version::Config,
  brc20_store: &'a M,
  brc20s_store: &'a N,
  msg: &ExecutionMessage,
  deploy: Deploy,
) -> Result<Vec<Event>, Error<N>> {
  // ignore inscribe inscription to coinbase.
  let to_script_key = msg.to.clone().ok_or(BRC20SError::InscribeToCoinbase)?;
  let mut events = Vec::new();
  // inscription message basic availability check
  if let Some(iserr) = deploy.validate_basic().err() {
    return Err(Error::BRC20SError(iserr));
  }

  let from_script_key = match msg.commit_from.clone() {
    Some(script) => script,
    None => {
      return Err(Error::BRC20SError(BRC20SError::InternalError(
        "commit from script pubkey not exist".to_string(),
      )));
    }
  };

  let tick_id = deploy.get_tick_id();
  let pid = deploy.get_pool_id();
  let ptype = deploy.get_pool_type();
  let only = deploy.get_only();
  let mut stake = deploy.get_stake_id();
  // temp disable
  // brc20-s can not be staked
  if !version::tick_can_staked(&stake, &config) {
    return Err(Error::BRC20SError(BRC20SError::StakeNoPermission(
      stake.to_string(),
    )));
  }

  // share pool can not be deploy
  if !only && !config.allow_share_pool {
    return Err(Error::BRC20SError(BRC20SError::ShareNoPermission()));
  }
  //check stake
  if !stake_is_exist(&stake, brc20s_store, brc20_store) {
    return Err(Error::BRC20SError(BRC20SError::StakeNotFound(
      stake.to_string(),
    )));
  }

  if stake.is_brc20() {
    let tick = get_raw_brc20_tick(stake.clone(), brc20_store);
    match tick {
      Some(t) => {
        stake = PledgedTick::BRC20Tick(t);
      }
      _ => {
        return Err(Error::BRC20SError(BRC20SError::InternalError(format!(
          "not found brc20 token:{}",
          stake.to_string()
        ))));
      }
    }
  }

  // check pool is exist, if true return error
  if brc20s_store
    .get_pid_to_poolinfo(&pid)
    .map_err(|e| Error::LedgerError(e))?
    .is_some()
  {
    return Err(Error::BRC20SError(BRC20SError::PoolAlreadyExist(
      pid.as_str().to_string(),
    )));
  }

  let erate: u128;

  let earn_tick = deploy.get_earn_tick();
  let dmax_str = deploy.distribution_max.as_str();
  let dmax: u128;

  //Get or create the tick
  if let Some(mut stored_tick) = brc20s_store
    .get_tick_info(&tick_id)
    .map_err(|e| Error::LedgerError(e))?
  {
    if stored_tick.name != earn_tick {
      return Err(Error::BRC20SError(BRC20SError::TickNameNotMatch(
        deploy.earn.clone(),
      )));
    }

    if !stored_tick.deployer.eq(&to_script_key) {
      return Err(Error::BRC20SError(BRC20SError::DeployerNotEqual(
        pid.as_str().to_string(),
        stored_tick.deployer.to_string(),
        to_script_key.to_string(),
      )));
    }

    if !to_script_key.eq(&from_script_key) {
      return Err(Error::BRC20SError(BRC20SError::FromToNotEqual(
        from_script_key.to_string(),
        to_script_key.to_string(),
      )));
    }

    // check stake has exist in tick's pools
    if brc20s_store
      .get_tickid_stake_to_pid(&tick_id, &stake)
      .map_err(|e| Error::LedgerError(e))?
      .is_some()
    {
      return Err(Error::BRC20SError(BRC20SError::StakeAlreadyExist(
        stake.to_string(),
        tick_id.hex(),
      )));
    }

    dmax = convert_amount_with_decimal(dmax_str, stored_tick.decimal)?.checked_to_u128()?;
    // check dmax
    if stored_tick.supply - stored_tick.allocated < dmax {
      return Err(Error::BRC20SError(BRC20SError::InsufficientTickSupply(
        deploy.distribution_max,
      )));
    }
    stored_tick.allocated += dmax;
    stored_tick.pids.push(pid.clone());
    erate = convert_amount_with_decimal(deploy.earn_rate.as_str(), stored_tick.decimal)?
      .checked_to_u128()?;
    brc20s_store
      .set_tick_info(&tick_id, &stored_tick)
      .map_err(|e| Error::LedgerError(e))?;
  } else {
    let decimal = Num::from_str(&deploy.decimals.map_or(MAX_DECIMAL_WIDTH.to_string(), |v| v))?
      .checked_to_u8()?;
    if decimal > MAX_DECIMAL_WIDTH {
      return Err(Error::BRC20SError(BRC20SError::DecimalsTooLarge(decimal)));
    }

    let supply_str = deploy.total_supply.ok_or(BRC20SError::InternalError(
      "the first deploy must be set total supply".to_string(),
    ))?;
    let total_supply = convert_amount_with_decimal(supply_str.as_str(), decimal)?;
    erate = convert_amount_with_decimal(deploy.earn_rate.as_str(), decimal)?.checked_to_u128()?;

    let supply = total_supply.checked_to_u128()?;
    let c_tick_id = caculate_tick_id(
      earn_tick.as_str(),
      convert_amount_without_decimal(supply, decimal)?.checked_to_u128()?,
      decimal,
      &from_script_key,
      &to_script_key,
    );
    if !c_tick_id.eq(&tick_id) {
      return Err(Error::BRC20SError(BRC20SError::InvalidPoolTickId(
        tick_id.hex(),
        c_tick_id.hex(),
      )));
    }

    let pids = vec![pid.clone()];
    dmax = convert_amount_with_decimal(dmax_str, decimal)?.checked_to_u128()?;
    let tick = TickInfo::new(
      tick_id,
      &earn_tick,
      &msg.inscription_id.clone(),
      dmax,
      decimal,
      0_u128,
      supply,
      &to_script_key,
      context.blockheight,
      context.blocktime,
      context.blockheight,
      pids,
    );
    brc20s_store
      .set_tick_info(&tick_id, &tick)
      .map_err(|e| Error::LedgerError(e))?;

    events.push(Event::DeployTick(DeployTickEvent {
      tick_id,
      name: earn_tick,
      supply: tick.supply,
      decimal: tick.decimal,
    }));
  };

  let pool = PoolInfo::new(
    &pid,
    &ptype,
    &msg.inscription_id.clone(),
    &stake,
    erate,
    0,
    0,
    dmax,
    "0".to_string(),
    context.blockheight,
    only,
    context.blockheight,
    context.blocktime,
  );

  brc20s_store
    .set_pid_to_poolinfo(&pool.pid, &pool)
    .map_err(|e| Error::LedgerError(e))?;
  brc20s_store
    .set_tickid_stake_to_pid(&tick_id, &stake, &pid)
    .map_err(|e| Error::LedgerError(e))?;

  events.push(Event::DeployPool(DeployPoolEvent {
    pid,
    ptype,
    stake,
    erate,
    dmax,
    only,
  }));
  Ok(events)
}

fn process_stake<'a, M: brc20::DataStoreReadWrite, N: brc20s::DataStoreReadWrite>(
  context: BlockContext,
  config: version::Config,
  brc20_store: &'a M,
  brc20s_store: &'a N,
  msg: &ExecutionMessage,
  stake_msg: Stake,
) -> Result<Event, Error<N>> {
  // ignore inscribe inscription to coinbase.
  let to_script_key = msg.to.clone().ok_or(BRC20SError::InscribeToCoinbase)?;
  if let Some(err) = stake_msg.validate_basic().err() {
    return Err(Error::BRC20SError(err));
  }
  let pool_id = stake_msg.get_pool_id();

  let from_script_key = match msg.commit_from.clone() {
    Some(script) => script,
    None => {
      return Err(Error::BRC20SError(BRC20SError::InternalError(
        "".to_string(),
      )));
    }
  };
  if !to_script_key.eq(&from_script_key) {
    return Err(Error::BRC20SError(BRC20SError::FromToNotEqual(
      from_script_key.to_string(),
      to_script_key.to_string(),
    )));
  }

  let mut pool = brc20s_store
    .get_pid_to_poolinfo(&pool_id)
    .map_err(|e| Error::LedgerError(e))?
    .ok_or(Error::BRC20SError(BRC20SError::PoolNotExist(
      pool_id.as_str().to_string(),
    )))?;

  let stake_tick = pool.stake.clone();
  let amount = convert_pledged_tick_with_decimal(
    &stake_tick,
    stake_msg.amount.as_str(),
    brc20s_store,
    brc20_store,
  )?;

  // check user balance of stake is more than ammount to staked
  let stake_balance =
    get_user_common_balance(&to_script_key, &stake_tick, brc20s_store, brc20_store);

  let is_first_stake: bool;
  let mut userinfo = match brc20s_store.get_pid_to_use_info(&to_script_key, &pool_id) {
    Ok(Some(info)) => {
      is_first_stake = info.staked == 0_u128;
      info
    }
    _ => {
      is_first_stake = true;
      UserInfo::default(&pool_id)
    }
  };

  let mut user_stakeinfo = brc20s_store
    .get_user_stakeinfo(&to_script_key, &stake_tick)
    .map_err(|e| Error::LedgerError(e))?
    .map_or(StakeInfo::new(vec![], &stake_tick, 0, 0), |v| v);

  // Verifying weather more than max_staked_pool_num at there is a bug which user deposit up to max_staked_pool_num use can not staked any pool.
  // So we disable follow code after update max_staked_pool_num

  if u64::try_from(user_stakeinfo.pool_stakes.len()).unwrap() == config.max_staked_pool_num {
    return Err(Error::BRC20SError(BRC20SError::InternalError(
      "the number of stake pool is full".to_string(),
    )));
  }

  let staked_total =
    Num::from(user_stakeinfo.total_only).checked_add(&Num::from(user_stakeinfo.max_share))?;
  if stake_balance.lt(&staked_total) {
    return Err(Error::BRC20SError(BRC20SError::InternalError(
      "got serious error stake_balance < user staked total".to_string(),
    )));
  }
  let has_staked = Num::from(userinfo.staked);
  let can_stake_balance = if pool.only {
    stake_balance.checked_sub(&staked_total)?
  } else {
    stake_balance
      .checked_sub(&Num::from(user_stakeinfo.total_only))?
      .checked_sub(&has_staked)?
  };
  if can_stake_balance.lt(&amount) {
    return Err(Error::BRC20SError(BRC20SError::InsufficientBalance(
      amount.truncate_to_str().unwrap(),
      can_stake_balance.to_string(),
    )));
  }

  let dec = get_stake_dec(&stake_tick, brc20s_store, brc20_store);
  reward::update_pool(
    &mut pool,
    context.blockheight,
    dec,
    config.pool_acc_reward_per_share_scale,
  )?;
  let mut reward = 0_u128;
  if !is_first_stake {
    reward = reward::withdraw_user_reward(&mut userinfo, &pool, dec)?;
  }
  // updated user balance of stakedhehe =
  userinfo.staked = has_staked.checked_add(&amount)?.checked_to_u128()?;
  reward::update_user_stake(&mut userinfo, &pool, dec)?;

  //update the stake_info of user
  user_stakeinfo
    .pool_stakes
    .retain(|(pid, _, _)| *pid != pool_id);
  user_stakeinfo
    .pool_stakes
    .insert(0, (pool_id.clone(), pool.only, userinfo.staked));

  if pool.only {
    user_stakeinfo.total_only = Num::from(user_stakeinfo.total_only)
      .checked_add(&amount)?
      .checked_to_u128()?;
  } else {
    user_stakeinfo.max_share = cmp::max(user_stakeinfo.max_share, userinfo.staked)
  }

  // Verifying weather more than max_staked_pool_num.
  if u64::try_from(user_stakeinfo.pool_stakes.len()).unwrap() > config.max_staked_pool_num {
    return Err(Error::BRC20SError(BRC20SError::InternalError(
      "the number of stake pool is full".to_string(),
    )));
  }

  // update pool_info for stake
  pool.staked = Num::from(pool.staked)
    .checked_add(&amount)?
    .checked_to_u128()?;

  brc20s_store
    .set_pid_to_use_info(&to_script_key, &pool_id, &userinfo)
    .map_err(|e| Error::LedgerError(e))?;

  brc20s_store
    .set_user_stakeinfo(&to_script_key, &stake_tick, &user_stakeinfo)
    .map_err(|e| Error::LedgerError(e))?;

  brc20s_store
    .set_pid_to_poolinfo(&pool_id, &pool)
    .map_err(|e| Error::LedgerError(e))?;

  Ok(Event::Deposit(DepositEvent {
    pid: pool_id,
    amt: amount.checked_to_u128()?,
    period_settlement_reward: reward,
  }))
}

fn process_unstake<'a, M: brc20::DataStoreReadWrite, N: brc20s::DataStoreReadWrite>(
  context: BlockContext,
  config: version::Config,
  brc20_store: &'a M,
  brc20s_store: &'a N,
  msg: &ExecutionMessage,
  unstake: UnStake,
) -> Result<Event, Error<N>> {
  // ignore inscribe inscription to coinbase.
  let to_script_key = msg.to.clone().ok_or(BRC20SError::InscribeToCoinbase)?;
  if let Some(err) = unstake.validate_basic().err() {
    return Err(Error::BRC20SError(err));
  }
  let pool_id = unstake.get_pool_id();
  let from_script_key = match msg.commit_from.clone() {
    Some(script) => script,
    None => {
      return Err(Error::BRC20SError(BRC20SError::InternalError(
        "".to_string(),
      )));
    }
  };
  if !to_script_key.eq(&from_script_key) {
    return Err(Error::BRC20SError(BRC20SError::FromToNotEqual(
      from_script_key.to_string(),
      to_script_key.to_string(),
    )));
  }

  let mut pool = brc20s_store
    .get_pid_to_poolinfo(&pool_id)
    .map_err(|e| Error::LedgerError(e))?
    .ok_or(Error::BRC20SError(BRC20SError::PoolNotExist(
      pool_id.as_str().to_string(),
    )))?;

  let stake_tick = pool.stake.clone();

  let amount = convert_pledged_tick_with_decimal(
    &stake_tick,
    unstake.amount.as_str(),
    brc20s_store,
    brc20_store,
  )?;

  let mut userinfo = brc20s_store
    .get_pid_to_use_info(&to_script_key, &pool_id)
    .map_or(Some(UserInfo::default(&pool_id)), |v| v)
    .unwrap_or(UserInfo::default(&pool_id));
  let has_staked = Num::from(userinfo.staked);
  if has_staked.lt(&amount) {
    return Err(Error::BRC20SError(BRC20SError::InsufficientBalance(
      has_staked.to_string(),
      amount.truncate_to_str().unwrap(),
    )));
  }

  let dec = get_stake_dec(&stake_tick, brc20s_store, brc20_store);
  reward::update_pool(
    &mut pool,
    context.blockheight,
    dec,
    config.pool_acc_reward_per_share_scale,
  )?;
  let reward = reward::withdraw_user_reward(&mut userinfo, &pool, dec)?;
  userinfo.staked = has_staked.checked_sub(&amount)?.checked_to_u128()?;
  pool.staked = Num::from(pool.staked)
    .checked_sub(&amount)?
    .checked_to_u128()?;
  reward::update_user_stake(&mut userinfo, &pool, dec)?;

  let mut user_stakeinfo = brc20s_store
    .get_user_stakeinfo(&to_script_key, &stake_tick)
    .map_err(|e| Error::LedgerError(e))?
    .ok_or(Error::BRC20SError(BRC20SError::InsufficientBalance(
      amount.truncate_to_str().unwrap(),
      0_u128.to_string(),
    )))?;

  //update pool_stakes
  for pool_stake in user_stakeinfo.pool_stakes.iter_mut() {
    if pool_stake.0 == pool_id {
      pool_stake.2 = userinfo.staked;
      break;
    }
  }
  //remove staked is zero
  user_stakeinfo
    .pool_stakes
    .retain(|pool_stake| pool_stake.2 != 0);

  if pool.only {
    user_stakeinfo.total_only = Num::from(user_stakeinfo.total_only)
      .checked_sub(&amount)?
      .checked_to_u128()?;
  } else {
    user_stakeinfo.max_share = user_stakeinfo.calculate_max_share()?.checked_to_u128()?;
  }

  brc20s_store
    .set_pid_to_use_info(&to_script_key, &pool_id, &userinfo)
    .map_err(|e| Error::LedgerError(e))?;

  brc20s_store
    .set_pid_to_poolinfo(&pool_id, &pool)
    .map_err(|e| Error::LedgerError(e))?;

  brc20s_store
    .set_user_stakeinfo(&to_script_key, &stake_tick, &user_stakeinfo)
    .map_err(|e| Error::LedgerError(e))?;

  Ok(Event::Withdraw(WithdrawEvent {
    pid: pool_id,
    amt: amount.checked_to_u128()?,
    period_settlement_reward: reward,
  }))
}

fn process_passive_unstake<'a, M: brc20::DataStoreReadWrite, N: brc20s::DataStoreReadWrite>(
  context: BlockContext,
  config: version::Config,
  brc20_store: &'a M,
  brc20s_store: &'a N,
  msg: &ExecutionMessage,
  passive_unstake: PassiveUnStake,
) -> Result<Vec<Event>, Error<N>> {
  if let Some(iserr) = passive_unstake.validate_basics().err() {
    return Err(Error::BRC20SError(iserr));
  }
  let from_script_key = msg.from.clone();

  // passive msg set from/commit_from/to = msg.from for passing unstake
  let mut passive_msg = msg.clone();
  passive_msg.commit_from = Some(msg.from.clone());
  passive_msg.to = Some(msg.from.clone());

  let stake_tick = passive_unstake.get_stake_tick();
  let stake_info = brc20s_store
    .get_user_stakeinfo(&from_script_key, &stake_tick)
    .map_err(|e| Error::LedgerError(e))?;
  let stake_info = match stake_info {
    Some(info) => info,
    None => {
      return Err(Error::BRC20SError(BRC20SError::StakeNotFound(
        passive_unstake.stake,
      )));
    }
  };

  let balance = get_user_common_balance(&from_script_key, &stake_tick, brc20s_store, brc20_store);
  let staked_total =
    Num::from(stake_info.total_only).checked_add(&Num::from(stake_info.max_share))?;

  // the balance which is minused by passive_amt, so if it >= staked_total, it can staked. others we need passive_withdraw
  if balance.ge(&staked_total) {
    // user remain can make user to stake. so nothing to do
    return Ok(vec![]);
  };

  let mut events = Vec::new();

  let stake_alterive = staked_total.checked_sub(&balance)?;

  let pids: Vec<(Pid, u128)> = stake_info.calculate_withdraw_pools(&stake_alterive)?;
  for (pid, stake) in pids.iter() {
    let withdraw_stake =
      convert_pledged_tick_without_decimal(&stake_tick, *stake, brc20s_store, brc20_store)?;
    let stake_msg = UnStake::new(pid.as_str(), withdraw_stake.to_string().as_str());
    passive_msg.op = Operation::UnStake(stake_msg.clone());
    process_unstake(
      context,
      config.clone(),
      brc20_store,
      brc20s_store,
      &passive_msg,
      stake_msg,
    )?;
    events.push(Event::PassiveWithdraw(PassiveWithdrawEvent {
      pid: pid.clone(),
      amt: *stake,
    }));
  }

  Ok(events)
}
fn process_mint<'a, M: brc20::DataStoreReadWrite, N: brc20s::DataStoreReadWrite>(
  context: BlockContext,
  config: version::Config,
  brc20_store: &'a M,
  brc20s_store: &'a N,
  msg: &ExecutionMessage,
  mint: Mint,
) -> Result<Event, Error<N>> {
  // ignore inscribe inscription to coinbase.
  let to_script_key = msg.to.clone().ok_or(BRC20SError::InscribeToCoinbase)?;
  if let Some(iserr) = mint.validate_basic().err() {
    return Err(Error::BRC20SError(iserr));
  }

  let from_script_key = match msg.commit_from.clone() {
    Some(script) => script,
    None => {
      return Err(Error::BRC20SError(BRC20SError::InternalError(
        "".to_string(),
      )));
    }
  };
  if !to_script_key.eq(&from_script_key) {
    return Err(Error::BRC20SError(BRC20SError::FromToNotEqual(
      from_script_key.to_string(),
      to_script_key.to_string(),
    )));
  }

  // check tick
  let tick_id = mint.get_tick_id()?;
  let mut tick_info = brc20s_store
    .get_tick_info(&tick_id)
    .map_err(|e| Error::LedgerError(e))?
    .ok_or(BRC20SError::TickNotFound(mint.tick.clone()))?;

  let tick_name = Tick::from_str(mint.tick.as_str())?;
  if tick_info.name != tick_name {
    return Err(Error::BRC20SError(BRC20SError::TickNameNotMatch(mint.tick)));
  }

  // check amount
  let mut amt = Num::from_str(&mint.amount)?;
  if amt.scale() > i64::from(tick_info.decimal) {
    return Err(Error::BRC20SError(BRC20SError::AmountOverflow(mint.amount)));
  }
  let base = BIGDECIMAL_TEN.checked_powu(u64::from(tick_info.decimal))?;
  amt = amt.checked_mul(&base)?;
  if amt.sign() == Sign::NoSign {
    return Err(Error::BRC20SError(BRC20SError::InvalidZeroAmount));
  }

  // get user info and pool info
  let pool_id = mint.get_pool_id()?;
  let mut user_info = brc20s_store
    .get_pid_to_use_info(&to_script_key, &pool_id)
    .unwrap_or(Some(UserInfo::default(&pool_id)))
    .unwrap_or(UserInfo::default(&pool_id));
  let mut pool_info = brc20s_store
    .get_pid_to_poolinfo(&pool_id)
    .map_err(|e| Error::LedgerError(e))?
    .ok_or(Error::BRC20SError(BRC20SError::PoolNotExist(mint.pool_id)))?;

  // calculate reward
  let dec = get_stake_dec(&pool_info.stake, brc20s_store, brc20_store);
  if user_info.pending_reward >= amt.checked_to_u128()? {
    user_info.pending_reward -= amt.checked_to_u128()?;
    user_info.minted += amt.checked_to_u128()?;
  } else {
    reward::update_pool(
      &mut pool_info,
      context.blockheight,
      dec,
      config.pool_acc_reward_per_share_scale,
    )?;
    reward::withdraw_user_reward(&mut user_info, &pool_info, dec)?;
    reward::update_user_stake(&mut user_info, &pool_info, dec)?;
    if amt > user_info.pending_reward.into() {
      return Err(Error::BRC20SError(BRC20SError::AmountExceedLimit(
        amt.truncate_to_str().unwrap(),
      )));
    }
    user_info.pending_reward -= amt.checked_to_u128()?;
    user_info.minted += amt.checked_to_u128()?;
  }

  // update tick info
  tick_info.circulation += amt.checked_to_u128()?;
  tick_info.latest_mint_block = context.blockheight;

  // update user balance
  let mut user_balance = brc20s_store
    .get_balance(&to_script_key, &tick_id)
    .map_err(|e| Error::LedgerError(e))?
    .map_or(Balance::new(tick_id), |v| v);

  user_balance.overall_balance = Into::<Num>::into(user_balance.overall_balance)
    .checked_add(&amt)?
    .checked_to_u128()?;

  // update user info and pool info
  brc20s_store
    .set_pid_to_use_info(&to_script_key, &pool_id, &user_info)
    .map_err(|e| Error::LedgerError(e))?;
  brc20s_store
    .set_pid_to_poolinfo(&pool_id, &pool_info)
    .map_err(|e| Error::LedgerError(e))?;

  // update tick info
  brc20s_store
    .set_tick_info(&tick_id, &tick_info)
    .map_err(|e| Error::LedgerError(e))?;

  // update user balance
  brc20s_store
    .set_token_balance(&to_script_key, &tick_id, user_balance)
    .map_err(|e| Error::LedgerError(e))?;

  Ok(Event::Mint(MintEvent {
    pid: pool_id,
    amt: amt.checked_to_u128()?,
  }))
}

fn process_inscribe_transfer<'a, M: brc20::DataStoreReadWrite, N: brc20s::DataStoreReadWrite>(
  _context: BlockContext,
  _config: version::Config,
  _brc20_store: &'a M,
  brc20s_store: &'a N,
  msg: &ExecutionMessage,
  transfer: Transfer,
) -> Result<Event, Error<N>> {
  // ignore inscribe inscription to coinbase.
  let to_script_key = msg.to.clone().ok_or(BRC20SError::InscribeToCoinbase)?;
  // check tick
  let tick_id = TickId::from_str(transfer.tick_id.as_str())?;
  let tick_info = brc20s_store
    .get_tick_info(&tick_id)
    .map_err(|e| Error::LedgerError(e))?
    .ok_or(BRC20SError::TickNotFound(tick_id.hex()))?;

  let tick_name = Tick::from_str(transfer.tick.as_str())?;
  if tick_info.name != tick_name {
    return Err(Error::BRC20SError(BRC20SError::TickNameNotMatch(
      transfer.tick,
    )));
  }

  // check amount
  let mut amt = Num::from_str(&transfer.amount)?;
  if amt.scale() > i64::from(tick_info.decimal) {
    return Err(Error::BRC20SError(BRC20SError::AmountOverflow(
      transfer.amount,
    )));
  }
  let base = BIGDECIMAL_TEN.checked_powu(u64::from(tick_info.decimal))?;
  amt = amt.checked_mul(&base)?;
  if amt.sign() == Sign::NoSign {
    return Err(Error::BRC20SError(BRC20SError::InvalidZeroAmount));
  }

  // update balance
  let mut balance = brc20s_store
    .get_balance(&to_script_key, &tick_id)
    .map_err(|e| Error::LedgerError(e))?
    .map_or(Balance::new(tick_id), |v| v);

  let overall = Into::<Num>::into(balance.overall_balance);
  let transferable = Into::<Num>::into(balance.transferable_balance);
  let available = overall.checked_sub(&transferable)?;
  if available < amt {
    return Err(Error::BRC20SError(BRC20SError::InsufficientBalance(
      available.to_string(),
      amt.truncate_to_str().unwrap(),
    )));
  }
  balance.transferable_balance = transferable.checked_add(&amt)?.checked_to_u128()?;

  // insert transferable assets
  let amount = amt.checked_to_u128()?;
  let transferable_assets = TransferableAsset {
    inscription_id: msg.inscription_id,
    amount,
    tick_id,
    owner: to_script_key.clone(),
  };

  //update balance
  brc20s_store
    .set_token_balance(&to_script_key, &tick_id, balance)
    .map_err(|e| Error::LedgerError(e))?;

  brc20s_store
    .set_transferable_assets(
      &to_script_key,
      &tick_id,
      &msg.inscription_id,
      &transferable_assets,
    )
    .map_err(|e| Error::LedgerError(e))?;

  brc20s_store
    .insert_inscribe_transfer_inscription(
      msg.inscription_id,
      TransferInfo {
        tick_id,
        tick_name,
        amt: amount,
      },
    )
    .map_err(|e| Error::LedgerError(e))?;

  Ok(Event::InscribeTransfer(InscribeTransferEvent {
    tick_id,
    amt: amount,
  }))
}

fn process_transfer<'a, M: brc20::DataStoreReadWrite, N: brc20s::DataStoreReadWrite>(
  _context: BlockContext,
  _config: version::Config,
  _brc20_store: &'a M,
  brc20s_store: &'a N,
  msg: &ExecutionMessage,
) -> Result<Event, Error<N>> {
  let from_script_key = msg.from.clone();
  let transferable = brc20s_store
    .get_transferable_by_id(&from_script_key, &msg.inscription_id)
    .map_err(|e| Error::LedgerError(e))?
    .ok_or(BRC20SError::TransferableNotFound(msg.inscription_id))?;

  let amt = Into::<Num>::into(transferable.amount);

  if transferable.owner != from_script_key {
    return Err(Error::BRC20SError(BRC20SError::TransferableOwnerNotMatch(
      msg.inscription_id,
    )));
  }

  // redirect receiver to sender if transfer to conibase.
  let mut out_msg = None;
  let to_script_key = if msg.to.clone().is_none() {
    out_msg =
      Some("redirect receiver to sender, reason: transfer inscription to coinbase".to_string());
    msg.from.clone()
  } else {
    msg.to.clone().unwrap()
  };

  brc20s_store
    .get_tick_info(&transferable.tick_id)
    .map_err(|e| Error::LedgerError(e))?
    .ok_or(BRC20SError::TickNotFound(transferable.tick_id.hex()))?;

  // update from key balance.
  let mut from_balance = brc20s_store
    .get_balance(&from_script_key, &transferable.tick_id)
    .map_err(|e| Error::LedgerError(e))?
    .map_or(Balance::new(transferable.tick_id), |v| v);

  let from_overall = Into::<Num>::into(from_balance.overall_balance);
  let from_transferable = Into::<Num>::into(from_balance.transferable_balance);

  let from_overall = from_overall.checked_sub(&amt)?.checked_to_u128()?;
  let from_transferable = from_transferable.checked_sub(&amt)?.checked_to_u128()?;

  from_balance.overall_balance = from_overall;
  from_balance.transferable_balance = from_transferable;

  brc20s_store
    .set_token_balance(&from_script_key, &transferable.tick_id, from_balance)
    .map_err(|e| Error::LedgerError(e))?;
  // redirect receiver to sender if transfer to conibase.
  // let to_script_key = if let None = to_script_key.clone() {
  //   from_script_key.clone()
  // } else {
  //   to_script_key.unwrap()
  // };

  // update to key balance.
  let mut to_balance = brc20s_store
    .get_balance(&to_script_key, &transferable.tick_id)
    .map_err(|e| Error::LedgerError(e))?
    .map_or(Balance::new(transferable.tick_id), |v| v);

  let to_overall = Into::<Num>::into(to_balance.overall_balance);
  to_balance.overall_balance = to_overall.checked_add(&amt)?.checked_to_u128()?;

  brc20s_store
    .set_token_balance(&to_script_key, &transferable.tick_id, to_balance)
    .map_err(|e| Error::LedgerError(e))?;

  brc20s_store
    .remove_transferable(&from_script_key, &transferable.tick_id, &msg.inscription_id)
    .map_err(|e| Error::LedgerError(e))?;

  brc20s_store
    .remove_inscribe_transfer_inscription(msg.inscription_id)
    .map_err(|e| Error::LedgerError(e))?;

  Ok(Event::Transfer(TransferEvent {
    tick_id: transferable.tick_id,
    amt: amt.checked_to_u128()?,
    msg: out_msg,
  }))
}

#[allow(unused)]
#[cfg(test)]
mod tests {
  use std::ops::Sub;

  use super::super::*;
  use super::*;
  use crate::index::INSCRIPTION_ID_TO_INSCRIPTION_ENTRY;
  use crate::okx::datastore::brc20::redb as brc20_db;
  use crate::okx::datastore::brc20::DataStoreReadWrite;
  use crate::okx::datastore::brc20::{Balance as BRC20Balance, TokenInfo};
  use crate::okx::datastore::brc20s::redb as brc20s_db;
  use crate::okx::datastore::brc20s::DataStoreReadOnly;
  use crate::okx::datastore::brc20s::DataStoreReadWrite as BRC20SDataStoreReadWrite;
  use crate::okx::datastore::brc20s::Event::PassiveWithdraw;
  use crate::okx::datastore::brc20s::PledgedTick;
  use crate::okx::protocol::brc20s::params::NATIVE_TOKEN;
  use crate::okx::protocol::brc20s::test::{
    mock_create_brc20s_message, mock_deploy_msg, mock_passive_unstake_msg, mock_stake_msg,
    mock_unstake_msg,
  };
  use crate::test::Hash;
  use bech32::CheckBase32;
  use bitcoin::hashes::sha256;
  use bitcoin::hashes::HashEngine;
  use bitcoin::Address;
  use chrono::Local;
  use redb::Database;
  use tempfile::NamedTempFile;

  fn execute_for_test<'a, M: brc20::DataStoreReadWrite, N: brc20s::DataStoreReadWrite>(
    brc20_store: &'a M,
    brc20s_store: &'a N,
    msg: &ExecutionMessage,
    height: u64,
    config: version::Config,
  ) -> Result<Vec<Event>, BRC20SError> {
    let context = BlockContext {
      blockheight: height,
      blocktime: 1687245485,
      network: Network::Bitcoin,
    };
    let result = match msg.clone().op {
      Operation::Deploy(deploy) => {
        process_deploy(context, config, brc20_store, brc20s_store, msg, deploy)
      }
      Operation::Mint(mint) => {
        match process_mint(context, config, brc20_store, brc20s_store, msg, mint) {
          Ok(event) => Ok(vec![event]),
          Err(e) => Err(e),
        }
      }
      Operation::Stake(stake) => {
        match process_stake(context, config, brc20_store, brc20s_store, msg, stake) {
          Ok(event) => Ok(vec![event]),
          Err(e) => Err(e),
        }
      }
      Operation::UnStake(unstake) => {
        match process_unstake(context, config, brc20_store, brc20s_store, msg, unstake) {
          Ok(event) => Ok(vec![event]),
          Err(e) => Err(e),
        }
      }
      Operation::PassiveUnStake(passive_unstake) => process_passive_unstake(
        context,
        config,
        brc20_store,
        brc20s_store,
        msg,
        passive_unstake,
      ),
      Operation::InscribeTransfer(inscribe_transfer) => {
        match process_inscribe_transfer(
          context,
          config,
          brc20_store,
          brc20s_store,
          msg,
          inscribe_transfer,
        ) {
          Ok(event) => Ok(vec![event]),
          Err(e) => Err(e),
        }
      }
      Operation::Transfer(_) => {
        match process_transfer(context, config, brc20_store, brc20s_store, msg) {
          Ok(event) => Ok(vec![event]),
          Err(e) => Err(e),
        }
      }
    };

    match result {
      Ok(events) => Ok(events),
      Err(Error::BRC20SError(e)) => Err(e),
      Err(e) => Err(BRC20SError::InternalError(e.to_string())),
    }
  }

  fn set_brc20_token_user<M: brc20::DataStoreReadWrite>(
    brc20_store: &M,
    tick: &str,
    addr: &ScriptKey,
    balance: u128,
    dec: u8,
  ) -> Result<(), BRC20SError> {
    let token = brc20::Tick::from_str(tick).unwrap();
    let inscription_id =
      InscriptionId::from_str("1111111111111111111111111111111111111111111111111111111111111111i1")
        .unwrap();
    let token_info = TokenInfo {
      tick: token.clone(),
      inscription_id,
      inscription_number: 0,
      supply: 0_u128,
      minted: 0_u128,
      limit_per_mint: 0,
      decimal: dec,
      deploy_by: addr.clone(),
      deployed_number: 0,
      deployed_timestamp: 0,
      latest_mint_number: 0,
    };
    brc20_store.insert_token_info(&token, &token_info);

    let base = BIGDECIMAL_TEN.checked_powu(u64::from(dec))?;
    let overall_balance = Num::from(balance).checked_mul(&base)?.checked_to_u128()?;
    let balance = BRC20Balance {
      tick: token,
      overall_balance,
      transferable_balance: 0_u128,
    };
    brc20_store.update_token_balance(addr, balance);
    Ok(())
  }

  fn assert_stake_info<M: brc20s::DataStoreReadWrite>(
    brc20s_data_store: &M,
    pid: &str,
    from_script: &ScriptKey,
    stake_tick: &PledgedTick,
    expect_pool_info: &str,
    expect_stake_info: &str,
    expect_user_info: &str,
  ) {
    let temp_pid = Pid::from_str(pid).unwrap();
    let mut stake_info = brc20s_data_store
      .get_user_stakeinfo(from_script, stake_tick)
      .unwrap()
      .unwrap();
    let pool_stakes: Vec<(Pid, bool, u128)> =
      stake_info.pool_stakes.iter().rev().cloned().collect();
    stake_info.pool_stakes = pool_stakes;
    let pool_info = brc20s_data_store
      .get_pid_to_poolinfo(&temp_pid)
      .unwrap()
      .unwrap();
    let user_info = brc20s_data_store
      .get_pid_to_use_info(from_script, &temp_pid)
      .unwrap()
      .unwrap();
    println!(
      "stake_info: {}\n",
      serde_json::to_string(&stake_info).unwrap()
    );
    println!(
      "user_info: {}\n",
      serde_json::to_string(&user_info).unwrap()
    );
    println!(
      "pool_info: {}\n",
      serde_json::to_string(&pool_info).unwrap()
    );
    assert_eq!(serde_json::to_string(&pool_info).unwrap(), expect_pool_info);
    assert_eq!(
      serde_json::to_string(&stake_info).unwrap(),
      expect_stake_info
    );
    assert_eq!(serde_json::to_string(&user_info).unwrap(), expect_user_info);
  }

  #[test]
  fn test_process_deploy() {
    let dbfile = NamedTempFile::new().unwrap();
    let db = Database::create(dbfile.path()).unwrap();
    let wtx = db.begin_write().unwrap();

    let brc20_data_store = brc20_db::DataStore::new(&wtx);
    let brc20s_data_store = brc20s_db::DataStore::new(&wtx);

    let deploy = Deploy {
      pool_type: "pool".to_string(),
      pool_id: "13395c5283#1f".to_string(),
      stake: "btc1".to_string(),
      earn: "ordi1".to_string(),
      earn_rate: "10".to_string(),
      distribution_max: "12000000".to_string(),
      decimals: Some("18".to_string()),
      total_supply: Some("21000000".to_string()),
      only: Some("1".to_string()),
    };

    let addr1 =
      Address::from_str("bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e").unwrap();
    let script = ScriptKey::from_address(addr1.assume_checked());
    let inscription_id =
      InscriptionId::from_str("1111111111111111111111111111111111111111111111111111111111111111i1")
        .unwrap();
    let msg = mock_create_brc20s_message(
      script.clone(),
      script.clone(),
      Operation::Deploy(deploy.clone()),
    );
    let result = set_brc20_token_user(&brc20_data_store, "btc1", &msg.from, 200_u128, 18_u8).err();
    assert_eq!(None, result);
    let context = BlockContext {
      blockheight: 0,
      blocktime: 1687245485,
      network: Network::Bitcoin,
    };
    let config = version::get_config_by_network(context.network, context.blockheight);
    let result = process_deploy(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      deploy.clone(),
    );

    let result: Result<Vec<Event>, BRC20SError> = match result {
      Ok(event) => Ok(event),
      Err(Error::BRC20SError(e)) => Err(e),
      Err(e) => Err(BRC20SError::InternalError(e.to_string())),
    };

    match result {
      Ok(event) => {
        println!("success:{}", serde_json::to_string_pretty(&event).unwrap());
      }
      Err(e) => {
        assert_eq!("error", e.to_string())
      }
    }
    let tick_id = deploy.get_tick_id();
    let pid = deploy.get_pool_id();
    let tick_info = brc20s_data_store.get_tick_info(&tick_id).unwrap().unwrap();
    let pool_info = brc20s_data_store
      .get_pid_to_poolinfo(&pid)
      .unwrap()
      .unwrap();

    let expect_tick_info = r#"{"tick_id":"13395c5283","name":"ordi1","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","allocated":12000000000000000000000000,"decimal":18,"circulation":0,"supply":21000000000000000000000000,"deployer":{"Address":"bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e"},"deploy_block":0,"deploy_block_time":1687245485,"latest_mint_block":0,"pids":["13395c5283#1f"]}"#;
    let expect_pool_info = r#"{"pid":"13395c5283#1f","ptype":"Pool","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","stake":{"BRC20Tick":"btc1"},"erate":10000000000000000000,"minted":0,"staked":0,"dmax":12000000000000000000000000,"acc_reward_per_share":"0","last_update_block":0,"only":true,"deploy_block":0,"deploy_block_time":1687245485}"#;
    assert_eq!(expect_pool_info, serde_json::to_string(&pool_info).unwrap());
    assert_eq!(expect_tick_info, serde_json::to_string(&tick_info).unwrap());

    let msg = mock_create_brc20s_message(
      script.clone(),
      script.clone(),
      Operation::Deploy(deploy.clone()),
    );
    let context = BlockContext {
      blockheight: 10,
      blocktime: 1687245485,
      network: Network::Bitcoin,
    };
    let config = version::get_config_by_network(context.network, context.blockheight);
    let result = process_deploy(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      deploy.clone(),
    );

    let result: Result<Vec<Event>, BRC20SError> = match result {
      Ok(event) => Ok(event),
      Err(Error::BRC20SError(e)) => Err(e),
      Err(e) => Err(BRC20SError::InternalError(e.to_string())),
    };

    assert_eq!(
      Err(BRC20SError::PoolAlreadyExist(pid.as_str().to_string())),
      result
    );

    let token = brc20::Tick::from_str("orea".to_string().as_str()).unwrap();
    let token_info = TokenInfo {
      tick: token.clone(),
      inscription_id,
      inscription_number: 0,
      supply: 0,
      minted: 0,
      limit_per_mint: 0,
      decimal: 0,
      deploy_by: script.clone(),
      deployed_number: 0,
      deployed_timestamp: 0,
      latest_mint_number: 0,
    };
    brc20_data_store.insert_token_info(&token, &token_info);

    let mut second_deply = deploy;
    second_deply.pool_id = "13395c5283#11".to_string();
    second_deply.stake = "orea".to_string();
    second_deply.distribution_max = "9000000".to_string();
    second_deply.earn_rate = "0.1".to_string();
    let msg = mock_create_brc20s_message(
      script.clone(),
      script,
      Operation::Deploy(second_deply.clone()),
    );
    let context = BlockContext {
      blockheight: 20,
      blocktime: 1687245485,
      network: Network::Bitcoin,
    };
    let config = version::get_config_by_network(context.network, context.blockheight);
    let result = process_deploy(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      second_deply.clone(),
    );

    let result: Result<Vec<Event>, BRC20SError> = match result {
      Ok(event) => Ok(event),
      Err(Error::BRC20SError(e)) => Err(e),
      Err(e) => Err(BRC20SError::InternalError(e.to_string())),
    };

    assert!(result.is_ok());
    let tick_id = second_deply.get_tick_id();
    let pid = second_deply.get_pool_id();
    let tick_info = brc20s_data_store.get_tick_info(&tick_id).unwrap().unwrap();
    let pool_info = brc20s_data_store
      .get_pid_to_poolinfo(&pid)
      .unwrap()
      .unwrap();

    let expect_tick_info = r#"{"tick_id":"13395c5283","name":"ordi1","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","allocated":21000000000000000000000000,"decimal":18,"circulation":0,"supply":21000000000000000000000000,"deployer":{"Address":"bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e"},"deploy_block":0,"deploy_block_time":1687245485,"latest_mint_block":0,"pids":["13395c5283#1f","13395c5283#11"]}"#;
    let expect_pool_info = r#"{"pid":"13395c5283#11","ptype":"Pool","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","stake":{"BRC20Tick":"orea"},"erate":100000000000000000,"minted":0,"staked":0,"dmax":9000000000000000000000000,"acc_reward_per_share":"0","last_update_block":20,"only":true,"deploy_block":20,"deploy_block_time":1687245485}"#;
    assert_eq!(expect_pool_info, serde_json::to_string(&pool_info).unwrap());
    assert_eq!(expect_tick_info, serde_json::to_string(&tick_info).unwrap());
  }

  #[test]
  fn test_process_deploy_common() {
    let dbfile = NamedTempFile::new().unwrap();
    let db = Database::create(dbfile.path()).unwrap();
    let wtx = db.begin_write().unwrap();

    let brc20_data_store = brc20_db::DataStore::new(&wtx);
    let brc20s_data_store = brc20s_db::DataStore::new(&wtx);

    let deploy = Deploy {
      pool_type: "pool".to_string(),
      pool_id: "13395c5283#1f".to_string(),
      stake: "BTC1".to_string(),
      earn: "ordi1".to_string(),
      earn_rate: "10".to_string(),
      distribution_max: "12000000".to_string(),
      decimals: Some("18".to_string()),
      total_supply: Some("21000000".to_string()),
      only: Some("1".to_string()),
    };
    let addr1 =
      Address::from_str("bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e").unwrap();
    let script = ScriptKey::from_address(addr1.assume_checked());
    let inscription_id =
      InscriptionId::from_str("1111111111111111111111111111111111111111111111111111111111111111i1")
        .unwrap();

    let msg = mock_create_brc20s_message(
      script.clone(),
      script.clone(),
      Operation::Deploy(deploy.clone()),
    );
    let result = set_brc20_token_user(&brc20_data_store, "btc1", &msg.from, 200_u128, 18_u8).err();
    let result = set_brc20_token_user(&brc20_data_store, "abc1", &msg.from, 200_u128, 18_u8).err();
    assert_eq!(None, result);
    let context = BlockContext {
      blockheight: 0,
      blocktime: 1687245485,
      network: Network::Bitcoin,
    };
    let config = version::get_config_by_network(context.network, context.blockheight);
    let result = process_deploy(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      deploy.clone(),
    );

    let result: Result<Vec<Event>, BRC20SError> = match result {
      Ok(event) => Ok(event),
      Err(Error::BRC20SError(e)) => Err(e),
      Err(e) => Err(BRC20SError::InternalError(e.to_string())),
    };

    match result {
      Ok(event) => {
        println!("success:{}", serde_json::to_string_pretty(&event).unwrap());
      }
      Err(e) => {
        println!("error:{}", e)
      }
    }
    let tick_id = deploy.get_tick_id();
    let pid = deploy.get_pool_id();
    let tick_info = brc20s_data_store.get_tick_info(&tick_id).unwrap().unwrap();
    let pool_info = brc20s_data_store
      .get_pid_to_poolinfo(&pid)
      .unwrap()
      .unwrap();

    let expect_tick_info = r#"{"tick_id":"13395c5283","name":"ordi1","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","allocated":12000000000000000000000000,"decimal":18,"circulation":0,"supply":21000000000000000000000000,"deployer":{"Address":"bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e"},"deploy_block":0,"deploy_block_time":1687245485,"latest_mint_block":0,"pids":["13395c5283#1f"]}"#;
    let expect_pool_info = r#"{"pid":"13395c5283#1f","ptype":"Pool","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","stake":{"BRC20Tick":"btc1"},"erate":10000000000000000000,"minted":0,"staked":0,"dmax":12000000000000000000000000,"acc_reward_per_share":"0","last_update_block":0,"only":true,"deploy_block":0,"deploy_block_time":1687245485}"#;
    assert_eq!(expect_pool_info, serde_json::to_string(&pool_info).unwrap());
    assert_eq!(expect_tick_info, serde_json::to_string(&tick_info).unwrap());
    //add brc20 tokeninfo
    {
      let token = brc20::Tick::from_str("ore1".to_string().as_str()).unwrap();
      let token_info = TokenInfo {
        tick: token.clone(),
        inscription_id,
        inscription_number: 0,
        supply: 0,
        minted: 0,
        limit_per_mint: 0,
        decimal: 0,
        deploy_by: script.clone(),
        deployed_number: 0,
        deployed_timestamp: 0,
        latest_mint_number: 0,
      };
      brc20_data_store.insert_token_info(&token, &token_info);

      let token = brc20::Tick::from_str("ore2".to_string().as_str()).unwrap();
      let token_info = TokenInfo {
        tick: token.clone(),
        inscription_id,
        inscription_number: 0,
        supply: 0,
        minted: 0,
        limit_per_mint: 0,
        decimal: 0,
        deploy_by: script.clone(),
        deployed_number: 0,
        deployed_timestamp: 0,
        latest_mint_number: 0,
      };
      brc20_data_store.insert_token_info(&token, &token_info);

      let token = brc20::Tick::from_str("ore3".to_string().as_str()).unwrap();
      let token_info = TokenInfo {
        tick: token.clone(),
        inscription_id,
        inscription_number: 0,
        supply: 0,
        minted: 0,
        limit_per_mint: 0,
        decimal: 0,
        deploy_by: script.clone(),
        deployed_number: 0,
        deployed_timestamp: 0,
        latest_mint_number: 0,
      };
      brc20_data_store.insert_token_info(&token, &token_info);
    }
    //pool already exist
    {
      let msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Deploy(deploy.clone()),
      );
      let context = BlockContext {
        blockheight: 0,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_deploy(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        deploy.clone(),
      );

      let result: Result<Vec<Event>, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };

      assert_eq!(
        Err(BRC20SError::PoolAlreadyExist(pid.as_str().to_string())),
        result
      );
    }
    //deploy second pool
    {
      let mut second_deploy = deploy.clone();
      second_deploy.pool_id = "13395c5283#01".to_string();
      second_deploy.stake = "ore1".to_string();
      second_deploy.distribution_max = "8000000".to_string();
      let msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Deploy(second_deploy.clone()),
      );
      let context = BlockContext {
        blockheight: 0,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_deploy(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        second_deploy.clone(),
      );

      let result: Result<Vec<Event>, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };

      let tick_id = second_deploy.get_tick_id();
      let pid = second_deploy.get_pool_id();
      let tick_info = brc20s_data_store.get_tick_info(&tick_id).unwrap().unwrap();
      let pool_info = brc20s_data_store
        .get_pid_to_poolinfo(&pid)
        .unwrap()
        .unwrap();

      let expect_tick_info = r#"{"tick_id":"13395c5283","name":"ordi1","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","allocated":20000000000000000000000000,"decimal":18,"circulation":0,"supply":21000000000000000000000000,"deployer":{"Address":"bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e"},"deploy_block":0,"deploy_block_time":1687245485,"latest_mint_block":0,"pids":["13395c5283#1f","13395c5283#01"]}"#;
      let expect_pool_info = r#"{"pid":"13395c5283#01","ptype":"Pool","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","stake":{"BRC20Tick":"ore1"},"erate":10000000000000000000,"minted":0,"staked":0,"dmax":8000000000000000000000000,"acc_reward_per_share":"0","last_update_block":0,"only":true,"deploy_block":0,"deploy_block_time":1687245485}"#;
      assert_eq!(expect_pool_info, serde_json::to_string(&pool_info).unwrap());
      assert_eq!(expect_tick_info, serde_json::to_string(&tick_info).unwrap());
    }

    // deploy share pool
    {
      let mut second_deploy = deploy.clone();
      second_deploy.pool_id = "13395c5283#02".to_string();
      second_deploy.stake = "ore2".to_string();
      second_deploy.distribution_max = "100000".to_string();
      second_deploy.only = Some("".to_string());
      let msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Deploy(second_deploy.clone()),
      );
      let context = BlockContext {
        blockheight: 0,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_deploy(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        second_deploy.clone(),
      );
      assert!(result.is_ok());
      let result: Result<Vec<Event>, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };

      let tick_id = second_deploy.get_tick_id();
      let pid = second_deploy.get_pool_id();
      let tick_info = brc20s_data_store.get_tick_info(&tick_id).unwrap().unwrap();
      let pool_info = brc20s_data_store
        .get_pid_to_poolinfo(&pid)
        .unwrap()
        .unwrap();

      let expect_tick_info = r#"{"tick_id":"13395c5283","name":"ordi1","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","allocated":20100000000000000000000000,"decimal":18,"circulation":0,"supply":21000000000000000000000000,"deployer":{"Address":"bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e"},"deploy_block":0,"deploy_block_time":1687245485,"latest_mint_block":0,"pids":["13395c5283#1f","13395c5283#01","13395c5283#02"]}"#;
      let expect_pool_info = r#"{"pid":"13395c5283#02","ptype":"Pool","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","stake":{"BRC20Tick":"ore2"},"erate":10000000000000000000,"minted":0,"staked":0,"dmax":100000000000000000000000,"acc_reward_per_share":"0","last_update_block":0,"only":false,"deploy_block":0,"deploy_block_time":1687245485}"#;
      assert_eq!(expect_pool_info, serde_json::to_string(&pool_info).unwrap());
      assert_eq!(expect_tick_info, serde_json::to_string(&tick_info).unwrap());
    }

    // deploy pool stake
    {
      // stake is exist
      let mut second_deploy = deploy.clone();
      second_deploy.pool_id = "13395c5283#03".to_string();
      second_deploy.stake = "ore1".to_string();
      second_deploy.distribution_max = "100000".to_string();
      second_deploy.only = Some("".to_string());
      let msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Deploy(second_deploy.clone()),
      );
      let context = BlockContext {
        blockheight: 0,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_deploy(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        second_deploy.clone(),
      );

      let result: Result<Vec<Event>, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };

      assert_eq!(
        Err(BRC20SError::StakeAlreadyExist(
          second_deploy.stake.clone(),
          second_deploy.get_tick_id().hex()
        )),
        result
      );

      //stake not found
      let mut second_deploy = deploy.clone();
      second_deploy.pool_id = "13395c5283#03".to_string();
      second_deploy.stake = "err1".to_string();
      second_deploy.distribution_max = "100000".to_string();
      second_deploy.only = Some("".to_string());
      let msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Deploy(second_deploy.clone()),
      );
      let context = BlockContext {
        blockheight: 0,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_deploy(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        second_deploy.clone(),
      );

      let result: Result<Vec<Event>, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };

      assert_eq!(
        Err(BRC20SError::StakeNotFound(second_deploy.stake,)),
        result
      );
    }

    //deploy pool dmax > totalsupply
    {
      let mut second_deploy = deploy.clone();
      second_deploy.pool_id = "13395c5283#03".to_string();
      second_deploy.stake = "ore3".to_string();
      second_deploy.distribution_max = "10000000".to_string();
      second_deploy.only = Some("".to_string());
      let msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Deploy(second_deploy.clone()),
      );
      let context = BlockContext {
        blockheight: 0,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_deploy(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        second_deploy,
      );

      let result: Result<Vec<Event>, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };

      assert_eq!(
        Err(BRC20SError::InsufficientTickSupply("10000000".to_string())),
        result
      );
    }

    //deploy pool dmax > totalsupply
    {
      let mut second_deploy = deploy.clone();
      second_deploy.pool_id = "13395c5283#03".to_string();
      second_deploy.stake = "ore3".to_string();
      second_deploy.distribution_max = "100000".to_string();
      second_deploy.total_supply = Some("22000000".to_string());
      second_deploy.only = Some("".to_string());
      let msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Deploy(second_deploy.clone()),
      );
      let context = BlockContext {
        blockheight: 0,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_deploy(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        second_deploy.clone(),
      );

      let result: Result<Vec<Event>, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };

      let tick_id = second_deploy.get_tick_id();
      let pid = second_deploy.get_pool_id();
      let tick_info = brc20s_data_store.get_tick_info(&tick_id).unwrap().unwrap();
      let pool_info = brc20s_data_store
        .get_pid_to_poolinfo(&pid)
        .unwrap()
        .unwrap();

      let expect_tick_info = r#"{"tick_id":"13395c5283","name":"ordi1","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","allocated":20200000000000000000000000,"decimal":18,"circulation":0,"supply":21000000000000000000000000,"deployer":{"Address":"bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e"},"deploy_block":0,"deploy_block_time":1687245485,"latest_mint_block":0,"pids":["13395c5283#1f","13395c5283#01","13395c5283#02","13395c5283#03"]}"#;
      let expect_pool_info = r#"{"pid":"13395c5283#03","ptype":"Pool","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","stake":{"BRC20Tick":"ore3"},"erate":10000000000000000000,"minted":0,"staked":0,"dmax":100000000000000000000000,"acc_reward_per_share":"0","last_update_block":0,"only":false,"deploy_block":0,"deploy_block_time":1687245485}"#;
      assert_eq!(expect_pool_info, serde_json::to_string(&pool_info).unwrap());
      assert_eq!(expect_tick_info, serde_json::to_string(&tick_info).unwrap());
    }

    //invalid inscribe to coinbase
    {
      let mut second_deploy = deploy.clone();
      second_deploy.pool_id = "13395c5283#03".to_string();
      second_deploy.stake = "ore3".to_string();
      second_deploy.distribution_max = "100000".to_string();
      second_deploy.total_supply = Some("22000000".to_string());
      second_deploy.only = Some("".to_string());
      let mut msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Deploy(second_deploy.clone()),
      );
      msg.to = None;
      let context = BlockContext {
        blockheight: 0,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_deploy(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        second_deploy,
      );

      let result: Result<Vec<Event>, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };

      assert_eq!(Err(BRC20SError::InscribeToCoinbase), result);
    }

    //match msg.commit_from is none
    {
      let mut second_deploy = deploy.clone();
      second_deploy.pool_id = "13395c5283#03".to_string();
      second_deploy.stake = "ore3".to_string();
      second_deploy.distribution_max = "100000".to_string();
      second_deploy.total_supply = Some("22000000".to_string());
      second_deploy.only = Some("".to_string());
      let mut msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Deploy(second_deploy.clone()),
      );

      msg.commit_from = None;
      let context = BlockContext {
        blockheight: 0,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_deploy(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        second_deploy,
      );

      let result: Result<Vec<Event>, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };

      assert_eq!(
        Err(BRC20SError::InternalError(
          "commit from script pubkey not exist".to_string(),
        )),
        result
      );
    }

    //share pool can not be deploy
    {
      let mut second_deploy = deploy.clone();
      second_deploy.pool_id = "13395c5283#03".to_string();
      second_deploy.stake = "ore3".to_string();
      second_deploy.distribution_max = "100000".to_string();
      second_deploy.total_supply = Some("22000000".to_string());
      second_deploy.only = Some("0".to_string());
      let mut msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Deploy(second_deploy.clone()),
      );

      let context = BlockContext {
        blockheight: 0,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let mut config = version::get_config_by_network(context.network, context.blockheight);
      config.allow_share_pool = false;
      let result = process_deploy(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        second_deploy,
      );

      let result: Result<Vec<Event>, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };

      assert_eq!(Err(BRC20SError::ShareNoPermission()), result);
    }

    //temp_tick.name != tick_name
    {
      let mut second_deploy = deploy.clone();
      second_deploy.pool_id = "13395c5283#05".to_string();
      second_deploy.stake = "ore3".to_string();
      second_deploy.distribution_max = "100000".to_string();
      second_deploy.total_supply = Some("22000000".to_string());
      second_deploy.earn = "ordie".to_string();
      let mut msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Deploy(second_deploy.clone()),
      );

      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_deploy(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        second_deploy,
      );

      let result: Result<Vec<Event>, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };

      assert_eq!(
        Err(BRC20SError::TickNameNotMatch("ordie".to_string())),
        result
      );
    }

    // decimal > MAX_DECIMAL_WIDTH
    {
      let mut second_deploy = deploy;
      second_deploy.pool_id = "13395c5284#05".to_string();
      second_deploy.stake = "abc1".to_string();
      second_deploy.decimals = Some("19".to_string());
      second_deploy.distribution_max = "100000".to_string();
      second_deploy.total_supply = Some("22000000".to_string());
      second_deploy.earn = "ordi1".to_string();
      let mut msg = mock_create_brc20s_message(
        script.clone(),
        script,
        Operation::Deploy(second_deploy.clone()),
      );

      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_deploy(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        second_deploy,
      );

      let result: Result<Vec<Event>, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };

      assert_eq!(Err(BRC20SError::DecimalsTooLarge(19)), result);
    }
  }

  #[test]
  fn test_process_error_params() {
    let dbfile = NamedTempFile::new().unwrap();
    let db = Database::create(dbfile.path()).unwrap();
    let wtx = db.begin_write().unwrap();
    let mut inscription_id_to_inscription_entry =
      wtx.open_table(INSCRIPTION_ID_TO_INSCRIPTION_ENTRY).unwrap();

    let brc20_data_store = brc20_db::DataStore::new(&wtx);
    let brc20s_data_store = brc20s_db::DataStore::new(&wtx);

    let deploy = Deploy {
      pool_type: "pool".to_string(),
      pool_id: "13395c5283#1f".to_string(),
      stake: "btc1".to_string(),
      earn: "ordi1".to_string(),
      earn_rate: "10".to_string(),
      distribution_max: "12000000".to_string(),
      decimals: Some("18".to_string()),
      total_supply: Some("21000000".to_string()),
      only: Some("1".to_string()),
    };
    let addr1 =
      Address::from_str("bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e").unwrap();
    let script = ScriptKey::from_address(addr1.assume_checked());
    let inscription_id =
      InscriptionId::from_str("1111111111111111111111111111111111111111111111111111111111111111i1")
        .unwrap();

    //caculate tickid faile
    {
      let mut second_deply = deploy.clone();
      second_deply.total_supply = Some("20000000".to_string());
      let msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Deploy(second_deply.clone()),
      );
      let result =
        set_brc20_token_user(&brc20_data_store, "btc1", &msg.from, 200_u128, 18_u8).err();
      assert_eq!(None, result);
      let context = BlockContext {
        blockheight: 0,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_deploy(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        second_deply,
      );

      let result: Result<Vec<Event>, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };
      assert_eq!(
        Err(BRC20SError::InvalidPoolTickId(
          "13395c5283".to_string(),
          "9a839a5ec4".to_string()
        )),
        result
      );

      let mut second_deply = deploy.clone();
      second_deply.decimals = Some("17".to_string());
      let msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Deploy(second_deply.clone()),
      );
      let context = BlockContext {
        blockheight: 0,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_deploy(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        second_deply,
      );

      let result: Result<Vec<Event>, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };
      assert_eq!(
        Err(BRC20SError::InvalidPoolTickId(
          "13395c5283".to_string(),
          "66a4a34e93".to_string()
        )),
        result
      );

      let mut second_deply = deploy.clone();
      let mut msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Deploy(second_deply.clone()),
      );
      msg.from = ScriptKey::Address(
        Address::from_str("bc1pvk535u5eedhsx75r7mfvdru7t0kcr36mf9wuku7k68stc0ncss8qwzeahv")
          .unwrap(),
      );
      msg.commit_from = Some(msg.from.clone());
      let context = BlockContext {
        blockheight: 0,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_deploy(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        second_deply,
      );

      let result: Result<Vec<Event>, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };
      assert_eq!(
        Err(BRC20SError::InvalidPoolTickId(
          "13395c5283".to_string(),
          "c9a808b614".to_string()
        )),
        result
      );

      let mut second_deply = deploy.clone();
      let mut msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Deploy(second_deply.clone()),
      );
      msg.to = Some(ScriptKey::Address(
        Address::from_str("bc1pvk535u5eedhsx75r7mfvdru7t0kcr36mf9wuku7k68stc0ncss8qwzeahv")
          .unwrap(),
      ));
      let context = BlockContext {
        blockheight: 0,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_deploy(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        second_deply,
      );

      let result: Result<Vec<Event>, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };
      assert_eq!(
        Err(BRC20SError::InvalidPoolTickId(
          "13395c5283".to_string(),
          "22ec062391".to_string()
        )),
        result
      );
    }
    //err pool type
    {
      let mut err_pool_type = deploy.clone();
      err_pool_type.pool_type = "errtype".to_string();
      let msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Deploy(err_pool_type.clone()),
      );
      let context = BlockContext {
        blockheight: 0,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_deploy(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        err_pool_type,
      );

      let pid = deploy.get_pool_id();

      let result: Result<Vec<Event>, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };
      assert_eq!(Err(BRC20SError::UnknownPoolType), result);
    }

    //err pid
    {
      let mut err_pid = deploy.clone();
      err_pid.pool_id = "l8195197bc#1f".to_string();
      let msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Deploy(err_pid.clone()),
      );
      let context = BlockContext {
        blockheight: 0,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_deploy(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        err_pid.clone(),
      );

      let pid = deploy.get_pool_id();
      let result: Result<Vec<Event>, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };
      assert_eq!(
        Err(BRC20SError::InvalidPoolId(
          err_pid.pool_id,
          "the prefix of pool id is not hex".to_string()
        )),
        result
      );

      let mut err_pid = deploy.clone();
      err_pid.pool_id = "8195197bc#1f".to_string();
      let msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Deploy(err_pid.clone()),
      );
      let context = BlockContext {
        blockheight: 0,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_deploy(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        err_pid.clone(),
      );

      let pid = deploy.get_pool_id();
      let result: Result<Vec<Event>, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };
      assert_eq!(
        Err(BRC20SError::InvalidPoolId(
          err_pid.pool_id,
          "pool id length is not 13".to_string()
        )),
        result
      );

      let mut err_pid = deploy.clone();
      err_pid.pool_id = "13395c5283#lf".to_string();
      let msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Deploy(err_pid.clone()),
      );
      let context = BlockContext {
        blockheight: 0,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_deploy(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        err_pid.clone(),
      );

      let pid = deploy.get_pool_id();
      let result: Result<Vec<Event>, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };

      assert_eq!(
        Err(BRC20SError::InvalidPoolId(
          err_pid.pool_id,
          "the suffix of pool id is not hex".to_string()
        )),
        result
      );

      let mut err_pid = deploy.clone();
      err_pid.pool_id = "c81195197bc#f".to_string();
      let msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Deploy(err_pid.clone()),
      );
      let context = BlockContext {
        blockheight: 0,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_deploy(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        err_pid.clone(),
      );

      let pid = deploy.get_pool_id();
      let result: Result<Vec<Event>, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };
      assert_eq!(
        Err(BRC20SError::InvalidPoolId(
          err_pid.pool_id,
          "the prefix of pool id is not hex".to_string()
        )),
        result
      );

      let mut err_pid = deploy.clone();
      err_pid.pool_id = "13395c5283$1f".to_string();
      let msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Deploy(err_pid.clone()),
      );
      let context = BlockContext {
        blockheight: 0,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_deploy(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        err_pid.clone(),
      );

      let pid = deploy.get_pool_id();
      let result: Result<Vec<Event>, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };
      assert_eq!(
        Err(BRC20SError::InvalidPoolId(
          err_pid.pool_id,
          "pool id must contains '#'".to_string()
        )),
        result
      );

      let mut err_pid = deploy.clone();
      err_pid.pool_id = "c819519#bc#df".to_string();
      let msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Deploy(err_pid.clone()),
      );
      let context = BlockContext {
        blockheight: 0,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_deploy(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        err_pid.clone(),
      );

      let pid = deploy.get_pool_id();
      let result: Result<Vec<Event>, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };
      assert_eq!(
        Err(BRC20SError::InvalidPoolId(
          err_pid.pool_id,
          "pool id must contains only one '#'".to_string()
        )),
        result
      );

      let mut err_pid = deploy.clone();
      err_pid.pool_id = "c819519#bc#1f".to_string();
      let msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Deploy(err_pid.clone()),
      );
      let context = BlockContext {
        blockheight: 0,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_deploy(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        err_pid.clone(),
      );

      let pid = deploy.get_pool_id();
      let result: Result<Vec<Event>, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };
      assert_eq!(
        Err(BRC20SError::InvalidPoolId(
          err_pid.pool_id,
          "pool id must contains only one '#'".to_string()
        )),
        result
      );

      let mut err_pid = deploy.clone();
      err_pid.pool_id = "a8195197bc#1f".to_string();
      let msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Deploy(err_pid.clone()),
      );
      let context = BlockContext {
        blockheight: 0,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_deploy(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        err_pid,
      );

      let pid = deploy.get_pool_id();
      let result: Result<Vec<Event>, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };
      assert_eq!(
        Err(BRC20SError::InvalidPoolTickId(
          "a8195197bc".to_string(),
          "13395c5283".to_string()
        )),
        result
      );
    }

    //err stake,earn
    {
      let mut err_stake = deploy.clone();
      err_stake.stake = "he".to_string();
      let msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Deploy(err_stake.clone()),
      );
      let context = BlockContext {
        blockheight: 0,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_deploy(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        err_stake,
      );

      let pid = deploy.get_pool_id();
      let result: Result<Vec<Event>, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };
      assert_eq!(Err(BRC20SError::UnknownStakeType), result);

      let mut err_stake = deploy.clone();
      err_stake.stake = "hehehh".to_string();
      let msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Deploy(err_stake.clone()),
      );
      let context = BlockContext {
        blockheight: 0,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_deploy(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        err_stake,
      );

      let pid = deploy.get_pool_id();
      let result: Result<Vec<Event>, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };
      assert_eq!(Err(BRC20SError::UnknownStakeType), result);

      let mut err_stake = deploy.clone();
      err_stake.stake = "test".to_string();
      let msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Deploy(err_stake.clone()),
      );
      let context = BlockContext {
        blockheight: 0,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_deploy(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        err_stake.clone(),
      );

      let pid = deploy.get_pool_id();
      let result: Result<Vec<Event>, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };
      assert_eq!(Err(BRC20SError::StakeNotFound(err_stake.stake)), result);

      let mut err_earn = deploy.clone();
      err_earn.earn = "tes".to_string();
      let msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Deploy(err_earn.clone()),
      );
      let context = BlockContext {
        blockheight: 0,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_deploy(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        err_earn.clone(),
      );

      let pid = deploy.get_pool_id();
      let result: Result<Vec<Event>, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };
      assert_eq!(Err(BRC20SError::InvalidTickLen(err_earn.earn)), result);

      let mut err_earn = deploy.clone();
      err_earn.earn = "test".to_string();
      let msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Deploy(err_earn.clone()),
      );
      let context = BlockContext {
        blockheight: 0,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_deploy(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        err_earn.clone(),
      );

      let pid = deploy.get_pool_id();
      let result: Result<Vec<Event>, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };
      assert_ne!(Err(BRC20SError::InvalidTickLen(err_earn.earn)), result);

      let mut err_earn = deploy.clone();
      err_earn.earn = "testt".to_string();
      let msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Deploy(err_earn.clone()),
      );
      let context = BlockContext {
        blockheight: 0,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_deploy(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        err_earn.clone(),
      );

      let pid = deploy.get_pool_id();
      let result: Result<Vec<Event>, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };
      assert_ne!(Err(BRC20SError::InvalidTickLen(err_earn.earn)), result);

      let mut err_earn = deploy.clone();
      err_earn.earn = "testttt".to_string();
      let msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Deploy(err_earn.clone()),
      );
      let context = BlockContext {
        blockheight: 0,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_deploy(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        err_earn.clone(),
      );

      let pid = deploy.get_pool_id();
      let result: Result<Vec<Event>, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };
      assert_eq!(Err(BRC20SError::InvalidTickLen(err_earn.earn)), result);

      let mut err_earn = deploy.clone();
      err_earn.stake = "13395c5283".to_string();
      let msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Deploy(err_earn.clone()),
      );
      let context = BlockContext {
        blockheight: 0,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_deploy(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        err_earn.clone(),
      );

      let pid = deploy.get_pool_id();
      let result: Result<Vec<Event>, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };
      assert_eq!(
        Err(BRC20SError::StakeEqualEarn(
          err_earn.stake.to_string(),
          err_earn.earn
        )),
        result
      );
    }
    // err erate
    {
      let mut err_erate = deploy.clone();
      err_erate.earn_rate = "".to_string();
      let msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Deploy(err_erate.clone()),
      );
      let context = BlockContext {
        blockheight: 0,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_deploy(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        err_erate,
      );

      let pid = deploy.get_pool_id();
      let result: Result<Vec<Event>, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };
      assert_eq!(Err(BRC20SError::InvalidNum("".to_string())), result);

      let mut err_erate = deploy.clone();
      err_erate.earn_rate = "1l".to_string();
      let msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Deploy(err_erate.clone()),
      );
      let context = BlockContext {
        blockheight: 0,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_deploy(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        err_erate,
      );

      let pid = deploy.get_pool_id();
      let result: Result<Vec<Event>, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };
      assert_eq!(Err(BRC20SError::InvalidNum("1l".to_string())), result);
    }

    //err dmax
    {
      let mut err_dmax = deploy.clone();
      err_dmax.distribution_max = "".to_string();
      let msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Deploy(err_dmax.clone()),
      );
      let context = BlockContext {
        blockheight: 0,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_deploy(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        err_dmax,
      );

      let pid = deploy.get_pool_id();
      let result: Result<Vec<Event>, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };
      assert_eq!(Err(BRC20SError::InvalidNum("".to_string())), result);

      let mut err_dmax = deploy.clone();
      err_dmax.distribution_max = "1l".to_string();
      let msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Deploy(err_dmax.clone()),
      );
      let context = BlockContext {
        blockheight: 0,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_deploy(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        err_dmax,
      );

      let pid = deploy.get_pool_id();
      let result: Result<Vec<Event>, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };
      assert_eq!(Err(BRC20SError::InvalidNum("1l".to_string())), result);

      let mut err_dmax = deploy.clone();
      err_dmax.distribution_max = "21000001".to_string();
      let msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Deploy(err_dmax.clone()),
      );
      let context = BlockContext {
        blockheight: 0,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_deploy(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        err_dmax,
      );

      let pid = deploy.get_pool_id();
      let result: Result<Vec<Event>, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };
      assert_eq!(
        Err(BRC20SError::ExceedDmax(
          "21000001".to_string(),
          "21000000".to_string()
        )),
        result
      );
    }

    //err total_supply
    {
      let mut err_total = deploy.clone();
      err_total.total_supply = Some("".to_string());
      let msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Deploy(err_total.clone()),
      );
      let context = BlockContext {
        blockheight: 0,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_deploy(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        err_total,
      );

      let pid = deploy.get_pool_id();
      let result: Result<Vec<Event>, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };
      assert_eq!(Err(BRC20SError::InvalidNum("".to_string())), result);

      let mut err_dmax = deploy.clone();
      err_dmax.total_supply = Some("1l".to_string());
      let msg =
        mock_create_brc20s_message(script.clone(), script, Operation::Deploy(err_dmax.clone()));
      let context = BlockContext {
        blockheight: 0,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_deploy(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        err_dmax,
      );

      let pid = deploy.get_pool_id();
      let result: Result<Vec<Event>, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };
      assert_eq!(Err(BRC20SError::InvalidNum("1l".to_string())), result);
    }
  }

  #[test]
  fn test_process_stake() {
    let dbfile = NamedTempFile::new().unwrap();
    let db = Database::create(dbfile.path()).unwrap();
    let wtx = db.begin_write().unwrap();
    let brc20_data_store = brc20_db::DataStore::new(&wtx);
    let brc20s_data_store = brc20s_db::DataStore::new(&wtx);

    let deploy = Deploy {
      pool_type: "pool".to_string(),
      pool_id: "fea607ea9e#1f".to_string(),
      stake: "orea".to_string(),
      earn: "ordi".to_string(),
      earn_rate: "1000".to_string(),
      distribution_max: "12000000".to_string(),
      decimals: Some("2".to_string()),
      total_supply: Some("21000000".to_string()),
      only: Some("1".to_string()),
    };
    let addr1 =
      Address::from_str("bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e").unwrap();
    let script = ScriptKey::from_address(addr1.assume_checked());
    let inscription_id =
      InscriptionId::from_str("1111111111111111111111111111111111111111111111111111111111111111i1")
        .unwrap();

    let token = brc20::Tick::from_str("orea".to_string().as_str()).unwrap();
    let token_info = TokenInfo {
      tick: token.clone(),
      inscription_id,
      inscription_number: 0,
      supply: 21000000000_u128,
      minted: 3000000000_u128,
      limit_per_mint: 0,
      decimal: 3,
      deploy_by: script.clone(),
      deployed_number: 0,
      deployed_timestamp: 0,
      latest_mint_number: 0,
    };
    brc20_data_store.insert_token_info(&token, &token_info);
    let balance = BRC20Balance {
      tick: token.clone(),
      overall_balance: 3000000000_u128,
      transferable_balance: 0_u128,
    };
    let result = brc20_data_store.update_token_balance(&script, balance);
    if let Err(error) = result {
      panic!("update_token_balance err: {}", error)
    }

    let msg = mock_create_brc20s_message(
      script.clone(),
      script.clone(),
      Operation::Deploy(deploy.clone()),
    );
    let context = BlockContext {
      blockheight: 10,
      blocktime: 1687245485,
      network: Network::Bitcoin,
    };
    let config = version::get_config_by_network(context.network, context.blockheight);
    let result = process_deploy(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      deploy.clone(),
    );

    let result: Result<Vec<Event>, BRC20SError> = match result {
      Ok(event) => Ok(event),
      Err(Error::BRC20SError(e)) => Err(e),
      Err(e) => Err(BRC20SError::InternalError(e.to_string())),
    };

    match result {
      Ok(event) => {
        println!("success:{}", serde_json::to_string_pretty(&event).unwrap());
      }
      Err(e) => {
        assert_eq!("error", e.to_string())
      }
    }
    let tick_id = deploy.get_tick_id();
    let pid = deploy.get_pool_id();
    let tick_info = brc20s_data_store.get_tick_info(&tick_id).unwrap().unwrap();
    let pool_info = brc20s_data_store
      .get_pid_to_poolinfo(&pid)
      .unwrap()
      .unwrap();

    let expect_tick_info = r#"{"tick_id":"fea607ea9e","name":"ordi","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","allocated":1200000000,"decimal":2,"circulation":0,"supply":2100000000,"deployer":{"Address":"bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e"},"deploy_block":10,"deploy_block_time":1687245485,"latest_mint_block":10,"pids":["fea607ea9e#1f"]}"#;
    let expect_pool_info = r#"{"pid":"fea607ea9e#1f","ptype":"Pool","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","stake":{"BRC20Tick":"orea"},"erate":100000,"minted":0,"staked":0,"dmax":1200000000,"acc_reward_per_share":"0","last_update_block":10,"only":true,"deploy_block":10,"deploy_block_time":1687245485}"#;
    assert_eq!(expect_pool_info, serde_json::to_string(&pool_info).unwrap());
    assert_eq!(expect_tick_info, serde_json::to_string(&tick_info).unwrap());

    let stake_tick = PledgedTick::BRC20Tick(token.clone());
    let stake_msg = Stake {
      pool_id: pid.as_str().to_string(),
      amount: "1000000".to_string(),
    };

    let msg = mock_create_brc20s_message(
      script.clone(),
      script.clone(),
      Operation::Stake(stake_msg.clone()),
    );
    let context = BlockContext {
      blockheight: 20,
      blocktime: 1687245485,
      network: Network::Bitcoin,
    };
    let config = version::get_config_by_network(context.network, context.blockheight);
    let result = process_stake(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      stake_msg.clone(),
    );

    let result: Result<Event, BRC20SError> = match result {
      Ok(event) => Ok(event),
      Err(Error::BRC20SError(e)) => Err(e),
      Err(e) => Err(BRC20SError::InternalError(e.to_string())),
    };

    match result {
      Ok(event) => {
        println!("success:{}", serde_json::to_string_pretty(&event).unwrap());
      }
      Err(e) => {
        assert_eq!("error", e.to_string())
      }
    }
    let stakeinfo = brc20s_data_store
      .get_user_stakeinfo(&script, &stake_tick)
      .unwrap();

    let userinfo = brc20s_data_store
      .get_pid_to_use_info(&script, &pid)
      .unwrap();
    let pool_info = brc20s_data_store.get_pid_to_poolinfo(&pid).unwrap();
    let expect_stakeinfo = r#"{"stake":{"BRC20Tick":"orea"},"pool_stakes":[["fea607ea9e#1f",true,1000000000]],"max_share":0,"total_only":1000000000}"#;
    let expect_userinfo = r#"{"pid":"fea607ea9e#1f","staked":1000000000,"minted":0,"pending_reward":0,"reward_debt":0,"latest_updated_block":20}"#;
    let expect_poolinfo = r#"{"pid":"fea607ea9e#1f","ptype":"Pool","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","stake":{"BRC20Tick":"orea"},"erate":100000,"minted":0,"staked":1000000000,"dmax":1200000000,"acc_reward_per_share":"0","last_update_block":20,"only":true,"deploy_block":10,"deploy_block_time":1687245485}"#;

    assert_eq!(expect_poolinfo, serde_json::to_string(&pool_info).unwrap());
    assert_eq!(expect_stakeinfo, serde_json::to_string(&stakeinfo).unwrap());
    assert_eq!(expect_userinfo, serde_json::to_string(&userinfo).unwrap());
    {
      let stake_tick = PledgedTick::BRC20Tick(token.clone());
      let stake_msg = Stake {
        pool_id: pid.as_str().to_string(),
        amount: "1000000".to_string(),
      };

      let mut msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Stake(stake_msg.clone()),
      );
      let context = BlockContext {
        blockheight: 30,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_stake(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        stake_msg,
      );

      let result: Result<Event, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };

      match result {
        Ok(event) => {
          println!("success:{}", serde_json::to_string_pretty(&event).unwrap());
        }
        Err(e) => {
          assert_eq!("error", e.to_string())
        }
      }
      let stakeinfo = brc20s_data_store
        .get_user_stakeinfo(&script, &stake_tick)
        .unwrap();

      let userinfo = brc20s_data_store
        .get_pid_to_use_info(&script, &pid)
        .unwrap();
      let pool_info = brc20s_data_store.get_pid_to_poolinfo(&pid).unwrap();
      let expect_stakeinfo = r#"{"stake":{"BRC20Tick":"orea"},"pool_stakes":[["fea607ea9e#1f",true,2000000000]],"max_share":0,"total_only":2000000000}"#;
      let expect_userinfo = r#"{"pid":"fea607ea9e#1f","staked":2000000000,"minted":0,"pending_reward":1000000,"reward_debt":2000000,"latest_updated_block":30}"#;
      let expect_poolinfo = r#"{"pid":"fea607ea9e#1f","ptype":"Pool","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","stake":{"BRC20Tick":"orea"},"erate":100000,"minted":1000000,"staked":2000000000,"dmax":1200000000,"acc_reward_per_share":"1000000000000000","last_update_block":30,"only":true,"deploy_block":10,"deploy_block_time":1687245485}"#;
      println!(
        "expect_poolinfo:{}",
        serde_json::to_string(&pool_info).unwrap()
      );
      println!(
        "expect_stakeinfo:{}",
        serde_json::to_string(&stakeinfo).unwrap()
      );
      println!(
        "expect_userinfo:{}",
        serde_json::to_string(&userinfo).unwrap()
      );

      assert_eq!(expect_poolinfo, serde_json::to_string(&pool_info).unwrap());
      assert_eq!(expect_stakeinfo, serde_json::to_string(&stakeinfo).unwrap());
      assert_eq!(expect_userinfo, serde_json::to_string(&userinfo).unwrap());
    }

    // invalid inscribe to coinbase
    {
      let stake_tick = PledgedTick::BRC20Tick(token.clone());
      let stake_msg = Stake {
        pool_id: pid.as_str().to_string(),
        amount: "1000000".to_string(),
      };

      let mut msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Stake(stake_msg.clone()),
      );
      msg.to = None;

      let context = BlockContext {
        blockheight: 1,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_stake(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        stake_msg,
      );

      let result: Result<Event, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };

      assert_eq!(Err(BRC20SError::InscribeToCoinbase), result);
    }

    // stake msg validate_basic err
    {
      let stake_tick = PledgedTick::BRC20Tick(token.clone());
      let stake_msg = Stake {
        pool_id: pid.as_str().to_string(),
        amount: "a".to_string(),
      };

      let mut msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Stake(stake_msg.clone()),
      );

      let context = BlockContext {
        blockheight: 1,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_stake(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        stake_msg,
      );

      let result: Result<Event, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };

      assert_eq!(Err(BRC20SError::InvalidNum("a".to_string())), result);
    }

    // from_script_key is none
    {
      let stake_tick = PledgedTick::BRC20Tick(token.clone());
      let stake_msg = Stake {
        pool_id: pid.as_str().to_string(),
        amount: "1".to_string(),
      };

      let mut msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Stake(stake_msg.clone()),
      );
      msg.commit_from = None;

      let context = BlockContext {
        blockheight: 1,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_stake(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        stake_msg,
      );

      let result: Result<Event, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };

      assert_eq!(Err(BRC20SError::InternalError("".to_string())), result);
    }

    // user stake is 0
    {
      let stake_tick = PledgedTick::BRC20Tick(token.clone());
      let unstake_msg = UnStake {
        pool_id: pid.as_str().to_string(),
        amount: "2000000".to_string(),
      };

      let mut msg =
        mock_create_brc20s_message(script.clone(), script.clone(), Operation::Stake(stake_msg));

      let context = BlockContext {
        blockheight: 30,
        blocktime: 1687245486,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_unstake(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        unstake_msg,
      );

      let result: Result<Event, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };

      //stake again
      let stake_tick = PledgedTick::BRC20Tick(token.clone());
      let stake_msg = Stake {
        pool_id: pid.as_str().to_string(),
        amount: "2000000".to_string(),
      };

      let mut msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Stake(stake_msg.clone()),
      );

      let context = BlockContext {
        blockheight: 30,
        blocktime: 1687245486,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_stake(
        context,
        config.clone(),
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        stake_msg,
      );

      let result: Result<Event, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };

      let stakeinfo = brc20s_data_store
        .get_user_stakeinfo(&script, &stake_tick)
        .unwrap();
      let userinfo = brc20s_data_store
        .get_pid_to_use_info(&script, &pid)
        .unwrap();
      let pool_info = brc20s_data_store.get_pid_to_poolinfo(&pid).unwrap();
      let expect_stakeinfo = r#"{"stake":{"BRC20Tick":"orea"},"pool_stakes":[["fea607ea9e#1f",true,2000000000]],"max_share":0,"total_only":2000000000}"#;
      let expect_userinfo = r#"{"pid":"fea607ea9e#1f","staked":2000000000,"minted":0,"pending_reward":1000000,"reward_debt":2000000,"latest_updated_block":30}"#;
      let expect_poolinfo = r#"{"pid":"fea607ea9e#1f","ptype":"Pool","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","stake":{"BRC20Tick":"orea"},"erate":100000,"minted":1000000,"staked":2000000000,"dmax":1200000000,"acc_reward_per_share":"1000000000000000","last_update_block":30,"only":true,"deploy_block":10,"deploy_block_time":1687245485}"#;
      println!(
        "expect_poolinfo:{}",
        serde_json::to_string(&pool_info).unwrap()
      );
      println!(
        "expect_stakeinfo:{}",
        serde_json::to_string(&stakeinfo).unwrap()
      );
      println!(
        "expect_userinfo:{}",
        serde_json::to_string(&userinfo).unwrap()
      );

      assert_eq!(expect_poolinfo, serde_json::to_string(&pool_info).unwrap());
      assert_eq!(expect_stakeinfo, serde_json::to_string(&stakeinfo).unwrap());
      assert_eq!(expect_userinfo, serde_json::to_string(&userinfo).unwrap());
    }

    // MAX_STAKED_POOL_NUM 5
    {
      let mut deploy = Deploy {
        pool_type: "pool".to_string(),
        pool_id: "fea607ea9e#1f".to_string(),
        stake: "orea".to_string(),
        earn: "ordi".to_string(),
        earn_rate: "1000".to_string(),
        distribution_max: "12000000".to_string(),
        decimals: Some("2".to_string()),
        total_supply: Some("21000000".to_string()),
        only: Some("1".to_string()),
      };
      let addr1 =
        Address::from_str("bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e")
          .unwrap();
      let script = ScriptKey::from_address(addr1.assume_checked());
      let inscription_id = InscriptionId::from_str(
        "1111111111111111111111111111111111111111111111111111111111111111i1",
      )
      .unwrap();

      let mut msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::Deploy(deploy.clone()),
      );
      deploy.pool_id = "e3f8d0e378#01".to_string();
      deploy.decimals = Some("8".to_string());
      msg.op = Operation::Deploy(deploy.clone());
      let result = process_deploy(
        context,
        config.clone(),
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        deploy.clone(),
      );
      deploy.pool_id = "136bb1d966#01".to_string();
      deploy.decimals = Some("9".to_string());
      msg.op = Operation::Deploy(deploy.clone());
      let result = process_deploy(
        context,
        config.clone(),
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        deploy.clone(),
      );
      deploy.pool_id = "6af92d18d6#01".to_string();
      deploy.decimals = Some("10".to_string());
      msg.op = Operation::Deploy(deploy.clone());
      let result = process_deploy(
        context,
        config.clone(),
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        deploy.clone(),
      );
      deploy.pool_id = "d9fc11764c#01".to_string();
      deploy.decimals = Some("11".to_string());
      msg.op = Operation::Deploy(deploy.clone());
      let result = process_deploy(
        context,
        config.clone(),
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        deploy.clone(),
      );

      deploy.pool_id = "fa48a823af#01".to_string();
      deploy.decimals = Some("12".to_string());
      msg.op = Operation::Deploy(deploy.clone());
      let result = process_deploy(
        context,
        config.clone(),
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        deploy,
      );

      let stake_tick = PledgedTick::BRC20Tick(token);
      let mut stake_msg = Stake {
        pool_id: pid.as_str().to_string(),
        amount: "1".to_string(),
      };

      //stake 6 pool
      let mut msg =
        mock_create_brc20s_message(script.clone(), script, Operation::Stake(stake_msg.clone()));

      let context = BlockContext {
        blockheight: 10,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::zebra();
      let result = process_stake(
        context,
        config.clone(),
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        stake_msg.clone(),
      );

      stake_msg.pool_id = "e3f8d0e378#01".to_string();
      msg.op = Operation::Stake(stake_msg.clone());
      let result = process_stake(
        context,
        config.clone(),
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        stake_msg.clone(),
      );

      stake_msg.pool_id = "136bb1d966#01".to_string();
      msg.op = Operation::Stake(stake_msg.clone());
      let result = process_stake(
        context,
        config.clone(),
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        stake_msg.clone(),
      );

      stake_msg.pool_id = "6af92d18d6#01".to_string();
      msg.op = Operation::Stake(stake_msg.clone());
      let result = process_stake(
        context,
        config.clone(),
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        stake_msg.clone(),
      );

      stake_msg.pool_id = "d9fc11764c#01".to_string();
      msg.op = Operation::Stake(stake_msg.clone());
      let result = process_stake(
        context,
        config.clone(),
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        stake_msg.clone(),
      );

      stake_msg.pool_id = "fa48a823af#01".to_string();
      msg.op = Operation::Stake(stake_msg.clone());
      let result = process_stake(
        context,
        config.clone(),
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        stake_msg.clone(),
      );

      let result: Result<Event, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };

      assert_eq!(
        Err(BRC20SError::InternalError(
          "the number of stake pool is full".to_string()
        )),
        result
      );

      stake_msg.pool_id = "fa48a823af#01".to_string();
      msg.op = Operation::Stake(stake_msg.clone());
      let mut context = context;
      let config = version::koala();
      let result = process_stake(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        stake_msg,
      );

      let result: Result<Event, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };
      let pid = Pid::from_str("fa48a823af#01").unwrap();
      assert_eq!(
        Ok(Event::Deposit(DepositEvent {
          pid,
          amt: 1000,
          period_settlement_reward: 0
        })),
        result
      );
    }
  }

  #[test]
  fn test_process_unstake() {
    let dbfile = NamedTempFile::new().unwrap();
    let db = Database::create(dbfile.path()).unwrap();
    let wtx = db.begin_write().unwrap();
    let brc20_data_store = brc20_db::DataStore::new(&wtx);
    let brc20s_data_store = brc20s_db::DataStore::new(&wtx);

    let deploy = Deploy {
      pool_type: "pool".to_string(),
      pool_id: "fea607ea9e#1f".to_string(),
      stake: "orea".to_string(),
      earn: "ordi".to_string(),
      earn_rate: "1000".to_string(),
      distribution_max: "12000000".to_string(),
      decimals: Some("2".to_string()),
      total_supply: Some("21000000".to_string()),
      only: Some("1".to_string()),
    };
    let addr1 =
      Address::from_str("bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e").unwrap();
    let script = ScriptKey::from_address(addr1.assume_checked());
    let inscription_id =
      InscriptionId::from_str("1111111111111111111111111111111111111111111111111111111111111111i1")
        .unwrap();

    let token = brc20::Tick::from_str("orea".to_string().as_str()).unwrap();
    let token_info = TokenInfo {
      tick: token.clone(),
      inscription_id,
      inscription_number: 0,
      supply: 21000000000_u128,
      minted: 2000000000_u128,
      limit_per_mint: 0,
      decimal: 3,
      deploy_by: script.clone(),
      deployed_number: 0,
      deployed_timestamp: 0,
      latest_mint_number: 0,
    };
    brc20_data_store.insert_token_info(&token, &token_info);
    let balance = BRC20Balance {
      tick: token.clone(),
      overall_balance: 2000000000_u128,
      transferable_balance: 1000000000_u128,
    };
    let result = brc20_data_store.update_token_balance(&script, balance);
    if let Err(error) = result {
      panic!("update_token_balance err: {}", error)
    }

    let msg = mock_create_brc20s_message(
      script.clone(),
      script.clone(),
      Operation::Deploy(deploy.clone()),
    );
    let context = BlockContext {
      blockheight: 10,
      blocktime: 1687245485,
      network: Network::Bitcoin,
    };
    let config = version::get_config_by_network(context.network, context.blockheight);
    let result = process_deploy(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      deploy.clone(),
    );

    let result: Result<Vec<Event>, BRC20SError> = match result {
      Ok(event) => Ok(event),
      Err(Error::BRC20SError(e)) => Err(e),
      Err(e) => Err(BRC20SError::InternalError(e.to_string())),
    };

    match result {
      Ok(event) => {
        println!("success:{}", serde_json::to_string_pretty(&event).unwrap());
      }
      Err(e) => {
        assert_eq!("error", e.to_string())
      }
    }
    let tick_id = deploy.get_tick_id();
    let pid = deploy.get_pool_id();
    let tick_info = brc20s_data_store.get_tick_info(&tick_id).unwrap().unwrap();
    let pool_info = brc20s_data_store
      .get_pid_to_poolinfo(&pid)
      .unwrap()
      .unwrap();

    let expect_tick_info = r#"{"tick_id":"fea607ea9e","name":"ordi","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","allocated":1200000000,"decimal":2,"circulation":0,"supply":2100000000,"deployer":{"Address":"bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e"},"deploy_block":10,"deploy_block_time":1687245485,"latest_mint_block":10,"pids":["fea607ea9e#1f"]}"#;
    let expect_pool_info = r#"{"pid":"fea607ea9e#1f","ptype":"Pool","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","stake":{"BRC20Tick":"orea"},"erate":100000,"minted":0,"staked":0,"dmax":1200000000,"acc_reward_per_share":"0","last_update_block":10,"only":true,"deploy_block":10,"deploy_block_time":1687245485}"#;
    assert_eq!(expect_pool_info, serde_json::to_string(&pool_info).unwrap());
    assert_eq!(expect_tick_info, serde_json::to_string(&tick_info).unwrap());

    let stake_tick = PledgedTick::BRC20Tick(token.clone());
    let stake_msg = Stake {
      pool_id: pid.as_str().to_string(),
      amount: "1000000".to_string(),
    };

    let msg = mock_create_brc20s_message(
      script.clone(),
      script.clone(),
      Operation::Stake(stake_msg.clone()),
    );
    let context = BlockContext {
      blockheight: 20,
      blocktime: 1687245485,
      network: Network::Bitcoin,
    };
    let config = version::get_config_by_network(context.network, context.blockheight);
    let result = process_stake(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      stake_msg,
    );

    let result: Result<Event, BRC20SError> = match result {
      Ok(event) => Ok(event),
      Err(Error::BRC20SError(e)) => Err(e),
      Err(e) => Err(BRC20SError::InternalError(e.to_string())),
    };

    match result {
      Ok(event) => {
        println!("success:{}", serde_json::to_string_pretty(&event).unwrap());
      }
      Err(e) => {
        assert_eq!("error", e.to_string())
      }
    }
    let stakeinfo = brc20s_data_store
      .get_user_stakeinfo(&script, &stake_tick)
      .unwrap();

    let userinfo = brc20s_data_store
      .get_pid_to_use_info(&script, &pid)
      .unwrap();
    let pool_info = brc20s_data_store.get_pid_to_poolinfo(&pid).unwrap();
    let expect_stakeinfo = r#"{"stake":{"BRC20Tick":"orea"},"pool_stakes":[["fea607ea9e#1f",true,1000000000]],"max_share":0,"total_only":1000000000}"#;
    let expect_userinfo = r#"{"pid":"fea607ea9e#1f","staked":1000000000,"minted":0,"pending_reward":0,"reward_debt":0,"latest_updated_block":20}"#;
    let expect_poolinfo = r#"{"pid":"fea607ea9e#1f","ptype":"Pool","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","stake":{"BRC20Tick":"orea"},"erate":100000,"minted":0,"staked":1000000000,"dmax":1200000000,"acc_reward_per_share":"0","last_update_block":20,"only":true,"deploy_block":10,"deploy_block_time":1687245485}"#;

    assert_eq!(expect_poolinfo, serde_json::to_string(&pool_info).unwrap());
    assert_eq!(expect_stakeinfo, serde_json::to_string(&stakeinfo).unwrap());
    assert_eq!(expect_userinfo, serde_json::to_string(&userinfo).unwrap());
    {
      let stake_tick = PledgedTick::BRC20Tick(token);
      let unstake_msg = UnStake {
        pool_id: pid.as_str().to_string(),
        amount: "1000000".to_string(),
      };

      let mut msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::UnStake(unstake_msg.clone()),
      );
      let context = BlockContext {
        blockheight: 30,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_unstake(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        unstake_msg,
      );

      let result: Result<Event, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };

      match result {
        Ok(event) => {
          println!("success:{}", serde_json::to_string_pretty(&event).unwrap());
        }
        Err(e) => {
          assert_eq!("error", e.to_string())
        }
      }
      let stakeinfo = brc20s_data_store
        .get_user_stakeinfo(&script, &stake_tick)
        .unwrap();

      let userinfo = brc20s_data_store
        .get_pid_to_use_info(&script, &pid)
        .unwrap();
      let pool_info = brc20s_data_store.get_pid_to_poolinfo(&pid).unwrap();
      let expect_stakeinfo =
        r#"{"stake":{"BRC20Tick":"orea"},"pool_stakes":[],"max_share":0,"total_only":0}"#;
      let expect_userinfo = r#"{"pid":"fea607ea9e#1f","staked":0,"minted":0,"pending_reward":1000000,"reward_debt":0,"latest_updated_block":30}"#;
      let expect_poolinfo = r#"{"pid":"fea607ea9e#1f","ptype":"Pool","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","stake":{"BRC20Tick":"orea"},"erate":100000,"minted":1000000,"staked":0,"dmax":1200000000,"acc_reward_per_share":"1000000000000000","last_update_block":30,"only":true,"deploy_block":10,"deploy_block_time":1687245485}"#;
      println!(
        "expect_poolinfo:{}",
        serde_json::to_string(&pool_info).unwrap()
      );
      println!(
        "expect_stakeinfo:{}",
        serde_json::to_string(&stakeinfo).unwrap()
      );
      println!(
        "expect_userinfo:{}",
        serde_json::to_string(&userinfo).unwrap()
      );

      assert_eq!(expect_poolinfo, serde_json::to_string(&pool_info).unwrap());
      assert_eq!(expect_stakeinfo, serde_json::to_string(&stakeinfo).unwrap());
      assert_eq!(expect_userinfo, serde_json::to_string(&userinfo).unwrap());
    }
  }

  #[test]
  fn test_process_unstake_common() {
    let dbfile = NamedTempFile::new().unwrap();
    let db = Database::create(dbfile.path()).unwrap();
    let wtx = db.begin_write().unwrap();
    let brc20_data_store = brc20_db::DataStore::new(&wtx);
    let brc20s_data_store = brc20s_db::DataStore::new(&wtx);

    let deploy = Deploy {
      pool_type: "pool".to_string(),
      pool_id: "fea607ea9e#1f".to_string(),
      stake: "orea".to_string(),
      earn: "ordi".to_string(),
      earn_rate: "1000".to_string(),
      distribution_max: "12000000".to_string(),
      decimals: Some("2".to_string()),
      total_supply: Some("21000000".to_string()),
      only: Some("1".to_string()),
    };
    let addr1 =
      Address::from_str("bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e").unwrap();
    let script = ScriptKey::from_address(addr1.assume_checked());
    let inscription_id =
      InscriptionId::from_str("1111111111111111111111111111111111111111111111111111111111111111i1")
        .unwrap();

    let token = brc20::Tick::from_str("orea".to_string().as_str()).unwrap();
    let token_info = TokenInfo {
      tick: token.clone(),
      inscription_id,
      inscription_number: 0,
      supply: 21000000000_u128,
      minted: 2000000000_u128,
      limit_per_mint: 0,
      decimal: 3,
      deploy_by: script.clone(),
      deployed_number: 0,
      deployed_timestamp: 0,
      latest_mint_number: 0,
    };
    brc20_data_store.insert_token_info(&token, &token_info);
    let balance = BRC20Balance {
      tick: token.clone(),
      overall_balance: 2000000000_u128,
      transferable_balance: 1000000000_u128,
    };
    let result = brc20_data_store.update_token_balance(&script, balance);
    if let Err(error) = result {
      panic!("update_token_balance err: {}", error)
    }

    let msg = mock_create_brc20s_message(
      script.clone(),
      script.clone(),
      Operation::Deploy(deploy.clone()),
    );
    let context = BlockContext {
      blockheight: 0,
      blocktime: 1687245485,
      network: Network::Bitcoin,
    };
    let config = version::get_config_by_network(context.network, context.blockheight);
    let result = process_deploy(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      deploy.clone(),
    );

    let result: Result<Vec<Event>, BRC20SError> = match result {
      Ok(event) => Ok(event),
      Err(Error::BRC20SError(e)) => Err(e),
      Err(e) => Err(BRC20SError::InternalError(e.to_string())),
    };

    match result {
      Ok(event) => {
        println!("success:{}", serde_json::to_string_pretty(&event).unwrap());
      }
      Err(e) => {
        assert_eq!("error", e.to_string())
      }
    }
    let tick_id = deploy.get_tick_id();
    let pid = deploy.get_pool_id();
    let tick_info = brc20s_data_store.get_tick_info(&tick_id).unwrap().unwrap();
    let pool_info = brc20s_data_store
      .get_pid_to_poolinfo(&pid)
      .unwrap()
      .unwrap();

    let expect_tick_info = r#"{"tick_id":"fea607ea9e","name":"ordi","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","allocated":1200000000,"decimal":2,"circulation":0,"supply":2100000000,"deployer":{"Address":"bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e"},"deploy_block":0,"deploy_block_time":1687245485,"latest_mint_block":0,"pids":["fea607ea9e#1f"]}"#;
    let expect_pool_info = r#"{"pid":"fea607ea9e#1f","ptype":"Pool","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","stake":{"BRC20Tick":"orea"},"erate":100000,"minted":0,"staked":0,"dmax":1200000000,"acc_reward_per_share":"0","last_update_block":0,"only":true,"deploy_block":0,"deploy_block_time":1687245485}"#;
    assert_eq!(expect_pool_info, serde_json::to_string(&pool_info).unwrap());
    assert_eq!(expect_tick_info, serde_json::to_string(&tick_info).unwrap());

    let stake_tick = PledgedTick::BRC20Tick(token.clone());
    let stake_msg = Stake {
      pool_id: pid.as_str().to_string(),
      amount: "1000000".to_string(),
    };

    let msg = mock_create_brc20s_message(
      script.clone(),
      script.clone(),
      Operation::Stake(stake_msg.clone()),
    );
    let context = BlockContext {
      blockheight: 0,
      blocktime: 1687245485,
      network: Network::Bitcoin,
    };
    let config = version::get_config_by_network(context.network, context.blockheight);
    let result = process_stake(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      stake_msg,
    );

    let result: Result<Event, BRC20SError> = match result {
      Ok(event) => Ok(event),
      Err(Error::BRC20SError(e)) => Err(e),
      Err(e) => Err(BRC20SError::InternalError(e.to_string())),
    };

    match result {
      Ok(event) => {
        println!("success:{}", serde_json::to_string_pretty(&event).unwrap());
      }
      Err(e) => {
        assert_eq!("error", e.to_string())
      }
    }
    let stakeinfo = brc20s_data_store
      .get_user_stakeinfo(&script, &stake_tick)
      .unwrap();

    let userinfo = brc20s_data_store
      .get_pid_to_use_info(&script, &pid)
      .unwrap();
    let pool_info = brc20s_data_store.get_pid_to_poolinfo(&pid).unwrap();
    let expect_stakeinfo = r#"{"stake":{"BRC20Tick":"orea"},"pool_stakes":[["fea607ea9e#1f",true,1000000000]],"max_share":0,"total_only":1000000000}"#;
    let expect_userinfo = r#"{"pid":"fea607ea9e#1f","staked":1000000000,"minted":0,"pending_reward":0,"reward_debt":0,"latest_updated_block":0}"#;
    let expect_poolinfo = r#"{"pid":"fea607ea9e#1f","ptype":"Pool","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","stake":{"BRC20Tick":"orea"},"erate":100000,"minted":0,"staked":1000000000,"dmax":1200000000,"acc_reward_per_share":"0","last_update_block":0,"only":true,"deploy_block":0,"deploy_block_time":1687245485}"#;

    assert_eq!(expect_poolinfo, serde_json::to_string(&pool_info).unwrap());
    assert_eq!(expect_stakeinfo, serde_json::to_string(&stakeinfo).unwrap());
    assert_eq!(expect_userinfo, serde_json::to_string(&userinfo).unwrap());
    {
      let stake_tick = PledgedTick::BRC20Tick(token.clone());
      let unstake_msg = UnStake {
        pool_id: pid.as_str().to_string(),
        amount: "1000000".to_string(),
      };

      let mut msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::UnStake(unstake_msg.clone()),
      );
      let context = BlockContext {
        blockheight: 1,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_unstake(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        unstake_msg,
      );
    }

    //invalid inscribe to coinbase
    {
      let mut stake_tick = PledgedTick::BRC20Tick(token.clone());
      let unstake_msg = UnStake {
        pool_id: pid.as_str().to_string(),
        amount: "1000000".to_string(),
      };
      let mut msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::UnStake(unstake_msg.clone()),
      );
      msg.to = None;
      let result = process_unstake(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        unstake_msg,
      );

      let result: Result<Vec<Event>, BRC20SError> = match result {
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
        _ => panic!(),
      };

      assert_eq!(Err(BRC20SError::InscribeToCoinbase), result);
    }

    //validate_basic
    {
      let mut stake_tick = PledgedTick::BRC20Tick(token.clone());
      let unstake_msg = UnStake {
        pool_id: pid.as_str().to_string(),
        amount: "a".to_string(),
      };
      let mut msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::UnStake(unstake_msg.clone()),
      );
      let context = BlockContext {
        blockheight: 10,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_unstake(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        unstake_msg,
      );

      let result: Result<Vec<Event>, BRC20SError> = match result {
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
        _ => panic!(),
      };

      assert_eq!(Err(BRC20SError::InvalidNum("a".to_string())), result);
    }

    //msg.commit_from is none
    {
      let mut stake_tick = PledgedTick::BRC20Tick(token);
      let unstake_msg = UnStake {
        pool_id: pid.as_str().to_string(),
        amount: "1".to_string(),
      };
      let mut msg = mock_create_brc20s_message(
        script.clone(),
        script,
        Operation::UnStake(unstake_msg.clone()),
      );
      msg.commit_from = None;
      let context = BlockContext {
        blockheight: 10,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };
      let config = version::get_config_by_network(context.network, context.blockheight);
      let result = process_unstake(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        unstake_msg,
      );

      let result: Result<Vec<Event>, BRC20SError> = match result {
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
        _ => panic!(),
      };

      assert_eq!(Err(BRC20SError::InternalError("".to_string())), result);
    }
  }

  #[test]
  fn test_process_passive_unstake() {
    let dbfile = NamedTempFile::new().unwrap();
    let db = Database::create(dbfile.path()).unwrap();
    let wtx = db.begin_write().unwrap();
    let brc20_data_store = brc20_db::DataStore::new(&wtx);
    let brc20s_data_store = brc20s_db::DataStore::new(&wtx);

    let deploy = Deploy {
      pool_type: "pool".to_string(),
      pool_id: "fea607ea9e#1f".to_string(),
      stake: "orea".to_string(),
      earn: "ordi".to_string(),
      earn_rate: "1000".to_string(),
      distribution_max: "12000000".to_string(),
      decimals: Some("2".to_string()),
      total_supply: Some("21000000".to_string()),
      only: Some("1".to_string()),
    };
    let addr1 =
      Address::from_str("bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e").unwrap();
    let script = ScriptKey::from_address(addr1.assume_checked());
    let inscription_id =
      InscriptionId::from_str("1111111111111111111111111111111111111111111111111111111111111111i1")
        .unwrap();

    let token = brc20::Tick::from_str("orea".to_string().as_str()).unwrap();
    let token_info = TokenInfo {
      tick: token.clone(),
      inscription_id,
      inscription_number: 0,
      supply: 21000000000_u128,
      minted: 2000000000_u128,
      limit_per_mint: 0,
      decimal: 3,
      deploy_by: script.clone(),
      deployed_number: 0,
      deployed_timestamp: 0,
      latest_mint_number: 0,
    };
    brc20_data_store.insert_token_info(&token, &token_info);
    let balance = BRC20Balance {
      tick: token.clone(),
      overall_balance: 2000000000_u128,
      transferable_balance: 1000000000_u128,
    };
    let result = brc20_data_store.update_token_balance(&script, balance);
    if let Err(error) = result {
      panic!("update_token_balance err: {}", error)
    }

    let msg = mock_create_brc20s_message(
      script.clone(),
      script.clone(),
      Operation::Deploy(deploy.clone()),
    );
    let context = BlockContext {
      blockheight: 10,
      blocktime: 1687245485,
      network: Network::Bitcoin,
    };
    let config = version::get_config_by_network(context.network, context.blockheight);
    let result = process_deploy(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      deploy.clone(),
    );

    let result: Result<Vec<Event>, BRC20SError> = match result {
      Ok(event) => Ok(event),
      Err(Error::BRC20SError(e)) => Err(e),
      Err(e) => Err(BRC20SError::InternalError(e.to_string())),
    };

    match result {
      Ok(event) => {
        println!("success:{}", serde_json::to_string_pretty(&event).unwrap());
      }
      Err(e) => {
        assert_eq!("error", e.to_string())
      }
    }
    let tick_id = deploy.get_tick_id();
    let pid = deploy.get_pool_id();
    let tick_info = brc20s_data_store.get_tick_info(&tick_id).unwrap().unwrap();
    let pool_info = brc20s_data_store
      .get_pid_to_poolinfo(&pid)
      .unwrap()
      .unwrap();

    let expect_tick_info = r#"{"tick_id":"fea607ea9e","name":"ordi","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","allocated":1200000000,"decimal":2,"circulation":0,"supply":2100000000,"deployer":{"Address":"bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e"},"deploy_block":10,"deploy_block_time":1687245485,"latest_mint_block":10,"pids":["fea607ea9e#1f"]}"#;
    let expect_pool_info = r#"{"pid":"fea607ea9e#1f","ptype":"Pool","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","stake":{"BRC20Tick":"orea"},"erate":100000,"minted":0,"staked":0,"dmax":1200000000,"acc_reward_per_share":"0","last_update_block":10,"only":true,"deploy_block":10,"deploy_block_time":1687245485}"#;
    assert_eq!(expect_pool_info, serde_json::to_string(&pool_info).unwrap());
    assert_eq!(expect_tick_info, serde_json::to_string(&tick_info).unwrap());

    let stake_tick = PledgedTick::BRC20Tick(token.clone());
    let stake_msg = Stake {
      pool_id: pid.as_str().to_string(),
      amount: "1000000".to_string(),
    };

    let msg = mock_create_brc20s_message(
      script.clone(),
      script.clone(),
      Operation::Stake(stake_msg.clone()),
    );
    let context = BlockContext {
      blockheight: 20,
      blocktime: 1687245485,
      network: Network::Bitcoin,
    };
    let config = version::get_config_by_network(context.network, context.blockheight);
    let result = process_stake(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      stake_msg,
    );

    let result: Result<Event, BRC20SError> = match result {
      Ok(event) => Ok(event),
      Err(Error::BRC20SError(e)) => Err(e),
      Err(e) => Err(BRC20SError::InternalError(e.to_string())),
    };

    match result {
      Ok(event) => {
        println!("success:{}", serde_json::to_string_pretty(&event).unwrap());
      }
      Err(e) => {
        assert_eq!("error", e.to_string())
      }
    }
    let stakeinfo = brc20s_data_store
      .get_user_stakeinfo(&script, &stake_tick)
      .unwrap();

    let userinfo = brc20s_data_store
      .get_pid_to_use_info(&script, &pid)
      .unwrap();
    let pool_info = brc20s_data_store.get_pid_to_poolinfo(&pid).unwrap();
    let expect_stakeinfo = r#"{"stake":{"BRC20Tick":"orea"},"pool_stakes":[["fea607ea9e#1f",true,1000000000]],"max_share":0,"total_only":1000000000}"#;
    let expect_userinfo = r#"{"pid":"fea607ea9e#1f","staked":1000000000,"minted":0,"pending_reward":0,"reward_debt":0,"latest_updated_block":20}"#;
    let expect_poolinfo = r#"{"pid":"fea607ea9e#1f","ptype":"Pool","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","stake":{"BRC20Tick":"orea"},"erate":100000,"minted":0,"staked":1000000000,"dmax":1200000000,"acc_reward_per_share":"0","last_update_block":20,"only":true,"deploy_block":10,"deploy_block_time":1687245485}"#;

    assert_eq!(expect_poolinfo, serde_json::to_string(&pool_info).unwrap());
    assert_eq!(expect_stakeinfo, serde_json::to_string(&stakeinfo).unwrap());
    assert_eq!(expect_userinfo, serde_json::to_string(&userinfo).unwrap());
    {
      let stake_tick = PledgedTick::BRC20Tick(token.clone());
      let passive_unstake_msg = PassiveUnStake {
        stake: stake_tick.to_string(),
        amount: "2000000".to_string(),
      };
      let mut msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::PassiveUnStake(passive_unstake_msg.clone()),
      );
      let context = BlockContext {
        blockheight: 30,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };

      //mock brc20 transfer
      let balance = BRC20Balance {
        tick: token,
        overall_balance: 0_u128,
        transferable_balance: 0_u128,
      };
      let result = brc20_data_store.update_token_balance(&script, balance);
      if let Err(error) = result {
        panic!("update_token_balance err: {}", error)
      }
      let result = process_passive_unstake(
        context,
        config,
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        passive_unstake_msg,
      );

      let result: Result<Vec<Event>, BRC20SError> = match result {
        Ok(event) => Ok(event),
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
      };

      match result {
        Ok(event) => {
          println!("success:{}", serde_json::to_string_pretty(&event).unwrap());
        }
        Err(e) => {
          assert_eq!("error", e.to_string())
        }
      }
      let stakeinfo = brc20s_data_store
        .get_user_stakeinfo(&script, &stake_tick)
        .unwrap();

      let userinfo = brc20s_data_store
        .get_pid_to_use_info(&script, &pid)
        .unwrap();
      let pool_info = brc20s_data_store.get_pid_to_poolinfo(&pid).unwrap();
      let expect_stakeinfo =
        r#"{"stake":{"BRC20Tick":"orea"},"pool_stakes":[],"max_share":0,"total_only":0}"#;
      let expect_userinfo = r#"{"pid":"fea607ea9e#1f","staked":0,"minted":0,"pending_reward":1000000,"reward_debt":0,"latest_updated_block":30}"#;
      let expect_poolinfo = r#"{"pid":"fea607ea9e#1f","ptype":"Pool","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","stake":{"BRC20Tick":"orea"},"erate":100000,"minted":1000000,"staked":0,"dmax":1200000000,"acc_reward_per_share":"1000000000000000","last_update_block":30,"only":true,"deploy_block":10,"deploy_block_time":1687245485}"#;
      println!(
        "expect_poolinfo:{}",
        serde_json::to_string(&pool_info).unwrap()
      );
      println!(
        "expect_stakeinfo:{}",
        serde_json::to_string(&stakeinfo).unwrap()
      );
      println!(
        "expect_userinfo:{}",
        serde_json::to_string(&userinfo).unwrap()
      );

      assert_eq!(expect_poolinfo, serde_json::to_string(&pool_info).unwrap());
      assert_eq!(expect_stakeinfo, serde_json::to_string(&stakeinfo).unwrap());
      assert_eq!(expect_userinfo, serde_json::to_string(&userinfo).unwrap());
    }
  }

  #[test]
  fn test_process_passive_error() {
    let dbfile = NamedTempFile::new().unwrap();
    let db = Database::create(dbfile.path()).unwrap();
    let wtx = db.begin_write().unwrap();
    let brc20_data_store = brc20_db::DataStore::new(&wtx);
    let brc20s_data_store = brc20s_db::DataStore::new(&wtx);

    let deploy = Deploy {
      pool_type: "pool".to_string(),
      pool_id: "fea607ea9e#1f".to_string(),
      stake: "orea".to_string(),
      earn: "ordi".to_string(),
      earn_rate: "1000".to_string(),
      distribution_max: "12000000".to_string(),
      decimals: Some("2".to_string()),
      total_supply: Some("21000000".to_string()),
      only: Some("1".to_string()),
    };
    let addr1 =
      Address::from_str("bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e").unwrap();
    let script = ScriptKey::from_address(addr1.assume_checked());
    let inscription_id =
      InscriptionId::from_str("1111111111111111111111111111111111111111111111111111111111111111i1")
        .unwrap();

    let token = brc20::Tick::from_str("orea".to_string().as_str()).unwrap();
    let token_info = TokenInfo {
      tick: token.clone(),
      inscription_id,
      inscription_number: 0,
      supply: 21000000000_u128,
      minted: 2000000000_u128,
      limit_per_mint: 0,
      decimal: 3,
      deploy_by: script.clone(),
      deployed_number: 0,
      deployed_timestamp: 0,
      latest_mint_number: 0,
    };
    brc20_data_store.insert_token_info(&token, &token_info);
    let balance = BRC20Balance {
      tick: token.clone(),
      overall_balance: 2000000000_u128,
      transferable_balance: 1000000000_u128,
    };
    let result = brc20_data_store.update_token_balance(&script, balance);
    if let Err(error) = result {
      panic!("update_token_balance err: {}", error)
    }

    let msg = mock_create_brc20s_message(
      script.clone(),
      script.clone(),
      Operation::Deploy(deploy.clone()),
    );
    let context = BlockContext {
      blockheight: 0,
      blocktime: 1687245485,
      network: Network::Bitcoin,
    };
    let config = version::get_config_by_network(context.network, context.blockheight);
    let result = process_deploy(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      deploy.clone(),
    );

    let result: Result<Vec<Event>, BRC20SError> = match result {
      Ok(event) => Ok(event),
      Err(Error::BRC20SError(e)) => Err(e),
      Err(e) => Err(BRC20SError::InternalError(e.to_string())),
    };

    match result {
      Ok(event) => {
        println!("success:{}", serde_json::to_string_pretty(&event).unwrap());
      }
      Err(e) => {
        assert_eq!("error", e.to_string())
      }
    }
    let tick_id = deploy.get_tick_id();
    let pid = deploy.get_pool_id();
    let tick_info = brc20s_data_store.get_tick_info(&tick_id).unwrap().unwrap();
    let pool_info = brc20s_data_store
      .get_pid_to_poolinfo(&pid)
      .unwrap()
      .unwrap();

    let expect_tick_info = r#"{"tick_id":"fea607ea9e","name":"ordi","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","allocated":1200000000,"decimal":2,"circulation":0,"supply":2100000000,"deployer":{"Address":"bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e"},"deploy_block":0,"deploy_block_time":1687245485,"latest_mint_block":0,"pids":["fea607ea9e#1f"]}"#;
    let expect_pool_info = r#"{"pid":"fea607ea9e#1f","ptype":"Pool","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","stake":{"BRC20Tick":"orea"},"erate":100000,"minted":0,"staked":0,"dmax":1200000000,"acc_reward_per_share":"0","last_update_block":0,"only":true,"deploy_block":0,"deploy_block_time":1687245485}"#;
    assert_eq!(expect_pool_info, serde_json::to_string(&pool_info).unwrap());
    assert_eq!(expect_tick_info, serde_json::to_string(&tick_info).unwrap());

    let stake_tick = PledgedTick::BRC20Tick(token.clone());
    let stake_msg = Stake {
      pool_id: pid.as_str().to_string(),
      amount: "1000000".to_string(),
    };

    let msg = mock_create_brc20s_message(
      script.clone(),
      script.clone(),
      Operation::Stake(stake_msg.clone()),
    );
    let context = BlockContext {
      blockheight: 0,
      blocktime: 1687245485,
      network: Network::Bitcoin,
    };
    let config = version::get_config_by_network(context.network, context.blockheight);
    let result = process_stake(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      stake_msg,
    );

    let result: Result<Event, BRC20SError> = match result {
      Ok(event) => Ok(event),
      Err(Error::BRC20SError(e)) => Err(e),
      Err(e) => Err(BRC20SError::InternalError(e.to_string())),
    };

    match result {
      Ok(event) => {
        println!("success:{}", serde_json::to_string_pretty(&event).unwrap());
      }
      Err(e) => {
        assert_eq!("error", e.to_string())
      }
    }
    let stakeinfo = brc20s_data_store
      .get_user_stakeinfo(&script, &stake_tick)
      .unwrap();

    let userinfo = brc20s_data_store
      .get_pid_to_use_info(&script, &pid)
      .unwrap();
    let pool_info = brc20s_data_store.get_pid_to_poolinfo(&pid).unwrap();
    let expect_stakeinfo = r#"{"stake":{"BRC20Tick":"orea"},"pool_stakes":[["fea607ea9e#1f",true,1000000000]],"max_share":0,"total_only":1000000000}"#;
    let expect_userinfo = r#"{"pid":"fea607ea9e#1f","staked":1000000000,"minted":0,"pending_reward":0,"reward_debt":0,"latest_updated_block":0}"#;
    let expect_poolinfo = r#"{"pid":"fea607ea9e#1f","ptype":"Pool","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","stake":{"BRC20Tick":"orea"},"erate":100000,"minted":0,"staked":1000000000,"dmax":1200000000,"acc_reward_per_share":"0","last_update_block":0,"only":true,"deploy_block":0,"deploy_block_time":1687245485}"#;

    assert_eq!(expect_poolinfo, serde_json::to_string(&pool_info).unwrap());
    assert_eq!(expect_stakeinfo, serde_json::to_string(&stakeinfo).unwrap());
    assert_eq!(expect_userinfo, serde_json::to_string(&userinfo).unwrap());
    {
      let stake_tick = PledgedTick::BRC20Tick(token.clone());
      let passive_unstake_msg = PassiveUnStake {
        stake: stake_tick.to_string(),
        amount: "2000000".to_string(),
      };
      let mut msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::PassiveUnStake(passive_unstake_msg.clone()),
      );
      let context = BlockContext {
        blockheight: 1,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };

      //mock brc20 transfer
      let balance = BRC20Balance {
        tick: token.clone(),
        overall_balance: 0_u128,
        transferable_balance: 0_u128,
      };
      let result = brc20_data_store.update_token_balance(&script, balance);
      if let Err(error) = result {
        panic!("update_token_balance err: {}", error)
      }
      let result = process_passive_unstake(
        context,
        config.clone(),
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        passive_unstake_msg,
      );
    }

    // validate_basics
    {
      let stake_tick = PledgedTick::BRC20Tick(token.clone());
      let mut passive_unstake_msg = PassiveUnStake {
        stake: stake_tick.to_string(),
        amount: "a".to_string(),
      };
      let mut msg = mock_create_brc20s_message(
        script.clone(),
        script.clone(),
        Operation::PassiveUnStake(passive_unstake_msg.clone()),
      );
      let context = BlockContext {
        blockheight: 1,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };

      //mock brc20 transfer
      let balance = BRC20Balance {
        tick: token.clone(),
        overall_balance: 0_u128,
        transferable_balance: 0_u128,
      };
      let result = brc20_data_store.update_token_balance(&script, balance);
      if let Err(error) = result {
        panic!("update_token_balance err: {}", error)
      }
      let result = process_passive_unstake(
        context,
        config.clone(),
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        passive_unstake_msg,
      );

      let result: Result<Vec<Event>, BRC20SError> = match result {
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
        _ => panic!(),
      };

      assert_eq!(
        Err(BRC20SError::InvalidNum("ainvalid number: a".to_string())),
        result
      );
    }

    // no stake
    {
      let stake_tick = PledgedTick::BRC20Tick(token.clone());
      let mut passive_unstake_msg = PassiveUnStake {
        stake: stake_tick.to_string(),
        amount: "1".to_string(),
      };

      let addr1 = Address::from_str("bc1q9x30z7rz52c97jwc2j79w76y7l3ny54nlvd4ew").unwrap();
      let script1 = ScriptKey::from_address(addr1.assume_checked());

      let mut msg = mock_create_brc20s_message(
        script1.clone(),
        script1.clone(),
        Operation::PassiveUnStake(passive_unstake_msg.clone()),
      );
      let context = BlockContext {
        blockheight: 1,
        blocktime: 1687245485,
        network: Network::Bitcoin,
      };

      //mock brc20 transfer
      let balance = BRC20Balance {
        tick: token,
        overall_balance: 0_u128,
        transferable_balance: 0_u128,
      };
      let result = brc20_data_store.update_token_balance(&script1, balance);
      if let Err(error) = result {
        panic!("update_token_balance err: {}", error)
      }
      let result = process_passive_unstake(
        context,
        config.clone(),
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        passive_unstake_msg,
      );

      let result: Result<Vec<Event>, BRC20SError> = match result {
        Err(Error::BRC20SError(e)) => Err(e),
        Err(e) => Err(BRC20SError::InternalError(e.to_string())),
        _ => panic!(),
      };

      assert_eq!(Err(BRC20SError::StakeNotFound("orea".to_string())), result);
    }
  }

  #[test]
  fn test_process_deploy_most() {
    let dbfile = NamedTempFile::new().unwrap();
    let db = Database::create(dbfile.path()).unwrap();
    let wtx = db.begin_write().unwrap();

    let brc20_data_store = brc20_db::DataStore::new(&wtx);
    let brc20s_data_store = brc20s_db::DataStore::new(&wtx);

    let addr = "bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e";
    let (deploy, msg) = mock_deploy_msg(
      "pool", "01", "btc1", "ordi1", "10", "12000000", "21000000", 18, true, addr, addr,
    );
    let result = set_brc20_token_user(&brc20_data_store, "btc1", &msg.from, 200_u128, 18_u8).err();
    assert_eq!(None, result);

    let result = execute_for_test(
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      0,
      version::zebra(),
    );
    assert_eq!(None, result.err());
    let tick_id = deploy.get_tick_id();
    let tikc_id_str = tick_id.hex();
    let tick_info = brc20s_data_store.get_tick_info(&tick_id).unwrap().unwrap();
    let pool_info = brc20s_data_store
      .get_pid_to_poolinfo(&deploy.get_pool_id())
      .unwrap()
      .unwrap();

    let expect_tick_info = r#"{"tick_id":"13395c5283","name":"ordi1","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","allocated":12000000000000000000000000,"decimal":18,"circulation":0,"supply":21000000000000000000000000,"deployer":{"Address":"bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e"},"deploy_block":0,"deploy_block_time":1687245485,"latest_mint_block":0,"pids":["13395c5283#01"]}"#;
    let expect_pool_info = r#"{"pid":"13395c5283#01","ptype":"Pool","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","stake":{"BRC20Tick":"btc1"},"erate":10000000000000000000,"minted":0,"staked":0,"dmax":12000000000000000000000000,"acc_reward_per_share":"0","last_update_block":0,"only":true,"deploy_block":0,"deploy_block_time":1687245485}"#;
    assert_eq!(expect_pool_info, serde_json::to_string(&pool_info).unwrap());
    assert_eq!(expect_tick_info, serde_json::to_string(&tick_info).unwrap());

    {
      let result = execute_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        0,
        version::zebra(),
      );
      assert_eq!(Err(BRC20SError::PoolAlreadyExist(deploy.pool_id)), result);

      //brc20s stake can not deploy
      let brc20s_tick = tikc_id_str.as_str();
      let (deploy, msg) = mock_deploy_msg(
        "pool",
        "02",
        brc20s_tick,
        "ordi2",
        "10",
        "12000000",
        "21000000",
        18,
        true,
        addr,
        addr,
      );
      let result = execute_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        0,
        version::zebra(),
      );
      assert_eq!(Err(BRC20SError::StakeNoPermission(tikc_id_str)), result);

      //btc stake can deploy
      let (_, msg) = mock_deploy_msg(
        "pool",
        "02",
        "btc",
        "ordi1",
        "10",
        "12000000",
        "2100000000000000",
        18,
        true,
        addr,
        addr,
      );
      let mut config = version::zebra();
      config.allow_btc_staking = true;
      let result = execute_for_test(&brc20_data_store, &brc20s_data_store, &msg, 0, config);
      assert_eq!(None, result.err());
    }

    let result = set_brc20_token_user(&brc20_data_store, "orea", &msg.from, 200_u128, 18_u8).err();
    assert_eq!(None, result);

    {
      //from is not equal to to
      let new_addr = "bc1pvk535u5eedhsx75r7mfvdru7t0kcr36mf9wuku7k68stc0ncss8qwzeahv";
      let (mut deploy, mut msg) = mock_deploy_msg(
        "pool", "02", "orea", "ordi1", "0.1", "9000000", "21000000", 18, true, new_addr, addr,
      );
      deploy.pool_id = tick_id.hex() + "#02";
      msg.op = Operation::Deploy(deploy);
      let result = execute_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        0,
        version::zebra(),
      );
      assert_eq!(
        Err(BRC20SError::FromToNotEqual(
          new_addr.to_string(),
          addr.to_string()
        )),
        result
      );

      //address is not equal to deployer
      let new_addr = "bc1pvk535u5eedhsx75r7mfvdru7t0kcr36mf9wuku7k68stc0ncss8qwzeahv";
      let (mut deploy, mut msg) = mock_deploy_msg(
        "pool", "02", "orea", "ordi1", "0.1", "9000000", "21000000", 18, true, new_addr, new_addr,
      );
      deploy.pool_id = tick_id.hex() + "#02";
      let pool_id = deploy.pool_id.clone();
      msg.op = Operation::Deploy(deploy);
      let result = execute_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        0,
        version::zebra(),
      );
      assert_eq!(
        Err(BRC20SError::DeployerNotEqual(
          pool_id,
          addr.to_string(),
          new_addr.to_string()
        )),
        result
      );
    }
    let (deploy, msg) = mock_deploy_msg(
      "pool", "02", "orea", "ordi1", "0.1", "9000000", "21000000", 18, true, addr, addr,
    );
    let result = execute_for_test(
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      0,
      version::zebra(),
    );
    assert_eq!(None, result.err());

    let tick_id = deploy.get_tick_id();
    let pid = deploy.get_pool_id();
    let tick_info = brc20s_data_store.get_tick_info(&tick_id).unwrap().unwrap();
    let pool_info = brc20s_data_store
      .get_pid_to_poolinfo(&pid)
      .unwrap()
      .unwrap();

    let expect_tick_info = r#"{"tick_id":"13395c5283","name":"ordi1","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","allocated":21000000000000000000000000,"decimal":18,"circulation":0,"supply":21000000000000000000000000,"deployer":{"Address":"bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e"},"deploy_block":0,"deploy_block_time":1687245485,"latest_mint_block":0,"pids":["13395c5283#01","13395c5283#02"]}"#;
    let expect_pool_info = r#"{"pid":"13395c5283#02","ptype":"Pool","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","stake":{"BRC20Tick":"orea"},"erate":100000000000000000,"minted":0,"staked":0,"dmax":9000000000000000000000000,"acc_reward_per_share":"0","last_update_block":0,"only":true,"deploy_block":0,"deploy_block_time":1687245485}"#;
    assert_eq!(expect_pool_info, serde_json::to_string(&pool_info).unwrap());
    assert_eq!(expect_tick_info, serde_json::to_string(&tick_info).unwrap());
  }

  #[test]
  fn test_mint() {
    let dbfile = NamedTempFile::new().unwrap();
    let db = Database::create(dbfile.path()).unwrap();
    let wtx = db.begin_write().unwrap();
    let brc20_data_store = brc20_db::DataStore::new(&wtx);
    let brc20s_data_store = brc20s_db::DataStore::new(&wtx);

    // deploy brc20
    let script = ScriptKey::from_address(
      Address::from_str("bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e")
        .unwrap()
        .assume_checked(),
    );
    let inscription_id =
      InscriptionId::from_str("1111111111111111111111111111111111111111111111111111111111111111i1")
        .unwrap();

    let token = brc20::Tick::from_str("orea".to_string().as_str()).unwrap();
    let token_info = TokenInfo {
      tick: token.clone(),
      inscription_id,
      inscription_number: 0,
      supply: 21000000000_u128,
      minted: 2000000000_u128,
      limit_per_mint: 0,
      decimal: 3,
      deploy_by: script.clone(),
      deployed_number: 0,
      deployed_timestamp: 0,
      latest_mint_number: 0,
    };
    let _ = brc20_data_store.insert_token_info(&token, &token_info);
    let balance = BRC20Balance {
      tick: token.clone(),
      overall_balance: 2000000000_u128,
      transferable_balance: 0_u128,
    };
    let _ = brc20_data_store.update_token_balance(&script, balance);

    // deploy brc20-s
    let deploy = Deploy {
      pool_type: "pool".to_string(),
      pool_id: "fea607ea9e#1f".to_string(),
      stake: "orea".to_string(),
      earn: "ordi".to_string(),
      earn_rate: "1000".to_string(),
      distribution_max: "12000000".to_string(),
      decimals: Some("2".to_string()),
      total_supply: Some("21000000".to_string()),
      only: Some("1".to_string()),
    };
    let msg = mock_create_brc20s_message(
      script.clone(),
      script.clone(),
      Operation::Deploy(deploy.clone()),
    );

    let context = BlockContext {
      blockheight: 10,
      blocktime: 1687245485,
      network: Network::Bitcoin,
    };
    let config = version::get_config_by_network(context.network, context.blockheight);
    let result = process_deploy(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      deploy.clone(),
    );

    let result: Result<Vec<Event>, BRC20SError> = match result {
      Ok(event) => Ok(event),
      Err(Error::BRC20SError(e)) => Err(e),
      Err(e) => Err(BRC20SError::InternalError(e.to_string())),
    };

    match result {
      Ok(event) => {
        println!("success:{}", serde_json::to_string_pretty(&event).unwrap());
      }
      Err(e) => {
        assert_eq!("error", e.to_string())
      }
    }
    let pid = deploy.get_pool_id();

    // brc20-s stake
    let stake_tick = PledgedTick::BRC20Tick(token);
    let stake_msg = Stake {
      pool_id: pid.as_str().to_string(),
      amount: "1000000".to_string(),
    };

    let msg = mock_create_brc20s_message(
      script.clone(),
      script.clone(),
      Operation::Stake(stake_msg.clone()),
    );
    let context = BlockContext {
      blockheight: 20,
      blocktime: 1687245485,
      network: Network::Bitcoin,
    };
    let config = version::get_config_by_network(context.network, context.blockheight);
    let result = process_stake(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      stake_msg,
    );

    let result: Result<Event, BRC20SError> = match result {
      Ok(event) => Ok(event),
      Err(Error::BRC20SError(e)) => Err(e),
      Err(e) => Err(BRC20SError::InternalError(e.to_string())),
    };

    match result {
      Ok(event) => {
        println!("success:{}", serde_json::to_string_pretty(&event).unwrap());
      }
      Err(e) => {
        assert_eq!("error", e.to_string())
      }
    }
    let stake_info = brc20s_data_store
      .get_user_stakeinfo(&script, &stake_tick)
      .unwrap();

    let userinfo = brc20s_data_store
      .get_pid_to_use_info(&script, &pid)
      .unwrap();
    let pool_info = brc20s_data_store.get_pid_to_poolinfo(&pid).unwrap();
    let expect_stake_info = r#"{"stake":{"BRC20Tick":"orea"},"pool_stakes":[["fea607ea9e#1f",true,1000000000]],"max_share":0,"total_only":1000000000}"#;
    let expect_userinfo = r#"{"pid":"fea607ea9e#1f","staked":1000000000,"minted":0,"pending_reward":0,"reward_debt":0,"latest_updated_block":20}"#;
    let expect_pool_info = r#"{"pid":"fea607ea9e#1f","ptype":"Pool","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","stake":{"BRC20Tick":"orea"},"erate":100000,"minted":0,"staked":1000000000,"dmax":1200000000,"acc_reward_per_share":"0","last_update_block":20,"only":true,"deploy_block":10,"deploy_block_time":1687245485}"#;

    assert_eq!(expect_pool_info, serde_json::to_string(&pool_info).unwrap());
    assert_eq!(
      expect_stake_info,
      serde_json::to_string(&stake_info).unwrap()
    );
    assert_eq!(expect_userinfo, serde_json::to_string(&userinfo).unwrap());

    // brc20-s mint
    let mint_msg = Mint {
      tick: "ordi".to_string(),
      pool_id: pid.as_str().to_string(),
      amount: "1.1".to_string(),
    };

    let msg = mock_create_brc20s_message(
      script.clone(),
      script.clone(),
      Operation::Mint(mint_msg.clone()),
    );
    let context = BlockContext {
      blockheight: 30,
      blocktime: 1687245485,
      network: Network::Bitcoin,
    };

    // call control, commit_from != to
    let mut error_msg = msg.clone();
    error_msg.to = Some(ScriptKey::from_address(
      Address::from_str("bc1q9cv6smq87myk2ujs352c3lulwzvdfujd5059ny")
        .unwrap()
        .assume_checked(),
    ));
    match process_mint(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &error_msg,
      mint_msg.clone(),
    ) {
      Err(Error::BRC20SError(e)) => {
        assert_eq!("from bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e must equal to to bc1q9cv6smq87myk2ujs352c3lulwzvdfujd5059ny", e.to_string())
      }
      _ => {
        panic!("")
      }
    };

    // invalid inscribe to coinbase
    let mut error_msg = msg.clone();
    error_msg.to = None;
    match process_mint(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &error_msg,
      mint_msg.clone(),
    ) {
      Err(Error::BRC20SError(e)) => {
        assert_eq!("invalid inscribe to coinbase", e.to_string())
      }
      _ => {
        panic!("")
      }
    };

    // msg's commit_from is nil
    let mut error_msg = msg.clone();
    error_msg.commit_from = None;
    match process_mint(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &error_msg,
      mint_msg.clone(),
    ) {
      Err(Error::BRC20SError(e)) => {
        assert_eq!("internal error: ", e.to_string())
      }
      _ => {
        panic!("")
      }
    };

    // brc20-s mint, mint too large
    let mut error_mint_msg = mint_msg.clone();
    error_mint_msg.amount = "12000000.01".to_string();
    match process_mint(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      error_mint_msg,
    ) {
      Ok(event) => {
        println!("success:{}", serde_json::to_string_pretty(&event).unwrap());
      }
      Err(Error::BRC20SError(e)) => {
        assert_eq!("amount exceed limit: 1200000001", e.to_string())
      }
      _ => {
        panic!("")
      }
    };

    // brc20-s mint, mint overflow
    let mut error_mint_msg = mint_msg.clone();
    error_mint_msg.amount = "11.0111111111".to_string();
    match process_mint(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      error_mint_msg,
    ) {
      Ok(event) => {
        println!("success:{}", serde_json::to_string_pretty(&event).unwrap());
      }
      Err(Error::BRC20SError(e)) => {
        assert_eq!("amount overflow: 11.0111111111", e.to_string())
      }
      _ => {
        panic!("")
      }
    };

    // brc20-s mint, mint tick name diff
    let mut error_mint_msg = mint_msg.clone();
    error_mint_msg.tick = "orda".to_string();
    match process_mint(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      error_mint_msg,
    ) {
      Ok(event) => {
        println!("success:{}", serde_json::to_string_pretty(&event).unwrap());
      }
      Err(Error::BRC20SError(e)) => {
        assert_eq!("tick name orda is not match", e.to_string())
      }
      _ => {
        panic!("")
      }
    };

    // brc20-s mint, pid no exsit
    let mut error_mint_msg = mint_msg.clone();
    error_mint_msg.pool_id = "fea607ea9e#11".to_string();
    match process_mint(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      error_mint_msg,
    ) {
      Ok(event) => {
        println!("success:{}", serde_json::to_string_pretty(&event).unwrap());
      }
      Err(Error::BRC20SError(e)) => {
        assert_eq!("pool fea607ea9e#11 is not exist", e.to_string())
      }
      _ => {
        panic!("")
      }
    };

    // brc20-s mint, amount -1
    let mut error_mint_msg = mint_msg.clone();
    error_mint_msg.amount = "-1".to_string();
    match process_mint(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      error_mint_msg,
    ) {
      Ok(event) => {
        println!("success:{}", serde_json::to_string_pretty(&event).unwrap());
      }
      Err(Error::BRC20SError(e)) => {
        assert_eq!("invalid number: -1", e.to_string())
      }
      _ => {
        panic!("")
      }
    };

    // brc20-s mint, amount 0
    let mut error_mint_msg = mint_msg.clone();
    error_mint_msg.amount = "0".to_string();
    match process_mint(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      error_mint_msg,
    ) {
      Ok(event) => {
        println!("success:{}", serde_json::to_string_pretty(&event).unwrap());
      }
      Err(Error::BRC20SError(e)) => {
        assert_eq!("invalid number: 0", e.to_string())
      }
      _ => {
        panic!("")
      }
    };

    // brc20-s mint, ok
    match process_mint(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      mint_msg.clone(),
    ) {
      Ok(event) => {
        let userinfo = brc20s_data_store
          .get_pid_to_use_info(&script, &pid)
          .unwrap()
          .unwrap();
        println!("{}", userinfo);
        println!("success:{}", serde_json::to_string_pretty(&event).unwrap());
        assert_eq!(userinfo.minted, 110);
        assert_eq!(userinfo.pending_reward, 999890);
      }
      Err(Error::BRC20SError(e)) => {
        assert_eq!("pool fea607ea9e#11 is not exist", e.to_string())
      }
      _ => {
        panic!("")
      }
    };

    // brc20-s mint, ok
    match process_mint(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      mint_msg,
    ) {
      Ok(event) => {
        let userinfo = brc20s_data_store
          .get_pid_to_use_info(&script, &pid)
          .unwrap()
          .unwrap();
        println!("{}", userinfo);
        println!("success:{}", serde_json::to_string_pretty(&event).unwrap());
        assert_eq!(userinfo.minted, 220);
        assert_eq!(userinfo.pending_reward, 999780);
      }
      Err(Error::BRC20SError(e)) => {
        assert_eq!("pool fea607ea9e#11 is not exist", e.to_string())
      }
      _ => {
        panic!("")
      }
    };
  }

  #[test]
  fn test_transfer() {
    let db_file = NamedTempFile::new().unwrap();
    let db = Database::create(db_file.path()).unwrap();
    let wtx = db.begin_write().unwrap();
    let brc20_data_store = brc20_db::DataStore::new(&wtx);
    let brc20s_data_store = brc20s_db::DataStore::new(&wtx);

    // deploy brc20
    let script = ScriptKey::from_address(
      Address::from_str("bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e")
        .unwrap()
        .assume_checked(),
    );

    let script1 = ScriptKey::from_address(
      Address::from_str("bc1q9cv6smq87myk2ujs352c3lulwzvdfujd5059ny")
        .unwrap()
        .assume_checked(),
    );

    let inscription_id =
      InscriptionId::from_str("1111111111111111111111111111111111111111111111111111111111111111i1")
        .unwrap();

    let token = brc20::Tick::from_str("orea".to_string().as_str()).unwrap();
    let token_info = TokenInfo {
      tick: token.clone(),
      inscription_id,
      inscription_number: 0,
      supply: 21000000000_u128,
      minted: 2000000000_u128,
      limit_per_mint: 0,
      decimal: 3,
      deploy_by: script.clone(),
      deployed_number: 0,
      deployed_timestamp: 0,
      latest_mint_number: 0,
    };
    let _ = brc20_data_store.insert_token_info(&token, &token_info);
    let balance = BRC20Balance {
      tick: token.clone(),
      overall_balance: 2000000000_u128,
      transferable_balance: 0_u128,
    };
    let _ = brc20_data_store.update_token_balance(&script, balance.clone());
    let _ = brc20_data_store.update_token_balance(&script1, balance);

    // deploy brc20-s
    let deploy = Deploy {
      pool_type: "pool".to_string(),
      pool_id: "fea607ea9e#1f".to_string(),
      stake: "orea".to_string(),
      earn: "ordi".to_string(),
      earn_rate: "1000".to_string(),
      distribution_max: "12000000".to_string(),
      decimals: Some("2".to_string()),
      total_supply: Some("21000000".to_string()),
      only: Some("1".to_string()),
    };
    let msg = mock_create_brc20s_message(
      script.clone(),
      script.clone(),
      Operation::Deploy(deploy.clone()),
    );

    let context = BlockContext {
      blockheight: 10,
      blocktime: 1687245485,
      network: Network::Bitcoin,
    };
    let config = version::get_config_by_network(context.network, context.blockheight);
    let result = process_deploy(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      deploy.clone(),
    );

    let result: Result<Vec<Event>, BRC20SError> = match result {
      Ok(event) => Ok(event),
      Err(Error::BRC20SError(e)) => Err(e),
      Err(e) => Err(BRC20SError::InternalError(e.to_string())),
    };

    match result {
      Ok(event) => {
        println!("success:{}", serde_json::to_string_pretty(&event).unwrap());
      }
      Err(e) => {
        assert_eq!("error", e.to_string())
      }
    }
    let tick_id = deploy.get_tick_id();
    let pid = deploy.get_pool_id();

    // brc20-s stake
    let stake_tick = PledgedTick::BRC20Tick(token);
    let stake_msg = Stake {
      pool_id: pid.as_str().to_string(),
      amount: "1000000".to_string(),
    };

    let msg = mock_create_brc20s_message(
      script.clone(),
      script.clone(),
      Operation::Stake(stake_msg.clone()),
    );

    let msg1 = mock_create_brc20s_message(
      script1.clone(),
      script1.clone(),
      Operation::Stake(stake_msg.clone()),
    );

    let context = BlockContext {
      blockheight: 20,
      blocktime: 1687245485,
      network: Network::Bitcoin,
    };
    let config = version::get_config_by_network(context.network, context.blockheight);
    let result = process_stake(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      stake_msg.clone(),
    );

    let result = process_stake(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &msg1,
      stake_msg,
    );

    let result: Result<Event, BRC20SError> = match result {
      Ok(event) => Ok(event),
      Err(Error::BRC20SError(e)) => Err(e),
      Err(e) => Err(BRC20SError::InternalError(e.to_string())),
    };

    match result {
      Ok(event) => {
        println!("success:{}", serde_json::to_string_pretty(&event).unwrap());
      }
      Err(e) => {
        assert_eq!("error", e.to_string())
      }
    }
    let stake_info = brc20s_data_store
      .get_user_stakeinfo(&script, &stake_tick)
      .unwrap();

    let userinfo = brc20s_data_store
      .get_pid_to_use_info(&script, &pid)
      .unwrap();
    let pool_info = brc20s_data_store.get_pid_to_poolinfo(&pid).unwrap();
    let expect_stake_info = r#"{"stake":{"BRC20Tick":"orea"},"pool_stakes":[["fea607ea9e#1f",true,1000000000]],"max_share":0,"total_only":1000000000}"#;
    let expect_user_info = r#"{"pid":"fea607ea9e#1f","staked":1000000000,"minted":0,"pending_reward":0,"reward_debt":0,"latest_updated_block":20}"#;
    let expect_pool_info = r#"{"pid":"fea607ea9e#1f","ptype":"Pool","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","stake":{"BRC20Tick":"orea"},"erate":100000,"minted":0,"staked":2000000000,"dmax":1200000000,"acc_reward_per_share":"0","last_update_block":20,"only":true,"deploy_block":10,"deploy_block_time":1687245485}"#;

    assert_eq!(expect_pool_info, serde_json::to_string(&pool_info).unwrap());
    assert_eq!(
      expect_stake_info,
      serde_json::to_string(&stake_info).unwrap()
    );
    assert_eq!(expect_user_info, serde_json::to_string(&userinfo).unwrap());

    // brc20-s mint
    let mint_msg = Mint {
      tick: "ordi".to_string(),
      pool_id: pid.as_str().to_string(),
      amount: "10.1".to_string(),
    };

    let msg = mock_create_brc20s_message(
      script.clone(),
      script.clone(),
      Operation::Mint(mint_msg.clone()),
    );
    let msg1 = mock_create_brc20s_message(
      script1.clone(),
      script1.clone(),
      Operation::Mint(mint_msg.clone()),
    );
    let context = BlockContext {
      blockheight: 100000,
      blocktime: 1687245485,
      network: Network::Bitcoin,
    };

    // mint ok
    match process_mint(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      mint_msg.clone(),
    ) {
      Ok(event) => {
        let userinfo = brc20s_data_store
          .get_pid_to_use_info(&script, &pid)
          .unwrap()
          .unwrap();
        println!("{}", userinfo);
        println!("success:{}", serde_json::to_string_pretty(&event).unwrap());
        assert_eq!(userinfo.minted, 1010);
        assert_eq!(userinfo.pending_reward, 599998990);
      }
      Err(Error::BRC20SError(e)) => {
        assert_eq!("pool fea607ea9e#11 is not exist", e.to_string())
      }
      _ => {
        panic!("")
      }
    };

    // mint script1 ok
    match process_mint(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &msg1,
      mint_msg,
    ) {
      Ok(event) => {
        let userinfo = brc20s_data_store
          .get_pid_to_use_info(&script1, &pid)
          .unwrap()
          .unwrap();
        println!("{}", userinfo);
        println!("success:{}", serde_json::to_string_pretty(&event).unwrap());
        assert_eq!(userinfo.minted, 1010);
        assert_eq!(userinfo.pending_reward, 599998990);
      }
      Err(Error::BRC20SError(e)) => {
        assert_eq!("pool fea607ea9e#11 is not exist", e.to_string())
      }
      _ => {
        panic!("")
      }
    };

    // brc20s-inscribe-transfer
    let transfer_msg = Transfer {
      tick: "ordi".to_string(),
      tick_id: tick_id.clone().hex(),
      amount: "1.1".to_string(),
    };
    let msg = mock_create_brc20s_message(
      script.clone(),
      script.clone(),
      Operation::InscribeTransfer(transfer_msg.clone()),
    );
    let context = BlockContext {
      blockheight: 200000,
      blocktime: 1687245485,
      network: Network::Bitcoin,
    };
    let msg1 = mock_create_brc20s_message(
      script1.clone(),
      script1.clone(),
      Operation::InscribeTransfer(transfer_msg.clone()),
    );
    let context = BlockContext {
      blockheight: 200000,
      blocktime: 1687245485,
      network: Network::Bitcoin,
    };

    // brc20s-inscribe-transfer, invalid inscribe to coinbase
    let mut error_msg = msg.clone();
    error_msg.to = None;
    match process_inscribe_transfer(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &error_msg,
      transfer_msg.clone(),
    ) {
      Err(Error::BRC20SError(e)) => {
        assert_eq!("invalid inscribe to coinbase", e.to_string())
      }
      _ => {
        panic!("")
      }
    };

    // brc20s-inscribe-transfer, overflow
    let mut error_transfer_msg = transfer_msg.clone();
    error_transfer_msg.amount = "11.0111111111".to_string();
    match process_inscribe_transfer(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      error_transfer_msg,
    ) {
      Err(Error::BRC20SError(e)) => {
        assert_eq!("amount overflow: 11.0111111111", e.to_string())
      }
      _ => {
        panic!("")
      }
    };

    // brc20s-inscribe-transfer, tick name diff
    let mut error_transfer_msg = transfer_msg.clone();
    error_transfer_msg.tick = "orda".to_string();
    match process_inscribe_transfer(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      error_transfer_msg,
    ) {
      Err(Error::BRC20SError(e)) => {
        assert_eq!("tick name orda is not match", e.to_string())
      }
      _ => {
        panic!("")
      }
    };

    // brc20s-inscribe-transfer, balance not enough
    let mut error_transfer_msg = transfer_msg.clone();
    error_transfer_msg.amount = "10.2".to_string();
    match process_inscribe_transfer(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      error_transfer_msg,
    ) {
      Err(Error::BRC20SError(e)) => {
        assert_eq!("insufficient balance: 1010 1020", e.to_string())
      }
      _ => {
        panic!("")
      }
    };

    // brc20s-inscribe-transfer, ok
    match process_inscribe_transfer(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      transfer_msg.clone(),
    ) {
      Ok(event) => {
        let balance = brc20s_data_store
          .get_balance(&script, &tick_id)
          .unwrap()
          .unwrap();
        println!("{:?}", balance);
        println!("success:{}", serde_json::to_string_pretty(&event).unwrap());
        assert_eq!(balance.transferable_balance, 110);
        assert_eq!(balance.overall_balance, 1010);
      }
      Err(Error::BRC20SError(e)) => {
        assert_eq!("pool fea607ea9e#11 is not exist", e.to_string())
      }
      _ => {
        panic!("")
      }
    };

    // brc20s-inscribe-transfer, ok
    match process_inscribe_transfer(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &msg1,
      transfer_msg.clone(),
    ) {
      Ok(event) => {
        let balance = brc20s_data_store
          .get_balance(&script1, &tick_id)
          .unwrap()
          .unwrap();
        println!("{:?}", balance);
        println!("success:{}", serde_json::to_string_pretty(&event).unwrap());
        assert_eq!(balance.transferable_balance, 110);
        assert_eq!(balance.overall_balance, 1010);
      }
      Err(Error::BRC20SError(e)) => {
        assert_eq!("pool fea607ea9e#11 is not exist", e.to_string())
      }
      _ => {
        panic!("")
      }
    };

    // brc20s-transfer
    let msg = mock_create_brc20s_message(
      script.clone(),
      script.clone(),
      Operation::Transfer(transfer_msg.clone()),
    );
    let context = BlockContext {
      blockheight: 200000,
      blocktime: 1687245485,
      network: Network::Bitcoin,
    };

    // commit_from not self
    let mut error_msg = msg.clone();
    error_msg.from = ScriptKey::from_address(
      Address::from_str("bc1qzmh8f99f8ue8cy90a9xqflwtrhphg3sq76srhe")
        .unwrap()
        .assume_checked(),
    );
    match process_transfer(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &error_msg,
    ) {
      Err(Error::BRC20SError(e)) => {
        assert_eq!("transferable inscriptionId not found: 1111111111111111111111111111111111111111111111111111111111111111i1", e.to_string())
      }
      _ => {
        panic!("")
      }
    };

    // inscription_id not found
    let mut error_msg = msg.clone();
    error_msg.inscription_id =
      InscriptionId::from_str("2111111111111111111111111111111111111111111111111111111111111111i1")
        .unwrap();
    match process_transfer(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &error_msg,
    ) {
      Err(Error::BRC20SError(e)) => {
        assert_eq!("transferable inscriptionId not found: 2111111111111111111111111111111111111111111111111111111111111111i1", e.to_string())
      }
      _ => {
        panic!("")
      }
    };

    // inscribe to coinbase, ok
    let mut error_msg = msg.clone();
    error_msg.new_satpoint.outpoint.txid =
      Txid::from_str("2111111111111111111111111111111111111111111111111111111111111111").unwrap();
    match process_transfer(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &error_msg,
    ) {
      Ok(event) => {
        let balance = brc20s_data_store
          .get_balance(&script, &tick_id)
          .unwrap()
          .unwrap();
        println!("{:?}", balance);
        println!("success:{}", serde_json::to_string_pretty(&event).unwrap());
        assert_eq!(balance.transferable_balance, 0);
        assert_eq!(balance.overall_balance, 1010);
      }
      Err(Error::BRC20SError(e)) => {
        assert_eq!("invalid inscribe to coinbase", e.to_string())
      }
      _ => {
        panic!("")
      }
    };

    // inscribe to coinbase, second
    let mut error_msg = msg.clone();
    error_msg.new_satpoint.outpoint.txid =
      Txid::from_str("2111111111111111111111111111111111111111111111111111111111111111").unwrap();
    match process_transfer(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &error_msg,
    ) {
      Err(Error::BRC20SError(e)) => {
        assert_eq!("transferable inscriptionId not found: 1111111111111111111111111111111111111111111111111111111111111111i1", e.to_string())
      }
      _ => {
        panic!("")
      }
    };

    // brc20s-inscribe-transfer, second, ok
    match process_inscribe_transfer(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      transfer_msg.clone(),
    ) {
      Ok(event) => {
        let balance = brc20s_data_store
          .get_balance(&script, &tick_id)
          .unwrap()
          .unwrap();
        println!("{:?}", balance);
        println!("success:{}", serde_json::to_string_pretty(&event).unwrap());
        assert_eq!(balance.transferable_balance, 110);
        assert_eq!(balance.overall_balance, 1010);
      }
      Err(Error::BRC20SError(e)) => {
        assert_eq!("pool fea607ea9e#11 is not exist", e.to_string())
      }
      _ => {
        panic!("")
      }
    };

    // normal, ok
    match process_transfer(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
    ) {
      Ok(event) => {
        let balance = brc20s_data_store
          .get_balance(&script, &tick_id)
          .unwrap()
          .unwrap();
        println!("{:?}", balance);
        println!("success:{}", serde_json::to_string_pretty(&event).unwrap());
        assert_eq!(balance.transferable_balance, 0);
        assert_eq!(balance.overall_balance, 1010);
      }
      Err(Error::BRC20SError(e)) => {
        assert_eq!("invalid inscribe to coinbase", e.to_string())
      }
      _ => {
        panic!("")
      }
    };

    // normal, second
    match process_transfer(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
    ) {
      Err(Error::BRC20SError(e)) => {
        assert_eq!("transferable inscriptionId not found: 1111111111111111111111111111111111111111111111111111111111111111i1", e.to_string())
      }
      _ => {
        panic!("")
      }
    };

    // brc20s-inscribe-transfer, address to diff from, ok
    match process_inscribe_transfer(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      transfer_msg.clone(),
    ) {
      Ok(event) => {
        let balance = brc20s_data_store
          .get_balance(&script, &tick_id)
          .unwrap()
          .unwrap();
        println!("{:?}", balance);
        println!("success:{}", serde_json::to_string_pretty(&event).unwrap());
        assert_eq!(balance.transferable_balance, 110);
        assert_eq!(balance.overall_balance, 1010);

        let balance1 = brc20s_data_store
          .get_balance(&script1, &tick_id)
          .unwrap()
          .unwrap();
        println!("{:?}", balance1);
        assert_eq!(balance1.transferable_balance, 110);
        assert_eq!(balance1.overall_balance, 1010);
      }
      Err(Error::BRC20SError(e)) => {
        assert_eq!("pool fea607ea9e#11 is not exist", e.to_string())
      }
      _ => {
        panic!("")
      }
    };

    // normal, ok
    let mut error_msg = msg.clone();
    error_msg.to = Some(script1.clone());
    match process_transfer(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &error_msg,
    ) {
      Ok(event) => {
        let balance = brc20s_data_store
          .get_balance(&script, &tick_id)
          .unwrap()
          .unwrap();
        println!("{:?}", balance);
        println!("success:{}", serde_json::to_string_pretty(&event).unwrap());
        assert_eq!(balance.transferable_balance, 0);
        assert_eq!(balance.overall_balance, 900);

        let balance1 = brc20s_data_store
          .get_balance(&script1, &tick_id)
          .unwrap()
          .unwrap();
        println!("{:?}", balance1);
        assert_eq!(balance1.transferable_balance, 110);
        assert_eq!(balance1.overall_balance, 1120);
      }
      Err(Error::BRC20SError(e)) => {
        assert_eq!("invalid inscribe to coinbase", e.to_string())
      }
      _ => {
        panic!("")
      }
    };

    // normal, second
    match process_transfer(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
    ) {
      Err(Error::BRC20SError(e)) => {
        assert_eq!("transferable inscriptionId not found: 1111111111111111111111111111111111111111111111111111111111111111i1", e.to_string())
      }
      _ => {
        panic!("")
      }
    };

    // transferable.owner != from_script_key {
    let transferable_assets = TransferableAsset {
      inscription_id: msg.inscription_id,
      amount: 100_u128,
      tick_id,
      owner: script,
    };
    brc20s_data_store.set_transferable_assets(
      &script1,
      &tick_id,
      &msg.inscription_id,
      &transferable_assets,
    );

    brc20s_data_store.insert_inscribe_transfer_inscription(
      msg.inscription_id,
      TransferInfo {
        tick_id,
        tick_name: Tick::from_str(transfer_msg.tick.as_str()).unwrap(),
        amt: 100_u128,
      },
    );

    let mut error_msg = msg;
    error_msg.from = script1;
    match process_transfer(
      context,
      config.clone(),
      &brc20_data_store,
      &brc20s_data_store,
      &error_msg,
    ) {
      Err(Error::BRC20SError(e)) => {
        assert_eq!("transferable owner not match 1111111111111111111111111111111111111111111111111111111111111111i1", e.to_string())
      }
      _ => {
        panic!("")
      }
    };
  }

  #[test]
  fn test_process_stake_most() {
    let dbfile = NamedTempFile::new().unwrap();
    let db = Database::create(dbfile.path()).unwrap();
    let wtx = db.begin_write().unwrap();

    let brc20_data_store = brc20_db::DataStore::new(&wtx);
    let brc20s_data_store = brc20s_db::DataStore::new(&wtx);

    let addr = "bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e";
    let new_addr = "bc1pvk535u5eedhsx75r7mfvdru7t0kcr36mf9wuku7k68stc0ncss8qwzeahv";
    let (deploy, msg) = mock_deploy_msg(
      "pool", "01", "btc1", "ordi1", "10", "12000000", "21000000", 18, true, addr, addr,
    );
    let stake_tick = deploy.get_stake_id();
    let from_script = msg.from.clone();
    let to_script = msg.to.clone().unwrap();

    let result = set_brc20_token_user(&brc20_data_store, "btc1", &msg.from, 200_u128, 18_u8).err();
    assert_eq!(None, result);
    let pid_only1 = deploy.pool_id.as_str();
    let result = execute_for_test(
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      0,
      version::zebra(),
    );
    assert_eq!(None, result.err());
    let (deploy, msg) = mock_deploy_msg(
      "pool", "01", "btc1", "ordi2", "10", "12000000", "21000000", 18, true, addr, addr,
    );
    let pid_only2 = deploy.pool_id.as_str();
    let result = execute_for_test(
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      0,
      version::zebra(),
    );
    assert_eq!(None, result.err());
    let (deploy, msg) = mock_deploy_msg(
      "pool", "01", "btc1", "ordi3", "10", "12000000", "21000000", 18, false, addr, addr,
    );
    let pid_share1 = deploy.pool_id.as_str();
    let result = execute_for_test(
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      0,
      version::zebra(),
    );
    assert_eq!(None, result.err());
    let (deploy, msg) = mock_deploy_msg(
      "pool", "01", "btc1", "ordi4", "10", "12000000", "21000000", 18, false, addr, addr,
    );
    let pid_share2 = deploy.pool_id.as_str();
    let result = execute_for_test(
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      0,
      version::zebra(),
    );
    assert_eq!(None, result.err());

    print!("only1{}", pid_only1);
    print!("only2{}", pid_only2);
    print!("share1{}", pid_share1);
    print!("share2{}", pid_share2);
    {
      //pool is not exist
      let (stake, msg) = mock_stake_msg("0000000001#11", "100", addr, addr);
      let result = execute_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        0,
        version::zebra(),
      );
      assert_eq!(
        Err(BRC20SError::PoolNotExist("0000000001#11".to_string())),
        result
      );
      //from is not equal to
      let (stake, msg) = mock_stake_msg(pid_only1, "100", new_addr, addr);
      let result = execute_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        0,
        version::zebra(),
      );
      assert_eq!(
        Err(BRC20SError::FromToNotEqual(
          new_addr.to_string(),
          addr.to_string()
        )),
        result
      );
      //user balance < amount
      let (stake, msg) = mock_stake_msg(pid_only1, "300", addr, addr);
      let result = execute_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        0,
        version::zebra(),
      );
      assert_eq!(
        Err(BRC20SError::InsufficientBalance(
          "300000000000000000000".to_string(),
          "200000000000000000000".to_string(),
        )),
        result
      );
    }
    //first stake to only pool
    let (stake, msg) = mock_stake_msg(pid_only1, "50", addr, addr);
    let result = execute_for_test(
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      0,
      version::zebra(),
    );
    assert_eq!(None, result.err());

    let expect_stakeinfo = r#"{"stake":{"BRC20Tick":"btc1"},"pool_stakes":[["13395c5283#01",true,50000000000000000000]],"max_share":0,"total_only":50000000000000000000}"#;
    let expect_userinfo = r#"{"pid":"13395c5283#01","staked":50000000000000000000,"minted":0,"pending_reward":0,"reward_debt":0,"latest_updated_block":0}"#;
    let expect_poolinfo = r#"{"pid":"13395c5283#01","ptype":"Pool","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","stake":{"BRC20Tick":"btc1"},"erate":10000000000000000000,"minted":0,"staked":50000000000000000000,"dmax":12000000000000000000000000,"acc_reward_per_share":"0","last_update_block":0,"only":true,"deploy_block":0,"deploy_block_time":1687245485}"#;
    assert_stake_info(
      &brc20s_data_store,
      pid_only1,
      &from_script,
      &stake_tick,
      expect_poolinfo,
      expect_stakeinfo,
      expect_userinfo,
    );
    //first stake to share pool
    let (stake, msg) = mock_stake_msg(pid_share1, "49", addr, addr);
    let result = execute_for_test(
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      0,
      version::zebra(),
    );
    assert_eq!(None, result.err());

    let expect_stakeinfo = r#"{"stake":{"BRC20Tick":"btc1"},"pool_stakes":[["13395c5283#01",true,50000000000000000000],["fb641f54a2#01",false,49000000000000000000]],"max_share":49000000000000000000,"total_only":50000000000000000000}"#;
    let expect_userinfo = r#"{"pid":"fb641f54a2#01","staked":49000000000000000000,"minted":0,"pending_reward":0,"reward_debt":0,"latest_updated_block":0}"#;
    let expect_poolinfo = r#"{"pid":"fb641f54a2#01","ptype":"Pool","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","stake":{"BRC20Tick":"btc1"},"erate":10000000000000000000,"minted":0,"staked":49000000000000000000,"dmax":12000000000000000000000000,"acc_reward_per_share":"0","last_update_block":0,"only":false,"deploy_block":0,"deploy_block_time":1687245485}"#;
    assert_stake_info(
      &brc20s_data_store,
      pid_share1,
      &from_script,
      &stake_tick,
      expect_poolinfo,
      expect_stakeinfo,
      expect_userinfo,
    );
    {
      let (stake, msg) = mock_stake_msg(pid_only2, "49", addr, addr);
      let result = execute_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        0,
        version::zebra(),
      );
      assert_eq!(None, result.err());

      let (stake, msg) = mock_stake_msg(pid_share2, "50", addr, addr);
      let result = execute_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        0,
        version::zebra(),
      );
      assert_eq!(None, result.err());

      let expect_stakeinfo = r#"{"stake":{"BRC20Tick":"btc1"},"pool_stakes":[["13395c5283#01",true,50000000000000000000],["fb641f54a2#01",false,49000000000000000000],["7737ed558e#01",true,49000000000000000000],["b25c7ef626#01",false,50000000000000000000]],"max_share":50000000000000000000,"total_only":99000000000000000000}"#;
      let expect_userinfo = r#"{"pid":"b25c7ef626#01","staked":50000000000000000000,"minted":0,"pending_reward":0,"reward_debt":0,"latest_updated_block":0}"#;
      let expect_poolinfo = r#"{"pid":"b25c7ef626#01","ptype":"Pool","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","stake":{"BRC20Tick":"btc1"},"erate":10000000000000000000,"minted":0,"staked":50000000000000000000,"dmax":12000000000000000000000000,"acc_reward_per_share":"0","last_update_block":0,"only":false,"deploy_block":0,"deploy_block_time":1687245485}"#;
      assert_stake_info(
        &brc20s_data_store,
        pid_share2,
        &from_script,
        &stake_tick,
        expect_poolinfo,
        expect_stakeinfo,
        expect_userinfo,
      );
    }
    //user has stake 2 only pool 2 share pool, then stake to only pool but can_stake < amount
    let (stake, msg) = mock_stake_msg(pid_only2, "51.1", addr, addr);
    let result = execute_for_test(
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      0,
      version::zebra(),
    );
    assert_eq!(
      Some(BRC20SError::InsufficientBalance(
        "51100000000000000000".to_string(),
        "51000000000000000000".to_string(),
      )),
      result.err()
    );
    //user has stake 2 only pool 2 share pool, then stake to share pool but can_stake < amount
    let (stake, msg) = mock_stake_msg(pid_share2, "102", addr, addr);
    let result = execute_for_test(
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      0,
      version::zebra(),
    );
    assert_eq!(
      Some(BRC20SError::InsufficientBalance(
        "102000000000000000000".to_string(),
        "51000000000000000000".to_string(),
      )),
      result.err()
    );
    //user has stake 2 only pool 2 share pool, then stake to only pool
    let (stake, msg) = mock_stake_msg(pid_only2, "50", addr, addr);
    let result = execute_for_test(
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      1,
      version::zebra(),
    );
    assert_eq!(None, result.err());
    let expect_stakeinfo = r#"{"stake":{"BRC20Tick":"btc1"},"pool_stakes":[["13395c5283#01",true,50000000000000000000],["fb641f54a2#01",false,49000000000000000000],["b25c7ef626#01",false,50000000000000000000],["7737ed558e#01",true,99000000000000000000]],"max_share":50000000000000000000,"total_only":149000000000000000000}"#;
    let expect_userinfo = r#"{"pid":"7737ed558e#01","staked":99000000000000000000,"minted":0,"pending_reward":9999999999999999976,"reward_debt":20204081632653061176,"latest_updated_block":1}"#;
    let expect_poolinfo = r#"{"pid":"7737ed558e#01","ptype":"Pool","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","stake":{"BRC20Tick":"btc1"},"erate":10000000000000000000,"minted":10000000000000000000,"staked":99000000000000000000,"dmax":12000000000000000000000000,"acc_reward_per_share":"204081632653061224","last_update_block":1,"only":true,"deploy_block":0,"deploy_block_time":1687245485}"#;
    assert_stake_info(
      &brc20s_data_store,
      pid_only2,
      &from_script,
      &stake_tick,
      expect_poolinfo,
      expect_stakeinfo,
      expect_userinfo,
    );
    //user has stake 2 only pool 2 share pool, then stake to share pool
    let (stake, msg) = mock_stake_msg(pid_share1, "2", addr, addr);
    let result = execute_for_test(
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      1,
      version::zebra(),
    );
    assert_eq!(None, result.err());
    let expect_stakeinfo = r#"{"stake":{"BRC20Tick":"btc1"},"pool_stakes":[["13395c5283#01",true,50000000000000000000],["b25c7ef626#01",false,50000000000000000000],["7737ed558e#01",true,99000000000000000000],["fb641f54a2#01",false,51000000000000000000]],"max_share":51000000000000000000,"total_only":149000000000000000000}"#;
    let expect_userinfo = r#"{"pid":"fb641f54a2#01","staked":51000000000000000000,"minted":0,"pending_reward":9999999999999999976,"reward_debt":10408163265306122424,"latest_updated_block":1}"#;
    let expect_poolinfo = r#"{"pid":"fb641f54a2#01","ptype":"Pool","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","stake":{"BRC20Tick":"btc1"},"erate":10000000000000000000,"minted":10000000000000000000,"staked":51000000000000000000,"dmax":12000000000000000000000000,"acc_reward_per_share":"204081632653061224","last_update_block":1,"only":false,"deploy_block":0,"deploy_block_time":1687245485}"#;
    assert_stake_info(
      &brc20s_data_store,
      pid_share1,
      &from_script,
      &stake_tick,
      expect_poolinfo,
      expect_stakeinfo,
      expect_userinfo,
    );
  }

  #[test]
  fn test_process_unstake_most() {
    let dbfile = NamedTempFile::new().unwrap();
    let db = Database::create(dbfile.path()).unwrap();
    let wtx = db.begin_write().unwrap();

    let brc20_data_store = brc20_db::DataStore::new(&wtx);
    let brc20s_data_store = brc20s_db::DataStore::new(&wtx);

    let addr = "bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e";
    let new_addr = "bc1pvk535u5eedhsx75r7mfvdru7t0kcr36mf9wuku7k68stc0ncss8qwzeahv";
    let (deploy, msg) = mock_deploy_msg(
      "pool", "01", "btc1", "ordi1", "10", "12000000", "21000000", 18, true, addr, addr,
    );
    let stake_tick = deploy.get_stake_id();
    let from_script = msg.from.clone();
    let to_script = msg.to.clone().unwrap();

    let result = set_brc20_token_user(&brc20_data_store, "btc1", &msg.from, 200_u128, 18_u8).err();
    assert_eq!(None, result);
    let pid_only1 = deploy.pool_id.as_str();
    let result = execute_for_test(
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      0,
      version::zebra(),
    );
    assert_eq!(None, result.err());
    let (deploy, msg) = mock_deploy_msg(
      "pool", "01", "btc1", "ordi2", "10", "12000000", "21000000", 18, true, addr, addr,
    );
    let pid_only2 = deploy.pool_id.as_str();
    let result = execute_for_test(
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      0,
      version::zebra(),
    );
    assert_eq!(None, result.err());
    let (deploy, msg) = mock_deploy_msg(
      "pool", "01", "btc1", "ordi3", "10", "12000000", "21000000", 18, false, addr, addr,
    );
    let pid_share1 = deploy.pool_id.as_str();
    let result = execute_for_test(
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      0,
      version::zebra(),
    );
    assert_eq!(None, result.err());
    let (deploy, msg) = mock_deploy_msg(
      "pool", "01", "btc1", "ordi4", "10", "12000000", "21000000", 18, false, addr, addr,
    );
    let pid_share2 = deploy.pool_id.as_str();
    let result = execute_for_test(
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      0,
      version::zebra(),
    );
    assert_eq!(None, result.err());

    println!("only1  :{}", pid_only1);
    println!("only2  :{}", pid_only2);
    println!("share1 :{}", pid_share1);
    println!("share2 :{}", pid_share2);
    {
      //pool is not exist
      let (stake, msg) = mock_unstake_msg("0000000001#11", "100", addr, addr);
      let result = execute_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        0,
        version::zebra(),
      );
      assert_eq!(
        Err(BRC20SError::PoolNotExist("0000000001#11".to_string())),
        result
      );
      //from is not equal to
      let (stake, msg) = mock_unstake_msg(pid_only1, "100", new_addr, addr);
      let result = execute_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        0,
        version::zebra(),
      );
      assert_eq!(
        Err(BRC20SError::FromToNotEqual(
          new_addr.to_string(),
          addr.to_string()
        )),
        result
      );
      //user haven't stake
      let (stake, msg) = mock_unstake_msg(pid_only1, "300", addr, addr);
      let result = execute_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        0,
        version::zebra(),
      );
      assert_eq!(
        Err(BRC20SError::InsufficientBalance(
          "0".to_string(),
          "300000000000000000000".to_string(),
        )),
        result
      );
    }
    //stake to only pool
    let (stake, msg) = mock_stake_msg(pid_only1, "50", addr, addr);
    let result = execute_for_test(
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      0,
      version::zebra(),
    );
    assert_eq!(None, result.err());

    //unstake amt > stake amt
    let (stake, msg) = mock_unstake_msg(pid_only1, "300", addr, addr);
    let result = execute_for_test(
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      0,
      version::zebra(),
    );
    assert_eq!(
      Err(BRC20SError::InsufficientBalance(
        "50000000000000000000".to_string(),
        "300000000000000000000".to_string(),
      )),
      result
    );

    //unstake to only pool
    let (stake, msg) = mock_unstake_msg(pid_only1, "1", addr, addr);
    let result = execute_for_test(
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      1,
      version::zebra(),
    );
    let expect_stakeinfo = r#"{"stake":{"BRC20Tick":"btc1"},"pool_stakes":[["13395c5283#01",true,49000000000000000000]],"max_share":0,"total_only":49000000000000000000}"#;
    let expect_userinfo = r#"{"pid":"13395c5283#01","staked":49000000000000000000,"minted":0,"pending_reward":10000000000000000000,"reward_debt":9800000000000000000,"latest_updated_block":1}"#;
    let expect_poolinfo = r#"{"pid":"13395c5283#01","ptype":"Pool","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","stake":{"BRC20Tick":"btc1"},"erate":10000000000000000000,"minted":10000000000000000000,"staked":49000000000000000000,"dmax":12000000000000000000000000,"acc_reward_per_share":"200000000000000000","last_update_block":1,"only":true,"deploy_block":0,"deploy_block_time":1687245485}"#;
    assert_stake_info(
      &brc20s_data_store,
      pid_only1,
      &from_script,
      &stake_tick,
      expect_poolinfo,
      expect_stakeinfo,
      expect_userinfo,
    );

    //stake to share pool
    let (stake, msg) = mock_stake_msg(pid_share1, "50", addr, addr);
    let result = execute_for_test(
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      0,
      version::zebra(),
    );
    assert_eq!(None, result.err());
    //unstake to share pool
    let (stake, msg) = mock_unstake_msg(pid_share1, "1", addr, addr);
    let result = execute_for_test(
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      1,
      version::zebra(),
    );
    let expect_stakeinfo = r#"{"stake":{"BRC20Tick":"btc1"},"pool_stakes":[["13395c5283#01",true,49000000000000000000],["fb641f54a2#01",false,49000000000000000000]],"max_share":49000000000000000000,"total_only":49000000000000000000}"#;
    let expect_userinfo = r#"{"pid":"fb641f54a2#01","staked":49000000000000000000,"minted":0,"pending_reward":10000000000000000000,"reward_debt":9800000000000000000,"latest_updated_block":1}"#;
    let expect_poolinfo = r#"{"pid":"fb641f54a2#01","ptype":"Pool","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","stake":{"BRC20Tick":"btc1"},"erate":10000000000000000000,"minted":10000000000000000000,"staked":49000000000000000000,"dmax":12000000000000000000000000,"acc_reward_per_share":"200000000000000000","last_update_block":1,"only":false,"deploy_block":0,"deploy_block_time":1687245485}"#;
    assert_stake_info(
      &brc20s_data_store,
      pid_share1,
      &from_script,
      &stake_tick,
      expect_poolinfo,
      expect_stakeinfo,
      expect_userinfo,
    );

    {
      let (stake, msg) = mock_stake_msg(pid_only2, "50", addr, addr);
      let result = execute_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        0,
        version::zebra(),
      );
      assert_eq!(None, result.err());

      let (stake, msg) = mock_stake_msg(pid_share2, "50", addr, addr);
      let result = execute_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        0,
        version::zebra(),
      );
      assert_eq!(None, result.err());

      let expect_stakeinfo = r#"{"stake":{"BRC20Tick":"btc1"},"pool_stakes":[["13395c5283#01",true,49000000000000000000],["fb641f54a2#01",false,49000000000000000000],["7737ed558e#01",true,50000000000000000000],["b25c7ef626#01",false,50000000000000000000]],"max_share":50000000000000000000,"total_only":99000000000000000000}"#;
      let expect_userinfo = r#"{"pid":"b25c7ef626#01","staked":50000000000000000000,"minted":0,"pending_reward":0,"reward_debt":0,"latest_updated_block":0}"#;
      let expect_poolinfo = r#"{"pid":"b25c7ef626#01","ptype":"Pool","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","stake":{"BRC20Tick":"btc1"},"erate":10000000000000000000,"minted":0,"staked":50000000000000000000,"dmax":12000000000000000000000000,"acc_reward_per_share":"0","last_update_block":0,"only":false,"deploy_block":0,"deploy_block_time":1687245485}"#;
      assert_stake_info(
        &brc20s_data_store,
        pid_share2,
        &from_script,
        &stake_tick,
        expect_poolinfo,
        expect_stakeinfo,
        expect_userinfo,
      );
    }
    //user has stake 2 only pool 2 share pool, then unstake from only pool
    let (stake, msg) = mock_unstake_msg(pid_only2, "2", addr, addr);
    let result = execute_for_test(
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      1,
      version::zebra(),
    );
    let expect_stakeinfo = r#"{"stake":{"BRC20Tick":"btc1"},"pool_stakes":[["13395c5283#01",true,49000000000000000000],["fb641f54a2#01",false,49000000000000000000],["7737ed558e#01",true,48000000000000000000],["b25c7ef626#01",false,50000000000000000000]],"max_share":50000000000000000000,"total_only":97000000000000000000}"#;
    let expect_userinfo = r#"{"pid":"7737ed558e#01","staked":48000000000000000000,"minted":0,"pending_reward":10000000000000000000,"reward_debt":9600000000000000000,"latest_updated_block":1}"#;
    let expect_poolinfo = r#"{"pid":"7737ed558e#01","ptype":"Pool","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","stake":{"BRC20Tick":"btc1"},"erate":10000000000000000000,"minted":10000000000000000000,"staked":48000000000000000000,"dmax":12000000000000000000000000,"acc_reward_per_share":"200000000000000000","last_update_block":1,"only":true,"deploy_block":0,"deploy_block_time":1687245485}"#;
    assert_stake_info(
      &brc20s_data_store,
      pid_only2,
      &from_script,
      &stake_tick,
      expect_poolinfo,
      expect_stakeinfo,
      expect_userinfo,
    );
    //user has stake 2 only pool 2 share pool, then unstake from share pool
    let (stake, msg) = mock_unstake_msg(pid_share2, "2", addr, addr);
    let result = execute_for_test(
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      1,
      version::zebra(),
    );
    let expect_stakeinfo = r#"{"stake":{"BRC20Tick":"btc1"},"pool_stakes":[["13395c5283#01",true,49000000000000000000],["fb641f54a2#01",false,49000000000000000000],["7737ed558e#01",true,48000000000000000000],["b25c7ef626#01",false,48000000000000000000]],"max_share":49000000000000000000,"total_only":97000000000000000000}"#;
    let expect_userinfo = r#"{"pid":"b25c7ef626#01","staked":48000000000000000000,"minted":0,"pending_reward":10000000000000000000,"reward_debt":9600000000000000000,"latest_updated_block":1}"#;
    let expect_poolinfo = r#"{"pid":"b25c7ef626#01","ptype":"Pool","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","stake":{"BRC20Tick":"btc1"},"erate":10000000000000000000,"minted":10000000000000000000,"staked":48000000000000000000,"dmax":12000000000000000000000000,"acc_reward_per_share":"200000000000000000","last_update_block":1,"only":false,"deploy_block":0,"deploy_block_time":1687245485}"#;
    assert_stake_info(
      &brc20s_data_store,
      pid_share2,
      &from_script,
      &stake_tick,
      expect_poolinfo,
      expect_stakeinfo,
      expect_userinfo,
    );

    //user has stake 2 only pool 2 share pool, then unstake from share pool to 0
    let (stake, msg) = mock_unstake_msg(pid_share1, "49", addr, addr);
    let result = execute_for_test(
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      2,
      version::zebra(),
    );
    let expect_stakeinfo = r#"{"stake":{"BRC20Tick":"btc1"},"pool_stakes":[["13395c5283#01",true,49000000000000000000],["7737ed558e#01",true,48000000000000000000],["b25c7ef626#01",false,48000000000000000000]],"max_share":48000000000000000000,"total_only":97000000000000000000}"#;
    let expect_userinfo = r#"{"pid":"fb641f54a2#01","staked":0,"minted":0,"pending_reward":19999999999999999976,"reward_debt":0,"latest_updated_block":2}"#;
    let expect_poolinfo = r#"{"pid":"fb641f54a2#01","ptype":"Pool","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","stake":{"BRC20Tick":"btc1"},"erate":10000000000000000000,"minted":20000000000000000000,"staked":0,"dmax":12000000000000000000000000,"acc_reward_per_share":"404081632653061224","last_update_block":2,"only":false,"deploy_block":0,"deploy_block_time":1687245485}"#;
    assert_stake_info(
      &brc20s_data_store,
      pid_share1,
      &from_script,
      &stake_tick,
      expect_poolinfo,
      expect_stakeinfo,
      expect_userinfo,
    );
  }

  #[test]
  fn test_process_passive_unstake_normal() {
    let dbfile = NamedTempFile::new().unwrap();
    let db = Database::create(dbfile.path()).unwrap();
    let wtx = db.begin_write().unwrap();

    let brc20_data_store = brc20_db::DataStore::new(&wtx);
    let brc20s_data_store = brc20s_db::DataStore::new(&wtx);

    let addr = "bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e";
    let new_addr = "bc1pvk535u5eedhsx75r7mfvdru7t0kcr36mf9wuku7k68stc0ncss8qwzeahv";
    let (deploy, msg) = mock_deploy_msg(
      "pool", "01", "btc1", "ordi1", "10", "12000000", "21000000", 18, true, addr, addr,
    );
    let stake_tick = deploy.get_stake_id();
    let from_script = msg.from.clone();
    let to_script = msg.to.clone().unwrap();

    let result = set_brc20_token_user(&brc20_data_store, "btc1", &msg.from, 200_u128, 18_u8).err();
    assert_eq!(None, result);
    let pid_only1 = deploy.pool_id.as_str();
    let result = execute_for_test(
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      0,
      version::zebra(),
    );
    assert_eq!(None, result.err());
    let (deploy, msg) = mock_deploy_msg(
      "pool", "01", "btc1", "ordi2", "10", "12000000", "21000000", 18, true, addr, addr,
    );
    let pid_only2 = deploy.pool_id.as_str();
    let result = execute_for_test(
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      0,
      version::zebra(),
    );
    assert_eq!(None, result.err());
    let (deploy, msg) = mock_deploy_msg(
      "pool", "01", "btc1", "ordi3", "10", "12000000", "21000000", 18, false, addr, addr,
    );
    let pid_share1 = deploy.pool_id.as_str();
    let result = execute_for_test(
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      0,
      version::zebra(),
    );
    assert_eq!(None, result.err());
    let (deploy, msg) = mock_deploy_msg(
      "pool", "01", "btc1", "ordi4", "10", "12000000", "21000000", 18, false, addr, addr,
    );
    let pid_share2 = deploy.pool_id.as_str();
    let result = execute_for_test(
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      0,
      version::zebra(),
    );
    assert_eq!(None, result.err());

    println!("only1  :{}", pid_only1);
    println!("only2  :{}", pid_only2);
    println!("share1 :{}", pid_share1);
    println!("share2 :{}", pid_share2);
    {
      //pool is not exist
      let (stake, msg) = mock_passive_unstake_msg("0000000001", "100", addr, addr);
      let result = execute_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        0,
        version::zebra(),
      );
      assert_eq!(
        Err(BRC20SError::StakeNotFound("0000000001".to_string())),
        result
      );
      //no stake then passive unstake
      let (stake, msg) = mock_passive_unstake_msg("btc1", "50", addr, addr);
      let result = execute_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        0,
        version::zebra(),
      );
      assert_eq!(Err(BRC20SError::StakeNotFound("btc1".to_string(),)), result);
    }
    //stake to only pool
    let (stake, msg) = mock_stake_msg(pid_only1, "50", addr, addr);
    let result = execute_for_test(
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      0,
      version::zebra(),
    );
    assert_eq!(None, result.err());

    //sum - transfer > amt nothing do
    //simluate transfer
    let result = set_brc20_token_user(&brc20_data_store, "btc1", &msg.from, 50_u128, 18_u8).err();
    assert_eq!(None, result);
    let (stake, msg) = mock_passive_unstake_msg("btc1", "200", addr, addr);
    let result = execute_for_test(
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      0,
      version::zebra(),
    );
    assert_eq!(Ok(vec![]), result);

    //sum - transfer > amt passive unstake
    //simluate transfer
    let result = set_brc20_token_user(&brc20_data_store, "btc1", &msg.from, 10_u128, 18_u8).err();
    assert_eq!(None, result);
    let (stake, msg) = mock_passive_unstake_msg("btc1", "190", addr, addr);
    let result = execute_for_test(
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      1,
      version::zebra(),
    );
    assert_eq!(
      Ok(vec![PassiveWithdraw(PassiveWithdrawEvent {
        pid: Pid::from_str(pid_only1).unwrap(),
        amt: 40000000000000000000
      })]),
      result
    );

    let expect_stakeinfo = r#"{"stake":{"BRC20Tick":"btc1"},"pool_stakes":[["13395c5283#01",true,10000000000000000000]],"max_share":0,"total_only":10000000000000000000}"#;
    let expect_userinfo = r#"{"pid":"13395c5283#01","staked":10000000000000000000,"minted":0,"pending_reward":10000000000000000000,"reward_debt":2000000000000000000,"latest_updated_block":1}"#;
    let expect_poolinfo = r#"{"pid":"13395c5283#01","ptype":"Pool","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","stake":{"BRC20Tick":"btc1"},"erate":10000000000000000000,"minted":10000000000000000000,"staked":10000000000000000000,"dmax":12000000000000000000000000,"acc_reward_per_share":"200000000000000000","last_update_block":1,"only":true,"deploy_block":0,"deploy_block_time":1687245485}"#;
    assert_stake_info(
      &brc20s_data_store,
      pid_only1,
      &from_script,
      &stake_tick,
      expect_poolinfo,
      expect_stakeinfo,
      expect_userinfo,
    );

    //from is not equal to
    let result = set_brc20_token_user(&brc20_data_store, "btc1", &msg.from, 0_u128, 18_u8).err();
    assert_eq!(None, result);
    let (stake, msg) = mock_passive_unstake_msg("btc1", "50", addr, new_addr);
    let result = execute_for_test(
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      2,
      version::zebra(),
    );
    assert_eq!(
      Ok(vec![PassiveWithdraw(PassiveWithdrawEvent {
        pid: Pid::from_str(pid_only1).unwrap(),
        amt: 10000000000000000000
      })]),
      result
    );

    let expect_stakeinfo =
      r#"{"stake":{"BRC20Tick":"btc1"},"pool_stakes":[],"max_share":0,"total_only":0}"#;
    let expect_userinfo = r#"{"pid":"13395c5283#01","staked":0,"minted":0,"pending_reward":20000000000000000000,"reward_debt":0,"latest_updated_block":2}"#;
    let expect_poolinfo = r#"{"pid":"13395c5283#01","ptype":"Pool","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","stake":{"BRC20Tick":"btc1"},"erate":10000000000000000000,"minted":20000000000000000000,"staked":0,"dmax":12000000000000000000000000,"acc_reward_per_share":"1200000000000000000","last_update_block":2,"only":true,"deploy_block":0,"deploy_block_time":1687245485}"#;
    assert_stake_info(
      &brc20s_data_store,
      pid_only1,
      &from_script,
      &stake_tick,
      expect_poolinfo,
      expect_stakeinfo,
      expect_userinfo,
    );

    //reset banalce
    let result = set_brc20_token_user(&brc20_data_store, "btc1", &msg.from, 200_u128, 18_u8).err();
    assert_eq!(None, result);

    //stake to share pool
    let (stake, msg) = mock_stake_msg(pid_share1, "50", addr, addr);
    let result = execute_for_test(
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      0,
      version::zebra(),
    );
    assert_eq!(None, result.err());

    //sum - transfer > amt nothing do
    //simluate transfer
    let result = set_brc20_token_user(&brc20_data_store, "btc1", &msg.from, 50_u128, 18_u8).err();
    assert_eq!(None, result);
    let (stake, msg) = mock_passive_unstake_msg("btc1", "200", addr, addr);
    let result = execute_for_test(
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      0,
      version::zebra(),
    );
    assert_eq!(Ok(vec![]), result);

    //sum - transfer > amt passive unstake
    //simluate transfer
    let result = set_brc20_token_user(&brc20_data_store, "btc1", &msg.from, 10_u128, 18_u8).err();
    assert_eq!(None, result);
    let (stake, msg) = mock_passive_unstake_msg("btc1", "190", addr, addr);
    let result = execute_for_test(
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      1,
      version::zebra(),
    );
    assert_eq!(
      Ok(vec![PassiveWithdraw(PassiveWithdrawEvent {
        pid: Pid::from_str(pid_share1).unwrap(),
        amt: 40000000000000000000
      })]),
      result
    );

    let expect_stakeinfo = r#"{"stake":{"BRC20Tick":"btc1"},"pool_stakes":[["fb641f54a2#01",false,10000000000000000000]],"max_share":10000000000000000000,"total_only":0}"#;
    let expect_userinfo = r#"{"pid":"fb641f54a2#01","staked":10000000000000000000,"minted":0,"pending_reward":10000000000000000000,"reward_debt":2000000000000000000,"latest_updated_block":1}"#;
    let expect_poolinfo = r#"{"pid":"fb641f54a2#01","ptype":"Pool","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","stake":{"BRC20Tick":"btc1"},"erate":10000000000000000000,"minted":10000000000000000000,"staked":10000000000000000000,"dmax":12000000000000000000000000,"acc_reward_per_share":"200000000000000000","last_update_block":1,"only":false,"deploy_block":0,"deploy_block_time":1687245485}"#;
    assert_stake_info(
      &brc20s_data_store,
      pid_share1,
      &from_script,
      &stake_tick,
      expect_poolinfo,
      expect_stakeinfo,
      expect_userinfo,
    );

    //from is not equal to
    let result = set_brc20_token_user(&brc20_data_store, "btc1", &msg.from, 0_u128, 18_u8).err();
    assert_eq!(None, result);
    let (stake, msg) = mock_passive_unstake_msg("btc1", "50", addr, new_addr);
    let result = execute_for_test(
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      2,
      version::zebra(),
    );
    assert_eq!(
      Ok(vec![PassiveWithdraw(PassiveWithdrawEvent {
        pid: Pid::from_str(pid_share1).unwrap(),
        amt: 10000000000000000000
      })]),
      result
    );

    let expect_stakeinfo =
      r#"{"stake":{"BRC20Tick":"btc1"},"pool_stakes":[],"max_share":0,"total_only":0}"#;
    let expect_userinfo = r#"{"pid":"fb641f54a2#01","staked":0,"minted":0,"pending_reward":20000000000000000000,"reward_debt":0,"latest_updated_block":2}"#;
    let expect_poolinfo = r#"{"pid":"fb641f54a2#01","ptype":"Pool","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","stake":{"BRC20Tick":"btc1"},"erate":10000000000000000000,"minted":20000000000000000000,"staked":0,"dmax":12000000000000000000000000,"acc_reward_per_share":"1200000000000000000","last_update_block":2,"only":false,"deploy_block":0,"deploy_block_time":1687245485}"#;
    assert_stake_info(
      &brc20s_data_store,
      pid_share1,
      &from_script,
      &stake_tick,
      expect_poolinfo,
      expect_stakeinfo,
      expect_userinfo,
    );
  }

  fn prepare_env_for_test<'a, L: brc20::DataStoreReadWrite, K: brc20s::DataStoreReadWrite>(
    brc20_data_store: &'a L,
    brc20s_data_store: &'a K,
    addr: &str,
    stake: &str,
    earn: &str,
    pool_property: u8,
  ) -> Result<(Vec<(String, PledgedTick)>, ScriptKey), BRC20SError> {
    let mut results: Vec<(String, PledgedTick)> = Vec::new();
    let brc20s_tick = format!("{}1", earn);
    let pool_only1 = pool_property & 0b1000 > 0;
    let (deploy, msg) = mock_deploy_msg(
      "pool",
      "01",
      stake,
      brc20s_tick.as_str(),
      "10",
      "12000000",
      "21000000",
      18,
      pool_only1,
      addr,
      addr,
    );

    let from_script = msg.from.clone();
    let to_script = msg.to.clone().unwrap();

    let result = set_brc20_token_user(brc20_data_store, stake, &msg.from, 200_u128, 18_u8).err();
    assert_eq!(None, result);
    let pid_only1 = deploy.pool_id.as_str();
    let hehe = deploy.get_pool_id();

    let result = execute_for_test(
      brc20_data_store,
      brc20s_data_store,
      &msg,
      0,
      version::zebra(),
    );
    assert_eq!(None, result.err());
    let stake_tick_only1 = deploy.get_stake_id();
    results.push((pid_only1.to_string(), stake_tick_only1.clone()));

    let brc20s_tick = format!("{}2", earn);
    let pool_only2 = pool_property & 0b0010 > 0;
    let (deploy, msg) = mock_deploy_msg(
      "pool",
      "01",
      stake,
      brc20s_tick.as_str(),
      "10",
      "12000000",
      "21000000",
      18,
      pool_only2,
      addr,
      addr,
    );
    let pid_only2 = deploy.pool_id.as_str();
    let result = execute_for_test(
      brc20_data_store,
      brc20s_data_store,
      &msg,
      0,
      version::zebra(),
    );
    assert_eq!(None, result.err());
    let stake_tick_only2 = deploy.get_stake_id();
    results.push((pid_only2.to_string(), stake_tick_only2.clone()));

    let brc20s_tick = format!("{}3", earn);
    let pool_only3 = pool_property & 0b0100 > 0;
    let (deploy, msg) = mock_deploy_msg(
      "pool",
      "01",
      stake,
      brc20s_tick.as_str(),
      "10",
      "12000000",
      "21000000",
      18,
      pool_only3,
      addr,
      addr,
    );
    let pid_share1 = deploy.pool_id.as_str();
    let result = execute_for_test(
      brc20_data_store,
      brc20s_data_store,
      &msg,
      0,
      version::zebra(),
    );
    assert_eq!(None, result.err());
    let stake_tick_share1 = deploy.get_stake_id();
    results.push((pid_share1.to_string(), stake_tick_share1.clone()));

    let brc20s_tick = format!("{}4", earn);
    let pool_only4 = pool_property & 0b0001 > 0;
    let (deploy, msg) = mock_deploy_msg(
      "pool",
      "01",
      stake,
      brc20s_tick.as_str(),
      "10",
      "12000000",
      "21000000",
      18,
      pool_only4,
      addr,
      addr,
    );
    let pid_share2 = deploy.pool_id.as_str();
    let result = execute_for_test(
      brc20_data_store,
      brc20s_data_store,
      &msg,
      0,
      version::zebra(),
    );
    assert_eq!(None, result.err());
    let stake_tick_share2 = deploy.get_stake_id();
    results.push((pid_share2.to_string(), stake_tick_share2.clone()));

    //stake to
    let (stake, msg) = mock_stake_msg(pid_only1, "50", addr, addr);
    let result = execute_for_test(
      brc20_data_store,
      brc20s_data_store,
      &msg,
      0,
      version::zebra(),
    );
    assert_eq!(None, result.err());

    let (stake, msg) = mock_stake_msg(pid_share1, "50", addr, addr);
    let result = execute_for_test(
      brc20_data_store,
      brc20s_data_store,
      &msg,
      0,
      version::zebra(),
    );
    assert_eq!(None, result.err());

    let (stake, msg) = mock_stake_msg(pid_only2, "50", addr, addr);
    let result = execute_for_test(
      brc20_data_store,
      brc20s_data_store,
      &msg,
      0,
      version::zebra(),
    );
    assert_eq!(None, result.err());

    let (stake, msg) = mock_stake_msg(pid_share2, "50", addr, addr);
    let result = execute_for_test(
      brc20_data_store,
      brc20s_data_store,
      &msg,
      0,
      version::zebra(),
    );
    assert_eq!(None, result.err());
    let mut max_share = 0_u128;
    let mut total_only = 0_u128;
    if !pool_only1 {
      max_share = 50000000000000000000_u128;
    } else {
      total_only += 50000000000000000000_u128;
    }
    if !pool_only2 {
      max_share = 50000000000000000000_u128;
    } else {
      total_only += 50000000000000000000_u128;
    }
    if !pool_only3 {
      max_share = 50000000000000000000_u128;
    } else {
      total_only += 50000000000000000000_u128;
    }
    if !pool_only4 {
      max_share = 50000000000000000000_u128;
    } else {
      total_only += 50000000000000000000_u128;
    }
    let temp = format!(
      r##"{{"stake":{{"BRC20Tick":"btc1"}},"pool_stakes":[["a2c6a6a614#01",{},50000000000000000000],["934a4f7aff#01",{},50000000000000000000],["83050baa2b#01",{},50000000000000000000],["92c3f0f4ab#01",{},50000000000000000000]],"max_share":{},"total_only":{}}}"##,
      pool_only1.clone(),
      pool_only3.clone(),
      pool_only2.clone(),
      pool_only4.clone(),
      max_share,
      total_only
    );
    let expect_stakeinfo = temp.as_str();
    let expect_userinfo = r#"{"pid":"a2c6a6a614#01","staked":50000000000000000000,"minted":0,"pending_reward":0,"reward_debt":0,"latest_updated_block":0}"#;
    let temp = format!(
      r##"{{"pid":"a2c6a6a614#01","ptype":"Pool","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","stake":{{"BRC20Tick":"btc1"}},"erate":10000000000000000000,"minted":0,"staked":50000000000000000000,"dmax":12000000000000000000000000,"acc_reward_per_share":"0","last_update_block":0,"only":{},"deploy_block":0,"deploy_block_time":1687245485}}"##,
      pool_only1.clone()
    );
    let expect_poolinfo = temp.as_str();
    assert_stake_info(
      brc20s_data_store,
      pid_only1,
      &from_script,
      &stake_tick_only1,
      expect_poolinfo,
      expect_stakeinfo,
      expect_userinfo,
    );

    let expect_userinfo = r#"{"pid":"83050baa2b#01","staked":50000000000000000000,"minted":0,"pending_reward":0,"reward_debt":0,"latest_updated_block":0}"#;
    let temp = format!(
      r##"{{"pid":"83050baa2b#01","ptype":"Pool","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","stake":{{"BRC20Tick":"btc1"}},"erate":10000000000000000000,"minted":0,"staked":50000000000000000000,"dmax":12000000000000000000000000,"acc_reward_per_share":"0","last_update_block":0,"only":{},"deploy_block":0,"deploy_block_time":1687245485}}"##,
      pool_only2.clone()
    );
    let expect_poolinfo = temp.as_str();
    assert_stake_info(
      brc20s_data_store,
      pid_only2,
      &from_script,
      &stake_tick_only2,
      expect_poolinfo,
      expect_stakeinfo,
      expect_userinfo,
    );

    let expect_userinfo = r#"{"pid":"934a4f7aff#01","staked":50000000000000000000,"minted":0,"pending_reward":0,"reward_debt":0,"latest_updated_block":0}"#;
    let temp = format!(
      r##"{{"pid":"934a4f7aff#01","ptype":"Pool","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","stake":{{"BRC20Tick":"btc1"}},"erate":10000000000000000000,"minted":0,"staked":50000000000000000000,"dmax":12000000000000000000000000,"acc_reward_per_share":"0","last_update_block":0,"only":{},"deploy_block":0,"deploy_block_time":1687245485}}"##,
      pool_only3.clone()
    );
    let expect_poolinfo = temp.as_str();
    assert_stake_info(
      brc20s_data_store,
      pid_share1,
      &from_script,
      &stake_tick_share1,
      expect_poolinfo,
      expect_stakeinfo,
      expect_userinfo,
    );

    let expect_userinfo = r#"{"pid":"92c3f0f4ab#01","staked":50000000000000000000,"minted":0,"pending_reward":0,"reward_debt":0,"latest_updated_block":0}"#;
    let temp = format!(
      r##"{{"pid":"92c3f0f4ab#01","ptype":"Pool","inscription_id":"1111111111111111111111111111111111111111111111111111111111111111i1","stake":{{"BRC20Tick":"btc1"}},"erate":10000000000000000000,"minted":0,"staked":50000000000000000000,"dmax":12000000000000000000000000,"acc_reward_per_share":"0","last_update_block":0,"only":{},"deploy_block":0,"deploy_block_time":1687245485}}"##,
      pool_only4.clone()
    );
    let expect_poolinfo = temp.as_str();
    assert_stake_info(
      brc20s_data_store,
      pid_share2,
      &from_script,
      &stake_tick_share2,
      expect_poolinfo,
      expect_stakeinfo,
      expect_userinfo,
    );

    Ok((results, from_script))
  }

  #[test]
  fn test_process_passive_unstake_most() {
    // 1-only(50) 2-share(50) 3-only(50) 4-share(50) transfer 50 no passwithdraw
    {
      let dbfile = NamedTempFile::new().unwrap();
      let db = Database::create(dbfile.path()).unwrap();
      let wtx = db.begin_write().unwrap();

      let brc20_data_store = brc20_db::DataStore::new(&wtx);
      let brc20s_data_store = brc20s_db::DataStore::new(&wtx);

      let addr = "bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e";

      let stake = "btc1";
      let result = prepare_env_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        addr,
        stake,
        "ord",
        0b1010,
      );
      let (infos, from_script) = match result {
        Ok(r) => r,
        Err(e) => {
          panic!("err:{}", e);
        }
      };

      //withdraw 50
      let result =
        set_brc20_token_user(&brc20_data_store, stake, &from_script, 150_u128, 18_u8).err();
      assert_eq!(None, result);
      let (stake, msg) = mock_passive_unstake_msg(stake, "50", addr, addr);
      let result = execute_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        1,
        version::zebra(),
      );

      assert_eq!(Ok(vec![]), result);
    }

    // 1-only(50) 2-share(50) 3-only(50) 4-share(50) transfer 100  passwithdraw
    {
      let dbfile = NamedTempFile::new().unwrap();
      let db = Database::create(dbfile.path()).unwrap();
      let wtx = db.begin_write().unwrap();

      let brc20_data_store = brc20_db::DataStore::new(&wtx);
      let brc20s_data_store = brc20s_db::DataStore::new(&wtx);

      let addr = "bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e";

      let stake = "btc1";
      let result = prepare_env_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        addr,
        stake,
        "ord",
        0b1010,
      );
      let (infos, from_script) = match result {
        Ok(r) => r,
        Err(e) => {
          panic!("err:{}", e);
        }
      };

      //withdraw 100
      let result =
        set_brc20_token_user(&brc20_data_store, stake, &from_script, 100_u128, 18_u8).err();
      assert_eq!(None, result);
      let (stake, msg) = mock_passive_unstake_msg(stake, "100", addr, addr);
      let result = execute_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        1,
        version::zebra(),
      );

      assert_eq!(
        Ok(vec![
          PassiveWithdraw(PassiveWithdrawEvent {
            pid: Pid::from_str("92c3f0f4ab#01").unwrap(),
            amt: 50000000000000000000
          }),
          PassiveWithdraw(PassiveWithdrawEvent {
            pid: Pid::from_str("934a4f7aff#01").unwrap(),
            amt: 50000000000000000000
          }),
        ]),
        result
      );
    }

    // 1-only(50) 2-share(50) 3-only(50) 4-share(50) transfer 150  passwithdraw
    {
      let dbfile = NamedTempFile::new().unwrap();
      let db = Database::create(dbfile.path()).unwrap();
      let wtx = db.begin_write().unwrap();

      let brc20_data_store = brc20_db::DataStore::new(&wtx);
      let brc20s_data_store = brc20s_db::DataStore::new(&wtx);

      let addr = "bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e";

      let stake = "btc1";
      let result = prepare_env_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        addr,
        stake,
        "ord",
        0b1010,
      );
      let (infos, from_script) = match result {
        Ok(r) => r,
        Err(e) => {
          panic!("err:{}", e);
        }
      };

      //withdraw 150
      let result =
        set_brc20_token_user(&brc20_data_store, stake, &from_script, 50_u128, 18_u8).err();
      assert_eq!(None, result);
      let (stake, msg) = mock_passive_unstake_msg(stake, "150", addr, addr);
      let result = execute_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        1,
        version::zebra(),
      );

      assert_eq!(
        Ok(vec![
          PassiveWithdraw(PassiveWithdrawEvent {
            pid: Pid::from_str("92c3f0f4ab#01").unwrap(),
            amt: 50000000000000000000
          }),
          PassiveWithdraw(PassiveWithdrawEvent {
            pid: Pid::from_str("83050baa2b#01").unwrap(),
            amt: 50000000000000000000
          }),
          PassiveWithdraw(PassiveWithdrawEvent {
            pid: Pid::from_str("934a4f7aff#01").unwrap(),
            amt: 50000000000000000000
          }),
        ]),
        result
      );
    }

    // 1-only(50) 2-share(50) 3-only(50) 4-share(50) transfer 200  passwithdraw
    {
      let dbfile = NamedTempFile::new().unwrap();
      let db = Database::create(dbfile.path()).unwrap();
      let wtx = db.begin_write().unwrap();

      let brc20_data_store = brc20_db::DataStore::new(&wtx);
      let brc20s_data_store = brc20s_db::DataStore::new(&wtx);

      let addr = "bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e";

      let stake = "btc1";
      let result = prepare_env_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        addr,
        stake,
        "ord",
        0b1001,
      );
      let (infos, from_script) = match result {
        Ok(r) => r,
        Err(e) => {
          panic!("err:{}", e);
        }
      };

      //withdraw 150
      let result =
        set_brc20_token_user(&brc20_data_store, stake, &from_script, 0_u128, 18_u8).err();
      assert_eq!(None, result);
      let (stake, msg) = mock_passive_unstake_msg(stake, "200", addr, addr);
      let result = execute_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        1,
        version::zebra(),
      );

      assert_eq!(
        Ok(vec![
          PassiveWithdraw(PassiveWithdrawEvent {
            pid: Pid::from_str("92c3f0f4ab#01").unwrap(),
            amt: 50000000000000000000
          }),
          PassiveWithdraw(PassiveWithdrawEvent {
            pid: Pid::from_str("83050baa2b#01").unwrap(),
            amt: 50000000000000000000
          }),
          PassiveWithdraw(PassiveWithdrawEvent {
            pid: Pid::from_str("934a4f7aff#01").unwrap(),
            amt: 50000000000000000000
          }),
          PassiveWithdraw(PassiveWithdrawEvent {
            pid: Pid::from_str("a2c6a6a614#01").unwrap(),
            amt: 50000000000000000000
          }),
        ]),
        result
      );
    }

    // 1-only(50) 2-share(50) 3-share(50) 4-only(50) transfer 50 no passwithdraw
    {
      let dbfile = NamedTempFile::new().unwrap();
      let db = Database::create(dbfile.path()).unwrap();
      let wtx = db.begin_write().unwrap();

      let brc20_data_store = brc20_db::DataStore::new(&wtx);
      let brc20s_data_store = brc20s_db::DataStore::new(&wtx);

      let addr = "bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e";

      let stake = "btc1";
      let result = prepare_env_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        addr,
        stake,
        "ord",
        0b1001,
      );
      let (infos, from_script) = match result {
        Ok(r) => r,
        Err(e) => {
          panic!("err:{}", e);
        }
      };

      //withdraw 50
      let result =
        set_brc20_token_user(&brc20_data_store, stake, &from_script, 150_u128, 18_u8).err();
      assert_eq!(None, result);
      let (stake, msg) = mock_passive_unstake_msg(stake, "50", addr, addr);
      let result = execute_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        1,
        version::zebra(),
      );

      assert_eq!(Ok(vec![]), result);
    }

    // 1-only(50) 2-share(50) 3-share(50) 4-only(50)  transfer 100  passwithdraw
    {
      let dbfile = NamedTempFile::new().unwrap();
      let db = Database::create(dbfile.path()).unwrap();
      let wtx = db.begin_write().unwrap();

      let brc20_data_store = brc20_db::DataStore::new(&wtx);
      let brc20s_data_store = brc20s_db::DataStore::new(&wtx);

      let addr = "bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e";

      let stake = "btc1";
      let result = prepare_env_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        addr,
        stake,
        "ord",
        0b1001,
      );
      let (infos, from_script) = match result {
        Ok(r) => r,
        Err(e) => {
          panic!("err:{}", e);
        }
      };

      //withdraw 100
      let result =
        set_brc20_token_user(&brc20_data_store, stake, &from_script, 100_u128, 18_u8).err();
      assert_eq!(None, result);
      let (stake, msg) = mock_passive_unstake_msg(stake, "100", addr, addr);
      let result = execute_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        1,
        version::zebra(),
      );

      assert_eq!(
        Ok(vec![PassiveWithdraw(PassiveWithdrawEvent {
          pid: Pid::from_str("92c3f0f4ab#01").unwrap(),
          amt: 50000000000000000000
        }),]),
        result
      );
    }

    // 1-only(50) 2-share(50) 3-share(50) 4-only(50)  transfer 150  passwithdraw
    {
      let dbfile = NamedTempFile::new().unwrap();
      let db = Database::create(dbfile.path()).unwrap();
      let wtx = db.begin_write().unwrap();

      let brc20_data_store = brc20_db::DataStore::new(&wtx);
      let brc20s_data_store = brc20s_db::DataStore::new(&wtx);

      let addr = "bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e";

      let stake = "btc1";
      let result = prepare_env_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        addr,
        stake,
        "ord",
        0b1001,
      );
      let (infos, from_script) = match result {
        Ok(r) => r,
        Err(e) => {
          panic!("err:{}", e);
        }
      };

      //withdraw 150
      let result =
        set_brc20_token_user(&brc20_data_store, stake, &from_script, 50_u128, 18_u8).err();
      assert_eq!(None, result);
      let (stake, msg) = mock_passive_unstake_msg(stake, "150", addr, addr);
      let result = execute_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        1,
        version::zebra(),
      );

      assert_eq!(
        Ok(vec![
          PassiveWithdraw(PassiveWithdrawEvent {
            pid: Pid::from_str("92c3f0f4ab#01").unwrap(),
            amt: 50000000000000000000
          }),
          PassiveWithdraw(PassiveWithdrawEvent {
            pid: Pid::from_str("83050baa2b#01").unwrap(),
            amt: 50000000000000000000
          }),
          PassiveWithdraw(PassiveWithdrawEvent {
            pid: Pid::from_str("934a4f7aff#01").unwrap(),
            amt: 50000000000000000000
          }),
        ]),
        result
      );
    }

    // 1-only(50) 2-share(50) 3-share(50) 4-only(50)  transfer 200  passwithdraw
    {
      let dbfile = NamedTempFile::new().unwrap();
      let db = Database::create(dbfile.path()).unwrap();
      let wtx = db.begin_write().unwrap();

      let brc20_data_store = brc20_db::DataStore::new(&wtx);
      let brc20s_data_store = brc20s_db::DataStore::new(&wtx);

      let addr = "bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e";

      let stake = "btc1";
      let result = prepare_env_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        addr,
        stake,
        "ord",
        0b1001,
      );
      let (infos, from_script) = match result {
        Ok(r) => r,
        Err(e) => {
          panic!("err:{}", e);
        }
      };

      //withdraw 150
      let result =
        set_brc20_token_user(&brc20_data_store, stake, &from_script, 0_u128, 18_u8).err();
      assert_eq!(None, result);
      let (stake, msg) = mock_passive_unstake_msg(stake, "200", addr, addr);
      let result = execute_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        1,
        version::zebra(),
      );

      assert_eq!(
        Ok(vec![
          PassiveWithdraw(PassiveWithdrawEvent {
            pid: Pid::from_str("92c3f0f4ab#01").unwrap(),
            amt: 50000000000000000000
          }),
          PassiveWithdraw(PassiveWithdrawEvent {
            pid: Pid::from_str("83050baa2b#01").unwrap(),
            amt: 50000000000000000000
          }),
          PassiveWithdraw(PassiveWithdrawEvent {
            pid: Pid::from_str("934a4f7aff#01").unwrap(),
            amt: 50000000000000000000
          }),
          PassiveWithdraw(PassiveWithdrawEvent {
            pid: Pid::from_str("a2c6a6a614#01").unwrap(),
            amt: 50000000000000000000
          }),
        ]),
        result
      );
    }

    // 1-share(50) 2-only(50) 3-only(50) 4-share(50) transfer 50 no passwithdraw
    {
      let dbfile = NamedTempFile::new().unwrap();
      let db = Database::create(dbfile.path()).unwrap();
      let wtx = db.begin_write().unwrap();

      let brc20_data_store = brc20_db::DataStore::new(&wtx);
      let brc20s_data_store = brc20s_db::DataStore::new(&wtx);

      let addr = "bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e";

      let stake = "btc1";
      let result = prepare_env_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        addr,
        stake,
        "ord",
        0b0110,
      );
      let (infos, from_script) = match result {
        Ok(r) => r,
        Err(e) => {
          panic!("err:{}", e);
        }
      };

      //withdraw 50
      let result =
        set_brc20_token_user(&brc20_data_store, stake, &from_script, 150_u128, 18_u8).err();
      assert_eq!(None, result);
      let (stake, msg) = mock_passive_unstake_msg(stake, "50", addr, addr);
      let result = execute_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        1,
        version::zebra(),
      );

      assert_eq!(Ok(vec![]), result);
    }

    // 1-share(50) 2-only(50) 3-only(50) 4-share(50)  transfer 100  passwithdraw
    {
      let dbfile = NamedTempFile::new().unwrap();
      let db = Database::create(dbfile.path()).unwrap();
      let wtx = db.begin_write().unwrap();

      let brc20_data_store = brc20_db::DataStore::new(&wtx);
      let brc20s_data_store = brc20s_db::DataStore::new(&wtx);

      let addr = "bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e";

      let stake = "btc1";
      let result = prepare_env_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        addr,
        stake,
        "ord",
        0b0110,
      );
      let (infos, from_script) = match result {
        Ok(r) => r,
        Err(e) => {
          panic!("err:{}", e);
        }
      };

      //withdraw 100
      let result =
        set_brc20_token_user(&brc20_data_store, stake, &from_script, 100_u128, 18_u8).err();
      assert_eq!(None, result);
      let (stake, msg) = mock_passive_unstake_msg(stake, "100", addr, addr);
      let result = execute_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        1,
        version::zebra(),
      );

      assert_eq!(
        Ok(vec![
          PassiveWithdraw(PassiveWithdrawEvent {
            pid: Pid::from_str("92c3f0f4ab#01").unwrap(),
            amt: 50000000000000000000
          }),
          PassiveWithdraw(PassiveWithdrawEvent {
            pid: Pid::from_str("a2c6a6a614#01").unwrap(),
            amt: 50000000000000000000
          }),
        ]),
        result
      );
    }

    // 1-share(50) 2-only(50) 3-only(50) 4-share(50)  transfer 150  passwithdraw
    {
      let dbfile = NamedTempFile::new().unwrap();
      let db = Database::create(dbfile.path()).unwrap();
      let wtx = db.begin_write().unwrap();

      let brc20_data_store = brc20_db::DataStore::new(&wtx);
      let brc20s_data_store = brc20s_db::DataStore::new(&wtx);

      let addr = "bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e";

      let stake = "btc1";
      let result = prepare_env_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        addr,
        stake,
        "ord",
        0b0110,
      );
      let (infos, from_script) = match result {
        Ok(r) => r,
        Err(e) => {
          panic!("err:{}", e);
        }
      };

      //withdraw 150
      let result =
        set_brc20_token_user(&brc20_data_store, stake, &from_script, 50_u128, 18_u8).err();
      assert_eq!(None, result);
      let (stake, msg) = mock_passive_unstake_msg(stake, "150", addr, addr);
      let result = execute_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        1,
        version::zebra(),
      );

      assert_eq!(
        Ok(vec![
          PassiveWithdraw(PassiveWithdrawEvent {
            pid: Pid::from_str("92c3f0f4ab#01").unwrap(),
            amt: 50000000000000000000
          }),
          PassiveWithdraw(PassiveWithdrawEvent {
            pid: Pid::from_str("83050baa2b#01").unwrap(),
            amt: 50000000000000000000
          }),
          PassiveWithdraw(PassiveWithdrawEvent {
            pid: Pid::from_str("a2c6a6a614#01").unwrap(),
            amt: 50000000000000000000
          }),
        ]),
        result
      );
    }

    // 1-share(50) 2-only(50) 3-only(50) 4-share(50)  transfer 200  passwithdraw
    {
      let dbfile = NamedTempFile::new().unwrap();
      let db = Database::create(dbfile.path()).unwrap();
      let wtx = db.begin_write().unwrap();

      let brc20_data_store = brc20_db::DataStore::new(&wtx);
      let brc20s_data_store = brc20s_db::DataStore::new(&wtx);

      let addr = "bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e";

      let stake = "btc1";
      let result = prepare_env_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        addr,
        stake,
        "ord",
        0b1001,
      );
      let (infos, from_script) = match result {
        Ok(r) => r,
        Err(e) => {
          panic!("err:{}", e);
        }
      };

      //withdraw 150
      let result =
        set_brc20_token_user(&brc20_data_store, stake, &from_script, 0_u128, 18_u8).err();
      assert_eq!(None, result);
      let (stake, msg) = mock_passive_unstake_msg(stake, "200", addr, addr);
      let result = execute_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        1,
        version::zebra(),
      );

      assert_eq!(
        Ok(vec![
          PassiveWithdraw(PassiveWithdrawEvent {
            pid: Pid::from_str("92c3f0f4ab#01").unwrap(),
            amt: 50000000000000000000
          }),
          PassiveWithdraw(PassiveWithdrawEvent {
            pid: Pid::from_str("83050baa2b#01").unwrap(),
            amt: 50000000000000000000
          }),
          PassiveWithdraw(PassiveWithdrawEvent {
            pid: Pid::from_str("934a4f7aff#01").unwrap(),
            amt: 50000000000000000000
          }),
          PassiveWithdraw(PassiveWithdrawEvent {
            pid: Pid::from_str("a2c6a6a614#01").unwrap(),
            amt: 50000000000000000000
          }),
        ]),
        result
      );
    }

    // 1-only(50) 2-only(50) 3-only(50) 4-only(50) transfer 50 no passwithdraw
    {
      let dbfile = NamedTempFile::new().unwrap();
      let db = Database::create(dbfile.path()).unwrap();
      let wtx = db.begin_write().unwrap();

      let brc20_data_store = brc20_db::DataStore::new(&wtx);
      let brc20s_data_store = brc20s_db::DataStore::new(&wtx);

      let addr = "bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e";

      let stake = "btc1";
      let result = prepare_env_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        addr,
        stake,
        "ord",
        0b1111,
      );
      let (infos, from_script) = match result {
        Ok(r) => r,
        Err(e) => {
          panic!("err:{}", e);
        }
      };

      //withdraw 50
      let result =
        set_brc20_token_user(&brc20_data_store, stake, &from_script, 150_u128, 18_u8).err();
      assert_eq!(None, result);
      let (stake, msg) = mock_passive_unstake_msg(stake, "50", addr, addr);
      let result = execute_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        1,
        version::zebra(),
      );

      assert_eq!(
        Ok(vec![PassiveWithdraw(PassiveWithdrawEvent {
          pid: Pid::from_str("92c3f0f4ab#01").unwrap(),
          amt: 50000000000000000000
        })]),
        result
      );
    }

    // 1-only(50) 2-only(50) 3-only(50) 4-only(50)  transfer 100  passwithdraw
    {
      let dbfile = NamedTempFile::new().unwrap();
      let db = Database::create(dbfile.path()).unwrap();
      let wtx = db.begin_write().unwrap();

      let brc20_data_store = brc20_db::DataStore::new(&wtx);
      let brc20s_data_store = brc20s_db::DataStore::new(&wtx);

      let addr = "bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e";

      let stake = "btc1";
      let result = prepare_env_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        addr,
        stake,
        "ord",
        0b1111,
      );
      let (infos, from_script) = match result {
        Ok(r) => r,
        Err(e) => {
          panic!("err:{}", e);
        }
      };

      //withdraw 100
      let result =
        set_brc20_token_user(&brc20_data_store, stake, &from_script, 100_u128, 18_u8).err();
      assert_eq!(None, result);
      let (stake, msg) = mock_passive_unstake_msg(stake, "100", addr, addr);
      let result = execute_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        1,
        version::zebra(),
      );

      assert_eq!(
        Ok(vec![
          PassiveWithdraw(PassiveWithdrawEvent {
            pid: Pid::from_str("92c3f0f4ab#01").unwrap(),
            amt: 50000000000000000000
          }),
          PassiveWithdraw(PassiveWithdrawEvent {
            pid: Pid::from_str("83050baa2b#01").unwrap(),
            amt: 50000000000000000000
          }),
        ]),
        result
      );
    }

    // 1-only(50) 2-only(50) 3-only(50) 4-only(50)  transfer 150  passwithdraw
    {
      let dbfile = NamedTempFile::new().unwrap();
      let db = Database::create(dbfile.path()).unwrap();
      let wtx = db.begin_write().unwrap();

      let brc20_data_store = brc20_db::DataStore::new(&wtx);
      let brc20s_data_store = brc20s_db::DataStore::new(&wtx);

      let addr = "bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e";

      let stake = "btc1";
      let result = prepare_env_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        addr,
        stake,
        "ord",
        0b1111,
      );
      let (infos, from_script) = match result {
        Ok(r) => r,
        Err(e) => {
          panic!("err:{}", e);
        }
      };

      //withdraw 150
      let result =
        set_brc20_token_user(&brc20_data_store, stake, &from_script, 50_u128, 18_u8).err();
      assert_eq!(None, result);
      let (stake, msg) = mock_passive_unstake_msg(stake, "150", addr, addr);
      let result = execute_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        1,
        version::zebra(),
      );

      assert_eq!(
        Ok(vec![
          PassiveWithdraw(PassiveWithdrawEvent {
            pid: Pid::from_str("92c3f0f4ab#01").unwrap(),
            amt: 50000000000000000000
          }),
          PassiveWithdraw(PassiveWithdrawEvent {
            pid: Pid::from_str("83050baa2b#01").unwrap(),
            amt: 50000000000000000000
          }),
          PassiveWithdraw(PassiveWithdrawEvent {
            pid: Pid::from_str("934a4f7aff#01").unwrap(),
            amt: 50000000000000000000
          }),
        ]),
        result
      );
    }

    // 1-only(50) 2-only(50) 3-only(50) 4-only(50)  transfer 200  passwithdraw
    {
      let dbfile = NamedTempFile::new().unwrap();
      let db = Database::create(dbfile.path()).unwrap();
      let wtx = db.begin_write().unwrap();

      let brc20_data_store = brc20_db::DataStore::new(&wtx);
      let brc20s_data_store = brc20s_db::DataStore::new(&wtx);

      let addr = "bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e";

      let stake = "btc1";
      let result = prepare_env_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        addr,
        stake,
        "ord",
        0b1111,
      );
      let (infos, from_script) = match result {
        Ok(r) => r,
        Err(e) => {
          panic!("err:{}", e);
        }
      };

      //withdraw 150
      let result =
        set_brc20_token_user(&brc20_data_store, stake, &from_script, 0_u128, 18_u8).err();
      assert_eq!(None, result);
      let (stake, msg) = mock_passive_unstake_msg(stake, "200", addr, addr);
      let result = execute_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        1,
        version::zebra(),
      );

      assert_eq!(
        Ok(vec![
          PassiveWithdraw(PassiveWithdrawEvent {
            pid: Pid::from_str("92c3f0f4ab#01").unwrap(),
            amt: 50000000000000000000
          }),
          PassiveWithdraw(PassiveWithdrawEvent {
            pid: Pid::from_str("83050baa2b#01").unwrap(),
            amt: 50000000000000000000
          }),
          PassiveWithdraw(PassiveWithdrawEvent {
            pid: Pid::from_str("934a4f7aff#01").unwrap(),
            amt: 50000000000000000000
          }),
          PassiveWithdraw(PassiveWithdrawEvent {
            pid: Pid::from_str("a2c6a6a614#01").unwrap(),
            amt: 50000000000000000000
          }),
        ]),
        result
      );
    }

    // 1-share(50) 2-share(50) 3-share(50) 4-share(50) transfer 50 no passwithdraw
    {
      let dbfile = NamedTempFile::new().unwrap();
      let db = Database::create(dbfile.path()).unwrap();
      let wtx = db.begin_write().unwrap();

      let brc20_data_store = brc20_db::DataStore::new(&wtx);
      let brc20s_data_store = brc20s_db::DataStore::new(&wtx);

      let addr = "bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e";

      let stake = "btc1";
      let result = prepare_env_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        addr,
        stake,
        "ord",
        0b0000,
      );
      let (infos, from_script) = match result {
        Ok(r) => r,
        Err(e) => {
          panic!("err:{}", e);
        }
      };

      //withdraw 50
      let result =
        set_brc20_token_user(&brc20_data_store, stake, &from_script, 150_u128, 18_u8).err();
      assert_eq!(None, result);
      let (stake, msg) = mock_passive_unstake_msg(stake, "50", addr, addr);
      let result = execute_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        1,
        version::zebra(),
      );

      assert_eq!(Ok(vec![]), result);
    }

    // 1-share(50) 2-share(50) 3-share(50) 4-share(50)  transfer 100  passwithdraw
    {
      let dbfile = NamedTempFile::new().unwrap();
      let db = Database::create(dbfile.path()).unwrap();
      let wtx = db.begin_write().unwrap();

      let brc20_data_store = brc20_db::DataStore::new(&wtx);
      let brc20s_data_store = brc20s_db::DataStore::new(&wtx);

      let addr = "bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e";

      let stake = "btc1";
      let result = prepare_env_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        addr,
        stake,
        "ord",
        0b0000,
      );
      let (infos, from_script) = match result {
        Ok(r) => r,
        Err(e) => {
          panic!("err:{}", e);
        }
      };

      //withdraw 100
      let result =
        set_brc20_token_user(&brc20_data_store, stake, &from_script, 100_u128, 18_u8).err();
      assert_eq!(None, result);
      let (stake, msg) = mock_passive_unstake_msg(stake, "100", addr, addr);
      let result = execute_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        1,
        version::zebra(),
      );

      assert_eq!(Ok(vec![]), result);
    }

    // 1-share(50) 2-share(50) 3-share(50) 4-share(50)  transfer 150  passwithdraw
    {
      let dbfile = NamedTempFile::new().unwrap();
      let db = Database::create(dbfile.path()).unwrap();
      let wtx = db.begin_write().unwrap();

      let brc20_data_store = brc20_db::DataStore::new(&wtx);
      let brc20s_data_store = brc20s_db::DataStore::new(&wtx);

      let addr = "bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e";

      let stake = "btc1";
      let result = prepare_env_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        addr,
        stake,
        "ord",
        0b0000,
      );
      let (infos, from_script) = match result {
        Ok(r) => r,
        Err(e) => {
          panic!("err:{}", e);
        }
      };

      //withdraw 150
      let result =
        set_brc20_token_user(&brc20_data_store, stake, &from_script, 50_u128, 18_u8).err();
      assert_eq!(None, result);
      let (stake, msg) = mock_passive_unstake_msg(stake, "150", addr, addr);
      let result = execute_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        1,
        version::zebra(),
      );

      assert_eq!(Ok(vec![]), result);
    }

    // 1-share(50) 2-share(50) 3-share(50) 4-share(50)  transfer 200  passwithdraw
    {
      let dbfile = NamedTempFile::new().unwrap();
      let db = Database::create(dbfile.path()).unwrap();
      let wtx = db.begin_write().unwrap();

      let brc20_data_store = brc20_db::DataStore::new(&wtx);
      let brc20s_data_store = brc20s_db::DataStore::new(&wtx);

      let addr = "bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e";

      let stake = "btc1";
      let result = prepare_env_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        addr,
        stake,
        "ord",
        0b0000,
      );
      let (infos, from_script) = match result {
        Ok(r) => r,
        Err(e) => {
          panic!("err:{}", e);
        }
      };

      //withdraw 150
      let result =
        set_brc20_token_user(&brc20_data_store, stake, &from_script, 0_u128, 18_u8).err();
      assert_eq!(None, result);
      let (stake, msg) = mock_passive_unstake_msg(stake, "200", addr, addr);
      let result = execute_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        1,
        version::zebra(),
      );

      assert_eq!(
        Ok(vec![
          PassiveWithdraw(PassiveWithdrawEvent {
            pid: Pid::from_str("92c3f0f4ab#01").unwrap(),
            amt: 50000000000000000000
          }),
          PassiveWithdraw(PassiveWithdrawEvent {
            pid: Pid::from_str("83050baa2b#01").unwrap(),
            amt: 50000000000000000000
          }),
          PassiveWithdraw(PassiveWithdrawEvent {
            pid: Pid::from_str("934a4f7aff#01").unwrap(),
            amt: 50000000000000000000
          }),
          PassiveWithdraw(PassiveWithdrawEvent {
            pid: Pid::from_str("a2c6a6a614#01").unwrap(),
            amt: 50000000000000000000
          }),
        ]),
        result
      );
    }
  }

  #[test]
  fn test_process_passive_for_bench() {
    let dbfile = NamedTempFile::new().unwrap();
    let db = Database::create(dbfile.path()).unwrap();
    let wtx = db.begin_write().unwrap();

    let brc20_data_store = brc20_db::DataStore::new(&wtx);
    let brc20s_data_store = brc20s_db::DataStore::new(&wtx);

    let addr = "bc1pgllnmtxs0g058qz7c6qgaqq4qknwrqj9z7rqn9e2dzhmcfmhlu4sfadf5e";
    let new_addr = "bc1pvk535u5eedhsx75r7mfvdru7t0kcr36mf9wuku7k68stc0ncss8qwzeahv";
    let (deploy, msg) = mock_deploy_msg(
      "pool", "01", "btc1", "ordi1", "10", "12000000", "21000000", 18, true, addr, addr,
    );
    let stake_tick = deploy.get_stake_id();
    let from_script = msg.from.clone();
    let to_script = msg.to.clone().unwrap();
    let result =
      set_brc20_token_user(&brc20_data_store, "btc1", &msg.from, 2000000_u128, 18_u8).err();
    assert_eq!(None, result);

    let number = 128_u64;
    for i in 1..number {
      let brc20s_tick = format!("ord{}", i);
      let mut enc = sha256::Hash::engine();
      enc.input(brc20s_tick.as_bytes());
      let hash = sha256::Hash::from_engine(enc);
      let (deploy, msg) = mock_deploy_msg(
        "pool",
        "01",
        "btc1",
        &hash.to_string()[..(2 * 3)],
        "10",
        "12000000",
        "21000000",
        18,
        true,
        addr,
        addr,
      );
      let result = execute_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        0,
        version::koala(),
      );
      assert_eq!(None, result.err());

      let pid_only1 = deploy.get_pool_id();
      let (stake, msg) = mock_stake_msg(pid_only1.as_str(), "1", addr, addr);
      let result = execute_for_test(
        &brc20_data_store,
        &brc20s_data_store,
        &msg,
        100,
        version::koala(),
      );
      assert_eq!(None, result.err());
    }
    let result = brc20s_data_store
      .get_user_stakeinfo(&to_script, &stake_tick)
      .unwrap();
    match result {
      Some(info) => println!("stakeinfo len:{}", info.pool_stakes.len()),
      None => println!("stakeinfo no len"),
    };

    let result = set_brc20_token_user(&brc20_data_store, "btc1", &from_script, 0_u128, 18_u8).err();
    assert_eq!(None, result);
    let fmt = "%Y年%m月%d日 %H:%M:%S";
    let old = Local::now();

    let (stake, msg) = mock_passive_unstake_msg("btc1", "1", addr, addr);
    let result = execute_for_test(
      &brc20_data_store,
      &brc20s_data_store,
      &msg,
      1,
      version::koala(),
    );
    assert_eq!(None, result.err());
    let now = Local::now().sub(old);
    print!("\nend:{}\n", now);
  }
}
