use std::fs;
use std::io::Read;
use std::path::Path;

use bytes::BytesMut;
use prost::Message;
use serde_json::Value;
use tcx_primitive::FromHex;

use tcx_bch::{BchAddress, BchTransaction};
use tcx_btc_fork::{
    address::BtcForkAddress, BtcForkSegWitTransaction, BtcForkSignedTxOutput, BtcForkTransaction,
    BtcForkTxInput,
};
use tcx_chain::keystore::KeyType;
use tcx_chain::keystore_guard::KeystoreGuard;
use tcx_chain::{Account, HdKeystore, Metadata, Source};
use tcx_crypto::{XPUB_COMMON_IV, XPUB_COMMON_KEY_128};
use tcx_tron::TrxAddress;

use crate::api::hd_store_derive_param::Derivation;
use crate::api::keystore_common_export_result::ExportType;
use crate::api::{
    AccountResponse, AccountsResponse, ExternalAddressParam, HdStoreCreateParam,
    HdStoreDeriveParam, HdStoreImportParam, KeystoreCommonAccountsParam, KeystoreCommonExistsParam,
    KeystoreCommonExistsResult, KeystoreCommonExportResult, Response, WalletKeyParam, WalletResult,
};
use crate::api::{InitTokenCoreXParam, SignParam};
//use crate::calc_external_address_internal;
use crate::error_handling::Result;
use crate::filemanager::{
    cache_keystore, find_keystore_id_by_address, flush_keystore, WALLET_FILE_DIR,
};
use crate::filemanager::{delete_keystore_file, KEYSTORE_MAP};
use std::collections::HashMap;
use tcx_chain::signer::{MessageSigner, TransactionSigner};
use tcx_constants::coin_info::coin_info_from_param;
use tcx_constants::CoinInfo;
use tcx_crypto::aes::cbc::encrypt_pkcs7;
use tcx_crypto::hash::str_sha256;
use tcx_primitive::{Bip32DeterministicPublicKey, Ss58Codec};
use tcx_tron::transaction::{TronMessageInput, TronTxInput};

#[repr(C)]
pub struct Buffer {
    pub data: *mut u8,
    pub len: usize,
}

pub fn encode_message(msg: impl Message) -> Result<Vec<u8>> {
    let mut buf = BytesMut::with_capacity(msg.encoded_len());
    msg.encode(&mut buf)?;
    Ok(buf.to_vec())
}

fn derive_account(seed: &[u8], derivation: &Derivation) -> Result<Account> {
    let mut coin_info = coin_info_from_param(
        &derivation.chain_type,
        &derivation.network,
        &derivation.seg_wit,
    )?;
    coin_info.derivation_path = derivation.path.to_owned();
    match derivation.chain_type.as_str() {
        "BITCOINCASH" => HdKeystore::derive_account_from_coin::<BchAddress>(&coin_info, seed),
        "LITECOIN" => HdKeystore::derive_account_from_coin::<BtcForkAddress>(&coin_info, seed),
        "TRON" => HdKeystore::derive_account_from_coin::<TrxAddress>(&coin_info, seed),
        _ => Err(format_err!("unsupported_chain")),
    }
}

pub fn init_token_core_x(data: &[u8]) -> Result<()> {
    let InitTokenCoreXParam {
        file_dir,
        xpub_common_key,
        xpub_common_iv,
    } = InitTokenCoreXParam::decode(data).unwrap();
    *WALLET_FILE_DIR.write().unwrap() = file_dir.to_string();
    *XPUB_COMMON_KEY_128.write().unwrap() = xpub_common_key.to_string();
    *XPUB_COMMON_IV.write().unwrap() = xpub_common_iv.to_string();

    let p = Path::new(&file_dir);
    let walk_dir = std::fs::read_dir(p).expect("read dir");
    for entry in walk_dir {
        let entry = entry.expect("DirEntry");
        let fp = entry.path();
        if !fp
            .file_name()
            .expect("file_name")
            .to_str()
            .expect("file_name str")
            .ends_with(".json")
        {
            continue;
        }

        let mut f = fs::File::open(fp).expect("open file");
        let mut contents = String::new();

        let _ = f.read_to_string(&mut contents);
        let v: Value = serde_json::from_str(&contents).expect("read json from content");

        let version = v["version"].as_i64().expect("version");
        if version != i64::from(HdKeystore::VERSION) {
            continue;
        }
        let keystore: HdKeystore = serde_json::from_str(&contents)?;
        cache_keystore(keystore);
    }
    Ok(())
}

pub fn hd_store_create(data: &[u8]) -> Result<Vec<u8>> {
    let param: HdStoreCreateParam =
        HdStoreCreateParam::decode(data).expect("import wallet from mnemonic");

    let mut meta = Metadata::default();
    meta.name = param.name.to_owned();
    meta.password_hint = param.password_hint.to_owned();
    meta.source = Source::Mnemonic;

    //    let meta: Metadata = serde_json::from_value(v.clone())?;
    let ks = HdKeystore::new(&param.password, meta);

    flush_keystore(&ks)?;

    let wallet = WalletResult {
        id: ks.id.to_owned(),
        name: ks.meta.name.to_owned(),
        source: "MNEMONIC".to_owned(),
        accounts: vec![],
        created_at: ks.meta.timestamp.clone(),
    };
    let ret = encode_message(wallet)?;
    cache_keystore(ks.clone());
    Ok(ret)
}

pub fn hd_store_import(data: &[u8]) -> Result<Vec<u8>> {
    let param: HdStoreImportParam =
        HdStoreImportParam::decode(data).expect("import wallet from mnemonic");

    let mut meta = Metadata::default();
    meta.name = param.name.to_owned();
    meta.password_hint = param.password_hint.to_owned();
    meta.source = Source::Mnemonic;

    //    let meta: Metadata = serde_json::from_value(v.clone())?;
    let mut ks = HdKeystore::from_mnemonic(&param.mnemonic, &param.password, meta);

    {
        let mut guard_mut = KeystoreGuard::unlock_by_password(&mut ks, &param.password)?;

        let mut coin_info =
            coin_info_from_param(&param.chain_type, &param.network, &param.seg_wit)?;
        coin_info.derivation_path = param.path.to_string();
        let account = match param.chain_type.as_str() {
            "BITCOINCASH" => guard_mut
                .keystore_mut()
                .derive_coin::<BchAddress>(&coin_info),
            "LITECOIN" => guard_mut
                .keystore_mut()
                .derive_coin::<BtcForkAddress>(&coin_info),
            "TRON" => guard_mut
                .keystore_mut()
                .derive_coin::<TrxAddress>(&coin_info),
            _ => Err(format_err!("{}", "chain_type_not_support")),
        }?;

        let exist_kid_opt = find_keystore_id_by_address(&account.address);
        if let Some(exist_kid) = exist_kid_opt {
            if !param.overwrite {
                return Err(format_err!("{}", "wallet_exists"));
            } else {
                guard_mut.keystore_mut().id = exist_kid;
            }
        }
    }

    flush_keystore(&ks)?;

    let mut accounts: Vec<AccountResponse> = vec![];
    for account in &ks.active_accounts {
        let enc_xpub = enc_xpub(&account.ext_pub_key.to_string(), &account.network)?;
        let acc_rsp = AccountResponse {
            chain_type: account.coin.to_string(),
            address: account.address.to_string(),
            path: account.derivation_path.to_string(),
            extended_xpub_key: enc_xpub,
        };
        accounts.push(acc_rsp);
    }

    let wallet = WalletResult {
        id: ks.id.to_owned(),
        name: ks.meta.name.to_owned(),
        source: "MNEMONIC".to_owned(),
        accounts,
        created_at: ks.meta.timestamp.clone(),
    };
    let ret = encode_message(wallet)?;
    cache_keystore(ks.clone());
    Ok(ret)
}

fn enc_xpub(xpub: &str, network: &str) -> Result<String> {
    let xpk = Bip32DeterministicPublicKey::from_hex(xpub)?;
    let ext_pub_key: String;
    if network == "MAINNET" {
        ext_pub_key = xpk.to_ss58check_with_version(&[0x04, 0x88, 0xB2, 0x1E]);
    } else {
        ext_pub_key = xpk.to_ss58check_with_version(&[0x04, 0x35, 0x87, 0xCF]);
    }

    let key = tcx_crypto::XPUB_COMMON_KEY_128.read().unwrap();
    let iv = tcx_crypto::XPUB_COMMON_IV.read().unwrap();
    let key_bytes = hex::decode(&*key)?;
    let iv_bytes = hex::decode(&*key)?;
    let encrypted = encrypt_pkcs7(&ext_pub_key.as_bytes(), &key_bytes, &iv_bytes)?;
    Ok(base64::encode(&encrypted))
}

pub fn hd_store_derive(data: &[u8]) -> Result<Vec<u8>> {
    let param: HdStoreDeriveParam =
        HdStoreDeriveParam::decode(data).expect("hd_store_derive_param");
    let mut map = KEYSTORE_MAP.write().unwrap();
    let keystore = match map.get_mut(&param.id) {
        Some(keystore) => Ok(keystore),
        _ => Err(format_err!("{}", "wallet_not_found")),
    }?;

    let mut guard = KeystoreGuard::unlock_by_password(keystore, &param.password)?;
    let seed = guard.keystore().decrypt_seed(&param.password)?;
    let mut accounts: Vec<Account> = vec![];
    let mut account_responses: Vec<AccountResponse> = vec![];

    for derivation in param.derivations {
        let account = derive_account(&seed, &derivation)?;
        let enc_xpub = enc_xpub(&account.ext_pub_key.to_string(), &account.network)?;
        let account_rsp = AccountResponse {
            chain_type: derivation.chain_type.to_owned(),
            address: account.address.to_owned(),
            path: account.derivation_path.to_owned(),
            extended_xpub_key: enc_xpub,
        };
        account_responses.push(account_rsp);

        accounts.push(account);
    }

    guard.keystore_mut().active_accounts.append(&mut accounts);

    let accounts_rsp = AccountsResponse {
        accounts: account_responses,
    };

    encode_message(accounts_rsp)
}

pub fn keystore_common_export(data: &[u8]) -> Result<Vec<u8>> {
    let param: WalletKeyParam = WalletKeyParam::decode(data).expect("keystore_common_delete");
    let mut map = KEYSTORE_MAP.write().unwrap();
    let mut keystore = match map.get_mut(&param.id) {
        Some(keystore) => Ok(keystore),
        _ => Err(format_err!("{}", "wallet_not_found")),
    }?;

    if keystore.verify_password(&param.password) {
        let export_result: KeystoreCommonExportResult;
        if KeyType::PrivateKey != keystore.key_type {
            let mnemonic = keystore.mnemonic(&param.password)?;
            export_result = KeystoreCommonExportResult {
                id: keystore.id.to_string(),
                r#type: ExportType::Mnemonic as i32,
                value: mnemonic,
            };
        } else {
            // todo: check if need to unlock wallet
            let guard = KeystoreGuard::unlock_by_password(keystore, &param.password)?;
            let pk = guard.keystore().private_key()?;
            export_result = KeystoreCommonExportResult {
                id: guard.keystore().id.to_string(),
                r#type: ExportType::PrivateKey as i32,
                value: pk,
            };
        }

        encode_message(export_result)
    } else {
        Err(format_err!("{}", "password_incorrect"))
    }
}

pub fn keystore_common_verify(data: &[u8]) -> Result<Vec<u8>> {
    let param: WalletKeyParam = WalletKeyParam::decode(data).expect("keystore_common_delete");
    let map = KEYSTORE_MAP.read().unwrap();
    let keystore: &HdKeystore = match map.get(&param.id) {
        Some(keystore) => Ok(keystore),
        _ => Err(format_err!("{}", "wallet_not_found")),
    }?;

    // todo: check if need return is_success : false
    if keystore.verify_password(&param.password) {
        let rsp = Response {
            is_success: true,
            error: "".to_owned(),
        };
        encode_message(rsp)
    } else {
        Err(format_err!("{}", "password_incorrect"))
    }
}

pub fn keystore_common_delete(data: &[u8]) -> Result<Vec<u8>> {
    let param: WalletKeyParam = WalletKeyParam::decode(data).expect("keystore_common_delete");
    let mut map = KEYSTORE_MAP.write().unwrap();
    let keystore: &HdKeystore = match map.get(&param.id) {
        Some(keystore) => Ok(keystore),
        _ => Err(format_err!("{}", "wallet_not_found")),
    }?;

    if keystore.verify_password(&param.password) {
        delete_keystore_file(&param.id)?;
        map.remove(&param.id);
        let rsp = Response {
            is_success: true,
            error: "".to_owned(),
        };
        encode_message(rsp)
    } else {
        Err(format_err!("{}", "password_incorrect"))
    }
}

pub fn keystore_common_exists(data: &[u8]) -> Result<Vec<u8>> {
    let param: KeystoreCommonExistsParam =
        KeystoreCommonExistsParam::decode(data).expect("keystore_common_exists params");
    let key_hash = str_sha256(&param.value);
    let map: &mut HashMap<String, HdKeystore> = &mut KEYSTORE_MAP.write().unwrap();

    // todo: check the key_type
    let founded: Option<&HdKeystore> = map.values().find(|keystore| keystore.key_hash == key_hash);
    let result: KeystoreCommonExistsResult;
    if let Some(ks) = founded {
        result = KeystoreCommonExistsResult {
            is_exists: true,
            id: ks.id.to_owned(),
        }
    } else {
        result = KeystoreCommonExistsResult {
            is_exists: false,
            id: "".to_owned(),
        }
    }
    encode_message(result)
}

pub fn keystore_common_accounts(data: &[u8]) -> Result<Vec<u8>> {
    let param: KeystoreCommonAccountsParam =
        KeystoreCommonAccountsParam::decode(data).expect("keystore_common_accounts params");
    let map = KEYSTORE_MAP.read().unwrap();
    let keystore: &HdKeystore = match map.get(&param.id) {
        Some(keystore) => Ok(keystore),
        _ => Err(format_err!("{}", "wallet_not_found")),
    }?;

    let mut accounts: Vec<AccountResponse> = vec![];
    for account in &keystore.active_accounts {
        let enc_xpub = enc_xpub(&account.ext_pub_key, &account.network)?;
        let acc_rsp = AccountResponse {
            chain_type: account.coin.to_owned(),
            address: account.address.to_owned(),
            path: account.derivation_path.to_owned(),
            extended_xpub_key: enc_xpub.to_owned(),
        };
        accounts.push(acc_rsp);
    }

    let accounts_rsp = AccountsResponse { accounts };
    encode_message(accounts_rsp)
}

pub fn sign_tx(data: &[u8]) -> Result<Vec<u8>> {
    let param: SignParam = SignParam::decode(data).expect("SignTxParam");

    let mut map = KEYSTORE_MAP.write().unwrap();
    let keystore = match map.get_mut(&param.id) {
        Some(keystore) => Ok(keystore),
        _ => Err(format_err!("{}", "wallet_not_found")),
    }?;

    let guard = KeystoreGuard::unlock_by_password(keystore, &param.password)?;
    match param.chain_type.as_str() {
        "BITCOINCASH" | "LITECOIN" => sign_btc_fork_transaction(&param, &guard),
        "TRON" => sign_tron_tx(&param, &guard),
        _ => Err(format_err!("unsupported_chain")),
    }
}

pub fn sign_btc_fork_transaction(param: &SignParam, guard: &KeystoreGuard) -> Result<Vec<u8>> {
    let input: BtcForkTxInput =
        BtcForkTxInput::decode(&param.input.as_ref().expect("tx_input").value.clone())
            .expect("BitcoinForkTransactionInput");
    let coin = coin_info_from_param(&param.chain_type, &input.network, &input.seg_wit)?;
    let signed_tx: BtcForkSignedTxOutput = if param.chain_type.as_str() == "BITCOINCASH" {
        let tran = BchTransaction::new(input, coin);
        guard.keystore().sign_transaction(&tran)?
    } else if input.seg_wit.as_str() != "NONE" {
        let tran = BtcForkSegWitTransaction::new(input, coin);
        guard.keystore().sign_transaction(&tran)?
    } else {
        let tran = BtcForkTransaction::new(input, coin);
        guard.keystore().sign_transaction(&tran)?
    };
    encode_message(signed_tx)
}

pub fn sign_tron_tx(param: &SignParam, guard: &KeystoreGuard) -> Result<Vec<u8>> {
    let input: TronTxInput =
        TronTxInput::decode(&param.input.as_ref().expect("tx_input").value.clone())
            .expect("TronTxInput");
    let signed_tx = guard.keystore().sign_transaction(&input)?;

    encode_message(signed_tx)
}

pub fn tron_sign_message(data: &[u8]) -> Result<Vec<u8>> {
    let param: SignParam = SignParam::decode(data).expect("SignParam");

    let mut map = KEYSTORE_MAP.write().unwrap();
    let keystore = match map.get_mut(&param.id) {
        Some(keystore) => Ok(keystore),
        _ => Err(format_err!("{}", "wallet_not_found")),
    }?;

    let guard = KeystoreGuard::unlock_by_password(keystore, &param.password)?;

    let input: TronMessageInput =
        TronMessageInput::decode(param.input.expect("TronMessageInput").value.clone())
            .expect("TronMessageInput");
    let signed_tx = guard.keystore().sign_message(&input)?;
    encode_message(signed_tx)
}

#[cfg(test)]
mod tests {
    use crate::api::{HdStoreImportParam, InitTokenCoreXParam, SignParam, WalletResult};
    use crate::handler::{encode_message, sign_tx};
    use crate::handler::{hd_store_import, init_token_core_x};
    use prost::Message;
    use std::ffi::{CStr, CString};
    use std::os::raw::c_char;
    use std::panic;
    use std::path::Path;
    use tcx_btc_fork::{BtcForkSignedTxOutput, BtcForkTxInput, Utxo};
    use tcx_tron::transaction::{TronTxInput, TronTxOutput};

    static PASSWORD: &'static str = "Insecure Pa55w0rd";
    static MNEMONIC: &'static str =
        "inject kidney empty canal shadow pact comfort wife crush horse wife sketch";

    fn setup() {
        let param = InitTokenCoreXParam {
            file_dir: "../test-data".to_string(),
            xpub_common_key: "B888D25EC8C12BD5043777B1AC49F872".to_string(),
            xpub_common_iv: "9C0C30889CBCC5E01AB5B2BB88715799".to_string(),
        };

        unsafe {
            init_token_core_x(&encode_message(param).unwrap());
        }
    }

    fn run_test<T>(test: T) -> ()
    where
        T: FnOnce() -> () + panic::UnwindSafe,
    {
        setup();
        let result = panic::catch_unwind(|| test());
        //        teardown();
        assert!(result.is_ok())
    }

    #[test]
    pub fn test_hd_store_import() {
        run_test(|| {
            let param = HdStoreImportParam {
                chain_type: "BITCOINCASH".to_string(),
                mnemonic: MNEMONIC.to_string(),
                password: PASSWORD.to_string(),
                path: "m/44'/145'/0'/0/0".to_string(),
                source: "MNEMONIC".to_string(),
                name: "test-wallet".to_string(),
                network: "MAINNET".to_string(),
                seg_wit: "NONE".to_string(),
                password_hint: "imtoken".to_string(),
                overwrite: true,
            };
            let ret = hd_store_import(&encode_message(param).unwrap()).unwrap();
            let import_result: WalletResult = WalletResult::decode(&ret).unwrap();

            assert_eq!(import_result.chain_type, "BITCOINCASH");
            assert_eq!(
                import_result.address,
                "qzld7dav7d2sfjdl6x9snkvf6raj8lfxjcj5fa8y2r"
            );
        })
    }

    #[test]
    pub fn test_sign_bch_tx() {
        run_test(|| {
            let utxo = Utxo {
                tx_hash: "09c3a49c1d01f6341c43ea43dd0de571664a45b4e7d9211945cb3046006a98e2"
                    .to_string(),
                vout: 0,
                amount: 100000,
                address: "qzld7dav7d2sfjdl6x9snkvf6raj8lfxjcj5fa8y2r".to_string(),
                script_pub_key: "76a91488d9931ea73d60eaf7e5671efc0552b912911f2a88ac".to_string(),
                derived_path: "0/0".to_string(),
                sequence: 0,
            };
            let input = BtcForkTxInput {
                to: "qq40fskqshxem2gvz0xkf34ww3h6zwv4dcr7pm0z6s".to_string(),
                amount: 93454,
                unspents: vec![utxo],
                memo: "".to_string(),
                fee: 6000,
                change_idx: 1,
                change_address: "".to_string(),
                network: "MAINNET".to_owned(),
                seg_wit: "NONE".to_owned(),
            };
            let tx = SignParam {
                id: "9c6cbc21-1c43-4c8b-bb7a-5e538f908819".to_string(),
                password: "Insecure Password".to_string(),
                chain_type: "BITCOINCASH".to_string(),
                input: Some(::prost_types::Any {
                    type_url: "imtoken".to_string(),
                    value: encode_message(input).unwrap(),
                }),
            };
            let tx_bytes = encode_message(tx).unwrap();
            let ret = sign_tx(&tx_bytes).unwrap();
            let output: BtcForkSignedTxOutput = BtcForkSignedTxOutput::decode(&ret).unwrap();
            assert_eq!("0100000001e2986a004630cb451921d9e7b4454a6671e50ddd43ea431c34f6011d9ca4c309000000006b483045022100b3d91f406cdc33eb4d8f2b56491e6c87da2372eb83f1f384fc3f02f81a5b21b50220324dd7ecdc214721c542db252078473f9e7172bf592fa55332621c3e348be45041210251492dfb299f21e426307180b577f927696b6df0b61883215f88eb9685d3d449ffffffff020e6d0100000000001976a9142af4c2c085cd9da90c13cd64c6ae746fa139956e88ac22020000000000001976a9148835a675efb0db4fd00e9eb77aff38a6d5bd767c88ac00000000", output.signature);
        })
    }

    #[test]
    pub fn test_sign_tron_tx() {
        run_test(|| {
            let param = HdStoreImportParam {
                chain_type: "TRON".to_string(),
                mnemonic: MNEMONIC.to_string(),
                password: PASSWORD.to_string(),
                path: "m/44'/195'/0'/0/0".to_string(),
                source: "MNEMONIC".to_string(),
                name: "test-wallet".to_string(),
                network: "MAINNET".to_string(),
                seg_wit: "NONE".to_string(),
                password_hint: "imtoken".to_string(),
                overwrite: true,
            };
            let ret = hd_store_import(&encode_message(param).unwrap()).unwrap();
            let import_result: WalletResult = WalletResult::decode(&ret).unwrap();

            let raw_data = hex::decode("0a0202a22208e216e254e43ee10840c8cbe4e3df2d5a67080112630a2d747970652e676f6f676c65617069732e636f6d2f70726f746f636f6c2e5472616e73666572436f6e747261637412320a15415c68cc82c87446f602f019e5fd797437f5b79cc212154156a6076cd1537fa317c2606e4edfa4acd3e8e92e18a08d06709084e1e3df2d").unwrap();
            let input = TronTxInput { raw_data };
            let tx = SignParam {
                id: import_result.id.to_string(),
                password: PASSWORD.to_string(),
                chain_type: "TRON".to_string(),
                input: Some(::prost_types::Any {
                    type_url: "imtoken".to_string(),
                    value: encode_message(input).unwrap(),
                }),
            };
            let tx_bytes = encode_message(tx).unwrap();
            let ret = sign_tx(&tx_bytes).unwrap();
            let output: TronTxOutput = TronTxOutput::decode(&ret).unwrap();
            let expected_sign = "bbf5ce0549490613a26c3ac4fc8574e748eabda05662b2e49cea818216b9da18691e78cd6379000e9c8a35c13dfbf620f269be90a078b58799b56dc20da3bdf200";
            assert_eq!(expected_sign, hex::encode(output.signature));
        })
    }
}
