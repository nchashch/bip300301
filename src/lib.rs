use std::net::SocketAddr;

use base64::Engine as _;
use bitcoin::{
    consensus::{Decodable, Encodable},
    Amount, BlockHash,
};
use client::{BlockCommitment, GetBlockClient, U8Witness};
use http::HeaderValue;
use jsonrpsee::http_client::{HeaderMap, HttpClient, HttpClientBuilder};
use serde::{Deserialize, Serialize};

pub use bitcoin;
pub use client::MainClient;
pub use jsonrpsee;

pub mod client;

pub use client::Header;
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum WithdrawalBundleStatus {
    Failed,
    Confirmed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DepositInfo {
    /// Hash of the block in which this deposit was included
    pub block_hash: BlockHash,
    /// Position of this transaction within the block that included it
    pub tx_index: usize,
    pub outpoint: bitcoin::OutPoint,
    pub output: Output,
}

#[derive(Default, Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TwoWayPegData {
    pub deposits: Vec<DepositInfo>,
    pub deposit_block_hash: Option<bitcoin::BlockHash>,
    pub bundle_statuses: Vec<(bitcoin::Txid, WithdrawalBundleStatus)>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Output {
    pub address: String,
    pub value: u64,
}

#[derive(Copy, Clone, Debug, Eq, Error, Hash, Ord, PartialEq, PartialOrd)]
#[error("Block not found: {0}")]
pub struct BlockNotFoundError(pub bitcoin::BlockHash);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("jsonrpsee error ({})", .main_addr)]
    Jsonrpsee {
        #[source]
        source: jsonrpsee::core::ClientError,
        main_addr: SocketAddr,
    },
    #[error("header error")]
    InvalidHeaderValue(#[from] http::header::InvalidHeaderValue),
    #[error("bitcoin consensus encode error")]
    BitcoinConsensusEncode(#[from] bitcoin::consensus::encode::Error),
    #[error("hex error")]
    Hex(#[from] hex::FromHexError),
    #[error("no next block for prev_main_hash = {prev_main_hash}")]
    NoNextBlock { prev_main_hash: bitcoin::BlockHash },
    #[error("io error")]
    Io(#[from] bitcoin::io::Error),
}

/// Use the `builder` argument to manually set client options
pub fn client(
    main_addr: SocketAddr,
    builder: Option<HttpClientBuilder>,
    password: &str,
    user: &str,
) -> Result<HttpClient, Error> {
    let mut headers = HeaderMap::new();
    let auth = format!("{user}:{password}");
    let header_value = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(auth)
    );
    let header_value = HeaderValue::from_str(&header_value)?;
    headers.insert("authorization", header_value);
    builder
        .unwrap_or_default()
        .set_headers(headers)
        .build(format!("http://{main_addr}"))
        .map_err(|source| Error::Jsonrpsee { source, main_addr })
}

#[derive(Clone)]
pub struct Drivechain {
    pub sidechain_number: u8,
    pub client: HttpClient,
    pub main_addr: SocketAddr,
}

impl Drivechain {
    // Verify BMM against the specified mainchain block.
    pub async fn verify_bmm(
        &self,
        main_hash: bitcoin::BlockHash,
        bmm_bytes: bitcoin::BlockHash,
    ) -> Result<bool, Error> {
        use jsonrpsee::types::error::ErrorCode as JsonrpseeErrorCode;
        match self
            .client
            .verifybmm(main_hash, bmm_bytes, self.sidechain_number)
            .await
        {
            Ok(_) => Ok(true),
            Err(jsonrpsee::core::ClientError::Call(err))
                if JsonrpseeErrorCode::from(err.code()) == JsonrpseeErrorCode::ServerError(-1)
                    && err.message() == "h* not found in block" =>
            {
                Ok(false)
            }
            Err(source) => Err(Error::Jsonrpsee {
                source,
                main_addr: self.main_addr,
            }),
        }
    }

    // Verify BMM for the next block, polling until the next block is available
    // Returns the verification result and mainchain block hash
    pub async fn verify_bmm_next_block(
        &self,
        prev_main_hash: bitcoin::BlockHash,
        bmm_bytes: bitcoin::BlockHash,
        poll_interval: std::time::Duration,
    ) -> Result<(bool, bitcoin::BlockHash), Error> {
        let main_hash = loop {
            if let Some(next_block_hash) = self
                .client
                .get_block(prev_main_hash, U8Witness::<1>)
                .await
                .map_err(|source| Error::Jsonrpsee {
                    source,
                    main_addr: self.main_addr,
                })?
                .nextblockhash
            {
                break next_block_hash;
            }
            tokio::time::sleep(poll_interval).await;
        };
        let res = self.verify_bmm(main_hash, bmm_bytes).await?;
        Ok((res, main_hash))
    }

    pub async fn get_mainchain_tip(&self) -> Result<bitcoin::BlockHash, Error> {
        self.client
            .getbestblockhash()
            .await
            .map_err(|source| Error::Jsonrpsee {
                source,
                main_addr: self.main_addr,
            })
    }

    /// Returns a vector of pairs of txout indexes and block commitments
    pub async fn get_block_commitments(
        &self,
        main_hash: bitcoin::BlockHash,
    ) -> Result<Result<Vec<(u32, BlockCommitment)>, BlockNotFoundError>, Error> {
        use jsonrpsee::types::error::ErrorCode as JsonrpseeErrorCode;
        match self.client.get_block_commitments(main_hash).await {
            Ok(commitments) => Ok(Ok(commitments.0)),
            Err(jsonrpsee::core::ClientError::Call(err))
                if JsonrpseeErrorCode::from(err.code()) == JsonrpseeErrorCode::ServerError(-1)
                    && err.message() == "Block not found" =>
            {
                Ok(Err(BlockNotFoundError(main_hash)))
            }
            Err(source) => Err(Error::Jsonrpsee {
                source,
                main_addr: self.main_addr,
            }),
        }
    }

    pub async fn get_header(&self, block_hash: bitcoin::BlockHash) -> Result<Header, Error> {
        self.client
            .getblockheader(block_hash)
            .await
            .map_err(|source| Error::Jsonrpsee {
                source,
                main_addr: self.main_addr,
            })
    }

    /// Returns a [`DepositInfo`] for each deposit output in the specified
    /// interval, and the hash of the last deposit block in the specified
    /// interval.
    async fn get_deposit_outputs(
        &self,
        end: bitcoin::BlockHash,
        start: Option<bitcoin::BlockHash>,
    ) -> Result<(Vec<DepositInfo>, Option<bitcoin::BlockHash>), Error> {
        let deposits = self
            .client
            .listsidechaindepositsbyblock(self.sidechain_number, Some(end), start)
            .await
            .map_err(|source| Error::Jsonrpsee {
                source,
                main_addr: self.main_addr,
            })?;
        let mut last_block_hash = None;
        let mut last_total = Amount::ZERO;
        let mut deposit_infos = Vec::new();
        for deposit in &deposits {
            let transaction = hex::decode(&deposit.txhex)?;
            let transaction =
                bitcoin::Transaction::consensus_decode(&mut bitcoin::io::Cursor::new(transaction))?;
            let txid = transaction.compute_txid();
            if let Some(start) = start {
                if deposit.hashblock == start {
                    last_total = transaction.output[deposit.nburnindex].value;
                    #[cfg(feature = "tracing")]
                    if tracing::enabled!(tracing::Level::DEBUG) {
                        tracing::debug!("ignoring tx {txid}");
                    }
                    continue;
                }
            }
            let total = transaction.output[deposit.nburnindex].value;
            if total < last_total {
                last_total = total;
                #[cfg(feature = "tracing")]
                if tracing::enabled!(tracing::Level::DEBUG) {
                    tracing::debug!("ignoring tx {txid}");
                }
                continue;
            }
            let value = total - last_total;
            let outpoint = bitcoin::OutPoint {
                txid,
                vout: deposit.nburnindex as u32,
            };
            last_total = total;
            last_block_hash = Some(deposit.hashblock);
            let output = Output {
                address: deposit.strdest.clone(),
                value: value.to_sat(),
            };
            let deposit_info = DepositInfo {
                block_hash: deposit.hashblock,
                tx_index: deposit.ntx,
                outpoint,
                output,
            };
            deposit_infos.push(deposit_info);
        }
        Ok((deposit_infos, last_block_hash))
    }

    async fn get_withdrawal_bundle_statuses(
        &self,
    ) -> Result<Vec<(bitcoin::Txid, WithdrawalBundleStatus)>, Error> {
        let mut statuses = Vec::new();
        let spent_withdrawals =
            &self
                .client
                .listspentwithdrawals()
                .await
                .map_err(|source| Error::Jsonrpsee {
                    source,
                    main_addr: self.main_addr,
                })?;
        for spent in spent_withdrawals {
            if spent.nsidechain == self.sidechain_number {
                statuses.push((spent.hash, WithdrawalBundleStatus::Confirmed));
            }
        }
        let failed_withdrawals = &self
            .client
            .listfailedwithdrawals()
            .await
            .map_err(|source| Error::Jsonrpsee {
                source,
                main_addr: self.main_addr,
            })?;
        for failed in failed_withdrawals {
            statuses.push((failed.hash, WithdrawalBundleStatus::Failed));
        }
        Ok(statuses)
    }

    pub async fn get_two_way_peg_data(
        &self,
        end: bitcoin::BlockHash,
        start: Option<bitcoin::BlockHash>,
    ) -> Result<TwoWayPegData, Error> {
        let (deposits, deposit_block_hash) = self.get_deposit_outputs(end, start).await?;
        let bundle_statuses = self.get_withdrawal_bundle_statuses().await?;
        let two_way_peg_data = TwoWayPegData {
            deposits,
            deposit_block_hash,
            bundle_statuses,
        };
        Ok(two_way_peg_data)
    }

    pub async fn broadcast_withdrawal_bundle(
        &self,
        transaction: bitcoin::Transaction,
    ) -> Result<(), Error> {
        let mut rawtx = vec![];
        transaction.consensus_encode(&mut rawtx)?;
        let rawtx = hex::encode(&rawtx);
        self.client
            .receivewithdrawalbundle(self.sidechain_number, &rawtx)
            .await
            .map_err(|source| Error::Jsonrpsee {
                source,
                main_addr: self.main_addr,
            })?;
        Ok(())
    }

    /// Use the `builder` argument to manually set client options
    pub fn new(
        client_builder: Option<HttpClientBuilder>,
        sidechain_number: u8,
        main_addr: SocketAddr,
        user: &str,
        password: &str,
    ) -> Result<Self, Error> {
        let client = client(main_addr, client_builder, password, user)?;
        Ok(Drivechain {
            sidechain_number,
            client,
            main_addr,
        })
    }
}

#[cfg(test)]
mod tests;
