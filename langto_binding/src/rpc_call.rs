use serde_json::Value;
use reqwest;

pub async fn call_jsonrpc_raw(
    url: &str,
    method: &str,
    params: Value,
) -> Result<Value, reqwest::Error> {
    let client = reqwest::Client::new();
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    println!("Calling JSON-RPC: url = {}, method = {}, params = {}, \n\t req = {}", 
        url, method, params, req);
    let resp = client.post(url).json(&req).send().await?;
    let resp_json: Value = resp.json().await?;
    println!("resp : {}", resp_json);
    Ok(resp_json["result"].clone())
}