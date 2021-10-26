use serde::Deserialize;
use simd_json::OwnedValue;

#[derive(Deserialize)]
pub struct Identify {
    pub d: IdentifyInfo,
}

#[derive(Deserialize)]
pub struct IdentifyInfo {
    pub shard: [u64; 2],
}

#[derive(Deserialize)]
pub struct Ready {
    pub d: JsonObject,
}

pub type JsonObject = halfbrown::HashMap<String, OwnedValue>;