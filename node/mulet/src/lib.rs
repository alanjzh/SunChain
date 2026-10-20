#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

include!(concat!(env!("OUT_DIR"), "/mulet_bindings.rs"));


/// This is a void type given host context is an opaque pointer. Functions allow it to be a null ptr.
pub type mulet_host_context = ::std::os::raw::c_void;