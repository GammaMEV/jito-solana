use crate::transaction::VersionedTransaction;

pub mod error;
pub mod sanitized;
pub mod utils;

#[derive(Debug, PartialEq, Default, Eq, Clone, Serialize, Deserialize)]
pub struct Bundle {
    pub transactions: Vec<VersionedTransaction>,
}
