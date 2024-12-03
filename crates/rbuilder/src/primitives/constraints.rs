use alloy_eips::eip2718::Decodable2718;
use ethereum_consensus::{
    bellatrix::presets::minimal::Transaction,
    primitives::{BlsPublicKey, BlsSignature},
    ssz::prelude::*,
};
use reth_primitives::PooledTransactionsElement;
use sha2::{Digest, Sha256};

pub const MAX_CONSTRAINTS_PER_SLOT: usize = 256;

#[derive(Debug, Clone, Serializable, serde::Deserialize, serde::Serialize)]
pub struct SignedConstraints {
    pub message: ConstraintsMessage,
    pub signature: BlsSignature,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, Serializable)]
pub struct ConstraintsMessage {
    pub pubkey: BlsPublicKey,
    pub slot: u64,
    pub top: bool,
    pub transactions: List<Transaction, MAX_CONSTRAINTS_PER_SLOT>,
}

impl ConstraintsMessage {
    fn digest(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(&self.pubkey.to_vec());
        hasher.update(self.slot.to_le_bytes());
        hasher.update((self.top as u8).to_le_bytes());
        for tx in self.transactions.iter() {
            // Convert the opaque bytes to a EIP-2718 envelope and obtain the tx hash.
            // this is needed to handle type 3 transactions.
            // FIXME: don't unwrap here and handle the error properly
            let tx = PooledTransactionsElement::decode_2718(&mut tx.as_slice()).unwrap();
            hasher.update(tx.hash().as_slice());
        }

        hasher.finalize().into()
    }
}
