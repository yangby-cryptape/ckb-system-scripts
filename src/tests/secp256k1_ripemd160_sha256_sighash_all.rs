use super::{DummyDataLoader, BITCOIN_P2PKH_BIN, MAX_CYCLES, SECP256K1_DATA_BIN};
use ckb_crypto::secp::{Generator, Privkey, Pubkey};
use ckb_error::assert_error_eq;
use ckb_script::{ScriptError, TransactionScriptsVerifier};
use ckb_types::{
    bytes::Bytes,
    core::{
        cell::{CellMetaBuilder, ResolvedTransaction},
        Capacity, DepType, ScriptHashType, TransactionBuilder, TransactionView,
    },
    h160, h256,
    packed::{self, CellDep, CellInput, CellOutput, OutPoint, Script, WitnessArgs},
    prelude::*,
    H160, H256,
};
use rand::{thread_rng, Rng};
use sha2::{Digest, Sha256};

const ERROR_PUBKEY_RIPEMD160_HASH: i8 = -3;
const ERROR_SECP_VERIFICATION: i8 = -9;
const ERROR_WITNESS_SIZE: i8 = -12;

fn gen_tx(
    dummy: &mut DummyDataLoader,
    script_data: Bytes,
    lock_args: Bytes,
    pubkey: Bytes,
) -> TransactionView {
    let previous_tx_hash = {
        let mut rng = thread_rng();
        let mut buf = [0u8; 32];
        rng.fill(&mut buf);
        buf.pack()
    };
    let previous_index = 0;
    let capacity = Capacity::shannons(42);
    let previous_out_point = OutPoint::new(previous_tx_hash, previous_index);
    let contract_tx_hash = {
        let mut rng = thread_rng();
        let mut buf = [0u8; 32];
        rng.fill(&mut buf);
        buf.pack()
    };
    let contract_index = 0;
    let contract_out_point = OutPoint::new(contract_tx_hash.clone(), contract_index);
    // dep contract code
    let dep_cell = CellOutput::new_builder()
        .capacity(
            Capacity::bytes(script_data.len())
                .expect("script capacity")
                .pack(),
        )
        .build();
    let dep_cell_data_hash = CellOutput::calc_data_hash(&script_data);
    dummy
        .cells
        .insert(contract_out_point.clone(), (dep_cell, script_data));
    // secp256k1 data
    let secp256k1_data_out_point = {
        let tx_hash = {
            let mut rng = thread_rng();
            let mut buf = [0u8; 32];
            rng.fill(&mut buf);
            buf.pack()
        };
        OutPoint::new(tx_hash, 0)
    };
    let secp256k1_data_cell = CellOutput::new_builder()
        .capacity(
            Capacity::bytes(SECP256K1_DATA_BIN.len())
                .expect("data capacity")
                .pack(),
        )
        .build();
    dummy.cells.insert(
        secp256k1_data_out_point.clone(),
        (secp256k1_data_cell, SECP256K1_DATA_BIN.clone()),
    );
    // input unlock script
    let script = Script::new_builder()
        .args(lock_args.pack())
        .code_hash(dep_cell_data_hash)
        .hash_type(ScriptHashType::Data.pack())
        .build();
    let previous_output_cell = CellOutput::new_builder()
        .capacity(capacity.pack())
        .lock(script)
        .build();
    dummy.cells.insert(
        previous_out_point.clone(),
        (previous_output_cell, Bytes::new()),
    );
    TransactionBuilder::default()
        .input(CellInput::new(previous_out_point.clone(), 0))
        .witness(
            WitnessArgs::new_builder()
                .lock(pubkey.pack())
                .build()
                .as_bytes()
                .pack(),
        )
        .cell_dep(
            CellDep::new_builder()
                .out_point(contract_out_point)
                .dep_type(DepType::Code.pack())
                .build(),
        )
        .cell_dep(
            CellDep::new_builder()
                .out_point(secp256k1_data_out_point)
                .dep_type(DepType::Code.pack())
                .build(),
        )
        .output(CellOutput::new_builder().capacity(capacity.pack()).build())
        .output_data(Bytes::new().pack())
        .build()
}

#[derive(Copy, Clone)]
pub enum SigType {
    Recoverable,
    NonRecoverable,
}

impl SigType {
    pub fn signature_size(self) -> usize {
        match self {
            SigType::Recoverable => 65,
            SigType::NonRecoverable => 64,
        }
    }
}

// Special signature method, inconsistent with the default lock behavior,
// witness signature only sign transaction hash
pub fn sign_tx(tx: TransactionView, key: &Privkey) -> TransactionView {
    sign_tx_by_input_group(tx, key, 0, 0, SigType::Recoverable)
}
pub fn sign_tx_by_input_group(
    tx: TransactionView,
    key: &Privkey,
    begin_index: usize,
    len: usize,
    sig_type: SigType,
) -> TransactionView {
    let tx_hash: H256 = tx.hash().unpack();
    let signed_witnesses: Vec<packed::Bytes> = tx
        .inputs()
        .into_iter()
        .enumerate()
        .map(|(i, _)| {
            // digest the first witness
            if i == begin_index {
                let mut hasher = sha2::Sha256::new();
                hasher.input(&tx_hash);
                let witness = WitnessArgs::new_unchecked(tx.witnesses().get(i).unwrap().unpack());
                let zero_lock: Bytes = {
                    let mut buf = Vec::new();
                    buf.resize(sig_type.signature_size(), 0);
                    buf.extend_from_slice(&Unpack::<Bytes>::unpack(&witness.lock()));
                    buf.into()
                };
                let witness_for_digest =
                    witness.clone().as_builder().lock(zero_lock.pack()).build();
                hasher.input(&witness_for_digest.as_bytes());
                ((i + 1)..(i + len)).for_each(|n| {
                    let witness = tx.witnesses().get(n).unwrap();
                    if !witness.raw_data().is_empty() {
                        hasher.input(&witness.raw_data());
                    }
                });
                let mut message = [0u8; 32];
                message.copy_from_slice(&hasher.result());
                let sig = match sig_type {
                    SigType::Recoverable => {
                        let message = H256::from(message);
                        key.sign_recoverable(&message).expect("sign").serialize()
                    }
                    SigType::NonRecoverable => {
                        let context = &ckb_crypto::secp::SECP256K1;
                        let message = secp256k1::Message::from_slice(&message).unwrap();
                        let key = secp256k1::key::SecretKey::from_slice(key.as_bytes()).unwrap();
                        let signature = context.sign(&message, &key);
                        signature.serialize_compact().to_vec()
                    }
                };
                assert_eq!(sig_type.signature_size(), sig.len());
                let lock: Bytes = {
                    let mut buf = Vec::new();
                    buf.extend_from_slice(&sig);
                    buf.extend_from_slice(&zero_lock[sig_type.signature_size()..]);
                    buf.into()
                };
                witness
                    .as_builder()
                    .lock(lock.pack())
                    .build()
                    .as_bytes()
                    .pack()
            } else {
                tx.witnesses().get(i).unwrap()
            }
        })
        .collect();
    // calculate message
    tx.as_advanced_builder()
        .set_witnesses(signed_witnesses)
        .build()
}

fn build_resolved_tx(data_loader: &DummyDataLoader, tx: &TransactionView) -> ResolvedTransaction {
    let previous_out_point = tx
        .inputs()
        .get(0)
        .expect("should have at least one input")
        .previous_output();
    let resolved_cell_deps = tx
        .cell_deps()
        .into_iter()
        .map(|dep| {
            let deps_out_point = dep.clone();
            let (dep_output, dep_data) =
                data_loader.cells.get(&deps_out_point.out_point()).unwrap();
            CellMetaBuilder::from_cell_output(dep_output.to_owned(), dep_data.to_owned())
                .out_point(deps_out_point.out_point().clone())
                .build()
        })
        .collect();
    let (input_output, input_data) = data_loader.cells.get(&previous_out_point).unwrap();
    let input_cell =
        CellMetaBuilder::from_cell_output(input_output.to_owned(), input_data.to_owned())
            .out_point(previous_out_point)
            .build();
    ResolvedTransaction {
        transaction: tx.clone(),
        resolved_cell_deps,
        resolved_inputs: vec![input_cell],
        resolved_dep_groups: vec![],
    }
}

fn ripemd160(data: &[u8]) -> H160 {
    use ripemd160::Ripemd160;
    let digest: [u8; 20] = Ripemd160::digest(data).into();
    H160::from(digest)
}

fn sha256(data: &[u8]) -> H256 {
    let digest: [u8; 32] = Sha256::digest(data).into();
    H256::from(digest)
}

fn pubkey_uncompressed(pubkey: &Pubkey) -> Vec<u8> {
    let mut serialized = vec![4u8; 65];
    serialized[1..65].copy_from_slice(pubkey.as_ref());
    serialized
}

fn pubkey_compressed(pubkey: &Pubkey) -> Vec<u8> {
    pubkey.serialize()
}

fn pubkey_hash(serialized_pubkey: &[u8]) -> Vec<u8> {
    ripemd160(sha256(serialized_pubkey).as_bytes())
        .as_ref()
        .to_owned()
}

#[test]
fn test_rust_crypto() {
    assert_eq!(
        h160!("0x9c1185a5c5e9fc54612808977ee8f548b2258d31"),
        ripemd160(b"")
    );
    assert_eq!(
        h256!("0xe3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),
        sha256(b"")
    );
}

#[test]
fn test_sighash_all_unlock() {
    let mut data_loader = DummyDataLoader::new();
    let privkey = Generator::random_privkey();
    let pubkey = pubkey_compressed(&privkey.pubkey().expect("pubkey"));
    // compute pubkey hash
    let pubkey_hash = pubkey_hash(&pubkey);
    let tx = gen_tx(
        &mut data_loader,
        BITCOIN_P2PKH_BIN.clone(),
        pubkey_hash.into(),
        pubkey.into(),
    );
    let tx = sign_tx(tx, &privkey);
    let resolved_tx = build_resolved_tx(&data_loader, &tx);
    let verify_result =
        TransactionScriptsVerifier::new(&resolved_tx, &data_loader).verify(MAX_CYCLES);
    verify_result.expect("pass verification");
}

#[test]
fn test_sighash_all_unlock_with_uncompressed_pubkey() {
    let mut data_loader = DummyDataLoader::new();
    let privkey = Generator::random_privkey();
    let pubkey = pubkey_uncompressed(&privkey.pubkey().expect("pubkey"));
    let pubkey_hash = pubkey_hash(&pubkey);
    let tx = gen_tx(
        &mut data_loader,
        BITCOIN_P2PKH_BIN.clone(),
        pubkey_hash.into(),
        pubkey.into(),
    );
    let tx = sign_tx(tx, &privkey);
    let resolved_tx = build_resolved_tx(&data_loader, &tx);
    let verify_result =
        TransactionScriptsVerifier::new(&resolved_tx, &data_loader).verify(MAX_CYCLES);
    verify_result.expect("pass verification");
}

#[test]
fn test_sighash_all_unlock_with_uncompressed_pubkey_and_non_recoverable_signature() {
    let mut data_loader = DummyDataLoader::new();
    let privkey = Generator::random_privkey();
    let pubkey = pubkey_uncompressed(&privkey.pubkey().expect("pubkey"));
    let pubkey_hash = pubkey_hash(&pubkey);

    let tx = gen_tx(
        &mut data_loader,
        BITCOIN_P2PKH_BIN.clone(),
        pubkey_hash.into(),
        pubkey.into(),
    );
    // Create non-recoverable signature
    let tx = sign_tx_by_input_group(tx, &privkey, 0, 0, SigType::NonRecoverable);
    let resolved_tx = build_resolved_tx(&data_loader, &tx);
    let verify_result =
        TransactionScriptsVerifier::new(&resolved_tx, &data_loader).verify(MAX_CYCLES);
    verify_result.expect("pass verification");
}

#[test]
fn test_signing_with_wrong_key() {
    let mut data_loader = DummyDataLoader::new();
    let privkey = Generator::random_privkey();
    let wrong_privkey = Generator::random_privkey();
    let wrong_pubkey = pubkey_compressed(&wrong_privkey.pubkey().expect("pubkey"));
    let pubkey = pubkey_compressed(&privkey.pubkey().expect("pubkey"));
    let pubkey_hash = pubkey_hash(&pubkey);
    let tx = gen_tx(
        &mut data_loader,
        BITCOIN_P2PKH_BIN.clone(),
        pubkey_hash.into(),
        wrong_pubkey.into(),
    );
    let tx = sign_tx(tx, &wrong_privkey);
    let resolved_tx = build_resolved_tx(&data_loader, &tx);
    let verify_result =
        TransactionScriptsVerifier::new(&resolved_tx, &data_loader).verify(MAX_CYCLES);
    assert_error_eq!(
        verify_result.unwrap_err(),
        ScriptError::ValidationFailure(ERROR_PUBKEY_RIPEMD160_HASH),
    );
}

#[test]
fn test_signing_wrong_tx_hash() {
    let mut data_loader = DummyDataLoader::new();
    let privkey = Generator::random_privkey();
    let pubkey = pubkey_compressed(&privkey.pubkey().expect("pubkey"));
    let pubkey_hash = pubkey_hash(&pubkey);
    let tx = gen_tx(
        &mut data_loader,
        BITCOIN_P2PKH_BIN.clone(),
        pubkey_hash.into(),
        pubkey.into(),
    );
    let tx = sign_tx(tx, &privkey);
    // Change tx hash
    let tx = tx.as_advanced_builder().output(Default::default()).build();

    let resolved_tx = build_resolved_tx(&data_loader, &tx);
    let verify_result =
        TransactionScriptsVerifier::new(&resolved_tx, &data_loader).verify(MAX_CYCLES);
    assert_error_eq!(
        verify_result.unwrap_err(),
        ScriptError::ValidationFailure(ERROR_SECP_VERIFICATION),
    );
}

#[test]
fn test_super_long_witness() {
    let mut data_loader = DummyDataLoader::new();
    let privkey = Generator::random_privkey();
    let pubkey = pubkey_compressed(&privkey.pubkey().expect("pubkey"));
    // compute pubkey hash
    let pubkey_hash = pubkey_hash(&pubkey);
    let tx = gen_tx(
        &mut data_loader,
        BITCOIN_P2PKH_BIN.clone(),
        pubkey_hash.into(),
        pubkey.into(),
    );
    let tx_hash: H256 = tx.hash().unpack();

    let mut super_long_message: Vec<u8> = vec![];
    super_long_message.resize(40000, 1);

    let sig = privkey.sign_recoverable(&tx_hash).expect("sign");
    let mut lock = Bytes::from(sig.serialize());
    lock.extend_from_slice(&super_long_message);
    let tx = tx
        .as_advanced_builder()
        .set_witnesses(vec![])
        .witness(
            WitnessArgs::new_builder()
                .lock(lock.pack())
                .build()
                .as_bytes()
                .pack(),
        )
        .build();

    let resolved_tx = build_resolved_tx(&data_loader, &tx);
    let verify_result =
        TransactionScriptsVerifier::new(&resolved_tx, &data_loader).verify(MAX_CYCLES);
    assert_error_eq!(
        verify_result.unwrap_err(),
        ScriptError::ValidationFailure(ERROR_WITNESS_SIZE),
    );
}

#[test]
fn test_wrong_size_witness_args() {
    let mut data_loader = DummyDataLoader::new();
    let privkey = Generator::random_privkey();
    let pubkey = pubkey_uncompressed(&privkey.pubkey().expect("pubkey"));
    let pubkey_hash = pubkey_hash(&pubkey);
    let raw_tx = gen_tx(
        &mut data_loader,
        BITCOIN_P2PKH_BIN.clone(),
        pubkey_hash.into(),
        pubkey.into(),
    );
    // witness less than 2 args
    let tx = sign_tx(raw_tx.clone(), &privkey);
    let wrong_lock = Bytes::from("1243");
    let tx = tx
        .as_advanced_builder()
        .set_witnesses(vec![])
        .witness(
            WitnessArgs::new_builder()
                .lock(wrong_lock.pack())
                .build()
                .as_bytes()
                .pack(),
        )
        .build();
    let resolved_tx = build_resolved_tx(&data_loader, &tx);
    let verify_result =
        TransactionScriptsVerifier::new(&resolved_tx, &data_loader).verify(MAX_CYCLES);

    assert_error_eq!(
        verify_result.unwrap_err(),
        ScriptError::ValidationFailure(ERROR_WITNESS_SIZE),
    );
}
