// Copyright (c) Zefchain Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::{collections::HashSet, time::Duration};

use custom_debug_derive::Debug;
use futures::{future::try_join_all, stream::FuturesUnordered, StreamExt};
use linera_base::{
    crypto::{CryptoHash, ValidatorPublicKey},
    data_types::{Blob, BlockHeight},
    ensure,
    identifiers::{BlobId, ChainId},
};
use linera_chain::{
    data_types::BlockProposal,
    types::{
        CertificateValue, ConfirmedBlockCertificate, GenericCertificate, LiteCertificate,
        TimeoutCertificate, ValidatedBlockCertificate,
    },
};
use rand::seq::SliceRandom as _;
use tracing::{instrument, warn};

use crate::{
    data_types::{BlockHeightRange, ChainInfo, ChainInfoQuery, ChainInfoResponse},
    node::{CrossChainMessageDelivery, NodeError, ValidatorNode},
};

/// A validator node together with the validator's name.
#[derive(Clone, Debug)]
pub struct RemoteNode<N> {
    pub public_key: ValidatorPublicKey,
    #[debug(skip)]
    pub node: N,
}

impl<N: ValidatorNode> RemoteNode<N> {
    pub(crate) async fn handle_chain_info_query(
        &self,
        query: ChainInfoQuery,
    ) -> Result<Box<ChainInfo>, NodeError> {
        let chain_id = query.chain_id;
        let response = self.node.handle_chain_info_query(query).await?;
        self.check_and_return_info(response, chain_id)
    }

    #[instrument(level = "trace")]
    pub(crate) async fn handle_block_proposal(
        &self,
        proposal: Box<BlockProposal>,
    ) -> Result<Box<ChainInfo>, NodeError> {
        let chain_id = proposal.content.block.chain_id;
        let response = self.node.handle_block_proposal(*proposal).await?;
        self.check_and_return_info(response, chain_id)
    }

    pub(crate) async fn handle_timeout_certificate(
        &self,
        certificate: TimeoutCertificate,
    ) -> Result<Box<ChainInfo>, NodeError> {
        let chain_id = certificate.inner().chain_id();
        let response = self.node.handle_timeout_certificate(certificate).await?;
        self.check_and_return_info(response, chain_id)
    }

    pub(crate) async fn handle_confirmed_certificate(
        &self,
        certificate: ConfirmedBlockCertificate,
        delivery: CrossChainMessageDelivery,
    ) -> Result<Box<ChainInfo>, NodeError> {
        let chain_id = certificate.inner().chain_id();
        let response = self
            .node
            .handle_confirmed_certificate(certificate, delivery)
            .await?;
        self.check_and_return_info(response, chain_id)
    }

    pub(crate) async fn handle_validated_certificate(
        &self,
        certificate: ValidatedBlockCertificate,
    ) -> Result<Box<ChainInfo>, NodeError> {
        let chain_id = certificate.inner().chain_id();
        let response = self.node.handle_validated_certificate(certificate).await?;
        self.check_and_return_info(response, chain_id)
    }

    #[instrument(level = "trace")]
    pub(crate) async fn handle_lite_certificate(
        &self,
        certificate: LiteCertificate<'_>,
        delivery: CrossChainMessageDelivery,
    ) -> Result<Box<ChainInfo>, NodeError> {
        let chain_id = certificate.value.chain_id;
        let response = self
            .node
            .handle_lite_certificate(certificate, delivery)
            .await?;
        self.check_and_return_info(response, chain_id)
    }

    pub(crate) async fn handle_optimized_validated_certificate(
        &mut self,
        certificate: &ValidatedBlockCertificate,
        delivery: CrossChainMessageDelivery,
    ) -> Result<Box<ChainInfo>, NodeError> {
        if certificate.is_signed_by(&self.public_key) {
            let result = self
                .handle_lite_certificate(certificate.lite_certificate(), delivery)
                .await;
            match result {
                Err(NodeError::MissingCertificateValue) => {
                    warn!(
                        "Validator {} forgot a certificate value that they signed before",
                        self.public_key
                    );
                }
                _ => return result,
            }
        }
        self.handle_validated_certificate(certificate.clone()).await
    }

    pub(crate) async fn handle_optimized_confirmed_certificate(
        &mut self,
        certificate: &ConfirmedBlockCertificate,
        delivery: CrossChainMessageDelivery,
    ) -> Result<Box<ChainInfo>, NodeError> {
        if certificate.is_signed_by(&self.public_key) {
            let result = self
                .handle_lite_certificate(certificate.lite_certificate(), delivery)
                .await;
            match result {
                Err(NodeError::MissingCertificateValue) => {
                    warn!(
                        "Validator {} forgot a certificate value that they signed before",
                        self.public_key
                    );
                }
                _ => return result,
            }
        }
        self.handle_confirmed_certificate(certificate.clone(), delivery)
            .await
    }

    fn check_and_return_info(
        &self,
        response: ChainInfoResponse,
        chain_id: ChainId,
    ) -> Result<Box<ChainInfo>, NodeError> {
        let manager = &response.info.manager;
        let proposed = manager.requested_proposed.as_ref();
        let locking = manager.requested_locking.as_ref();
        ensure!(
            proposed.is_none_or(|proposal| proposal.content.block.chain_id == chain_id)
                && locking.is_none_or(|cert| cert.chain_id() == chain_id)
                && response.check(self.public_key).is_ok(),
            NodeError::InvalidChainInfoResponse
        );
        Ok(response.info)
    }

    #[instrument(level = "trace", skip_all)]
    pub(crate) async fn query_certificates_from(
        &self,
        chain_id: ChainId,
        start: BlockHeight,
        limit: u64,
    ) -> Result<Vec<ConfirmedBlockCertificate>, NodeError> {
        tracing::debug!(name = ?self.public_key, ?chain_id, ?start, ?limit, "Querying certificates");
        let range = BlockHeightRange {
            start,
            limit: Some(limit),
        };
        let query = ChainInfoQuery::new(chain_id).with_sent_certificate_hashes_in_range(range);
        let info = self.handle_chain_info_query(query).await?;
        self.node
            .download_certificates(info.requested_sent_certificate_hashes)
            .await?
            .into_iter()
            .map(|c| {
                ensure!(
                    c.inner().chain_id() == chain_id,
                    NodeError::UnexpectedCertificateValue
                );
                ConfirmedBlockCertificate::try_from(c)
                    .map_err(|_| NodeError::InvalidChainInfoResponse)
            })
            .collect()
    }

    #[instrument(level = "trace")]
    pub(crate) async fn download_certificate_for_blob(
        &self,
        blob_id: BlobId,
    ) -> Result<ConfirmedBlockCertificate, NodeError> {
        let last_used_hash = self.node.blob_last_used_by(blob_id).await?;
        let certificate = self.node.download_certificate(last_used_hash).await?;
        if !certificate.block().requires_or_creates_blob(&blob_id) {
            warn!(
                "Got invalid last used by certificate for blob {} from validator {}",
                blob_id, self.public_key
            );
            return Err(NodeError::InvalidCertificateForBlob(blob_id));
        }
        Ok(certificate)
    }

    /// Sends a pending validated block's blobs to the validator.
    #[instrument(level = "trace")]
    pub(crate) async fn send_pending_blobs(
        &self,
        chain_id: ChainId,
        blobs: Vec<Blob>,
    ) -> Result<(), NodeError> {
        let tasks = blobs
            .into_iter()
            .map(|blob| self.node.handle_pending_blob(chain_id, blob.into_content()));
        try_join_all(tasks).await?;
        Ok(())
    }

    #[instrument(level = "trace")]
    async fn try_download_blob(&self, blob_id: BlobId) -> Option<Blob> {
        match self.node.download_blob(blob_id).await {
            Ok(blob) => {
                let blob = Blob::new(blob);
                if blob.id() != blob_id {
                    tracing::info!(
                        "Validator {} sent an invalid blob {blob_id}.",
                        self.public_key
                    );
                    None
                } else {
                    Some(blob)
                }
            }
            Err(error) => {
                tracing::debug!(
                    "Failed to fetch blob {blob_id} from validator {}: {error}",
                    self.public_key
                );
                None
            }
        }
    }

    /// Returns the list of certificate hashes on the given chain in the given range of heights.
    /// Returns an error if the number of hashes does not match the size of the range.
    #[instrument(level = "trace")]
    pub(crate) async fn fetch_sent_certificate_hashes(
        &self,
        chain_id: ChainId,
        range: BlockHeightRange,
    ) -> Result<Vec<CryptoHash>, NodeError> {
        let query =
            ChainInfoQuery::new(chain_id).with_sent_certificate_hashes_in_range(range.clone());
        let response = self.handle_chain_info_query(query).await?;
        let hashes = response.requested_sent_certificate_hashes;

        if range
            .limit
            .is_some_and(|limit| hashes.len() as u64 != limit)
        {
            warn!(
                ?range,
                received_num = hashes.len(),
                "Validator sent invalid number of certificate hashes."
            );
            return Err(NodeError::InvalidChainInfoResponse);
        }
        Ok(hashes)
    }

    #[instrument(level = "trace")]
    pub async fn download_certificates(
        &self,
        hashes: Vec<CryptoHash>,
    ) -> Result<Vec<ConfirmedBlockCertificate>, NodeError> {
        if hashes.is_empty() {
            return Ok(Vec::new());
        }
        let certificates = self.node.download_certificates(hashes.clone()).await?;
        let returned = certificates
            .iter()
            .map(ConfirmedBlockCertificate::hash)
            .collect();
        ensure!(
            returned == hashes,
            NodeError::UnexpectedCertificates {
                returned,
                requested: hashes
            }
        );
        Ok(certificates)
    }

    /// Downloads a blob, but does not verify if it has actually been published and
    /// accepted by a quorum of validators.
    #[instrument(level = "trace", skip(validators))]
    pub async fn download_blob(
        validators: &[Self],
        blob_id: BlobId,
        timeout: Duration,
    ) -> Option<Blob> {
        // Sequentially try each validator in random order.
        let mut validators = validators.iter().collect::<Vec<_>>();
        validators.shuffle(&mut rand::thread_rng());
        let mut stream = validators
            .into_iter()
            .zip(0..)
            .map(|(remote_node, i)| async move {
                linera_base::time::timer::sleep(timeout * i * i).await;
                remote_node.try_download_blob(blob_id).await
            })
            .collect::<FuturesUnordered<_>>();
        while let Some(maybe_blob) = stream.next().await {
            if let Some(blob) = maybe_blob {
                return Some(blob);
            }
        }
        None
    }

    /// Downloads the blobs with the given IDs. This is done in one concurrent task per block.
    /// Each task goes through the validators sequentially in random order and tries to download
    /// it. Returns `None` if it couldn't find all blobs.
    #[instrument(level = "trace", skip(validators))]
    pub async fn download_blobs(
        blob_ids: &[BlobId],
        validators: &[Self],
        timeout: Duration,
    ) -> Option<Vec<Blob>> {
        let mut stream = blob_ids
            .iter()
            .map(|blob_id| Self::download_blob(validators, *blob_id, timeout))
            .collect::<FuturesUnordered<_>>();
        let mut blobs = Vec::new();
        while let Some(maybe_blob) = stream.next().await {
            blobs.push(maybe_blob?);
        }
        Some(blobs)
    }

    /// Checks that requesting these blobs when trying to handle this certificate is legitimate,
    /// i.e. that there are no duplicates and the blobs are actually required.
    pub fn check_blobs_not_found<T: CertificateValue>(
        &self,
        certificate: &GenericCertificate<T>,
        blob_ids: &[BlobId],
    ) -> Result<(), NodeError> {
        ensure!(!blob_ids.is_empty(), NodeError::EmptyBlobsNotFound);
        let required = certificate.inner().required_blob_ids();
        let public_key = &self.public_key;
        for blob_id in blob_ids {
            if !required.contains(blob_id) {
                warn!("validator {public_key} requested blob {blob_id:?} but it is not required");
                return Err(NodeError::UnexpectedEntriesInBlobsNotFound);
            }
        }
        let unique_missing_blob_ids = blob_ids.iter().cloned().collect::<HashSet<_>>();
        if blob_ids.len() > unique_missing_blob_ids.len() {
            warn!("blobs requested by validator {public_key} contain duplicates");
            return Err(NodeError::DuplicatesInBlobsNotFound);
        }
        Ok(())
    }
}
