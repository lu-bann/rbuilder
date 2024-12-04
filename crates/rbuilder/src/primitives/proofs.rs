use alloy_primitives::{Bytes, TxHash};
use alloy_rlp::Encodable;
use ethereum_consensus::{
    bellatrix::presets::minimal::Transaction, deneb::minimal::MAX_TRANSACTIONS_PER_PAYLOAD,
    phase0::Bytes32, ssz::prelude::*,
};
use reth_primitives::TransactionSigned;

pub const MAX_CONSTRAINTS_PER_SLOT: usize = 256;

pub type ExecutionPayloadTransactions = List<Transaction, MAX_TRANSACTIONS_PER_PAYLOAD>;

#[derive(Debug, thiserror::Error)]
pub enum ProofError {
    #[error("Leaves and indices length mismatch")]
    LengthMismatch,
    #[error("Mismatch in provided leaves and leaves to prove")]
    LeavesMismatch,
    #[error("Hash not found in constraints cache: {0:?}")]
    MissingHash(TxHash),
    #[error("Proof verification failed")]
    VerificationFailed,
    #[error("Decoding failed: {0}")]
    DecodingFailed(String),
}

/// InclusionProof is a Merkle Multiproof of inclusion of a set of TransactionHashes
#[derive(Debug, Clone, PartialEq, SimpleSerialize, serde::Serialize, serde::Deserialize)]
pub struct InclusionProofs {
    pub transaction_hashes: List<Bytes32, MAX_CONSTRAINTS_PER_SLOT>,
    pub generalized_indexes: List<u64, MAX_CONSTRAINTS_PER_SLOT>,
    pub merkle_hashes: List<Bytes32, MAX_TRANSACTIONS_PER_PAYLOAD>,
}

impl InclusionProofs {
    /// Returns the total number of leaves in the tree.
    pub fn total_leaves(&self) -> usize {
        self.transaction_hashes.len()
    }
}

pub fn calculate_merkle_multi_proofs(
    payload_transactions: Vec<TransactionSigned>,
    constraints: Vec<TransactionSigned>,
) -> Result<InclusionProofs, ProofError> {
    let mut raw_txs = Vec::with_capacity(payload_transactions.len());
    for tx in payload_transactions.clone() {
        let mut tx_bytes = Vec::new();
        tx.encode(&mut tx_bytes);
        raw_txs.push(Bytes::from(tx_bytes));
    }

    let ssz_txs: List<List<u8, 1073741824>, 1048576> = {
        let inner: Vec<List<u8, 1073741824>> = raw_txs
            .into_iter()
            .map(|tx| List::try_from(tx.to_vec()).unwrap())
            .collect();

        List::try_from(inner).unwrap()
    };

    let _root_node = ssz_txs.hash_tree_root().unwrap();

    let mut indexes: Vec<usize> = Vec::with_capacity(constraints.len());
    for constraint in constraints.clone() {
        let tx_hash = constraint.hash();
        let index = payload_transactions
            .iter()
            .position(|tx| tx.hash() == tx_hash)
            .ok_or(ProofError::MissingHash(tx_hash))?;
        indexes.push(index);
    }

    let path = indexes
        .iter()
        .map(|i| PathElement::from(*i))
        .collect::<Vec<PathElement>>();

    let (multi_proof, witness) = ssz_txs.multi_prove(&[&path]).unwrap();
    assert!(multi_proof.verify(witness).is_ok());
    let inclusion_proof = create_inclusion_proof_from_multi_proof(multi_proof, constraints)?;

    Ok(inclusion_proof)
}

/// Create InclusionProofs from a MultiProof and a list of constraints
fn create_inclusion_proof_from_multi_proof(
    multi_proof: ssz_rs::proofs::MultiProof,
    constraints: Vec<TransactionSigned>,
) -> Result<InclusionProofs, ProofError> {
    let mut generalised_indexes = Vec::with_capacity(multi_proof.indices.len());
    let mut merkle_hashes = Vec::with_capacity(multi_proof.branch.len());
    let mut transaction_hashes = Vec::with_capacity(constraints.len());

    for idx in multi_proof.indices {
        generalised_indexes.push(idx as u64);
    }

    for hash in multi_proof.branch {
        merkle_hashes.push(Bytes32::try_from(hash.as_slice()).unwrap());
    }

    for constraint in constraints {
        let tx_hash = constraint.hash();
        transaction_hashes.push(Bytes32::try_from(tx_hash.as_slice()).unwrap());
    }

    Ok(InclusionProofs {
        transaction_hashes: List::try_from(transaction_hashes).unwrap(),
        generalized_indexes: List::try_from(generalised_indexes).unwrap(),
        merkle_hashes: List::try_from(merkle_hashes).unwrap(),
    })
}

#[cfg(test)]
mod tests {
    use crate::primitives::{AccountNonce, TestDataGenerator};

    use super::*;
    use revm_primitives::Address;

    #[test]
    fn test_calculate_merkle_multi_proofs() {
        let mut test_data_generator = TestDataGenerator::default();
        let mut nonce = 0;

        let payload_txs = (0..5)
            .map(|_| {
                let tx = test_data_generator.create_tx_nonce(AccountNonce {
                    nonce,
                    account: Address::default(),
                });
                nonce += 1;
                tx.into_signed()
            })
            .collect::<Vec<TransactionSigned>>();
        let constraints = vec![payload_txs[1].clone(), payload_txs[3].clone()];

        let inclusion_proof = calculate_merkle_multi_proofs(payload_txs.clone(), constraints);
        assert!(inclusion_proof.is_ok())
    }
}
