use std::collections::HashMap;
use std::ops::Deref;

use anchor_client::solana_client::rpc_config::RpcSendTransactionConfig;

use anchor_client::solana_sdk::clock::Clock;
use anchor_client::solana_sdk::compute_budget::ComputeBudgetInstruction;
use anchor_client::solana_sdk::instruction::Instruction;
use anchor_client::solana_sdk::sysvar::SysvarId;
use anchor_client::{solana_sdk::pubkey::Pubkey, solana_sdk::signer::Signer, Program};
use anchor_lang::solana_program::instruction::AccountMeta;
use anchor_lang::AccountDeserialize;
use anchor_spl::associated_token::get_associated_token_address;

use anyhow::*;
use commons::quote::{get_bin_array_pubkeys_for_swap, quote_exact_in};
use lb_clmm::accounts;
use lb_clmm::constants::BASIS_POINT_MAX;
use lb_clmm::instruction;

use lb_clmm::state::bin::BinArray;
use lb_clmm::state::bin_array_bitmap_extension::{self, BinArrayBitmapExtension};
use lb_clmm::state::lb_pair::{hack, LbPair, RewardInfo};
use lb_clmm::utils::pda::*;

#[derive(Debug)]
pub struct SwapExactInParameters {
    pub lb_pair: Pubkey,
    pub amount_in: u64,
    pub swap_for_y: bool,
}

pub async fn swap<C: Deref<Target = impl Signer> + Clone>(
    params: SwapExactInParameters,
    program: &Program<C>,
    transaction_config: RpcSendTransactionConfig,
) -> Result<()> {
    let SwapExactInParameters {
        amount_in,
        lb_pair,
        swap_for_y,
    } = params;

    let lb_pair_state: LbPair = program.account(lb_pair).await?;

    let (user_token_in, user_token_out) = if swap_for_y {
        (
            get_associated_token_address(&program.payer(), &lb_pair_state.token_x_mint),
            get_associated_token_address(&program.payer(), &lb_pair_state.token_y_mint),
        )
    } else {
        (
            get_associated_token_address(&program.payer(), &lb_pair_state.token_y_mint),
            get_associated_token_address(&program.payer(), &lb_pair_state.token_x_mint),
        )
    };

    let (bitmap_extension_key, _bump) = derive_bin_array_bitmap_extension(lb_pair);

    let bitmap_extension = program
        .account::<BinArrayBitmapExtension>(bitmap_extension_key)
        .await
        .ok();

    let bin_arrays_for_swap = get_bin_array_pubkeys_for_swap(
        lb_pair,
        &lb_pair_state,
        bitmap_extension.as_ref(),
        swap_for_y,
        3,
    )?;

    let bin_arrays = program
        .async_rpc()
        .get_multiple_accounts(&bin_arrays_for_swap)
        .await?
        .into_iter()
        .zip(bin_arrays_for_swap.iter())
        .map(|(account, &key)| {
            let account = account?;
            Some((
                key,
                BinArray::try_deserialize(&mut account.data.as_ref()).ok()?,
            ))
        })
        .collect::<Option<HashMap<Pubkey, BinArray>>>()
        .context("Failed to fetch bin arrays")?;

    let clock = program
        .async_rpc()
        .get_account(&Clock::id())
        .await
        .map(|account| {
            let clock: Clock = bincode::deserialize(account.data.as_ref())?;
            Ok(clock)
        })??;

    let quote = quote_exact_in(
        lb_pair,
        &lb_pair_state,
        amount_in,
        swap_for_y,
        bin_arrays,
        bitmap_extension.as_ref(),
        clock.unix_timestamp as u64,
        clock.slot,
    )?;

    let (event_authority, _bump) =
        Pubkey::find_program_address(&[b"__event_authority"], &lb_clmm::ID);

    let accounts = accounts::Swap {
        lb_pair,
        bin_array_bitmap_extension: bitmap_extension
            .map(|_| bitmap_extension_key)
            .or(Some(lb_clmm::ID)),
        reserve_x: lb_pair_state.reserve_x,
        reserve_y: lb_pair_state.reserve_y,
        token_x_mint: lb_pair_state.token_x_mint,
        token_y_mint: lb_pair_state.token_y_mint,
        token_x_program: anchor_spl::token::ID,
        token_y_program: anchor_spl::token::ID,
        user: program.payer(),
        user_token_in,
        user_token_out,
        oracle: lb_pair_state.oracle,
        host_fee_in: Some(lb_clmm::ID),
        event_authority,
        program: lb_clmm::ID,
    };

    // 100 bps slippage
    let min_amount_out = quote.amount_out * 9900 / BASIS_POINT_MAX as u64;

    let ix = instruction::Swap {
        amount_in,
        min_amount_out,
    };

    let remaining_accounts = bin_arrays_for_swap
        .into_iter()
        .map(|key| AccountMeta::new(key, false))
        .collect::<Vec<_>>();

    let compute_budget_ix = ComputeBudgetInstruction::set_compute_unit_limit(1_400_000);

    let request_builder = program.request();
    let signature = request_builder
        .instruction(compute_budget_ix)
        .accounts(accounts)
        .accounts(remaining_accounts)
        .args(ix)
        .send_with_spinner_and_config(transaction_config)
        .await;

    println!("Swap. Signature: {:#?}", signature);

    signature?;

    Ok(())
}

pub async fn swap_exact_in_instructions<C: Deref<Target = impl Signer> + Clone>(
    params: SwapExactInParameters,
    program: &Program<C>,
) -> Result<Vec<Instruction>> {
    let SwapExactInParameters {
        amount_in,
        lb_pair,
        swap_for_y,
    } = params;

    // MI
    // let lb_pair_state: LbPair = program.account(lb_pair).await?;
    // let hack_lb_pair_state: hack::LbPair = program.account(lb_pair).await?;

    let data_bytes = program.async_rpc().get_account_data(&lb_pair).await?;
    assert_eq!(data_bytes.len(), 904);
    let hack_lb_pair_state = hack::LbPair::try_from_bytes(&data_bytes[8..])?;

    let mut lb_pair_state: LbPair = LbPair::default();

    let hack_reward_info_0 = hack_lb_pair_state.reward_infos[0];
    let hack_reward_info_1 = hack_lb_pair_state.reward_infos[1];

    let mut reward_info_0: RewardInfo = RewardInfo::default();
    let mut reward_info_1: RewardInfo = RewardInfo::default();

    reward_info_0.mint = hack_reward_info_0.mint;
    reward_info_0.vault = hack_reward_info_0.vault;
    reward_info_0.funder = hack_reward_info_0.funder;
    reward_info_0.reward_duration = hack_reward_info_0.reward_duration;
    reward_info_0.reward_duration_end = hack_reward_info_0.reward_duration_end;
    reward_info_0.reward_rate = hack_reward_info_0.reward_rate.as_u128();
    reward_info_0.last_update_time = hack_reward_info_0.last_update_time;
    reward_info_0.cumulative_seconds_with_empty_liquidity_reward =
        hack_reward_info_0.cumulative_seconds_with_empty_liquidity_reward;

    reward_info_1.mint = hack_reward_info_1.mint;
    reward_info_1.vault = hack_reward_info_1.vault;
    reward_info_1.funder = hack_reward_info_1.funder;
    reward_info_1.reward_duration = hack_reward_info_1.reward_duration;
    reward_info_1.reward_duration_end = hack_reward_info_1.reward_duration_end;
    reward_info_1.reward_rate = hack_reward_info_1.reward_rate.as_u128();
    reward_info_1.last_update_time = hack_reward_info_1.last_update_time;
    reward_info_1.cumulative_seconds_with_empty_liquidity_reward =
        hack_reward_info_1.cumulative_seconds_with_empty_liquidity_reward;

    lb_pair_state.parameters = hack_lb_pair_state.parameters;
    lb_pair_state.v_parameters = hack_lb_pair_state.v_parameters;
    lb_pair_state.bump_seed = hack_lb_pair_state.bump_seed;
    lb_pair_state.bin_step_seed = hack_lb_pair_state.bin_step_seed;
    lb_pair_state.pair_type = hack_lb_pair_state.pair_type;
    lb_pair_state.active_id = hack_lb_pair_state.active_id;
    lb_pair_state.bin_step = hack_lb_pair_state.bin_step;
    lb_pair_state.status = hack_lb_pair_state.status;
    lb_pair_state.require_base_factor_seed = hack_lb_pair_state.require_base_factor_seed;
    lb_pair_state.base_factor_seed = hack_lb_pair_state.base_factor_seed;
    lb_pair_state.activation_type = hack_lb_pair_state.activation_type;
    lb_pair_state._padding_0 = hack_lb_pair_state._padding_0;
    lb_pair_state.token_x_mint = hack_lb_pair_state.token_x_mint;
    lb_pair_state.token_y_mint = hack_lb_pair_state.token_y_mint;
    lb_pair_state.reserve_x = hack_lb_pair_state.reserve_x;
    lb_pair_state.reserve_y = hack_lb_pair_state.reserve_y;
    lb_pair_state.protocol_fee = hack_lb_pair_state.protocol_fee;
    lb_pair_state._padding_1 = hack_lb_pair_state._padding_1;
    lb_pair_state.reward_infos = [reward_info_0, reward_info_1];
    lb_pair_state.oracle = hack_lb_pair_state.oracle;
    lb_pair_state.bin_array_bitmap = hack_lb_pair_state.bin_array_bitmap;
    lb_pair_state.last_updated_at = hack_lb_pair_state.last_updated_at;
    lb_pair_state._padding_2 = hack_lb_pair_state._padding_2;
    lb_pair_state.pre_activation_swap_address = hack_lb_pair_state.pre_activation_swap_address;
    lb_pair_state.base_key = hack_lb_pair_state.base_key;
    lb_pair_state.activation_point = hack_lb_pair_state.activation_point;
    lb_pair_state.pre_activation_duration = hack_lb_pair_state.pre_activation_duration;
    lb_pair_state._padding_3 = hack_lb_pair_state._padding_3;
    lb_pair_state._padding_4 = hack_lb_pair_state._padding_4;
    lb_pair_state.creator = hack_lb_pair_state.creator;
    lb_pair_state._reserved = hack_lb_pair_state._reserved;
    // End copy
    println!("Pass through lb_pair_state workaround copy");

    let (user_token_in, user_token_out) = if swap_for_y {
        (
            get_associated_token_address(&program.payer(), &lb_pair_state.token_x_mint),
            get_associated_token_address(&program.payer(), &lb_pair_state.token_y_mint),
        )
    } else {
        (
            get_associated_token_address(&program.payer(), &lb_pair_state.token_y_mint),
            get_associated_token_address(&program.payer(), &lb_pair_state.token_x_mint),
        )
    };

    let (bitmap_extension_key, _bump) = derive_bin_array_bitmap_extension(lb_pair);

    println!(
        "derived bitmap_extension_key: {} from lb_pair: {}",
        &bitmap_extension_key, &lb_pair
    );

    // MI: use hack way
    // let bitmap_extension = program
    //     .account::<BinArrayBitmapExtension>(bitmap_extension_key)
    //     .await
    //     .ok();

    let data_bytes = program
        .async_rpc()
        .get_account_data(&bitmap_extension_key)
        .await?;
    println!(
        "Pass through get_account_data of bitmap_extension_key: {}",
        &bitmap_extension_key
    );

    let hack_bitmap_extension =
        bin_array_bitmap_extension::hack::BinArrayBitmapExtension::try_from_bytes(
            &data_bytes[8..],
        )?;

    let mut bitmap_extension: BinArrayBitmapExtension = BinArrayBitmapExtension::default();
    bitmap_extension.lb_pair = hack_bitmap_extension.lb_pair;
    bitmap_extension.positive_bin_array_bitmap = hack_bitmap_extension.positive_bin_array_bitmap;
    bitmap_extension.negative_bin_array_bitmap = hack_bitmap_extension.negative_bin_array_bitmap;

    let bitmap_extension = Some(bitmap_extension);

    let bin_arrays_for_swap = get_bin_array_pubkeys_for_swap(
        lb_pair,
        &lb_pair_state,
        bitmap_extension.as_ref(),
        swap_for_y,
        3,
    )?;

    let bin_arrays = program
        .async_rpc()
        .get_multiple_accounts(&bin_arrays_for_swap)
        .await?
        .into_iter()
        .zip(bin_arrays_for_swap.iter())
        .map(|(account, &key)| {
            let account = account?;
            Some((
                key,
                BinArray::try_deserialize(&mut account.data.as_ref()).ok()?,
            ))
        })
        .collect::<Option<HashMap<Pubkey, BinArray>>>()
        .context("Failed to fetch bin arrays")?;

    let clock = program
        .async_rpc()
        .get_account(&Clock::id())
        .await
        .map(|account| {
            let clock: Clock = bincode::deserialize(account.data.as_ref())?;
            Ok(clock)
        })??;

    let quote = quote_exact_in(
        lb_pair,
        &lb_pair_state,
        amount_in,
        swap_for_y,
        bin_arrays,
        bitmap_extension.as_ref(),
        clock.unix_timestamp as u64,
        clock.slot,
    )?;

    let (event_authority, _bump) =
        Pubkey::find_program_address(&[b"__event_authority"], &lb_clmm::ID);

    let accounts = accounts::Swap {
        lb_pair,
        bin_array_bitmap_extension: bitmap_extension
            .map(|_| bitmap_extension_key)
            .or(Some(lb_clmm::ID)),
        reserve_x: lb_pair_state.reserve_x,
        reserve_y: lb_pair_state.reserve_y,
        token_x_mint: lb_pair_state.token_x_mint,
        token_y_mint: lb_pair_state.token_y_mint,
        token_x_program: anchor_spl::token::ID,
        token_y_program: anchor_spl::token::ID,
        user: program.payer(),
        user_token_in,
        user_token_out,
        oracle: lb_pair_state.oracle,
        host_fee_in: Some(lb_clmm::ID),
        event_authority,
        program: lb_clmm::ID,
    };

    // 100 bps slippage
    let min_amount_out = quote.amount_out * 9900 / BASIS_POINT_MAX as u64;

    let ix = instruction::Swap {
        amount_in,
        min_amount_out,
    };

    let remaining_accounts = bin_arrays_for_swap
        .into_iter()
        .map(|key| AccountMeta::new(key, false))
        .collect::<Vec<_>>();

    // let compute_budget_ix = ComputeBudgetInstruction::set_compute_unit_limit(1_400_000);

    let request_builder = program.request();
    let instructions = request_builder
        // .instruction(compute_budget_ix)
        .accounts(accounts)
        .accounts(remaining_accounts)
        .args(ix)
        .instructions();

    Ok(instructions?)
}
