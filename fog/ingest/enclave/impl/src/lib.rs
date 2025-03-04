// Copyright (c) 2018-2023 The MobileCoin Foundation

//! MobileCoin Ingest Enclave Implementation
//!
//! This crate implements the inside-the-enclave version of the
//! IngestEnclaveAPI.

#![no_std]
#![deny(missing_docs)]
#![allow(clippy::result_large_err)]

extern crate alloc;

mod identity;
pub use identity::RistrettoIdentity;

mod rng_store;
pub use rng_store::{RngStore, StorageDataSize, StorageMetaSize};

use aligned_cmov::{typenum::U32, A8Bytes, Aligned, GenericArray};
use alloc::vec::Vec;
use mc_attest_core::{
    IasNonce, IntelSealed, Quote, QuoteNonce, Report, TargetInfo, VerificationReport,
};
use mc_attest_enclave_api::{
    EnclaveMessage, Error as AttestEnclaveError, PeerAuthRequest, PeerAuthResponse, PeerSession,
};
use mc_attest_trusted::SealAlgo;
use mc_common::{logger::Logger, ResponderId};
use mc_crypto_ake_enclave::AkeEnclaveState;
use mc_crypto_box::{CryptoBox, VersionedCryptoBox};
use mc_crypto_keys::{CompressedRistrettoPublic, RistrettoPrivate, RistrettoPublic, X25519Public};
use mc_fog_ingest_enclave_api::{
    Error, IngestEnclave, IngestEnclaveInitParams, Result, SealedIngestKey,
    SetIngressPrivateKeyResult,
};
use mc_fog_kex_rng::KexRngPubkey;
use mc_fog_recovery_db_iface::ETxOutRecord;
use mc_fog_types::{
    ingest::TxsForIngest,
    view::{FogTxOut, FogTxOutMetadata, TxOutRecord},
};
use mc_oblivious_traits::{subtle::ConstantTimeEq, ORAMStorageCreator};
use mc_rand::McRng;
use mc_sgx_compat::sync::Mutex;
use mc_sgx_report_cache_api::{ReportableEnclave, Result as ReportableEnclaveResult};
use mc_transaction_core::{fog_hint::FogHint, tx::TxOut};
use mc_util_from_random::FromRandom;
use zeroize::Zeroize;

/// When processing a chunk of transactions, we try to add all of them without
/// overflowing ORAM, and emit one or zero new rng records. If clearing the
/// table and processing the chunk fails this many times, we give up -- the
/// configuration should be changed, for bigger ORAM or smaller chunks.
const MAX_CHUNK_RETRIES: usize = 10;

/// Business logic of the SgxIngestEnclave
pub struct SgxIngestEnclave<OSC: ORAMStorageCreator<StorageDataSize, StorageMetaSize>> {
    /// State related to attested key exchange and reports. This contains the
    /// "ingress key", which is used to decrypt the fog hints. The public
    /// key of that is in the reports.
    ake: AkeEnclaveState<RistrettoIdentity>,
    /// The "egress key" which is used to perform key exchange with the users.
    /// The public key of this appears in the RngRecord objects.
    /// This MUST NOT be replicated to peer enclaves or sealed under any
    /// circumstance.
    egress_key: Mutex<RistrettoPrivate>,
    /// State related to oblivious storage of user rng counters
    rng_store: Mutex<Option<RngStore<OSC>>>,
    /// Logger object
    logger: Logger,
}

impl<OSC: ORAMStorageCreator<StorageDataSize, StorageMetaSize>> SgxIngestEnclave<OSC> {
    /// Create a new sgx ingest enclave
    pub fn new(logger: Logger) -> Self {
        Self {
            ake: Default::default(),
            egress_key: Mutex::new(RistrettoPrivate::from_random(&mut McRng::default())),
            rng_store: Mutex::new(None),
            logger,
        }
    }

    /// Attempt to ingest tx's. This is a helper function to `ingest_txs`,
    /// which either succeeds in ingesting all of them, or reports that the map
    /// overflowed and we have to change the egress key and try again.
    /// Returns `None` if overflow occurs.
    fn attempt_ingest_txs(
        chunk: &PreparedBlockData,
        ingress_key: &RistrettoPrivate,
        egress_key: &RistrettoPrivate,
        rng_store: &mut RngStore<OSC>,
    ) -> Option<Vec<ETxOutRecord>> {
        let mut rng = McRng::default();

        let mut new_tx_rows = Vec::new();

        // Use the constant time fog hint decryption
        for (index, (txo, fog_tx_out)) in chunk.tx_outs.iter().enumerate() {
            let mut user_id = FogHint::new(RistrettoPublic::from_random(&mut rng));
            // Note: This is ignored because the semantic we want is, user_id should be
            // random if decryption failed, and ct_decrypt has no side-effects
            // if decryption fails.
            let _success = FogHint::ct_decrypt(ingress_key, &txo.e_fog_hint, &mut user_id);

            let mut aligned_view_pubkey: A8Bytes<U32> = Aligned(*GenericArray::from_slice(
                user_id.get_view_pubkey().as_bytes(),
            ));

            // Note: This branch succeeds if the fog-hint was well-formed, or if
            // both the mac checks failed, because ct_decrypt doesn't write to
            // to the buffer if the mac check fails, and we initialize to a valid point.
            //
            // This branch may *fail* if a broken client puts bad data in a fog hint,
            // consensus cannot decrypt the fog hints and detect that.
            // It is okay to not be constant-time for that case because a well-formed client
            // will never do that.
            //
            // The interesting scenarios are:
            // - The Txo is really for a user of this Fog, and then one of the ct_decrypt
            //   succeeds, and yields that user's view pubkey
            // - The Txo is for a mobilecoind user without fog (and then the hint is a
            //   random cipher text), or the Txo is for a user of a different fog
            //   deployment. In these cases the mac check fails, and we get the random,
            //   valid curve point used to initialize user_id.
            //
            // In both of those cases this branch is taken.
            if let Ok(decompressed_view_pubkey) =
                RistrettoPublic::try_from(aligned_view_pubkey.as_slice())
            {
                // Get the next rng output for this user
                use mc_crypto_keys::KexReusablePrivate;
                let shared_secret = egress_key.key_exchange(&decompressed_view_pubkey);
                let (overflow, rng_output) = rng_store.next_rng_output(shared_secret.as_ref());

                // If we overflow, caller needs to make a new egress key, tear down the
                // whole rng store, and try again
                // This path isn't constant-time and that's okay because an observer doesn't
                // learn anything about the txos, rngs, or the associated users.
                if overflow {
                    return None;
                }

                // Create a TxOutRecord, flattening the Txo data and getting extra data like
                // global index, block index, timestamp.
                let meta = FogTxOutMetadata {
                    global_index: chunk.global_txo_index + index as u64,
                    block_index: chunk.block_index,
                    timestamp: chunk.timestamp,
                };
                let txo_record = TxOutRecord::new(fog_tx_out.clone(), meta);

                // Get the view-kew-encrypted payload for this TX
                let plaintext = mc_util_serial::encode(&txo_record);

                let payload = VersionedCryptoBox::default()
                    .encrypt(&mut rng, &decompressed_view_pubkey, &plaintext)
                    .expect("CryptoBox encryption should not fail");
                // Push the new row
                new_tx_rows.push(ETxOutRecord {
                    search_key: rng_output.to_vec(),
                    payload,
                });
            }

            // TODO: Figure out how to zeroize other stuff here e.g. fog hint,
            // but it looks like this may require changes in upstream code
            aligned_view_pubkey.zeroize();
        }

        Some(new_tx_rows)
    }
}

impl<OSC: ORAMStorageCreator<StorageDataSize, StorageMetaSize>> ReportableEnclave
    for SgxIngestEnclave<OSC>
{
    fn new_ereport(&self, qe_info: TargetInfo) -> ReportableEnclaveResult<(Report, QuoteNonce)> {
        Ok(self.ake.new_ereport(qe_info)?)
    }

    fn verify_quote(&self, quote: Quote, qe_report: Report) -> ReportableEnclaveResult<IasNonce> {
        Ok(self.ake.verify_quote(quote, qe_report)?)
    }

    fn verify_ias_report(&self, ias_report: VerificationReport) -> ReportableEnclaveResult<()> {
        self.ake.verify_ias_report(ias_report)?;
        Ok(())
    }

    fn get_ias_report(&self) -> ReportableEnclaveResult<VerificationReport> {
        Ok(self.ake.get_ias_report()?)
    }
}

impl<OSC: ORAMStorageCreator<StorageDataSize, StorageMetaSize>> IngestEnclave
    for SgxIngestEnclave<OSC>
{
    fn enclave_init(&self, params: IngestEnclaveInitParams) -> Result<()> {
        self.ake.init(params.responder_id, Default::default())?;

        // if we were passed a sealed key, unseal it and overwrite the private key
        if let Some(sealed) = params.sealed_key {
            let cached = IntelSealed::try_from(sealed)?;
            let (key, _mac) = cached.unseal_raw()?;
            let new_key = RistrettoPrivate::try_from(&key[..])?;
            let mut lock = self.ake.get_identity().private_key.lock()?;
            *lock = new_key;
        }

        // initialize the rng store
        {
            let mut lock = self.rng_store.lock()?;
            *lock = Some(RngStore::new(params.desired_capacity, self.logger.clone()));
        }

        Ok(())
    }

    fn get_ingress_pubkey(&self) -> Result<RistrettoPublic> {
        Ok(self.ake.get_identity().get_public_key())
    }

    fn get_sealed_ingress_private_key(
        &self,
    ) -> Result<(SealedIngestKey, CompressedRistrettoPublic)> {
        // seal the private key and return it
        let lock = self.ake.get_identity().private_key.lock()?;
        let pubkey = RistrettoPublic::from(&*lock);
        let sealed_key = seal_private_key(&lock)?;
        Ok((sealed_key, pubkey.into()))
    }

    fn get_ingress_private_key(
        &self,
        peer: PeerSession,
    ) -> Result<(EnclaveMessage<PeerSession>, CompressedRistrettoPublic)> {
        if !self.ake.is_peer_known(&peer)? {
            return Err(Error::Attest(AttestEnclaveError::NotFound));
        }

        let private_key = *self.ake.get_identity().private_key.lock()?;
        let public_key = RistrettoPublic::from(&private_key);

        Ok((
            self.ake.peer_encrypt(&peer, &[], private_key.as_ref())?,
            public_key.into(),
        ))
    }

    fn set_ingress_private_key(
        &self,
        msg: EnclaveMessage<PeerSession>,
    ) -> Result<SetIngressPrivateKeyResult> {
        let key = self.ake.peer_decrypt(msg)?;
        let new_private_key = RistrettoPrivate::try_from(&key[..])?;
        let new_public_key = RistrettoPublic::from(&new_private_key);

        let sealed_key = seal_private_key(&new_private_key)?;
        let did_private_key_change: bool;

        {
            let mut lock = self.ake.get_identity().private_key.lock()?;
            did_private_key_change = !bool::from(lock.ct_eq(&new_private_key));
            *lock = new_private_key;
        }

        Ok(SetIngressPrivateKeyResult {
            new_public_key,
            sealed_key,
            did_private_key_change,
        })
    }

    fn get_kex_rng_pubkey(&self) -> Result<KexRngPubkey> {
        let egress_key = self.egress_key.lock()?;
        let public_key = CompressedRistrettoPublic::from(&RistrettoPublic::from(&*egress_key));
        Ok(KexRngPubkey {
            public_key: AsRef::<[u8; 32]>::as_ref(&public_key).to_vec(),
            version: self
                .rng_store
                .lock()?
                .as_ref()
                .expect("enclave was not initialized")
                .kex_rng_algo_version(),
        })
    }

    // Process incoming txs
    fn ingest_txs(&self, chunk: TxsForIngest) -> Result<(Vec<ETxOutRecord>, Option<KexRngPubkey>)> {
        let mut chunk_retries = MAX_CHUNK_RETRIES;
        let mut new_kex_rng_pubkey = None;

        // N.B. We should try to always lock in one order to prevent deadlocks
        let ingress_key = self.ake.get_identity().private_key.lock()?;
        let mut egress_key = self.egress_key.lock()?;
        let mut rng_store_lk = self.rng_store.lock()?;
        let rng_store = rng_store_lk.as_mut().expect("enclave was not initialized");

        // Converting a TxOut to a FogTxOut can fail if the masked amount is missing,
        // so flush all those errors out now.
        let prepared_block_data = PreparedBlockData::try_from(chunk)?;

        // Try to ingest the new tx's
        loop {
            if let Some(e_tx_out_records) =
                Self::attempt_ingest_txs(&prepared_block_data, &ingress_key, &egress_key, rng_store)
            {
                return Ok((e_tx_out_records, new_kex_rng_pubkey));
            } else {
                // If attempt_ingest_txs fails, that means the rng store overflowed.
                // If this happened too many times give up
                if chunk_retries == 0 {
                    return Err(Error::ChunkTooBig(
                        prepared_block_data.tx_outs.len(),
                        MAX_CHUNK_RETRIES,
                        rng_store.capacity(),
                    ));
                }
                chunk_retries -= 1;
                // We need to clear that table and make a new egress key.
                // We will also emit a new KexRngPubkey in this round.
                // Once we have done this, we can try to ingest again.
                // If the capacity of the rng store is large enough to hold one block,
                // then this will not be an infinite loop.
                *egress_key = RistrettoPrivate::from_random(&mut McRng::default());
                let public_key =
                    CompressedRistrettoPublic::from(&RistrettoPublic::from(&*egress_key));
                new_kex_rng_pubkey = Some(KexRngPubkey {
                    public_key: AsRef::<[u8; 32]>::as_ref(&public_key).to_vec(),
                    version: rng_store.kex_rng_algo_version(),
                });
                rng_store.clear();
            }
        }
    }

    fn new_keys(&self) -> Result<()> {
        let mut ingress_key = self.ake.get_identity().private_key.lock()?;
        let mut egress_key = self.egress_key.lock()?;
        let mut rng_store_lk = self.rng_store.lock()?;
        let rng_store = rng_store_lk.as_mut().expect("enclave was not initialized");

        *ingress_key = RistrettoPrivate::from_random(&mut McRng::default());
        *egress_key = RistrettoPrivate::from_random(&mut McRng::default());
        rng_store.clear();
        Ok(())
    }

    fn new_egress_key(&self) -> Result<()> {
        let mut egress_key = self.egress_key.lock()?;
        let mut rng_store_lk = self.rng_store.lock()?;
        let rng_store = rng_store_lk.as_mut().expect("enclave was not initialized");

        *egress_key = RistrettoPrivate::from_random(&mut McRng::default());
        rng_store.clear();
        Ok(())
    }

    fn get_identity(&self) -> Result<X25519Public> {
        Ok(self.ake.get_kex_identity())
    }

    fn peer_init(&self, peer_id: &ResponderId) -> Result<PeerAuthRequest> {
        Ok(self.ake.peer_init(peer_id)?)
    }

    fn peer_accept(&self, req: PeerAuthRequest) -> Result<(PeerAuthResponse, PeerSession)> {
        Ok(self.ake.peer_accept(req)?)
    }

    fn peer_connect(
        &self,
        peer_id: &ResponderId,
        msg: PeerAuthResponse,
    ) -> Result<(PeerSession, VerificationReport)> {
        Ok(self.ake.peer_connect(peer_id, msg)?)
    }

    fn peer_close(&self, session_id: &PeerSession) -> Result<()> {
        Ok(self.ake.peer_close(session_id)?)
    }
}

// Helper for sealing a key, which maps the error to IngestEnclaveError
fn seal_private_key(src: &RistrettoPrivate) -> Result<SealedIngestKey> {
    Ok(IntelSealed::seal_raw(src.as_ref(), &[])?.as_ref().to_vec())
}

// Helper to work around TxOut => FogTxOut being fallible.
// During attempt_ingest_txs, we must be constant-time and it complicates things
// if operations can fail, because an adversary may be able to use that as
// leverage.
//
// Instead we want to convert all TxOut to FogTxOut up front before calling
// attempt_ingest_txs, so that all the failures happen before anything sensitive
// happens.
//
// This is done by converting TxsForIngest to PreparedBlockData up front, and
// then proceeding to process the prepared block data.
#[derive(Clone, Eq, PartialEq, Debug)]
struct PreparedBlockData {
    /// The index of the block in the blockchain where this chunk of Txs
    /// appeared
    pub block_index: u64,
    /// The number of txo's appearing in the entire blockchain before this chunk
    /// This is needed to compute TxOutRecord::global_tx_out_index
    pub global_txo_index: u64,
    /// The txout's of this chunk, also formatted as FogTxOut
    pub tx_outs: Vec<(TxOut, FogTxOut)>,
    /// The timestamp of this block
    pub timestamp: u64,
}

impl TryFrom<TxsForIngest> for PreparedBlockData {
    type Error = Error;

    fn try_from(src: TxsForIngest) -> Result<PreparedBlockData> {
        let tx_outs = src
            .redacted_txs
            .into_iter()
            .map(|txo| match FogTxOut::try_from(&txo) {
                Ok(ftxo) => Ok((txo, ftxo)),
                Err(err) => Err(Error::InvalidBlockData(err)),
            })
            .collect::<Result<_>>()?;

        Ok(PreparedBlockData {
            block_index: src.block_index,
            global_txo_index: src.global_txo_index,
            timestamp: src.timestamp,
            tx_outs,
        })
    }
}
