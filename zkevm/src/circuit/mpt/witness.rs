use super::{CanRead, TrieProof, AccountProof, AccountData, extend_address_to_h256, StorageProof};
use crate::circuit::builder::verify_proof_leaf;
use eth_types::{Hash, Bytes, H256, U256};
use ethers_core::abi::Address;
use mpt_circuits::hash::Hashable;
use mpt_circuits::serde::{HexBytes, Hash as SMTHash, SMTNode, SMTPath, SMTTrace, StateData};
use std::collections::HashMap;
use types::eth::{AccountProofWrapper, BlockResult, StorageProofWrapper};
use halo2_proofs::halo2curves::bn256::Fr;
use halo2_proofs::halo2curves::group::ff::{Field, PrimeField};
use halo2_proofs::arithmetic::FieldExt;
use zktrie::{ZkMemoryDb, ZkTrieNode, ZkTrie};

use num_bigint::BigUint;
use std::io::{Error as IoError, Read};

pub struct WitnessGenerator {
    pub db: ZkMemoryDb,
    pub trie: ZkTrie,
    pub accounts: HashMap<Address, Option<AccountData>>,
    pub storages: HashMap<Address, ZkTrie>,
}

static FILED_ERROR_READ: &str = "invalid input field";
static FILED_ERROR_OUT: &str = "output field fail";

extern "C" fn hash_scheme(a: *const u8, b: *const u8, out: *mut u8) -> *const i8 {
    use std::slice;
    let a : [u8; 32 ] = TryFrom::try_from(unsafe { slice::from_raw_parts(a, 32) }).expect("length specified" );
    let b : [u8; 32 ] = TryFrom::try_from(unsafe { slice::from_raw_parts(b, 32) }).expect("length specified" );
    let mut out : [u8; 32 ] = TryFrom::try_from(unsafe { slice::from_raw_parts_mut(out, 32) }).expect("length specified" );

    let fa = Fr::from_bytes(&a);
    let fa = if fa.is_some().into() {
        fa.unwrap()
    } else {
        return FILED_ERROR_READ.as_ptr().cast();
    };
    let fb = Fr::from_bytes(&b);
    let fb = if fb.is_some().into() {
        fb.unwrap()
    } else {
        return FILED_ERROR_READ.as_ptr().cast();
    };

    let h = Fr::hash([fa, fb]);
    let repr_h = h.to_repr().as_ref();
    if repr_h.len() == 32 {
        out.as_mut_slice().copy_from_slice(h.to_repr().as_ref());
        std::ptr::null()
    }else {
        FILED_ERROR_OUT.as_ptr().cast()
    }

}

impl WitnessGenerator {

    pub fn init() {
        zktrie::init_hash_scheme(hash_scheme);
    }

    pub fn new(block: &BlockResult) -> Self {
        let mut db = ZkMemoryDb::new();
        let storage_trace = &block.storage_trace;

        storage_trace
        .proofs
        .iter()
        .flatten()
        .flat_map(|(_, proofs)|proofs.iter())
        .for_each(|bytes| {
            db.add_node_bytes(bytes.as_ref()).unwrap();
        });

        storage_trace.storage_proofs.iter()
            .flat_map(|(_, v_proofs)|v_proofs.iter())
            .flat_map(|(_, proofs)|proofs.iter())
            .for_each(|bytes| {
                db.add_node_bytes(bytes.as_ref()).unwrap();
            });

        let accounts: HashMap<Address, Option<AccountData>> = storage_trace
            .proofs
            .iter()
            .flatten()
            .map(|(account, proofs)| {
                let proof : AccountProof = verify_proof_leaf(proofs.as_slice().try_into().unwrap(), &extend_address_to_h256(account));
                (*account, proof.key.as_ref().map(|_|proof.data))
            })
            .collect();

        let mut storages = HashMap::new();
        for (account, storage_map) in storage_trace.storage_proofs.iter() {
            assert!(accounts.contains_key(account));
            let acc_data = accounts.get(account).unwrap();
            let mut s_trie = db.new_trie(&acc_data.map(|d|d.storage_root).unwrap_or_else(Hash::zero).0).unwrap();
            
            for (k, v_proofs) in storage_map {
                let mut k_buf: [u8; 32] = [0; 32];
                k.to_big_endian(&mut k_buf[..]);
                let proof: StorageProof = verify_proof_leaf(v_proofs.as_slice().try_into().unwrap(), &k_buf);
                let mut data: zktrie::StoreData = [0; 32];
                proof.data.as_ref().to_big_endian(&mut data);
                s_trie.update_store(&k_buf, &data).unwrap();
            }

            storages.insert(*account, s_trie);
        }

        let trie = db.new_trie(&storage_trace.root_before.0).unwrap();

        Self {
            db,
            trie,
            accounts,
            storages,
        }
    }

    fn trace_storage_update(
        &mut self,
        address: Address,
        key: &[u8; 32],
        value: &[u8; 32],
    ) -> SMTTrace {
        let storage_key = hash_zktrie_key(key);
        let key = HexBytes(*key);
        let store_value = HexBytes(*value);
        let trie = self.storages.get_mut(&address).unwrap();

        let store_before = trie
            .get_store(key.as_ref())
            .and_then(|v| if &v == &Hash::zero().0 { None } else { Some(v) })
            .map(|v| StateData {
                key,
                value: HexBytes(v),
            });
        let storage_before_proofs = trie.prove(key.as_ref());
        let storage_before_path = decode_proof_for_mpt_path(storage_key, storage_before_proofs);
        let store_after = if value != &Hash::zero().0 {
            Some(StateData {
                key,
                value: store_value,
            })
        } else {
            trie.delete(key.as_ref());
            None
        };
        let storage_after_proofs = trie.prove(key.as_ref());
        let storage_after_path = decode_proof_for_mpt_path(storage_key, storage_after_proofs);

        let mut out = self.trace_account_update(address, |acc| {
            let mut acc = acc.clone();
            acc.storage_root = H256::from(storage_after_path.as_ref().unwrap().root.as_ref());
            Some(acc)
        });
        if store_before.is_some() {
            out.state_key = Some(
                storage_before_path
                    .as_ref()
                    .unwrap()
                    .leaf
                    .as_ref()
                    .unwrap()
                    .sibling,
            );
        } else if store_after.is_some() {
            out.state_key = Some(
                storage_after_path
                    .as_ref()
                    .unwrap()
                    .leaf
                    .as_ref()
                    .unwrap()
                    .sibling,
            );
        } else {
            let high = Fr::from_u128(u128::from_be_bytes((&key.0[..16]).try_into().unwrap()));
            let low = Fr::from_u128(u128::from_be_bytes((&key.0[16..]).try_into().unwrap()));
            let hash = Fr::hash([high, low]);
            let mut buf = [0u8; 32];
            buf.as_mut_slice().copy_from_slice(hash.to_repr().as_ref());            
            out.state_key = Some(HexBytes(buf));
        }

        out.state_path = [storage_before_path.ok(), storage_after_path.ok()];
        out.state_update = Some([store_before, store_after]);
        out
    }

    fn trace_account_update<U>(&mut self, address: Address, update_account_data: U) -> SMTTrace
    where
        U: FnOnce(&AccountData) -> Option<AccountData>,
    {
        let account_data_before = self
            .accounts
            .get(&address)
            .expect("todo: handle this")
            .clone();

        let proofs = self.trie.prove(address.as_bytes());
        let address_key = hash_zktrie_key(&extend_address_to_h256(&address));

        let account_path_before = decode_proof_for_mpt_path(address_key, proofs).unwrap();

        let account_data_after = update_account_data(&account_data_before.unwrap_or_default());

        if let Some(account_data_after) = account_data_after {
            let mut nonce = [0u8; 32];
            U256::from(account_data_after.nonce).to_big_endian(&mut nonce.as_mut_slice());
            let mut balance = [0u8; 32];
            U256::from(account_data_after.balance).to_big_endian(&mut balance.as_mut_slice());
            let mut code_hash = [0u8; 32];
            U256::from(account_data_after.code_hash.0).to_big_endian(&mut code_hash.as_mut_slice());
            let acc_data = [nonce, balance, code_hash, [0; 32]];
            self.trie.update_account(address.as_bytes(), &acc_data).expect("todo: handle this");
            self.accounts.insert(address, Some(account_data_after));
        } else {
            self.trie.delete(address.as_bytes());
            self.accounts.remove(&address);
        }

        let proofs = self.trie.prove(address.as_bytes());
        let account_path_after = decode_proof_for_mpt_path(address_key, proofs).unwrap();


        SMTTrace {
            address: HexBytes(address.0),
            account_path: [account_path_before.clone(), account_path_after.clone()],
            account_update: [account_data_before.map(Into::into), account_data_after.map(Into::into)],
            account_key: HexBytes(address_key.to_repr().as_ref().try_into().unwrap()),
            state_path: [None, None],
            common_state_root: account_data_before.map(|data| HexBytes(data.storage_root.0)).or_else(||Some(HexBytes([0;32]))),
            state_key: None,
            state_update: None,
        }
    }

    pub fn handle_new_state(&mut self, account_proof: &AccountProofWrapper) -> SMTTrace {
        match account_proof.storage {
            Some(ref storage) => {
                let mut addr = [0u8; 32];
                storage.key.unwrap().to_big_endian(&mut addr.as_mut_slice());
                let mut value = [0u8; 32];
                storage.value.unwrap().to_big_endian(&mut value.as_mut_slice());
                self.trace_storage_update(
                    account_proof.address.unwrap(),
                    &addr,
                    &value
                )
            }
            None => {
                let mut acc_data: AccountData = account_proof.into();
                self.trace_account_update(
                    account_proof.address.unwrap(),
                    |acc_before| {
                        acc_data.storage_root = acc_before.storage_root;
                        Some(acc_data)
                    }
                )
            }
        }
    }
}

fn smt_hash_from_u256(i: &U256) -> SMTHash {
    let mut out : [u8; 32] = [0; 32];
    i.to_little_endian(&mut out);
    HexBytes(out)
}

fn smt_hash_from_bytes(bt: &[u8]) -> SMTHash {
    let mut out : Vec<_> = bt.iter().copied().rev().collect();
    out.resize(32, 0);
    HexBytes(out.try_into().expect("extract size has been set"))
}

fn hash_zktrie_key(key_buf: &[u8; 32]) -> Fr {

    let first_16bytes: [u8; 16] = key_buf[..16].try_into().expect("expect first 16 bytes");
    let last_16bytes: [u8; 16] = key_buf[16..].try_into().expect("expect last 16 bytes");

    let bt_high = Fr::from_u128(u128::from_be_bytes(first_16bytes));
    let bt_low = Fr::from_u128(u128::from_be_bytes(last_16bytes));

    Fr::hash([bt_high, bt_low])
}

#[derive(Debug, Default, Clone)]
struct LeafNodeHash (H256);

impl CanRead for LeafNodeHash {
    fn try_parse(mut _rd: impl Read) -> Result<Self, IoError> {
        panic!("this entry is not used")
    }
    fn parse_leaf(data: &[u8]) -> Result<Self, IoError>{
        let node = ZkTrieNode::parse(data);
        Ok(Self(node.value_hash().expect("leaf should has value hash").into()))
    }
}

impl AsRef<[u8]> for LeafNodeHash {
    fn as_ref(&self) -> &[u8] {
        self.0.as_ref()
    }
}


fn decode_proof_for_mpt_path(mut key_fr: Fr, proofs: Vec<Vec<u8>>) -> Result<SMTPath, IoError> {

    let root = if let Some(arr) = proofs.first() {
        let n = ZkTrieNode::parse(arr.as_slice());
        HexBytes(n.key())
    } else {
        HexBytes::<32>([0; 32])
    };

    let proof_bytes: Vec<_> = proofs.into_iter().map(Bytes::from).collect();
    let trie_proof = TrieProof::<LeafNodeHash>::try_from(proof_bytes.as_slice())?;

    // convert path part
    let invert_2 = Fr::one().double().invert().unwrap();
    let mut path_bit_now = BigUint::from(1 as u32);
    let mut path_part : BigUint =  Default::default();
    let mut path = Vec::new();

    for (left, right) in trie_proof.path.iter() {
        let is_bit_one : bool = key_fr.is_odd().into();
        path.push(
            if is_bit_one {
                SMTNode {
                    value: smt_hash_from_u256(right),
                    sibling: smt_hash_from_u256(left),
                }
            } else {
                SMTNode {
                    value: smt_hash_from_u256(left),
                    sibling: smt_hash_from_u256(right),
                }
            }
        );
        key_fr = if is_bit_one {key_fr.mul(&invert_2) - invert_2 } else {key_fr.mul(&invert_2)};
        if is_bit_one {path_part += &path_bit_now};
        path_bit_now *= 2 as u32;
    }

    let leaf = trie_proof.key.as_ref().map(|h| 
        SMTNode {
            value: smt_hash_from_bytes(trie_proof.data.as_ref()),
            sibling: smt_hash_from_bytes(h.as_bytes()),
        }
    );

    Ok(SMTPath{
        root,
        leaf,
        path,
        path_part,
    })
}

