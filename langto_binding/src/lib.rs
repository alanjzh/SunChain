// #![feature(trivial_bounds)]
use ::safer_ffi::prelude::*;
use sp_runtime::{
		generic,
		traits::BlakeTwo256,
	};
// use sp_core::H256;
use url::Url;

pub use sp_runtime::OpaqueExtrinsic as UncheckedExtrinsic;
pub mod rpc_call;
const LOCAL_NODE_URL: &str = "http://127.0.0.1:9944";

#[cfg(feature = "mcoklangto_data")]
pub mod mock;

pub type BlockNumber = u32;
	/// Opaque block header type.

pub type Header = generic::Header<BlockNumber, BlakeTwo256>;
// #[derive_ReprC]
// #[repr(C)]
#[derive(Debug, Clone)]    /// 
pub struct HeaderBinding {
    pub header: Header,
}
// pub type Block = generic::Block<HeaderBinding, UncheckedExtrinsic>;
// 	/// Opaque block identifier type.
// pub type BlockId = generic::BlockId<Block>;
// 	/// Opaque block hash type.
// pub type Hash = <BlakeTwo256 as HashT>::Output;

// #[derive_ReprC]
// #[repr(C)]
// #[derive(Debug, Clone)]
// pub struct BlockBinding {
//     pub block: Block,
// }




// #[derive_ReprC]
// #[repr(C)]
// #[derive(Debug, Clone)]
// pub struct BlockBinding {
//     pub block: BlockType,
// }

// // Dummy Digest and DecodeWithMemTracking
// #[derive(Debug, Clone, Eq, PartialEq)]
// pub struct Digest {}
// trait DecodeWithMemTracking: Decode {}
// impl DecodeWithMemTracking for ReprCOpaqueExtrinsic {}

// use safer_ffi::prelude::char_p::CharPtr;

// #[derive_ReprC]
// #[repr(C)]
// #[derive(Debug, Clone)]
// pub struct OseRequest <'a> {
//     pub method: char_p::Ref<'a>,
//     pub params: char_p::Ref<'a>,
// }

// #[derive_ReprC]
// #[repr(C)]
// #[derive(Debug, Clone)]
// pub struct OseResponse <'a>{
//     pub code: u64,
//     pub res: char_p::Ref<'a>,
// }
// impl<'a> OseResponse<'a> {
//     pub fn new(code: u64, res: char_p::Ref<'a>) -> Self {
//         OseResponse { code, res }
//     }
// }

// #[ffi_export]
// fn call_by_ose<'a>(req: &'a OseRequest<'a>) -> OseResponse<'a> {
//     println!("Received request: {:?}", req);
//     // Example: echo back the method as the response
//     OseResponse::new(0, req.method.clone())
// }

#[derive_ReprC]
#[repr(C)]
#[derive(Debug)]
pub struct OseCall {
    pub method: safer_ffi::prelude::str::Box,
    pub params: safer_ffi::prelude::str::Box,
    pub result: Option<safer_ffi::prelude::str::Box>,
}

#[ffi_export]
fn call_by_ose_use_box(data: &mut OseCall) -> u64 {
    println!("Received request: {:?}", data);

    // OseResponse::new(0, req.method.clone())
    let r = safer_ffi::prelude::str::Box::from(format!("Method: {:?}, Params: {:?}", 
            data.method, data.params));
    // let _ = std::mem::replace(&mut data.result, r);        
    // unsafe {}
    data.result = Some(r);
    println!("Response: {:?}", data.result);
    0
}

#[cfg(not(feature = "mcoklangto_data"))]
#[ffi_export]
fn call_by_ose_with_char_p<'a>(
    method: char_p::Ref<'a>,
    params: char_p::Ref<'a>,
) -> Option<safer_ffi::prelude::str::Box> {
    println!("Received request: method = {:?}, params = {:?}", method, params);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut resv : serde_json::Value = serde_json::json!({});

    rt.block_on(async {
        let url = LOCAL_NODE_URL;
        let method = method.to_str();
        let params = serde_json::from_str(params.to_str()).unwrap_or(serde_json::Value::Null);
        
        let result = rpc_call::call_jsonrpc_raw(url, method, params).await;
        match result {
            Ok(res) => {
                println!("Response: {:?}", res);
                resv = res; 
            },
            Err(e) => eprintln!("Error: {:?}", e),
        }
    });
    
    let mut c_ret = resv.to_string();
    c_ret.insert(c_ret.len(),'\0');
    Some(
        safer_ffi::prelude::str::Box::from(c_ret)
    )
}

#[cfg(feature = "mcoklangto_data")]
#[ffi_export]
fn call_by_ose_with_char_p<'a>(
    method: char_p::Ref<'a>,
    params: char_p::Ref<'a>,
) -> Option<safer_ffi::prelude::str::Box> {
    println!("Received request: method = {:?}, params = {:?}", method, params);
    // let rt = tokio::runtime::Runtime::new().unwrap();
    // let mut resv : serde_json::Value = serde_json::json!({});
    let resv : serde_json::Value = match method.to_str() {
        "system_chain" => mock::CHAIN_NAME.into(),
        "rpc_methods" => mock::CHAIN_METHOD.into(),
        "chain_getHeader" => mock::CHAIN_HEADER.into(),
        "state_getMetadata" => mock::CHAIN_METADATA.into(),
        _ => "".into(),
    } ;
    
    Some(
        safer_ffi::prelude::str::Box::from(resv.to_string())
    )
}

#[ffi_export]
fn call_by_ose_with_char_p_and_url<'a>(
    method: char_p::Ref<'a>,
    params: char_p::Ref<'a>,
    node_url: char_p::Ref<'a>,
) -> Option<safer_ffi::prelude::str::Box> {
    println!("Received request: method = {:?}, params = {:?}, url = {:?}", method, params, node_url);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut resv : serde_json::Value = serde_json::json!({});

    rt.block_on(async {
        let mut url = node_url.to_str();
        url = if url.is_empty() || Url::parse(url).is_err() {
            println!("node_url {} is empty or incorrect, Using default local node URL.", url);
            LOCAL_NODE_URL
        } else {
            url
        };
        let method = method.to_str();
        let params = serde_json::from_str(params.to_str()).unwrap_or(serde_json::Value::Null);
        
        let result = rpc_call::call_jsonrpc_raw(url, method, params).await;
        match result {
            Ok(res) => {
                println!("Response: {:?}", res);
                resv = res; 
            },
            Err(e) => eprintln!("Error: {:?}", e),
        }
    });
    
    
    let mut c_ret = resv.to_string();
    c_ret.insert(c_ret.len(),'\0');
    Some(
        safer_ffi::prelude::str::Box::from(c_ret)
    )
}

#[ffi_export]
fn release_str_box(b: safer_ffi::prelude::str::Box) {
    println!("Drop box : {:?}", b);
    drop(b)
}


// The following function is only necessary for the header generation.
#[cfg(feature = "headers")] // c.f. the `Cargo.toml` section
pub fn generate_headers() -> ::std::io::Result<()> {
    ::safer_ffi::headers::builder()
        .to_file("sunchain_langto_binding.h")?
        .generate()
}


#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_rpc_call() {
        let url = LOCAL_NODE_URL;
        let method = "system_chain";
        let params = serde_json::json!([]);
        
        let result = rpc_call::call_jsonrpc_raw(url, method, params).await;
        match result {
            Ok(res) => println!("Response: {:?}", res),
            Err(e) => eprintln!("Error: {:?}", e),
        }
    }
}
