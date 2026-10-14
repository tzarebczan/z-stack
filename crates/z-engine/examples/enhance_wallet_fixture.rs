//! Disposable public viewing fixture. No wallet files, seed export, node or production service.
use base64::{engine::general_purpose::STANDARD, Engine};
use orchard::{
    note::{ExtractedNoteCommitment, NoteVersion, RandomSeed, Rho},
    note_encryption::{IronwoodDomain, IronwoodNoteEncryption},
    value::NoteValue,
    Note,
};
use prost::Message;
use std::{
    io::{Read, Write},
    net::TcpListener,
    time::Duration,
};
use zcash_client_backend::{
    data_api::enhance_pir::{EnhanceRecord, EnhanceRecordParts, EnhanceTransactionMetadata},
    proto::compact_formats::{ChainMetadata, CompactBlock, CompactOrchardAction, CompactTx},
};
use zcash_note_encryption::Domain;
use zip32::Scope;
mod fixture {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../z-enhance-pir/tests/fixture.rs"
    ));
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let account = z_engine::keys::account_from_mnemonic(
        z_engine::keys::REGTEST_FAUCET_MNEMONIC,
        z_engine::Network::Mainnet,
        0,
    )?;
    let ufvk =
        zcash_keys::keys::UnifiedFullViewingKey::decode(&z_engine::Network::Mainnet, &account.ufvk)
            .map_err(|_| "fixture viewing key")?;
    let fvk = ufvk.orchard().ok_or("fixture orchard key")?;
    let rho = Rho::from_bytes(&[1; 32]).unwrap();
    let note = Note::from_parts(
        fvk.address_at(0u32, Scope::External),
        NoteValue::from_raw(50000),
        rho,
        RandomSeed::from_bytes([7; 32], &rho).unwrap(),
        NoteVersion::V3,
    )
    .unwrap();
    let mut memo = [0; 512];
    memo[..13].copy_from_slice(b"private hello");
    let cmx = ExtractedNoteCommitment::from(note.commitment());
    let enc = IronwoodNoteEncryption::new(None, note, memo);
    let ciphertext = enc.encrypt_note_plaintext();
    let action = CompactOrchardAction {
        nullifier: vec![1; 32],
        cmx: cmx.to_bytes().to_vec(),
        ephemeral_key: IronwoodDomain::epk_bytes(enc.epk()).0.to_vec(),
        ciphertext: ciphertext[..52].to_vec(),
    };
    let block = CompactBlock {
        height: 3500000,
        hash: (0..32).collect(),
        prev_hash: vec![0; 32],
        time: 1,
        vtx: vec![CompactTx {
            txid: vec![3; 32],
            ironwood_actions: vec![action],
            ..Default::default()
        }],
        chain_metadata: Some(ChainMetadata {
            ironwood_commitment_tree_size: 1,
            ..Default::default()
        }),
        ..Default::default()
    };
    let record = EnhanceRecord::from_parts(EnhanceRecordParts {
        enc_ciphertext_suffix: ciphertext[52..].try_into().unwrap(),
        cv_net: [0; 32],
        out_ciphertext: [0; 80],
        has_transparent_inputs: false,
        has_transparent_outputs: false,
        metadata: EnhanceTransactionMetadata::new(3500500, Some(10000)).unwrap(),
    });
    let profile = fixture::profile();
    let mut display_hash: [u8; 32] = std::array::from_fn(|i| i as u8);
    display_hash.reverse();
    let server = fixture::Fixture::with_records(&profile, &[*record.as_bytes()], display_hash);
    let encoded = block.encode_to_vec();
    let blob = [(encoded.len() as u32).to_be_bytes().as_slice(), &encoded].concat();
    let metadata = serde_json::to_vec(
        &serde_json::json!({"manifest":serde_json::from_slice::<serde_json::Value>(&server.manifest)?,
        "material":serde_json::from_slice::<serde_json::Value>(&server.material)?,"ufvk":account.ufvk,"blob":STANDARD.encode(blob),
        "height":3500000,"hash":hex::encode(display_hash),"records":1,
        "tree":{"height":3499999,"hash":"00".repeat(32),"saplingTree":"","orchardTree":"","ironwoodTree":""}}),
    )?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    println!(
        "{}",
        serde_json::json!({"port":listener.local_addr()?.port()})
    );
    for stream in listener.incoming() {
        let mut stream = stream?;
        stream.set_read_timeout(Some(Duration::from_secs(120)))?;
        stream.set_write_timeout(Some(Duration::from_secs(120)))?;
        let mut header = Vec::new();
        while !header.ends_with(b"\r\n\r\n") {
            if header.len() >= 8192 {
                break;
            }
            let mut b = [0];
            if stream.read_exact(&mut b).is_err() {
                break;
            }
            header.push(b[0]);
        }
        let header = String::from_utf8(header)?;
        let first = header.lines().next().unwrap_or("");
        let lengths = header
            .lines()
            .filter_map(|l| l.split_once(':'))
            .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
            .map(|(_, v)| v.trim().parse::<usize>())
            .collect::<Result<Vec<_>, _>>()?;
        let length = lengths.first().copied().unwrap_or(0);
        if lengths.len() > 1 || length > 282740 {
            continue;
        }
        let mut body = vec![0; length];
        if stream.read_exact(&mut body).is_err() {
            continue;
        }
        let (status, bytes) = if first == "GET /metadata HTTP/1.1" && body.is_empty() {
            (200, metadata.clone())
        } else if first == "POST /query HTTP/1.1" && body.len() == 110708 {
            (200, server.answer(&body))
        } else {
            (400, vec![])
        };
        write!(
            stream,
            "HTTP/1.1 {status} fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            bytes.len()
        )?;
        stream.write_all(&bytes)?;
    }
    Ok(())
}
