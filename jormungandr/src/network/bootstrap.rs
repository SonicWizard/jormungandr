use super::{grpc, BlockConfig};
use crate::blockcfg::{Block, HeaderHash};
use crate::blockchain::{self, Blockchain, Error as BlockchainError, PreCheckedHeader, Ref, Tip};
use crate::settings::start::network::Peer;
use chain_core::property::HasHeader;
use network_core::client::{BlockService, Client as _};
use network_core::error::Error as NetworkError;
use network_grpc::client::Connection;
use slog::Logger;
use thiserror::Error;
use tokio::prelude::*;
use tokio_compat::prelude::*;

use std::fmt::Debug;
use std::io;
use std::sync::Arc;

const APPLY_FREQUENCY_BOOTSTRAP: usize = 128;

#[derive(Error, Debug)]
pub enum Error {
    #[error("runtime initialization failed")]
    RuntimeInit { source: io::Error },
    #[error("failed to connect to bootstrap peer")]
    Connect { source: grpc::ConnectError },
    #[error("connection broken")]
    ClientNotReady { source: NetworkError },
    #[error("bootstrap pull request failed")]
    PullRequestFailed { source: NetworkError },
    #[error("bootstrap pull stream failed")]
    PullStreamFailed { source: NetworkError },
    #[error("block header check failed")]
    HeaderCheckFailed { source: BlockchainError },
    #[error("received block {0} is already present")]
    BlockAlreadyPresent(HeaderHash),
    #[error("received block {0} is not connected to the block chain")]
    BlockMissingParent(HeaderHash),
    #[error("failed to apply block to the blockchain")]
    ApplyBlockFailed { source: BlockchainError },
    #[error("failed to select the new tip")]
    ChainSelectionFailed { source: BlockchainError },
}

pub async fn bootstrap_from_peer(
    peer: Peer,
    blockchain: Blockchain,
    branch: Tip,
    logger: Logger,
) -> Result<Arc<Ref>, Error> {
    info!(logger, "connecting to bootstrap peer {}", peer.connection);

    let (client, tip) = grpc::connect(peer.address(), None)
        .map_err(|e| Error::Connect { source: e })
        .and_then(|client: Connection<BlockConfig>| {
            client
                .ready()
                .map_err(|e| Error::ClientNotReady { source: e })
        })
        .join(branch.get_ref())
        .compat()
        .await?;

    let tip_hash = tip.hash();
    debug!(logger, "pulling blocks starting from {}", tip_hash);

    let stream = client
        .pull_blocks_to_tip(&[tip_hash])
        .compat()
        .await
        .map_err(|e| Error::PullRequestFailed { source: e })?;

    let tip = bootstrap_from_stream(blockchain, tip, stream, logger).await?;

    blockchain::process_new_ref(logger, blockchain, branch, tip.clone())
        .await
        .map_err(|e| Error::ChainSelectionFailed { source: e })
        .map(|()| tip)
}

async fn bootstrap_from_stream<S>(
    blockchain: Blockchain,
    tip: Arc<Ref>,
    stream: S,
    logger: Logger,
) -> Result<Arc<Ref>, Error>
where
    S: Stream<Item = Block, Error = NetworkError>,
    S::Error: Debug,
{
    use futures03::stream::TryStreamExt;

    let fold_logger = logger.clone();

    let block0 = blockchain.block0().clone();
    let logger2 = logger.clone();
    let blockchain2 = blockchain.clone();

    stream
        .compat()
        .map_err(|e| Error::PullStreamFailed { source: e })
        .try_filter(move |block| {
            let header_hash = block.header.hash();
            async move { header_hash != block0 }
        })
        .try_fold(tip, |_, block| {
            async move { handle_block(blockchain.clone(), block, fold_logger.clone()).await }
        })
        .await
}

async fn handle_block(
    blockchain: Blockchain,
    block: Block,
    logger: Logger,
) -> Result<Arc<Ref>, Error> {
    let header = block.header();
    trace!(
        logger,
        "received block from the bootstrap node: {:#?}",
        header
    );
    let end_blockchain = blockchain.clone();
    let pre_checked = blockchain
        .pre_check_header(header, true)
        .await
        .map_err(|e| Error::HeaderCheckFailed { source: e })?;
    let (header, parent_ref) = match pre_checked {
        PreCheckedHeader::AlreadyPresent { header, .. } => {
            return Err(Error::BlockAlreadyPresent(header.hash()))
        }
        PreCheckedHeader::MissingParent { header, .. } => {
            return Err(Error::BlockMissingParent(header.hash()))
        }
        PreCheckedHeader::HeaderWithCache { header, parent_ref } => (header, parent_ref),
    };

    let post_checked = blockchain
        .post_check_header(header, parent_ref)
        .compat()
        .await
        .map_err(|e| Error::HeaderCheckFailed { source: e })?;

    debug!(
        logger,
        "validated block";
        "hash" => %post_checked.header().hash(),
        "block_date" => %post_checked.header().block_date(),
    );

    end_blockchain
        .apply_and_store_block(post_checked, block)
        .await
        .map_err(|e| Error::ApplyBlockFailed { source: e })
}
