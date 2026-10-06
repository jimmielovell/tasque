use crate::BoxError;
use serde::Serialize;
use serde::de::DeserializeOwned;

#[cfg(not(any(feature = "bincode", feature = "json")))]
compile_error!(
    "tasque needs a format for persisted jobs: enable the `bincode` (default) or `json` feature"
);

#[cfg(feature = "json")]
pub(crate) fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, BoxError> {
    Ok(serde_json::to_vec(value)?)
}

#[cfg(feature = "json")]
pub(crate) fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, BoxError> {
    Ok(serde_json::from_slice(bytes)?)
}

#[cfg(all(feature = "bincode", not(feature = "json")))]
pub(crate) fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, BoxError> {
    Ok(bincode::serde::encode_to_vec(
        value,
        bincode::config::standard(),
    )?)
}

#[cfg(all(feature = "bincode", not(feature = "json")))]
pub(crate) fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, BoxError> {
    let (value, read) = bincode::serde::decode_from_slice(bytes, bincode::config::standard())?;
    if read != bytes.len() {
        return Err(format!("{} bytes left over after decoding", bytes.len() - read).into());
    }
    Ok(value)
}
