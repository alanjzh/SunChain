use ::safer_ffi::prelude::*;
use safer_ffi::c_int;
mod service;
mod cli;
mod command;
mod chain_spec;
mod rpc;
mod node_primitives;
mod benchmarking;
mod eth;
mod client;
mod exchange_pool;
pub mod oracle_keys;
mod premium_feed;
pub mod oracle_feed;
pub mod pricefeed;
mod exchange_p2p;
mod exchange_proposer;

pub fn initialize(args: Vec<String>) -> Result<(), String> {
    let _ = command::run(Some(args));

    Ok(())
}

#[ffi_export]
pub fn initialize_by_mulet<'a>(
    argc: i32,
    argv: *const *const u8,
) -> u64 {
    let args: Vec<String> = unsafe {
        std::slice::from_raw_parts(argv as *const *const std::os::raw::c_char, argc as usize)
            .iter()
            .map(|&arg| {
                let c_str = std::ffi::CStr::from_ptr(arg as *const std::os::raw::c_char);
                c_str.to_string_lossy().into_owned()
            })
            .collect()
    };

    let result = initialize(args);
    match result {
        Ok(_) => 0,
        Err(e) => {
            eprintln!("Initialization failed: {}", e);
            0
        }
    }
}

// extern "C" {
//     #[doc = " @brief mulet main entry\n @param argc\n @param argv\n @return 0: normal starup; not 0 starup with exception"]
//     pub fn mulet_start(
//         argc: ::std::os::raw::c_int,
//         argv: *mut *mut ::std::os::raw::c_char,
//     ) -> ::std::os::raw::c_int;
// }

// extern "C" {
//     #[doc = " @brief get mulet activated status\n @return true: activated, false: unactivated"]
//     pub fn mulet_is_activated() -> bool;
// }
// extern "C" {
//     #[doc = " @brief close mulet, notify mulet cleaning resource to stop running"]
//     pub fn mulet_stop();
// }

#[doc = " @brief mulet main entry\n @param argc\n @param argv\n @return 0: normal starup; not 0 starup with exception"]
#[ffi_export]
fn mulet_start (
    argc: i32,
    argv: *const *const u8,
) -> i32
{
    let args: Vec<String> = unsafe {
        std::slice::from_raw_parts(argv as *const *const std::os::raw::c_char, argc as usize)
            .iter()
            .map(|&arg| {
                let c_str = std::ffi::CStr::from_ptr(arg as *const std::os::raw::c_char);
                c_str.to_string_lossy().into_owned()
            })
            .collect()
    };

    let result = initialize(args);
    match result {
        Ok(_) => 0, 
        Err(e) => {
            eprintln!("Mulet start failed: {}", e);
            -1 
        }
    } 
}

#[doc = " @brief get mulet activated status\n @return true: activated, false: unactivated"]
#[ffi_export]
fn mulet_is_activated ()
  -> bool
{
  true
}

#[doc = " @brief close mulet, notify mulet cleaning resource to stop running"]
#[ffi_export]
fn mulet_stop ()
{
    println!("Sunchain is stopping...");
}