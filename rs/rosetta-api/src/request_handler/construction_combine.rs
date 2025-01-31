use crate::convert::{from_hex, make_read_state_from_update};
use crate::errors::{ApiError, Details};
use crate::models::{ConstructionCombineResponse, EnvelopePair, SignatureType, SignedTransaction};
use crate::request_handler::{make_sig_data, verify_network_id, RosettaRequestHandler};
use crate::{convert, models};
use ic_canister_client_sender::Ed25519KeyPair as EdKeypair;
use ic_canister_client_sender::Secp256k1KeyPair;
use ic_types::messages::{
    Blob, HttpCallContent, HttpReadStateContent, HttpRequestEnvelope, MessageId,
};
use rosetta_core::models::RosettaSupportedKeyPair;
use std::collections::HashMap;

impl RosettaRequestHandler {
    /// Create Network Transaction from Signatures.
    /// See https://www.rosetta-api.org/docs/ConstructionApi.html#constructioncombine
    // This returns Envelopes encoded in a CBOR string
    pub fn construction_combine(
        &self,
        msg: models::ConstructionCombineRequest,
    ) -> Result<ConstructionCombineResponse, ApiError> {
        verify_network_id(self.ledger.ledger_canister_id(), &msg.network_identifier)?;

        let mut signatures_by_sig_data: HashMap<Vec<u8>, _> = HashMap::new();

        for sig in &msg.signatures {
            let sig_data = convert::from_hex(&sig.signing_payload.hex_bytes)?;
            signatures_by_sig_data.insert(sig_data, sig);
        }

        let unsigned_transaction = msg.unsigned_transaction()?;

        let mut envelopes: SignedTransaction = vec![];

        for (request_type, update) in unsigned_transaction.updates {
            let mut request_envelopes = vec![];

            for ingress_expiry in &unsigned_transaction.ingress_expiries {
                let mut update = update.clone();
                update.ingress_expiry = *ingress_expiry;

                let read_state = make_read_state_from_update(&update);

                let transaction_signature = signatures_by_sig_data
                    .get(&make_sig_data(&update.id()))
                    .ok_or_else(|| {
                        ApiError::internal_error(
                            "Could not find signature for transaction".to_string(),
                        )
                    })?;
                let read_state_signature = signatures_by_sig_data
                    .get(&make_sig_data(&MessageId::from(
                        read_state.representation_independent_hash(),
                    )))
                    .ok_or_else(|| {
                        ApiError::internal_error(
                            "Could not find signature for read-state".to_string(),
                        )
                    })?;
                let envelope = match transaction_signature.signature_type {
                    SignatureType::Ed25519 => Ok(HttpRequestEnvelope::<HttpCallContent> {
                        content: HttpCallContent::Call { update },
                        sender_pubkey: Some(Blob(
                            EdKeypair::der_encode_pk(
                                EdKeypair::hex_decode_pk(
                                    &transaction_signature.public_key.hex_bytes,
                                )
                                .map_err(|err| {
                                    ApiError::InvalidPublicKey(
                                        false,
                                        Details::from(format!("{:?}", err)),
                                    )
                                })?,
                            )
                            .map_err(|err| {
                                ApiError::InvalidPublicKey(
                                    false,
                                    Details::from(format!("{:?}", err)),
                                )
                            })?,
                        )),
                        sender_sig: Some(Blob(from_hex(&transaction_signature.hex_bytes)?)),
                        sender_delegation: None,
                    }),
                    SignatureType::Ecdsa => Ok(HttpRequestEnvelope::<HttpCallContent> {
                        content: HttpCallContent::Call { update },
                        sender_pubkey: Some(Blob(
                            Secp256k1KeyPair::der_encode_pk(
                                Secp256k1KeyPair::hex_decode_pk(
                                    &transaction_signature.public_key.hex_bytes,
                                )
                                .map_err(|err| {
                                    ApiError::InvalidPublicKey(
                                        false,
                                        Details::from(format!("{:?}", err)),
                                    )
                                })?,
                            )
                            .map_err(|err| {
                                ApiError::InvalidPublicKey(
                                    false,
                                    Details::from(format!("{:?}", err)),
                                )
                            })?,
                        )),
                        sender_sig: Some(Blob(from_hex(&transaction_signature.hex_bytes)?)),
                        sender_delegation: None,
                    }),
                    sig_type => Err(ApiError::InvalidRequest(
                        false,
                        format!("Sginature Type {} not supported byt rosetta", sig_type).into(),
                    )),
                }?;

                let read_state_envelope = match read_state_signature.signature_type {
                    SignatureType::Ed25519 => Ok(HttpRequestEnvelope::<HttpReadStateContent> {
                        content: HttpReadStateContent::ReadState { read_state },
                        sender_pubkey: Some(Blob(
                            EdKeypair::der_encode_pk(
                                EdKeypair::hex_decode_pk(
                                    &read_state_signature.public_key.hex_bytes,
                                )
                                .map_err(|err| {
                                    ApiError::InvalidPublicKey(
                                        false,
                                        Details::from(format!("{:?}", err)),
                                    )
                                })?,
                            )
                            .map_err(|err| {
                                ApiError::InvalidPublicKey(
                                    false,
                                    Details::from(format!("{:?}", err)),
                                )
                            })?,
                        )),
                        sender_sig: Some(Blob(from_hex(&read_state_signature.hex_bytes)?)),
                        sender_delegation: None,
                    }),
                    SignatureType::Ecdsa => Ok(HttpRequestEnvelope::<HttpReadStateContent> {
                        content: HttpReadStateContent::ReadState { read_state },
                        sender_pubkey: Some(Blob(
                            Secp256k1KeyPair::der_encode_pk(
                                Secp256k1KeyPair::hex_decode_pk(
                                    &transaction_signature.public_key.hex_bytes,
                                )
                                .map_err(|err| {
                                    ApiError::InvalidPublicKey(
                                        false,
                                        Details::from(format!("{:?}", err)),
                                    )
                                })?,
                            )
                            .map_err(|err| {
                                ApiError::InvalidPublicKey(
                                    false,
                                    Details::from(format!("{:?}", err)),
                                )
                            })?,
                        )),

                        sender_sig: Some(Blob(from_hex(&read_state_signature.hex_bytes)?)),
                        sender_delegation: None,
                    }),
                    sig_type => Err(ApiError::InvalidRequest(
                        false,
                        format!("Sginature Type {} not supported byt rosetta", sig_type).into(),
                    )),
                }?;
                request_envelopes.push(EnvelopePair {
                    update: envelope,
                    read_state: read_state_envelope,
                });
            }

            envelopes.push((request_type, request_envelopes));
        }

        let envelopes = hex::encode(serde_cbor::to_vec(&envelopes).map_err(|_| {
            ApiError::InternalError(false, "Serialization of envelope failed".into())
        })?);

        Ok(ConstructionCombineResponse {
            signed_transaction: envelopes,
        })
    }
}
