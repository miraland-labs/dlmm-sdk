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

use lb_clmm::state::bin::{self, Bin, BinArray};
use lb_clmm::state::bin_array_bitmap_extension::{self, BinArrayBitmapExtension};
use lb_clmm::state::lb_pair::{self, LbPair, RewardInfo};
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

    // let lb_pair_state: LbPair = program.account(lb_pair).await?;

    // MI
    let data_bytes = program.async_rpc().get_account_data(&lb_pair).await?;
    assert_eq!(data_bytes.len(), 904);
    let hack_lb_pair_state = lb_pair::hack::LbPair::try_from_bytes(&data_bytes[8..])?;

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

    // MI added
    let (_user_mint_in, user_mint_out) = if swap_for_y {
        (lb_pair_state.token_x_mint, lb_pair_state.token_y_mint)
    } else {
        (lb_pair_state.token_y_mint, lb_pair_state.token_x_mint)
    };

    let (bitmap_extension_key, _bump) = derive_bin_array_bitmap_extension(lb_pair);

    // MI
    println!(
        "derived bitmap_extension_key: {} from lb_pair: {}",
        &bitmap_extension_key, &lb_pair
    );

    // let bitmap_extension = program
    //     .account::<BinArrayBitmapExtension>(bitmap_extension_key)
    //     .await
    //     .ok();

    // MI: use hack way
    let data_bytes = program
        .async_rpc()
        .get_account_data(&bitmap_extension_key)
        .await
        .ok();

    println!(
        "Pass through get_account_data of bitmap_extension_key: {} and date_bytes is {:#?}",
        &bitmap_extension_key, data_bytes
    );

    let bitmap_extension = if data_bytes.is_some() {
        let exact_data_bytes = &data_bytes.unwrap()[8..];
        let hack_bitmap_extension =
            bin_array_bitmap_extension::hack::BinArrayBitmapExtension::try_from_bytes(
                &exact_data_bytes,
            )?;

        let mut bitmap_extension: BinArrayBitmapExtension = BinArrayBitmapExtension::default();
        bitmap_extension.lb_pair = hack_bitmap_extension.lb_pair;
        bitmap_extension.positive_bin_array_bitmap =
            hack_bitmap_extension.positive_bin_array_bitmap;
        bitmap_extension.negative_bin_array_bitmap =
            hack_bitmap_extension.negative_bin_array_bitmap;

        Some(bitmap_extension)
    } else {
        None
    };

    println!("Pass through fetching bitmap_extension account");

    let bin_arrays_for_swap = get_bin_array_pubkeys_for_swap(
        lb_pair,
        &lb_pair_state,
        bitmap_extension.as_ref(),
        swap_for_y,
        3,
    )?;

    println!("Pass through getting bin_arrays_for_swap");

    let bin_arrays = program
        .async_rpc()
        .get_multiple_accounts(&bin_arrays_for_swap)
        .await?
        .into_iter()
        .zip(bin_arrays_for_swap.iter())
        .map(|(account, &key)| {
            let account = account?;

            // MI
            let data_bytes = account.data;
            let hack_bin_array =
                bin::hack::BinArray::try_from_bytes(&data_bytes[8..]).expect("should be bin array");

            let bin_array: BinArray = BinArray {
                index: hack_bin_array.index,
                version: hack_bin_array.version,
                _padding: hack_bin_array._padding,
                lb_pair: hack_bin_array.lb_pair,
                bins: hack_bin_array
                    .bins
                    .iter()
                    .map(|b| Bin {
                        amount_x: b.amount_x,
                        amount_y: b.amount_y,
                        price: b.price.as_u128(),
                        liquidity_supply: b.liquidity_supply.as_u128(),
                        reward_per_token_stored: [
                            b.reward_per_token_stored[0].as_u128(),
                            b.reward_per_token_stored[1].as_u128(),
                        ],
                        fee_amount_x_per_token_stored: b.fee_amount_x_per_token_stored.as_u128(),
                        fee_amount_y_per_token_stored: b.fee_amount_y_per_token_stored.as_u128(),
                        amount_x_in: b.amount_x_in.as_u128(),
                        amount_y_in: b.amount_y_in.as_u128(),
                    })
                    .collect::<Vec<_>>()
                    .try_into()
                    .unwrap(),
            };

            Some((
                key,
                // BinArray::try_deserialize(&mut account.data.as_ref()).ok()?,
                // *BinArray::try_from_bytes(&account.data).ok()?, // MI
                bin_array,
            ))
        })
        .collect::<Option<HashMap<Pubkey, BinArray>>>()
        .context("Failed to fetch bin arrays")?;

    println!("Pass through getting bin_arrays");

    let clock = program
        .async_rpc()
        .get_account(&Clock::id())
        .await
        .map(|account| {
            let clock: Clock = bincode::deserialize(account.data.as_ref())?;
            Ok(clock)
        })??;

    println!("Pass through getting clock");

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

    println!("Pass through getting quote with quote_exact_in");

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

    println!("Pass through getting accounts with accounts::Swap");

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
    let mut is_creating_ata = false;
    let mut ata_ix = Vec::new();
    // let user_output_token_account = spl_associated_token_account::get_associated_token_address(
    //     &program.payer(),
    //     &user_mint_out,
    // );
    if let std::result::Result::Ok(response) = program
        .async_rpc()
        .get_token_account_balance(&user_token_out)
        .await
    {
        if let Some(_amount) = response.ui_amount {
            // println!("payer has valid token account.");
        } else {
            // println!("will create token account for payer");
            is_creating_ata = true;
            // ata_ix = spl_associated_token_account::instruction::create_associated_token_account(
            //     &program.payer(),
            //     &program.payer(),
            //     &user_token_out,
            //     &anchor_spl::token::ID,
            // );
            ata_ix = create_ata_token_or_not(
                &program.payer(),
                &program.payer(),
                &user_mint_out,
                Some(&anchor_spl::token::ID),
            );
        }
    } else {
        // println!("Adding create ata ix for payer");
        is_creating_ata = true;
        // ata_ix = spl_associated_token_account::instruction::create_associated_token_account(
        //     &program.payer(),
        //     &program.payer(),
        //     &user_token_out,
        //     &anchor_spl::token::ID,
        // );
        ata_ix = create_ata_token_or_not(
            &program.payer(),
            &program.payer(),
            &user_mint_out,
            Some(&anchor_spl::token::ID),
        );
    }

    let request_builder = program.request();

    let instructions = request_builder
        // .instruction(compute_budget_ix)
        .accounts(accounts)
        .accounts(remaining_accounts)
        .args(ix)
        .instructions()?;

    let mut ixs = Vec::new();
    if is_creating_ata {
        ixs.extend(ata_ix);
        ixs.extend(instructions);
    } else {
        ixs.extend(instructions);
    }

    Ok(ixs)
}

pub fn create_ata_token_or_not(
    funding: &Pubkey,
    owner: &Pubkey,
    mint: &Pubkey,
    token_program: Option<&Pubkey>,
) -> Vec<Instruction> {
    vec![
        spl_associated_token_account::instruction::create_associated_token_account_idempotent(
            funding,
            owner,
            mint,
            // token_program.unwrap_or(&spl_token::id()),
            token_program.unwrap_or(&anchor_spl::token::ID),
        ),
    ]
}

pub fn create_ata_token(
    funding: &Pubkey,
    owner: &Pubkey,
    mint: &Pubkey,
    token_program: Option<&Pubkey>,
) -> Vec<Instruction> {
    vec![
        spl_associated_token_account::instruction::create_associated_token_account(
            funding,
            owner,
            mint,
            // token_program.unwrap_or(&spl_token::id()),
            token_program.unwrap_or(&anchor_spl::token::ID),
        ),
    ]
}
