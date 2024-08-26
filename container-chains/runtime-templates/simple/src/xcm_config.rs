// Copyright (C) Moondance Labs Ltd.
// This file is part of Tanssi.

// Tanssi is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Tanssi is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Tanssi.  If not, see <http://www.gnu.org/licenses/>

use {
    super::{
        currency::MICROUNIT,
        weights::{self, xcm::XcmWeight as XcmGenericWeights},
        AccountId, AllPalletsWithSystem, AssetRate, Balance, Balances, ForeignAssetsCreator,
        MaintenanceMode, MessageQueue, ParachainInfo, ParachainSystem, PolkadotXcm, Runtime,
        RuntimeBlockWeights, RuntimeCall, RuntimeEvent, RuntimeOrigin, TransactionByteFee,
        WeightToFee, XcmpQueue,
        TokenId, NATIVE_TOKEN_ID, BlockNumber, MaxReserves, MaxLocks, TreasuryPalletId, Tokens, Currencies,
    },
    cumulus_primitives_core::{AggregateMessageOrigin, ParaId},
    frame_support::{
        parameter_types,
        traits::{Everything, Nothing, PalletInfoAccess, TransformOrigin, Contains},
        weights::Weight,
    },
    frame_system::EnsureRoot,
    pallet_xcm::XcmPassthrough,
    pallet_xcm_executor_utils::{
        filters::{IsReserveFilter, IsTeleportFilter},
        DefaultTrustPolicy,
    },
    parachains_common::message_queue::{NarrowOriginToSibling, ParaIdToSibling},
    polkadot_runtime_common::xcm_sender::ExponentialPrice,
    sp_core::ConstU32,
    sp_runtime::{traits::{AccountIdConversion, Convert}, Perbill, Percent},
    staging_xcm::latest::prelude::*,
    staging_xcm_builder::{
        AccountId32Aliases, AllowKnownQueryResponses, AllowSubscriptionsFrom,
        AllowTopLevelPaidExecutionFrom, ConvertedConcreteId, EnsureXcmOrigin, FungibleAdapter,
        IsConcrete, ParentIsPreset, RelayChainAsNative, SiblingParachainAsNative,
        SiblingParachainConvertsVia, SignedAccountId32AsNative, SignedToAccountId32,
        SovereignSignedViaLocation, TakeWeightCredit, UsingComponents, WeightInfoBounds,
        WithComputedOrigin, FixedWeightBounds, TakeRevenue,
    },
    staging_xcm_executor::XcmExecutor,
    ava_protocol_primitives::{AbsoluteAndRelativeReserveProvider, Amount},
    orml_traits::{parameter_type_with_key, FixedConversionRateProvider, asset_registry::Inspect, MultiCurrency},
    common_runtime::CurrencyHooks,
};

parameter_types! {
    // Self Reserve location, defines the multilocation identifying the self-reserve currency
    // This is used to match it also against our Balances pallet when we receive such
    // a Location: (Self Balances pallet index)
    // We use the RELATIVE multilocation
    pub SelfReserve: Location = Location {
        parents:0,
        interior: [
            PalletInstance(<Balances as PalletInfoAccess>::index() as u8)
        ].into()
    };

    // One XCM operation is 1_000_000_000 weight - almost certainly a conservative estimate.
    pub UnitWeightCost: Weight = Weight::from_parts(1_000_000_000, 64 * 1024);

    // TODO: revisit
    pub const RelayNetwork: NetworkId = NetworkId::Westend;

    // The relay chain Origin type
    pub RelayChainOrigin: RuntimeOrigin = cumulus_pallet_xcm::Origin::Relay.into();

    pub const MaxAssetsIntoHolding: u32 = 64;

    /// Maximum number of instructions in a single XCM fragment. A sanity check against
    /// weight caculations getting too crazy.
    pub MaxInstructions: u32 = 100;

    // The universal location within the global consensus system
    pub UniversalLocation: InteriorLocation = [GlobalConsensus(RelayNetwork::get()), Parachain(ParachainInfo::parachain_id().into())].into();

    pub const BaseDeliveryFee: u128 = 100 * MICROUNIT;
}

#[cfg(feature = "runtime-benchmarks")]
parameter_types! {
    pub ReachableDest: Option<Location> = Some(Parent.into());
}

pub type XcmBarrier = (
    // Weight that is paid for may be consumed.
    TakeWeightCredit,
    // Expected responses are OK.
    AllowKnownQueryResponses<PolkadotXcm>,
    WithComputedOrigin<
        (
            // If the message is one that immediately attemps to pay for execution, then allow it.
            AllowTopLevelPaidExecutionFrom<Everything>,
            // Subscriptions for version tracking are OK.
            AllowSubscriptionsFrom<Everything>,
        ),
        UniversalLocation,
        ConstU32<8>,
    >,
);

/// Type for specifying how a `Location` can be converted into an `AccountId`. This is used
/// when determining ownership of accounts for asset transacting and when attempting to use XCM
/// `Transact` in order to determine the dispatch Origin.
pub type LocationToAccountId = (
    // The parent (Relay-chain) origin converts to the default `AccountId`.
    ParentIsPreset<AccountId>,
    // Sibling parachain origins convert to AccountId via the `ParaId::into`.
    SiblingParachainConvertsVia<polkadot_parachain_primitives::primitives::Sibling, AccountId>,
    // If we receive a Location of type AccountKey20, just generate a native account
    AccountId32Aliases<RelayNetwork, AccountId>,
    // Generate remote accounts according to polkadot standards
    staging_xcm_builder::HashedDescription<
        AccountId,
        staging_xcm_builder::DescribeFamily<staging_xcm_builder::DescribeAllTerminal>,
    >,
);

/// Local origins on this chain are allowed to dispatch XCM sends/executions.
pub type LocalOriginToLocation = SignedToAccountId32<RuntimeOrigin, AccountId, RelayNetwork>;

/// Means for transacting the native currency on this chain.
pub type CurrencyTransactor = FungibleAdapter<
    // Use this currency:
    Balances,
    // Use this currency when it is a fungible asset matching the given location or name:
    IsConcrete<SelfReserve>,
    // Convert an XCM Location into a local account id:
    LocationToAccountId,
    // Our chain's account ID type (we can't get away without mentioning it explicitly):
    AccountId,
    // We don't track any teleports of `Balances`.
    (),
>;

/// This is the type we use to convert an (incoming) XCM origin into a local `Origin` instance,
/// ready for dispatching a transaction with Xcm's `Transact`. There is an `OriginKind` which can
/// biases the kind of local `Origin` it will become.
pub type XcmOriginToTransactDispatchOrigin = (
    // Sovereign account converter; this attempts to derive an `AccountId` from the origin location
    // using `LocationToAccountId` and then turn that into the usual `Signed` origin. Useful for
    // foreign chains who want to have a local sovereign account on this chain which they control.
    SovereignSignedViaLocation<LocationToAccountId, RuntimeOrigin>,
    // Native converter for Relay-chain (Parent) location; will convert to a `Relay` origin when
    // recognised.
    RelayChainAsNative<RelayChainOrigin, RuntimeOrigin>,
    // Native converter for sibling Parachains; will convert to a `SiblingPara` origin when
    // recognised.
    SiblingParachainAsNative<cumulus_pallet_xcm::Origin, RuntimeOrigin>,
    // Native signed account converter; this just converts an `AccountId32` origin into a normal
    // `RuntimeOrigin::Signed` origin of the same 32-byte value.
    SignedAccountId32AsNative<RelayNetwork, RuntimeOrigin>,
    // Xcm origins can be represented natively under the Xcm pallet's Xcm origin.
    XcmPassthrough<RuntimeOrigin>,
);

/// Means for transacting assets on this chain.
pub type AssetTransactors = (CurrencyTransactor, ForeignFungiblesTransactor);
pub type XcmWeigher =
    WeightInfoBounds<XcmGenericWeights<RuntimeCall>, RuntimeCall, MaxInstructions>;
/// The means for routing XCM messages which are not for local execution into the right message
/// queues.
pub type XcmRouter = (
    // Two routers - use UMP to communicate with the relay chain:
    cumulus_primitives_utility::ParentAsUmp<ParachainSystem, PolkadotXcm, PriceForParentDelivery>,
    // ..and XCMP to communicate with the sibling chains.
    XcmpQueue,
);

pub struct XcmConfig;
impl staging_xcm_executor::Config for XcmConfig {
    type RuntimeCall = RuntimeCall;
    type XcmSender = XcmRouter;
    type AssetTransactor = AssetTransactors;
    type OriginConverter = XcmOriginToTransactDispatchOrigin;
    type IsReserve = IsReserveFilter<Runtime>;
    type IsTeleporter = IsTeleportFilter<Runtime>;
    type UniversalLocation = UniversalLocation;
    type Barrier = XcmBarrier;
    type Weigher = XcmWeigher;
    type Trader = (
        UsingComponents<WeightToFee, SelfReserve, AccountId, Balances, ()>,
        cumulus_primitives_utility::TakeFirstAssetTrader<
            AccountId,
            AssetRateAsMultiplier,
            // Use this currency when it is a fungible asset matching the given location or name:
            (ConvertedConcreteId<AssetId, Balance, ForeignAssetsCreator, JustTry>,),
            ForeignAssets,
            (),
        >,
    );
    type ResponseHandler = PolkadotXcm;
    type AssetTrap = PolkadotXcm;
    type AssetClaims = PolkadotXcm;
    type SubscriptionService = PolkadotXcm;
    type PalletInstancesInfo = AllPalletsWithSystem;
    type MaxAssetsIntoHolding = MaxAssetsIntoHolding;
    type AssetLocker = ();
    type AssetExchanger = ();
    type FeeManager = ();
    type MessageExporter = ();
    type UniversalAliases = Nothing;
    type CallDispatcher = RuntimeCall;
    type SafeCallFilter = Everything;
    type Aliasers = Nothing;
    type TransactionalProcessor = staging_xcm_builder::FrameTransactionalProcessor;
    type HrmpNewChannelOpenRequestHandler = ();
    type HrmpChannelAcceptedHandler = ();
    type HrmpChannelClosingHandler = ();
    type XcmRecorder = ();
}

impl pallet_xcm::Config for Runtime {
    type RuntimeEvent = RuntimeEvent;
    type SendXcmOrigin = EnsureXcmOrigin<RuntimeOrigin, LocalOriginToLocation>;
    type XcmRouter = XcmRouter;
    type ExecuteXcmOrigin = EnsureXcmOrigin<RuntimeOrigin, LocalOriginToLocation>;
    type XcmExecuteFilter = Everything;
    type XcmExecutor = XcmExecutor<XcmConfig>;
    type XcmTeleportFilter = Nothing;
    type XcmReserveTransferFilter = Everything;
    type Weigher = XcmWeigher;
    type UniversalLocation = UniversalLocation;
    type RuntimeOrigin = RuntimeOrigin;
    type RuntimeCall = RuntimeCall;
    const VERSION_DISCOVERY_QUEUE_SIZE: u32 = 100;
    type AdvertisedXcmVersion = pallet_xcm::CurrentXcmVersion;
    type Currency = Balances;
    type CurrencyMatcher = ();
    type TrustedLockers = ();
    type SovereignAccountOf = LocationToAccountId;
    type MaxLockers = ConstU32<8>;
    type MaxRemoteLockConsumers = ConstU32<0>;
    type RemoteLockConsumerIdentifier = ();
    // TODO pallet-xcm weights
    type WeightInfo = weights::pallet_xcm::SubstrateWeight<Runtime>;
    type AdminOrigin = EnsureRoot<AccountId>;
}

pub type PriceForSiblingParachainDelivery =
    ExponentialPrice<SelfReserve, BaseDeliveryFee, TransactionByteFee, XcmpQueue>;

pub type PriceForParentDelivery =
    ExponentialPrice<SelfReserve, BaseDeliveryFee, TransactionByteFee, ParachainSystem>;

impl cumulus_pallet_xcmp_queue::Config for Runtime {
    type RuntimeEvent = RuntimeEvent;
    type ChannelInfo = ParachainSystem;
    type VersionWrapper = PolkadotXcm;
    type ControllerOrigin = EnsureRoot<AccountId>;
    type ControllerOriginConverter = XcmOriginToTransactDispatchOrigin;
    type WeightInfo = weights::cumulus_pallet_xcmp_queue::SubstrateWeight<Self>;
    type PriceForSiblingDelivery = PriceForSiblingParachainDelivery;
    // Enqueue XCMP messages from siblings for later processing.
    type XcmpQueue = TransformOrigin<MessageQueue, AggregateMessageOrigin, ParaId, ParaIdToSibling>;
    type MaxInboundSuspended = sp_core::ConstU32<1_000>;
}

impl cumulus_pallet_xcm::Config for Runtime {
    type RuntimeEvent = RuntimeEvent;
    type XcmExecutor = XcmExecutor<XcmConfig>;
}

parameter_types! {
    pub const RelayOrigin: AggregateMessageOrigin = AggregateMessageOrigin::Parent;
}

impl cumulus_pallet_dmp_queue::Config for Runtime {
    type WeightInfo = weights::cumulus_pallet_dmp_queue::SubstrateWeight<Runtime>;
    type RuntimeEvent = RuntimeEvent;
    type DmpSink = frame_support::traits::EnqueueWithOrigin<MessageQueue, RelayOrigin>;
}

parameter_types! {
    pub MessageQueueServiceWeight: Weight = Perbill::from_percent(25) * RuntimeBlockWeights::get().max_block;
}

impl pallet_message_queue::Config for Runtime {
    type RuntimeEvent = RuntimeEvent;
    type WeightInfo = weights::pallet_message_queue::SubstrateWeight<Runtime>;
    #[cfg(feature = "runtime-benchmarks")]
    type MessageProcessor = pallet_message_queue::mock_helpers::NoopMessageProcessor<
        cumulus_primitives_core::AggregateMessageOrigin,
    >;
    #[cfg(not(feature = "runtime-benchmarks"))]
    type MessageProcessor = staging_xcm_builder::ProcessXcmMessage<
        AggregateMessageOrigin,
        XcmExecutor<XcmConfig>,
        RuntimeCall,
    >;
    type Size = u32;
    // The XCMP queue pallet is only ever able to handle the `Sibling(ParaId)` origin:
    type QueueChangeHandler = NarrowOriginToSibling<XcmpQueue>;
    // NarrowOriginToSibling calls XcmpQueue's is_pause if Origin is sibling. Allows all other origins
    type QueuePausedQuery = (MaintenanceMode, NarrowOriginToSibling<XcmpQueue>);
    // TODO verify values
    type HeapSize = sp_core::ConstU32<{ 64 * 1024 }>;
    type MaxStale = sp_core::ConstU32<8>;
    type ServiceWeight = MessageQueueServiceWeight;
    type IdleMaxServiceWeight = MessageQueueServiceWeight;
}

parameter_types! {
    // we just reuse the same deposits
    pub const ForeignAssetsAssetDeposit: Balance = 0;
    pub const ForeignAssetsAssetAccountDeposit: Balance = 0;
    pub const ForeignAssetsApprovalDeposit: Balance = 0;
    pub const ForeignAssetsAssetsStringLimit: u32 = 50;
    pub const ForeignAssetsMetadataDepositBase: Balance = 0;
    pub const ForeignAssetsMetadataDepositPerByte: Balance = 0;
    pub CheckingAccount: AccountId = PolkadotXcm::check_account();
}

#[cfg(feature = "runtime-benchmarks")]
/// Simple conversion of `u32` into an `AssetId` for use in benchmarking.
pub struct ForeignAssetBenchmarkHelper;
#[cfg(feature = "runtime-benchmarks")]
impl pallet_assets::BenchmarkHelper<AssetId> for ForeignAssetBenchmarkHelper {
    fn create_asset_id_parameter(id: u32) -> AssetId {
        id.try_into()
            .expect("number too large to create benchmarks")
    }
}
#[cfg(feature = "runtime-benchmarks")]
impl pallet_asset_rate::AssetKindFactory<AssetId> for ForeignAssetBenchmarkHelper {
    fn create_asset_kind(id: u32) -> AssetId {
        id.try_into()
            .expect("number too large to create benchmarks")
    }
}

pub type AssetId = u16;
pub type ForeignAssetsInstance = pallet_assets::Instance1;
impl pallet_assets::Config<ForeignAssetsInstance> for Runtime {
    type RuntimeEvent = RuntimeEvent;
    type Balance = Balance;
    type AssetId = AssetId;
    type AssetIdParameter = AssetId;
    type Currency = Balances;
    type CreateOrigin = frame_support::traits::NeverEnsureOrigin<AccountId>;
    type ForceOrigin = EnsureRoot<AccountId>;
    type AssetDeposit = ForeignAssetsAssetDeposit;
    type MetadataDepositBase = ForeignAssetsMetadataDepositBase;
    type MetadataDepositPerByte = ForeignAssetsMetadataDepositPerByte;
    type ApprovalDeposit = ForeignAssetsApprovalDeposit;
    type StringLimit = ForeignAssetsAssetsStringLimit;
    type Freezer = ();
    type Extra = ();
    type WeightInfo = weights::pallet_assets::SubstrateWeight<Runtime>;
    type CallbackHandle = ();
    type AssetAccountDeposit = ForeignAssetsAssetAccountDeposit;
    type RemoveItemsLimit = frame_support::traits::ConstU32<1000>;
    #[cfg(feature = "runtime-benchmarks")]
    type BenchmarkHelper = ForeignAssetBenchmarkHelper;
}

impl pallet_foreign_asset_creator::Config for Runtime {
    type RuntimeEvent = RuntimeEvent;
    type ForeignAsset = Location;
    type ForeignAssetCreatorOrigin = EnsureRoot<AccountId>;
    type ForeignAssetModifierOrigin = EnsureRoot<AccountId>;
    type ForeignAssetDestroyerOrigin = EnsureRoot<AccountId>;
    type Fungibles = ForeignAssets;
    type WeightInfo = weights::pallet_foreign_asset_creator::SubstrateWeight<Runtime>;
    type OnForeignAssetCreated = ();
    type OnForeignAssetDestroyed = ();
}

impl pallet_asset_rate::Config for Runtime {
    type CreateOrigin = EnsureRoot<AccountId>;
    type RemoveOrigin = EnsureRoot<AccountId>;
    type UpdateOrigin = EnsureRoot<AccountId>;
    type Currency = Balances;
    type AssetKind = AssetId;
    type RuntimeEvent = RuntimeEvent;
    type WeightInfo = weights::pallet_asset_rate::SubstrateWeight<Runtime>;
    #[cfg(feature = "runtime-benchmarks")]
    type BenchmarkHelper = ForeignAssetBenchmarkHelper;
}

use {
    crate::ForeignAssets,
    staging_xcm_builder::{FungiblesAdapter, NoChecking},
    staging_xcm_executor::traits::JustTry,
};

/// Means for transacting foreign assets from different global consensus.
pub type ForeignFungiblesTransactor = FungiblesAdapter<
    // Use this fungibles implementation:
    ForeignAssets,
    // Use this currency when it is a fungible asset matching the given location or name:
    (ConvertedConcreteId<AssetId, Balance, ForeignAssetsCreator, JustTry>,),
    // Convert an XCM Location into a local account id:
    LocationToAccountId,
    // Our chain's account ID type (we can't get away without mentioning it explicitly):
    AccountId,
    // We dont need to check teleports here.
    NoChecking,
    // The account to use for tracking teleports.
    CheckingAccount,
>;

/// Multiplier used for dedicated `TakeFirstAssetTrader` with `ForeignAssets` instance.
pub type AssetRateAsMultiplier =
    parachains_common::xcm_config::AssetFeeAsExistentialDepositMultiplier<
        Runtime,
        WeightToFee,
        AssetRate,
        ForeignAssetsInstance,
    >;

parameter_types! {
    pub const TrustPolicyMaxAssets: u32 = 1000;
    pub const AllNativeTrustPolicy: DefaultTrustPolicy = DefaultTrustPolicy::AllNative;
    pub const AllNeverTrustPolicy: DefaultTrustPolicy = DefaultTrustPolicy::Never;
}
impl pallet_xcm_executor_utils::Config for Runtime {
    type RuntimeEvent = RuntimeEvent;
    type TrustPolicyMaxAssets = TrustPolicyMaxAssets;
    type ReserveDefaultTrustPolicy = AllNativeTrustPolicy;
    type SetReserveTrustOrigin = EnsureRoot<AccountId>;
    type TeleportDefaultTrustPolicy = AllNeverTrustPolicy;
    type SetTeleportTrustOrigin = EnsureRoot<AccountId>;
    type WeightInfo = weights::pallet_xcm_executor_utils::SubstrateWeight<Runtime>;
}

pub struct ToTreasury;
impl TakeRevenue for ToTreasury {
	fn take_revenue(revenue: Asset) {
		if let Asset { id: AssetId(id), fun: Fungibility::Fungible(amount) } =
			revenue
		{
			if let Some(currency_id) = TokenIdConvert::convert(id) {
				if currency_id == NATIVE_TOKEN_ID {
					// Deposit to native treasury account
					// 20% burned, 80% to the treasury
					let to_treasury = Percent::from_percent(80).mul_floor(amount);
					// Due to the way XCM works the amount has already been taken off the total allocation balance.
					// Thus whatever we deposit here gets added back to the total allocation, and the rest is burned.
					let _ = Currencies::deposit(currency_id, &TreasuryAccount::get(), to_treasury);
				} else {
					// Deposit to foreign treasury account
					let _ = Currencies::deposit(
						currency_id,
						&TemporaryForeignTreasuryAccount::get(),
						amount,
					);
				}
			}
		}
	}
}

type AssetRegistryOf<T> = orml_asset_registry::module::Pallet<T>;

pub struct FeePerSecondProvider;
impl FixedConversionRateProvider for FeePerSecondProvider {
    fn get_fee_per_second(location: &Location) -> Option<u128> {
        let metadata = match location.interior.first() {
            Some(Junction::Parachain(para_id)) if *para_id == u32::from(ParachainInfo::parachain_id()) => {
                AssetRegistryOf::<Runtime>::metadata(NATIVE_TOKEN_ID)?
            },
            _ => AssetRegistryOf::<Runtime>::metadata_by_location(location)?,
        };
        
        metadata.additional.fee_per_second
    }
}

// pub type Trader =
// 	(AssetRegistryTrader<FixedRateAssetRegistryTrader<FeePerSecondProvider>, ToTreasury>,);

pub struct TokenIdConvert;
impl Convert<TokenId, Option<Location>> for TokenIdConvert {
	fn convert(id: TokenId) -> Option<Location> {
        match AssetRegistryOf::<Runtime>::location(&id) {
            Ok(Some(multi_location)) =>{
                let location: Location = Location::try_from(multi_location).unwrap();
                Some(location)
            },
            _ => None,
        }
	}
}

impl Convert<Location, Option<TokenId>> for TokenIdConvert {
    fn convert(location: Location) -> Option<TokenId> {
        if let Some(Junction::Parachain(para_id)) = location.interior.first() {
            if *para_id == u32::from(ParachainInfo::parachain_id()) {
                return Some(NATIVE_TOKEN_ID);
            }
        }

        AssetRegistryOf::<Runtime>::asset_id(&location)
    }
}

impl Convert<Asset, Option<TokenId>> for TokenIdConvert {
	fn convert(asset: Asset) -> Option<TokenId> {
		let Asset { id: AssetId(location), .. } = asset;
        Self::convert(location)
	}
}

pub struct AccountIdToMultiLocation;
impl Convert<AccountId, Location> for AccountIdToMultiLocation {
	fn convert(account: AccountId) -> Location {
		AccountId32 { network: None, id: account.into() }.into()
	}
}

parameter_types! {
	pub SelfLocation: Location = Here.into_location();
	pub SelfLocationAbsolute: Location = Location::new(1, Parachain(ParachainInfo::parachain_id().into()));
	pub const BaseXcmWeight: Weight = Weight::from_parts(100_000_000, 0);
	pub const MaxAssetsForTransfer: usize = 1;
}

parameter_type_with_key! {
	pub ParachainMinFee: |_location: Location| -> Option<u128> {
		None
	};
}

impl orml_xtokens::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type Balance = Balance;
	type CurrencyId = TokenId;
	type CurrencyIdConvert = TokenIdConvert;
	type AccountIdToLocation = AccountIdToMultiLocation;
	type SelfLocation = SelfLocation;
	type XcmExecutor = XcmExecutor<XcmConfig>;
	type Weigher = FixedWeightBounds<UnitWeightCost, RuntimeCall, MaxInstructions>;
	type BaseXcmWeight = BaseXcmWeight;
	type UniversalLocation = UniversalLocation;
	type MaxAssetsForTransfer = MaxAssetsForTransfer;
    // Default impl. Refer to `orml-xtokens` docs for more details.
    type MinXcmFee = ParachainMinFee;
    type LocationsFilter = Everything;
    type ReserveProvider = AbsoluteAndRelativeReserveProvider<SelfLocationAbsolute>;
    type RateLimiter = ();
    type RateLimiterId = ();
}

parameter_types! {
	pub TreasuryAccount: AccountId = TreasuryPalletId::get().into_account_truncating();
	// Until we can codify how to handle forgien tokens that we collect in XCMP fees
	// we will send the tokens to a special account to be dealt with.
	pub TemporaryForeignTreasuryAccount: AccountId = hex_literal::hex!["8acc2955e592588af0eeec40384bf3b498335ecc90df5e6980f0141e1314eb37"].into();
}

pub struct DustRemovalWhitelist;
impl Contains<AccountId> for DustRemovalWhitelist {
	fn contains(a: &AccountId) -> bool {
		*a == TreasuryAccount::get() || *a == TemporaryForeignTreasuryAccount::get()
	}
}

impl orml_tokens::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type Balance = Balance;
	type Amount = Amount;
	type CurrencyId = TokenId;
	type WeightInfo = ();
	type ExistentialDeposits = orml_asset_registry::ExistentialDeposits<Runtime>;
	type CurrencyHooks = CurrencyHooks<Runtime, TreasuryAccount>;
	type MaxLocks = MaxLocks;
	type MaxReserves = MaxReserves;
	type ReserveIdentifier = [u8; 8];
	type DustRemovalWhitelist = DustRemovalWhitelist;
}

parameter_types! {
	pub const GetNativeCurrencyId: TokenId = NATIVE_TOKEN_ID;
}

impl orml_currencies::Config for Runtime {
	type MultiCurrency = Tokens;
	type NativeCurrency =
		orml_currencies::BasicCurrencyAdapter<Runtime, Balances, Amount, BlockNumber>;
	type GetNativeCurrencyId = GetNativeCurrencyId;
	type WeightInfo = ();
}

impl pallet_xcmp_handler::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type RuntimeCall = RuntimeCall;
	type MultiCurrency = Currencies;
	type CurrencyId = TokenId;
	type GetNativeCurrencyId = GetNativeCurrencyId;
	type SelfParaId = parachain_info::Pallet<Runtime>;
	type AccountIdToMultiLocation = AccountIdToMultiLocation;
	type CurrencyIdToMultiLocation = TokenIdConvert;
	type UniversalLocation = UniversalLocation;
	type XcmSender = XcmRouter;
	type XcmExecutor = XcmExecutor<XcmConfig>;
	type Weigher = FixedWeightBounds<UnitWeightCost, RuntimeCall, MaxInstructions>;
	type ReserveProvider = AbsoluteAndRelativeReserveProvider<SelfLocationAbsolute>;
	type SelfLocation = SelfLocationAbsolute;
}