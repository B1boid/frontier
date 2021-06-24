// SPDX-License-Identifier: Apache-2.0
// This file is part of Frontier.
//
// Copyright (c) 2020 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! # VM Pallet
//!
//! ## Parastate Patch Notes
//! The VM pallet allow to use WasmEdge as a VM in the substrate-base blockchain.
//! This chain runs with POA consensus, such the economy is based on the tokens in the EVM.
//! Such that the calls to execute EVM with the frontier currency as gas fee are disabled.
//! Authority can set up their ethereum address in this pallet.
//!
//! The VM pallet allows unmodified VM code to be executed in a Substrate-based blockchain.
//! - [`vm::Config`]
//!
//! ## VM Engine
//!
//! The VM pallet uses [`SputnikVM`](https://github.com/rust-blockchain/evm) as the underlying EVM engine.
//! The engine is overhauled so that it's [`modular`](https://github.com/corepaper/evm).
//!
//! ## Execution Lifecycle
//!
//! There are a separate set of accounts managed by the VM pallet. Substrate based accounts can call the VM Pallet
//! to deposit or withdraw balance from the Substrate base-currency into a different balance managed and used by
//! the VM pallet. Once a user has populated their balance, they can create and call smart contracts using this pallet.
//!
//! There's one-to-one mapping from Substrate accounts and VM external accounts that is defined by a conversion function.
//!
//! ## VM Pallet vs Ethereum Network
//!
//! The VM pallet should be able to produce nearly identical results compared to the Ethereum mainnet,
//! including gas cost and balance changes.
//!
//! Observable differences include:
//!
//! - The available length of block hashes may not be 256 depending on the configuration of the System pallet
//! in the Substrate runtime.
//! - Difficulty and coinbase, which do not make sense in this pallet and is currently hard coded to zero.
//!
//! We currently do not aim to make unobservable behaviors, such as state root, to be the same. We also don't aim to follow
//! the exact same transaction / receipt format. However, given one Ethereum transaction and one Substrate account's
//! private key, one should be able to convert any Ethereum transaction into a transaction compatible with this pallet.
//!
//! The gas configurations are configurable. Right now, a pre-defined Istanbul hard fork configuration option is provided.

// Ensure we're `no_std` when compiling for Wasm.
#![cfg_attr(not(feature = "std"), no_std)]

mod tests;
pub mod runner;

pub use crate::runner::Runner;
pub use fp_vm::{
	Account, Log, Vicinity, ExecutionInfo, CallInfo, CreateInfo, Precompile,
	PrecompileSet, LinearCostPrecompile, ExtendExitReason, EVMCStatusCode
};
pub use evm::{ExitReason, ExitSucceed, ExitError, ExitRevert, ExitFatal};

use sp_std::vec::Vec;
#[cfg(feature = "std")]
use codec::{Encode, Decode};
#[cfg(feature = "std")]
use serde::{Serialize, Deserialize};
use frame_support::weights::{Weight, PostDispatchInfo};
use frame_support::traits::{Currency, ExistenceRequirement, WithdrawReasons, Imbalance, OnUnbalanced};
use frame_support::ensure;
use frame_system::RawOrigin;
use sp_core::{U256, H256, H160, Hasher};
use sp_runtime::{AccountId32, traits::{UniqueSaturatedInto, BadOrigin, Saturating}};
use evm::Config as EvmConfig;

pub use pallet::*;

#[frame_support::pallet]
pub mod pallet {
	use frame_support::pallet_prelude::*;
	use frame_system::pallet_prelude::*;
	use super::*;

	#[pallet::pallet]
	#[pallet::generate_store(pub(super) trait Store)]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config + pallet_timestamp::Config {
		/// Calculator for current gas price.
		type FeeCalculator: FeeCalculator;

		/// Maps Ethereum gas to Substrate weight.
		type GasWeightMapping: GasWeightMapping;

		/// Allow the origin to call on behalf of given address.
		type CallOrigin: EnsureAddressOrigin<Self::Origin>;
		/// Allow the origin to withdraw on behalf of given address.
		type WithdrawOrigin: EnsureAddressOrigin<Self::Origin, Success=Self::AccountId>;

		/// Mapping from address to account id.
		type AddressMapping: AddressMapping<Self::AccountId>;
		/// Currency type for withdraw and balance storage.
		type Currency: Currency<Self::AccountId>;

		/// The overarching event type.
		type Event: From<Event<Self>> + IsType<<Self as frame_system::Config>::Event>;
		/// Precompiles associated with this EVM engine.
		type Precompiles: PrecompileSet;
		/// Chain ID of EVM.
		type ChainId: Get<u64>;
		/// The block gas limit. Can be a simple constant, or an adjustment algorithm in another pallet.
		type BlockGasLimit: Get<U256>;
		/// EVM execution runner.
		type Runner: Runner<Self>;

		/// To handle fee deduction for EVM transactions. An example is this pallet being used by `pallet_ethereum`
		/// where the chain implementing `pallet_ethereum` should be able to configure what happens to the fees
		/// Similar to `OnChargeTransaction` of `pallet_transaction_payment`
		type OnChargeTransaction: OnChargeEVMTransaction<Self>;

		/// EVM config used in the pallet.
		fn config() -> &'static EvmConfig {
			&ISTANBUL_CONFIG
		}
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		#[pallet::weight(0)]
		pub fn set_eth_addr(origin: OriginFor<T>, eth_addr: H160) -> DispatchResultWithPostInfo {
			let sender = ensure_signed(origin)?;

			Self::deposit_event(Event::EthAddrSet((sender.clone(), eth_addr)));
			<EthAddrOf<T>>::insert(&sender, eth_addr);

			Ok(().into())
		}

		/// Withdraw balance from EVM into currency/balances pallet.
		#[pallet::weight(0)]
		fn withdraw(origin: OriginFor<T>, address: H160, value: BalanceOf<T>) -> DispatchResult {
			let destination = T::WithdrawOrigin::ensure_address_origin(&address, origin)?;
			let address_account_id = T::AddressMapping::into_account_id(address);

			T::Currency::transfer(
				&address_account_id,
				&destination,
				value,
				ExistenceRequirement::AllowDeath,
			)?;

			Ok(())
		}

		/// Issue an EVM call operation. This is similar to a message call transaction in Ethereum.
		#[pallet::weight(T::GasWeightMapping::gas_to_weight(*gas_limit))]
		pub(super) fn call(
			origin: OriginFor<T>,
			source: H160,
			target: H160,
			input: Vec<u8>,
			value: U256,
			gas_limit: u64,
			gas_price: U256,
			nonce: Option<U256>,
		) -> DispatchResultWithPostInfo {
			T::CallOrigin::ensure_address_origin(&source, origin)?;

			// Disable the call from polkadot.js
			#[cfg(not(feature = "debug"))]
			ensure!(false, Error::<T>::Forbidden);

			let info = T::Runner::call(
				source,
				target,
				input,
				value,
				gas_limit,
				Some(gas_price),
				nonce,
				T::config(),
			)?;

			match info.exit_reason {
				ExtendExitReason::ExitReason(ExitReason::Succeed(_)) => {
					Pallet::<T>::deposit_event(Event::<T>::Executed(target));
				},
				ExtendExitReason::EVMCStatusCode(EVMCStatusCode::EvmcSuccess) => {
					Pallet::<T>::deposit_event(Event::<T>::Executed(target));
				},
				_ => {
					Pallet::<T>::deposit_event(Event::<T>::ExecutedFailed(target));
				},
			};

			Ok(PostDispatchInfo {
				actual_weight: Some(T::GasWeightMapping::gas_to_weight(info.used_gas.unique_saturated_into())),
				pays_fee: Pays::No,
			})
		}

		/// Issue an EVM create operation. This is similar to a contract creation transaction in
		/// Ethereum.
		#[pallet::weight(T::GasWeightMapping::gas_to_weight(*gas_limit))]
		fn create(
			origin: OriginFor<T>,
			source: H160,
			init: Vec<u8>,
			value: U256,
			gas_limit: u64,
			gas_price: U256,
			nonce: Option<U256>,
		) -> DispatchResultWithPostInfo {
			T::CallOrigin::ensure_address_origin(&source, origin)?;

			// Disable the call from polkadot.js
			#[cfg(not(feature = "debug"))]
			ensure!(false, Error::<T>::Forbidden);

			let info = T::Runner::create(
				source,
				init,
				value,
				gas_limit,
				Some(gas_price),
				nonce,
				T::config(),
			)?;

			match info {
				CreateInfo {
					exit_reason: ExtendExitReason::ExitReason(ExitReason::Succeed(_)),
					value: create_address,
					..
				} => {
					Pallet::<T>::deposit_event(Event::<T>::Created(create_address));
				},
				CreateInfo {
					exit_reason: ExtendExitReason::EVMCStatusCode(EVMCStatusCode::EvmcSuccess),
					value: create_address,
					..
				} => {
					Pallet::<T>::deposit_event(Event::<T>::Created(create_address));
				},
				CreateInfo {
					exit_reason: _,
					value: create_address,
					..
				} => {
					Pallet::<T>::deposit_event(Event::<T>::CreatedFailed(create_address));
				},
			}

			Ok(PostDispatchInfo {
				actual_weight: Some(T::GasWeightMapping::gas_to_weight(info.used_gas.unique_saturated_into())),
				pays_fee: Pays::No,
			})
		}

		/// Issue an EVM create2 operation.
		#[pallet::weight(T::GasWeightMapping::gas_to_weight(*gas_limit))]
		fn create2(
			origin: OriginFor<T>,
			source: H160,
			init: Vec<u8>,
			salt: H256,
			value: U256,
			gas_limit: u64,
			gas_price: U256,
			nonce: Option<U256>,
		) -> DispatchResultWithPostInfo {
			T::CallOrigin::ensure_address_origin(&source, origin)?;

			// Disable the call from polkadot.js
			#[cfg(not(feature = "debug"))]
			ensure!(false, Error::<T>::Forbidden);

			let info = T::Runner::create2(
				source,
				init,
				salt,
				value,
				gas_limit,
				Some(gas_price),
				nonce,
				T::config(),
			)?;

			match info {
				CreateInfo {
					exit_reason: ExtendExitReason::ExitReason(ExitReason::Succeed(_)),
					value: create_address,
					..
				} => {
					Pallet::<T>::deposit_event(Event::<T>::Created(create_address));
				},
				CreateInfo {
					exit_reason: ExtendExitReason::EVMCStatusCode(EVMCStatusCode::EvmcSuccess),
					value: create_address,
					..
				} => {
					Pallet::<T>::deposit_event(Event::<T>::Created(create_address));
				},
				CreateInfo {
					exit_reason: _,
					value: create_address,
					..
				} => {
					Pallet::<T>::deposit_event(Event::<T>::CreatedFailed(create_address));
				},
			}

			Ok(PostDispatchInfo {
				actual_weight: Some(T::GasWeightMapping::gas_to_weight(info.used_gas.unique_saturated_into())),
				pays_fee: Pays::No,
			})
		}
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	#[pallet::metadata(T::AccountId = "AccountId")]
	pub enum Event<T: Config> {
		/// Setup the etherem address for block reward
		EthAddrSet((T::AccountId, H160)),
		/// Ethereum Reward to miner fail
		EthRewardFailed(H160),
		/// Ethereum events from contracts.
		Log(Log),
		/// A contract has been created at given \[address\].
		Created(H160),
		/// A \[contract\] was attempted to be created, but the execution failed.
		CreatedFailed(H160),
		/// A \[contract\] has been executed successfully with states applied.
		Executed(H160),
		/// A \[contract\] has been executed with errors. States are reverted with only gas fees applied.
		ExecutedFailed(H160),
		/// A deposit has been made at a given address. \[sender, address, value\]
		BalanceDeposit(T::AccountId, H160, U256),
		/// A withdrawal has been made from a given address. \[sender, address, value\]
		BalanceWithdraw(T::AccountId, H160, U256),
	}

	#[pallet::error]
	pub enum Error<T> {
		/// Not enough balance to perform action
		BalanceLow,
		/// Calculating total fee overflowed
		FeeOverflow,
		/// Calculating total payment overflowed
		PaymentOverflow,
		/// Withdraw fee failed
		WithdrawFailed,
		/// Gas price is too low.
		GasPriceTooLow,
		/// Nonce is invalid
		InvalidNonce,
		/// EVM is forbidden for the call from pallet,
		/// and only allow from traditional Ethereum client
		Forbidden,
		/// Reward miner failed
		RewardFailed,
	}

	#[pallet::genesis_config]
	pub struct GenesisConfig {
		pub accounts: std::collections::BTreeMap<H160, GenesisAccount>,
	}

	#[cfg(feature = "std")]
	impl Default for GenesisConfig {
		fn default() -> Self {
			Self {
				accounts: Default::default(),
			}
		}
	}

	#[pallet::genesis_build]
	impl<T: Config> GenesisBuild<T> for GenesisConfig {
		fn build(&self) {
			for (address, account) in &self.accounts {
				let account_id = T::AddressMapping::into_account_id(*address);

				// ASSUME: in one single EVM transaction, the nonce will not increase more than
				// `u128::max_value()`.
				for _ in 0..account.nonce.low_u128() {
					frame_system::Pallet::<T>::inc_account_nonce(&account_id);
				}

				T::Currency::deposit_creating(
					&account_id,
					account.balance.low_u128().unique_saturated_into(),
				);

				<AccountCodes<T>>::insert(address, &account.code);

				for (index, value) in &account.storage {
					<AccountStorages<T>>::insert(address, index, value);
				}
			}
		}
	}

	#[pallet::storage]
	#[pallet::getter(fn eth_addr)]
	pub type EthAddrOf<T: Config> = StorageMap<_, Twox64Concat, T::AccountId, H160>;

	#[pallet::storage]
	#[pallet::getter(fn account_codes)]
	pub type AccountCodes<T: Config> = StorageMap<_, Blake2_128Concat, H160, Vec<u8>, ValueQuery>;

	#[pallet::storage]
	#[pallet::getter(fn account_storages)]
	pub type AccountStorages<T: Config> = StorageDoubleMap<
		_,
		Blake2_128Concat,
		H160,
		Blake2_128Concat,
		H256,
		H256,
		ValueQuery,
	>;
}

/// Type alias for currency balance.
pub type BalanceOf<T> = <<T as Config>::Currency as Currency<<T as frame_system::Config>::AccountId>>::Balance;

/// Type alias for negative imbalance during fees
type NegativeImbalanceOf<C, T> = <C as Currency<<T as frame_system::Config>::AccountId>>::NegativeImbalance;

/// Trait that outputs the current transaction gas price.
pub trait FeeCalculator {
	/// Return the minimal required gas price.
	fn min_gas_price() -> U256;
}

impl FeeCalculator for () {
	fn min_gas_price() -> U256 { U256::zero() }
}

pub trait EnsureAddressOrigin<OuterOrigin> {
	/// Success return type.
	type Success;

	/// Perform the origin check.
	fn ensure_address_origin(
		address: &H160,
		origin: OuterOrigin,
	) -> Result<Self::Success, BadOrigin> {
		Self::try_address_origin(address, origin).map_err(|_| BadOrigin)
	}

	/// Try with origin.
	fn try_address_origin(
		address: &H160,
		origin: OuterOrigin,
	) -> Result<Self::Success, OuterOrigin>;
}

/// Ensure that the EVM address is the same as the Substrate address. This only works if the account
/// ID is `H160`.
pub struct EnsureAddressSame;

impl<OuterOrigin> EnsureAddressOrigin<OuterOrigin> for EnsureAddressSame where
	OuterOrigin: Into<Result<RawOrigin<H160>, OuterOrigin>> + From<RawOrigin<H160>>,
{
	type Success = H160;

	fn try_address_origin(
		address: &H160,
		origin: OuterOrigin,
	) -> Result<H160, OuterOrigin> {
		origin.into().and_then(|o| match o {
			RawOrigin::Signed(who) if &who == address => Ok(who),
			r => Err(OuterOrigin::from(r))
		})
	}
}

/// Ensure that the origin is root.
pub struct EnsureAddressRoot<AccountId>(sp_std::marker::PhantomData<AccountId>);

impl<OuterOrigin, AccountId> EnsureAddressOrigin<OuterOrigin> for EnsureAddressRoot<AccountId> where
	OuterOrigin: Into<Result<RawOrigin<AccountId>, OuterOrigin>> + From<RawOrigin<AccountId>>,
{
	type Success = ();

	fn try_address_origin(
		_address: &H160,
		origin: OuterOrigin,
	) -> Result<(), OuterOrigin> {
		origin.into().and_then(|o| match o {
			RawOrigin::Root => Ok(()),
			r => Err(OuterOrigin::from(r)),
		})
	}
}

/// Ensure that the origin never happens.
pub struct EnsureAddressNever<AccountId>(sp_std::marker::PhantomData<AccountId>);

impl<OuterOrigin, AccountId> EnsureAddressOrigin<OuterOrigin> for EnsureAddressNever<AccountId> {
	type Success = AccountId;

	fn try_address_origin(
		_address: &H160,
		origin: OuterOrigin,
	) -> Result<AccountId, OuterOrigin> {
		Err(origin)
	}
}

/// Ensure that the address is truncated hash of the origin. Only works if the account id is
/// `AccountId32`.
pub struct EnsureAddressTruncated;

impl<OuterOrigin> EnsureAddressOrigin<OuterOrigin> for EnsureAddressTruncated where
	OuterOrigin: Into<Result<RawOrigin<AccountId32>, OuterOrigin>> + From<RawOrigin<AccountId32>>,
{
	type Success = AccountId32;

	fn try_address_origin(
		address: &H160,
		origin: OuterOrigin,
	) -> Result<AccountId32, OuterOrigin> {
		origin.into().and_then(|o| match o {
			RawOrigin::Signed(who)
				if AsRef::<[u8; 32]>::as_ref(&who)[0..20] == address[0..20] => Ok(who),
			r => Err(OuterOrigin::from(r))
		})
	}
}

pub trait AddressMapping<A> {
	fn into_account_id(address: H160) -> A;
}

/// Identity address mapping.
pub struct IdentityAddressMapping;

impl AddressMapping<H160> for IdentityAddressMapping {
	fn into_account_id(address: H160) -> H160 { address }
}

/// Hashed address mapping.
pub struct HashedAddressMapping<H>(sp_std::marker::PhantomData<H>);

impl<H: Hasher<Out=H256>> AddressMapping<AccountId32> for HashedAddressMapping<H> {
	fn into_account_id(address: H160) -> AccountId32 {
		let mut data = [0u8; 24];
		data[0..4].copy_from_slice(b"evm:");
		data[4..24].copy_from_slice(&address[..]);
		let hash = H::hash(&data);

		AccountId32::from(Into::<[u8; 32]>::into(hash))
	}
}

/// A mapping function that converts Ethereum gas to Substrate weight
pub trait GasWeightMapping {
	fn gas_to_weight(gas: u64) -> Weight;
	fn weight_to_gas(weight: Weight) -> u64;
}

impl GasWeightMapping for () {
	fn gas_to_weight(gas: u64) -> Weight {
		gas as Weight
	}
	fn weight_to_gas(weight: Weight) -> u64 {
		weight as u64
	}
}

static ISTANBUL_CONFIG: EvmConfig = EvmConfig::istanbul();

#[cfg(feature = "std")]
#[derive(Clone, Eq, PartialEq, Encode, Decode, Debug, Serialize, Deserialize)]
/// Account definition used for genesis block construction.
pub struct GenesisAccount {
	/// Account nonce.
	pub nonce: U256,
	/// Account balance.
	pub balance: U256,
	/// Full account storage.
	pub storage: std::collections::BTreeMap<H256, H256>,
	/// Account code.
	pub code: Vec<u8>,
}

impl<T: Config> Pallet<T> {
	/// Check whether an account is empty.
	pub fn is_account_empty(address: &H160) -> bool {
		let account = Self::account_basic(address);
		let code_len = <AccountCodes<T>>::decode_len(address).unwrap_or(0);

		account.nonce == U256::zero() &&
			account.balance == U256::zero() &&
			code_len == 0
	}

	/// Remove an account if its empty.
	pub fn remove_account_if_empty(address: &H160) {
		if Self::is_account_empty(address) {
			Self::remove_account(address);
		}
	}

	/// Remove an account.
	pub fn remove_account(address: &H160) {
		if <AccountCodes<T>>::contains_key(address) {
			let account_id = T::AddressMapping::into_account_id(*address);
			let _ = frame_system::Pallet::<T>::dec_consumers(&account_id);
		}

		<AccountCodes<T>>::remove(address);
		<AccountStorages<T>>::remove_prefix(address);
	}

	/// Create an account.
	pub fn create_account(address: H160, code: Vec<u8>) {
		if code.is_empty() {
			return
		}

		if !<AccountCodes<T>>::contains_key(&address) {
			let account_id = T::AddressMapping::into_account_id(address);
			let _ = frame_system::Pallet::<T>::inc_consumers(&account_id);
		}

		<AccountCodes<T>>::insert(address, code);
	}

	/// Get the account basic in EVM format.
	pub fn account_basic(address: &H160) -> Account {
		let account_id = T::AddressMapping::into_account_id(*address);

		let nonce = frame_system::Pallet::<T>::account_nonce(&account_id);
		let balance = T::Currency::free_balance(&account_id);

		Account {
			nonce: U256::from(UniqueSaturatedInto::<u128>::unique_saturated_into(nonce)),
			balance: U256::from(UniqueSaturatedInto::<u128>::unique_saturated_into(balance)),
		}
	}
}

/// Handle withdrawing, refunding and depositing of transaction fees.
/// Similar to `OnChargeTransaction` of `pallet_transaction_payment`
pub trait OnChargeEVMTransaction<T: Config> {
	type LiquidityInfo: Default;

	/// Before the transaction is executed the payment of the transaction fees
	/// need to be secured.
	fn withdraw_fee(who: &H160, fee: U256) -> Result<Self::LiquidityInfo, Error<T>>;

	/// After the transaction was executed the actual fee can be calculated.
	/// This function should refund any overpaid fees and optionally deposit
	/// the corrected amount.
	fn correct_and_deposit_fee(
		who: &H160,
		corrected_fee: U256,
		already_withdrawn: Self::LiquidityInfo,
	) -> Result<(), Error<T>>;
}

/// Implements the transaction payment for a pallet implementing the `Currency`
/// trait (eg. the pallet_balances) using an unbalance handler (implementing
/// `OnUnbalanced`).
/// Similar to `CurrencyAdapter` of `pallet_transaction_payment`
pub struct EVMCurrencyAdapter<C, OU>(sp_std::marker::PhantomData<(C, OU)>);

impl<T, C, OU> OnChargeEVMTransaction<T> for EVMCurrencyAdapter<C, OU>
where
	T: Config,
	C: Currency<<T as frame_system::Config>::AccountId>,
	C::PositiveImbalance: Imbalance<
		<C as Currency<<T as frame_system::Config>::AccountId>>::Balance,
		Opposite = C::NegativeImbalance,
	>,
	C::NegativeImbalance: Imbalance<
		<C as Currency<<T as frame_system::Config>::AccountId>>::Balance,
		Opposite = C::PositiveImbalance,
	>,
	OU: OnUnbalanced<NegativeImbalanceOf<C, T>>,
{
	// Kept type as Option to satisfy bound of Default
	type LiquidityInfo = Option<NegativeImbalanceOf<C, T>>;

	fn withdraw_fee(who: &H160, fee: U256) -> Result<Self::LiquidityInfo, Error<T>> {
		let account_id = T::AddressMapping::into_account_id(*who);
		let imbalance = C::withdraw(
			&account_id,
			fee.low_u128().unique_saturated_into(),
			WithdrawReasons::FEE,
			ExistenceRequirement::AllowDeath,
		)
		.map_err(|_| Error::<T>::BalanceLow)?;
		Ok(Some(imbalance))
	}

	fn correct_and_deposit_fee(
		who: &H160,
		corrected_fee: U256,
		already_withdrawn: Self::LiquidityInfo,
	) -> Result<(), Error<T>> {
		if let Some(paid) = already_withdrawn {
			let account_id = T::AddressMapping::into_account_id(*who);

			// Calculate how much refund we should return
			let refund_amount = paid
				.peek()
				.saturating_sub(corrected_fee.low_u128().unique_saturated_into());
			// refund to the account that paid the fees. If this fails, the
			// account might have dropped below the existential balance. In
			// that case we don't refund anything.
			let refund_imbalance = C::deposit_into_existing(&account_id, refund_amount)
				.unwrap_or_else(|_| C::PositiveImbalance::zero());
			// merge the imbalance caused by paying the fees and refunding parts of it again.
			let adjusted_paid = paid
				.offset(refund_imbalance)
				.map_err(|_| Error::<T>::BalanceLow)?;
			OU::on_unbalanced(adjusted_paid);
		}
		Ok(())
	}
}

/// Implementation for () does not specify what to do with imbalance
impl<T> OnChargeEVMTransaction<T> for ()
	where
	T: Config,
	<T::Currency as Currency<<T as frame_system::Config>::AccountId>>::PositiveImbalance:
		Imbalance<<T::Currency as Currency<<T as frame_system::Config>::AccountId>>::Balance, Opposite = <T::Currency as Currency<<T as frame_system::Config>::AccountId>>::NegativeImbalance>,
	<T::Currency as Currency<<T as frame_system::Config>::AccountId>>::NegativeImbalance:
		Imbalance<<T::Currency as Currency<<T as frame_system::Config>::AccountId>>::Balance, Opposite = <T::Currency as Currency<<T as frame_system::Config>::AccountId>>::PositiveImbalance>, {
	// Kept type as Option to satisfy bound of Default
	type LiquidityInfo = Option<NegativeImbalanceOf<T::Currency, T>>;

	fn withdraw_fee(
		who: &H160,
		fee: U256,
	) -> Result<Self::LiquidityInfo, Error<T>> {
		EVMCurrencyAdapter::<<T as Config>::Currency, ()>::withdraw_fee(who, fee)
	}

	fn correct_and_deposit_fee(
		who: &H160,
		corrected_fee: U256,
		already_withdrawn: Self::LiquidityInfo,
	) -> Result<(), Error<T>> {
		EVMCurrencyAdapter::<<T as Config>::Currency, ()>::correct_and_deposit_fee(who, corrected_fee, already_withdrawn)
	}
}

impl<T> pallet_authorship::EventHandler<T::AccountId, T::BlockNumber> for Module<T>
where
	T: Config + pallet_authorship::Config + pallet_session::Config,
{
	fn note_author(author: T::AccountId) {
		if let Some(eth_addr) = <EthAddrOf<T>>::get(author) {
			Self::reward(eth_addr);
		}
	}
	fn note_uncle(_author: T::AccountId, _age: T::BlockNumber) {}
}

impl<T: Config> Module<T> {
    pub(crate) fn reward(
        eth_address: H160,
    ) {
		if T::Runner::mint(eth_address, U256::from(1000000000000000000u128), T::config()).is_err() {
			Pallet::<T>::deposit_event(Event::<T>::EthRewardFailed(eth_address));
		}
	}
}
