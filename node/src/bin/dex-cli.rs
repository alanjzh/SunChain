//! Places real orders against a running chain, end to end.
//!
//! The DEX has two distinct signing paths, and mixing them up is the easiest mistake here:
//!
//! * **`deposit_collateral`** is an ordinary signed *extrinsic*. The signature covers the
//!   call plus the transaction extension, and it pays fees.
//! * **order actions** are *off-chain signed payloads* submitted through
//!   `exchange_submitAction`. The signature covers only
//!   `(subaccount, action_nonce, action)`, and the identity lives in the `signer` field.
//!
//! Both paths are verified by `EthereumSignature`, which recovers over
//! **`keccak256(payload)`**. `ecdsa::Pair::sign` hashes with blake2 instead, so signing here
//! goes through `sign_prehashed(&keccak_256(..))`. The mismatch is silent: the wallet toolkit
//! accepts the key, the group of sixty-five bytes looks like a signature, and the reply is a
//! flat `bad signature` with nothing pointing at the hash function.
//!
//! Keeping both in one binary is the point: it means the whole flow — fund, price, place,
//! read back — can be exercised without a UI.
//!
//! *Nothing here can be trusted to fail loudly.* Submitting an order answers only "accepted
//! into the pool"; the batch that later executes it reports `Ok` even when it refuses every
//! action inside; and a refused action still consumes its nonce, so retrying fails
//! differently and hides the cause. The checks below exist because each of those was mistaken
//! for success exactly once.
//!
//! ```text
//! cargo run --bin dex-cli -- --key 0x<64 hex> deposit --amount 1000000000
//! cargo run --bin dex-cli -- feed --market 0 --price 60000 --every 45
//! cargo run --bin dex-cli -- --key 0x<64 hex> buy  --market 0 --price 60000 --lots 1
//! ```

use sunchain_runtime::{
	apis::{ExchangeAction, ExchangeSignedAction, OrderRequest, Side, TimeInForce},
	configs::{ExchangeSignature, ExchangeSigner},
	Address, BlockNumber, RuntimeCall, SignedPayload, TxExtension, UncheckedExtrinsic,
};
use scale_codec::{Decode, Encode};
use pallet_exchange_core::Call as CoreCall;
use serde_json::{json, Value};
use frame_support::crypto::ecdsa::ECDSAExt;
use exchange_primitives::{price_to_ticks, FEED_SCALE};
use sp_core::{ecdsa, keccak_256, Pair};
use sp_io::TestExternalities;
use sp_runtime::{generic::Era, ApplyExtrinsicResult, DispatchError, MultiSignature};

type R = sunchain_runtime::Runtime;

/// The account a key controls.
///
/// The public key must be **uncompressed** first (`to_eth_address` does that); keccaking the
/// 33-byte compressed form yields a different — and wrong — address. Runs inside a trivial
/// externalities scope because the helper goes through `sp_io`'s hashing host functions.
fn account_of(pair: &ecdsa::Pair) -> [u8; 20] {
	TestExternalities::default().execute_with(|| {
		pair.public().to_eth_address().expect("public key is a valid curve point")
	})
}

/// Signed extrinsics must be built inside an externalities scope: `SignedPayload::new` asks
/// each extension for its implicit data, and `CheckGenesis` reads the genesis hash out of
/// storage. Off-chain signing is not "no environment", it is "a minimal one".
fn in_ext<T>(genesis: [u8; 32], f: impl FnOnce() -> T) -> T {
	TestExternalities::default().execute_with(|| {
		frame_system::BlockHash::<R>::insert(0u32, sp_core::H256(genesis));
		f()
	})
}

fn rpc(url: &str, method: &str, params: Value) -> Value {
	let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
	let client = reqwest::blocking::Client::new();
	client
		.post(url)
		.header("Content-Type", "application/json")
		.body(body.to_string())
		.send()
		.expect("rpc request")
		.json()
		.expect("rpc response is json")
}

fn from_hex(s: &str) -> Vec<u8> {
	(0..s.len())
		.step_by(2)
		.map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
		.collect()
}

fn to_hex(bytes: &[u8]) -> String {
	bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn main() {
	let args: Vec<String> = std::env::args().collect();
	let url = flag(&args, "--rpc", "http://localhost:9944");
	let command = command_of(&args);

	// `feed` signs with the oracle's own keys, which it resolves from `oracle_keys`; the
	// trading commands sign with whatever key the caller passed.
	let key_hex = flag(&args, "--key", "");
	if key_hex.is_empty() && command != "feed" {
		eprintln!("usage: dex-cli --key 0x<64 hex> [deposit|buy|sell] [options]");
		eprintln!("       dex-cli feed --market 0 --price 60000");
		std::process::exit(2);
	}
	let trader = (!key_hex.is_empty()).then(|| {
		let secret = from_hex(key_hex.trim_start_matches("0x"));
		let pair = ecdsa::Pair::from_seed_slice(&secret).expect("valid secp256k1 secret");
		let who = account_of(&pair);
		println!("account 0x{}", to_hex(&who));
		(pair, who)
	});
	let genesis_hex = rpc(&url, "chain_getBlockHash", json!([0]))["result"]
		.as_str()
		.expect("genesis hash")
		.to_string();
	let genesis = {
		let bytes = from_hex(genesis_hex.trim_start_matches("0x"));
		let mut out = [0u8; 32];
		out.copy_from_slice(&bytes);
		out
	};

	match command.as_str() {
		"deposit" => {
			let (pair, who) = trader.expect("--key is required to deposit");
			let amount: u128 = flag(&args, "--amount", "1000000000").parse().expect("amount");
			let call = RuntimeCall::ExchangeCore(CoreCall::deposit_collateral { amount });
			// Read the nonce rather than assuming 0: a hardcoded one works exactly once, and
			// every later failure looks like the transaction being rejected rather than like
			// a stale nonce in this file.
			let nonce: u32 = rpc(&url, "system_accountNextIndex", json!([format!("0x{}", to_hex(&who))]))
				["result"]
				.as_u64()
				.expect("nonce") as u32;
			let xt = in_ext(genesis, || {
			let extra: TxExtension = (
				frame_system::CheckNonZeroSender::<R>::new(),
				frame_system::CheckSpecVersion::<R>::new(),
				frame_system::CheckTxVersion::<R>::new(),
				frame_system::CheckGenesis::<R>::new(),
				frame_system::CheckEra::<R>::from(Era::Immortal),
				frame_system::CheckNonce::<R>::from(nonce),
				frame_system::CheckWeight::<R>::new(),
				pallet_transaction_payment::ChargeTransactionPayment::<R>::from(0),
				frame_metadata_hash_extension::CheckMetadataHash::<R>::new(false),
				frame_system::WeightReclaim::<R>::new(),
			);
			let payload = SignedPayload::new(call.clone(), extra.clone()).expect("payload");
			// keccak, not blake2: see the module docs.
			let digest = payload.using_encoded(keccak_256);
			let signature = pair.sign_prehashed(&digest);
			UncheckedExtrinsic::new_signed(
				call,
				Address::from(who),
				sunchain_runtime::Signature::from(MultiSignature::Ecdsa(signature)),
				extra,
			)
			});
			let result = rpc(&url, "author_submitExtrinsic", json!([format!("0x{}", to_hex(&xt.encode()))]));
			println!("deposit -> {result}");
		},
		"feed" => {
			let market_id: u32 = flag(&args, "--market", "0").parse().expect("market");
			// Whole quote units; the tick size comes from the chain so this tool cannot
			// disagree with what the market actually takes.
			let price_units: u128 = flag(&args, "--price", "60000").parse().expect("price");
			let tick = tick_size_quote(&url, market_id);
			let ticks = price_to_ticks(
				price_units.saturating_mul(FEED_SCALE as u128),
				FEED_SCALE,
				QUOTE_DECIMALS,
				tick,
			)
			.expect("price does not fit in ticks");
			println!("market {market_id}: tick={tick} price={ticks} ticks");

			// A price is only good for `MaxPriceAge` blocks, so publishing once lets every
			// later order be refused for staleness. `--every` keeps it warm; the interval must
			// leave room for the previous vote to be included, or the next nonce clashes with
			// it and both are dropped.
			let every: u64 = flag(&args, "--every", "0").parse().expect("every");
			loop {
				// Every source has to publish: the oracle needs a quorum before a price
				// exists at all, and half a quorum is indistinguishable from none.
				for key in sunchain::oracle_keys::source_keys() {
					let source = key.to_account_id();
					let nonce = last_nonce(&url, &source).saturating_add(1);
					let prices = vec![(market_id, ticks)];
					// The payload comes from the pallet rather than being rebuilt here — a
					// byte difference produces a vote that is silently rejected.
					let payload = pallet_oracle::Pallet::<R>::vote_payload(&source, &prices, nonce);
					let signature = sunchain_runtime::Signature::from(MultiSignature::Ecdsa(
						key.pair().sign_prehashed(&keccak_256(&payload)),
					));
					let xt = UncheckedExtrinsic::new_bare(RuntimeCall::Oracle(
						pallet_oracle::Call::submit_price_vote {
							source,
							signature,
							prices: prices.clone().try_into().expect("one price fits"),
							nonce,
						},
					));
					let reply = rpc(
						&url,
						"author_submitExtrinsic",
						json!([format!("0x{}", to_hex(&xt.encode()))]),
					);
					println!(
						"  source 0x{} nonce {nonce} -> {}",
						to_hex(&source.0),
						short(&reply)
					);
				}
				if every == 0 {
					break
				}
				std::thread::sleep(std::time::Duration::from_secs(every));
			}
		},
		side @ ("buy" | "sell") => {
			let (pair, who) = trader.expect("--key is required to trade");
			let market_id: u32 = flag(&args, "--market", "0").parse().expect("market");
			let price_ticks: u64 = flag(&args, "--price", "60000").parse().expect("price");
			let quantity_lots: u64 = flag(&args, "--lots", "1").parse().expect("lots");
			let nonce: u64 = flag(&args, "--nonce", "1").parse().expect("nonce");
			// An order at `expires_at: 0` is already expired by the time it executes, and is
			// rejected with `InvalidExpiry` — after its nonce has been consumed, so retrying
			// under the same nonce fails differently and the real cause never surfaces.
			let expiry_blocks: BlockNumber = flag(&args, "--expiry", "10000").parse().expect("expiry");
			let expires_at = head_block(&url) + expiry_blocks;

			let action = ExchangeAction::Place(OrderRequest::<BlockNumber> {
				market_id,
				client_order_id: nonce,
				side: if side == "buy" { Side::Buy } else { Side::Sell },
				price_ticks,
				quantity_lots,
				tif: TimeInForce::GoodTilCancelled,
				post_only: false,
				reduce_only: false,
				expires_at,
			});
			let subaccount = 0u32;
			// Exactly what the runtime re-derives in `validate_signed_action`, then
			// keccak'd — see the module docs.
			let payload = (subaccount, nonce, &action).encode();
			let signature = pair.sign_prehashed(&keccak_256(&payload));

			let mut raw = [0u8; 65];
			raw.copy_from_slice(&signature.0);
			let signed = ExchangeSignedAction {
				signer: ExchangeSigner(who),
				subaccount,
				action_nonce: nonce,
				action,
				signature: ExchangeSignature(raw),
			};
			// Ask the runtime why before asking anyone to act. `submitAction` answers only
			// "accepted into the pool", and the batch runs later — so a rejected order looks
			// identical to a successful one from here, unless it is checked explicitly.
			let verdict = validate(&url, &signed);
			println!("validate -> {verdict}");

			// The pre-check covers signature and nonce only; it says nothing about whether the
			// order will actually be accepted once it runs. The batch is unsigned, so when it
			// rejects this there is nobody left to report it: the action evaporates and the
			// nonce still advances. Dry-running the very bytes the proposer will build is the
			// only way to hear the reason.
			let batch = RuntimeCall::ExchangeCore(CoreCall::execute_exchange_batch {
				actions: frame_support::BoundedVec::truncate_from(vec![signed.clone()]),
			});
			let dry = UncheckedExtrinsic::new_bare(batch).encode();
			println!("dry-run -> {}", dry_run_outcome(&rpc(
				&url,
				"system_dryRun",
				json!([format!("0x{}", to_hex(&dry))]),
			)));
			let encoded = format!("0x{}", to_hex(&signed.encode()));
			let reply = rpc(&url, "exchange_submitAction", json!([encoded]));
			println!("order -> {reply}");

			// All the promises above stop here. The batch swallows a rejected action and still
			// consumes its nonce, so "accepted", "executed" and even "dispatched Ok" prove
			// nothing about the order. Resting in the book is the only evidence there is.
			let prefix = format!(
				"0x{}",
				to_hex(&[sp_core::hashing::twox_128(b"ExchangeCore").as_slice(), sp_core::hashing::twox_128(b"Orders").as_slice()].concat())
			);
			for attempt in 0..12 {
				std::thread::sleep(std::time::Duration::from_secs(2));
				let keys = rpc(&url, "state_getKeysPaged", json!([prefix, 50]));
				let resting = count_orders(&url, &keys, &who);
				if resting > 0 {
					println!("resting orders: {resting}");
					break
				}
				if attempt == 11 {
					println!(
						"resting orders: none — either it matched immediately (see Positions) \
						 or it was refused; a refused action leaves nothing behind but an \
						 advanced nonce"
					);
				}
			}
		},
		other => {
			eprintln!("unknown command: {other}");
			std::process::exit(2)
		},
	}
}

/// How many of the listed `Orders` entries were placed by `who`.
///
/// The owner is the first field of an encoded order, so this can be matched without decoding
/// the whole struct — which keeps the ownership test in one place instead of in whatever
/// reader happens to look at `Orders` next.
fn count_orders(url: &str, keys: &Value, who: &[u8; 20]) -> usize {
	let Some(keys) = keys["result"].as_array() else { return 0 };
	keys.iter()
		.filter_map(|k| k.as_str())
		.filter(|k| {
			let value = rpc(url, "state_getStorage", json!([k]));
			value["result"]
				.as_str()
				.map(|hex| {
					let bytes = from_hex(hex.trim_start_matches("0x"));
					bytes.len() >= 20 && bytes[..20] == *who
				})
				.unwrap_or(false)
		})
		.count()
}

/// USDC, like USDT: six decimals. Only used to scale the price below.
const QUOTE_DECIMALS: u8 = 6;

/// A market's own tick size, read rather than assumed.
fn tick_size_quote(url: &str, market_id: u32) -> u128 {
	let bytes = raw_storage(url, b"ExchangeCore", b"Markets", &blake2_concat(&market_id.encode()));
	let market = pallet_exchange_core::types::Market::<BlockNumber>::decode(&mut &bytes[..])
		.expect("market value");
	market.tick_size_quote
}

/// How far this source's votes have got, read rather than counted locally.
fn last_nonce(url: &str, who: &sunchain_runtime::AccountId) -> u64 {
	let bytes = raw_storage(url, b"Oracle", b"Sources", &[]);
	let records: Vec<pallet_oracle::SourceRecord<sunchain_runtime::AccountId, u32>> =
		Decode::decode(&mut &bytes[..]).expect("oracle sources");
	records.into_iter().find(|r| r.key == *who).map(|r| r.last_nonce as u64).unwrap_or(0)
}

/// `Blake2_128Concat`: the hasher `StorageMap` defaults to, spelled out.
fn blake2_concat(key: &[u8]) -> Vec<u8> {
	let mut out = sp_core::hashing::blake2_128(key).to_vec();
	out.extend_from_slice(key);
	out
}

fn raw_storage(url: &str, pallet: &[u8], item: &[u8], appended: &[u8]) -> Vec<u8> {
	let mut key = sp_core::hashing::twox_128(pallet).to_vec();
	key.extend_from_slice(&sp_core::hashing::twox_128(item));
	key.extend_from_slice(appended);
	let hex = rpc(url, "state_getStorage", json!([format!("0x{}", to_hex(&key))]))["result"]
		.as_str()
		.expect("storage value")
		.to_string();
	from_hex(hex.trim_start_matches("0x"))
}

/// Result or error, without the surrounding JSON-RPC envelope.
fn short(reply: &Value) -> String {
	match (&reply["result"], &reply["error"]) {
		(result, error) if error.is_object() => format!("{}", error["message"]),
		(result, _) => result.to_string(),
	}
}

/// What the runtime would do with the bytes we are about to hand the proposer.
fn dry_run_outcome(reply: &Value) -> String {
	match reply["result"].as_str() {
		Some(hex) => {
			let bytes = from_hex(hex.trim_start_matches("0x"));
			match ApplyExtrinsicResult::decode(&mut &bytes[..]) {
				Ok(outcome) => format!("{outcome:?}"),
				Err(decode) => format!("undecodable ({decode:?}): 0x{hex}"),
			}
		},
		None => format!("{reply}"),
	}
}

/// Latest block number, so an expiry can be expressed relative to *now*.
///
/// A relative expiry is the only honest default: a fixed height would look fine today and
/// be silently in the past tomorrow.
fn head_block(url: &str) -> BlockNumber {
	let number = rpc(url, "chain_getHeader", json!([]))["result"]["number"]
		.as_str()
		.expect("head block number")
		.to_string();
	BlockNumber::from_str_radix(number.trim_start_matches("0x"), 16).expect("head block number")
}

/// Runs the runtime's pre-check and translates its answer into something readable.
fn validate(url: &str, signed: &ExchangeSignedAction) -> String {
	let encoded = format!("0x{}", to_hex(&signed.encode()));
	let reply = rpc(url, "state_call", json!(["ExchangeRuntimeApi_validate_exchange_action", encoded]));
	match reply["result"].as_str() {
		Some(hex) => {
			let bytes = from_hex(hex.trim_start_matches("0x"));
			match Result::<(), DispatchError>::decode(&mut &bytes[..]) {
				Ok(Ok(())) => "ok".into(),
				Ok(Err(e)) => format!("REJECTED: {e:?}"),
				Err(decode) => format!("undecodable verdict ({decode:?}): 0x{hex}"),
			}
		},
		None => format!("{reply}"),
	}
}

/// The sub-command, skipping flags and the values that follow them.
fn command_of(args: &[String]) -> String {
	const VALUED: &[&str] = &[
		"--key", "--rpc", "--amount", "--market", "--price", "--lots", "--nonce", "--expiry",
	];
	let mut i = 1;
	let mut found = String::new();
	while i < args.len() {
		if args[i].starts_with("--") {
			if VALUED.contains(&args[i].as_str()) {
				i += 1;
			}
		} else if found.is_empty() {
			found = args[i].clone();
		}
		i += 1;
	}
	found
}

fn flag(args: &[String], name: &str, default: &str) -> String {
	args.windows(2)
		.find(|w| w[0] == name)
		.map(|w| w[1].clone())
		.unwrap_or_else(|| default.to_string())
}
