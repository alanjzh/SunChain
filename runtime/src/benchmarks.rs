// This is free and unencumbered software released into the public domain.
//
// Anyone is free to copy, modify, publish, use, compile, sell, or
// distribute this software, either in source code form or as a compiled
// binary, for any purpose, commercial or non-commercial, and by any
// means.
//
// In jurisdictions that recognize copyright laws, the author or authors
// of this software dedicate any and all copyright interest in the
// software to the public domain. We make this dedication for the benefit
// of the public at large and to the detriment of our heirs and
// successors. We intend this dedication to be an overt act of
// relinquishment in perpetuity of all present and future rights to this
// software under copyright law.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
// EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
// MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.
// IN NO EVENT SHALL THE AUTHORS BE LIABLE FOR ANY CLAIM, DAMAGES OR
// OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE,
// ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR
// OTHER DEALINGS IN THE SOFTWARE.
//
// For more information, please refer to <http://unlicense.org>

frame_benchmarking::define_benchmarks!(
	[frame_benchmarking, BaselineBench::<Runtime>]
	[frame_system, SystemBench::<Runtime>]
	[frame_system_extensions, SystemExtensionsBench::<Runtime>]
	[pallet_balances, Balances]
	[pallet_timestamp, Timestamp]
	[pallet_sudo, Sudo]
	[pallet_template, Template]
	[pallet_evm, EVM]
	// [pallet_assets, Assets] -- see the note in `runtime/Cargo.toml`: enabling this requires
	// `pallet-assets/runtime-benchmarks`, which currently breaks the build via
	// `pallet-revive-fixtures`. Re-enable both at the same time.
	[pallet_exchange_core, ExchangeCore]
	[pallet_oracle, Oracle]
	// `pallet_liquidation` is deliberately absent: `add_benchmark!` needs the pallet to
	// implement `Benchmarking`, and its two calls cannot be benchmarked in isolation — setting
	// up an underwater account requires reaching into `pallet-exchange-core`'s storage, which
	// the benchmark cannot do because the pallet is generic over `ExchangeEngine` rather than
	// over the core's `Config`.
	//
	// It is also not worth much: the pallet only forwards to the engine and pays the keeper, so
	// its weights are *derived* from the engine's measured components (see
	// `pallets/liquidation/src/weights.rs`). The work that matters is measured by
	// `pallet_exchange_core`'s `liquidate_account` and `adl_close` benchmarks above.
);
