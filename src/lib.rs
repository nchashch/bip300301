mod client;
use base64::Engine as _;
use bitcoin::consensus::{Decodable, Encodable};
use jsonrpsee::http_client::{HeaderMap, HttpClient, HttpClientBuilder};
use std::collections::HashMap;
use std::net::SocketAddr;

pub use bitcoin;
pub use client::MainClient;
pub use jsonrpsee;

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub enum WithdrawalBundleStatus {
    Failed,
    Confirmed,
}

#[derive(Default, Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TwoWayPegData {
    pub deposits: HashMap<bitcoin::OutPoint, Output>,
    pub deposit_block_hash: Option<bitcoin::BlockHash>,
    pub bundle_statuses: HashMap<bitcoin::Txid, WithdrawalBundleStatus>,
}

#[derive(Clone)]
pub struct Drivechain {
    pub sidechain_number: u8,
    pub client: HttpClient,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Output {
    pub address: String,
    pub value: u64,
}

impl Drivechain {
    pub async fn verify_bmm(
        &self,
        prev_main_hash: &bitcoin::BlockHash,
        bmm_bytes: &bitcoin::BlockHash,
        poll_interval: std::time::Duration,
    ) -> Result<(), Error> {
        let main_hash = loop {
            if let Some(next_block_hash) = self
                .client
                .getblock(prev_main_hash, None)
                .await?
                .nextblockhash
            {
                break next_block_hash;
            }
            tokio::time::sleep(poll_interval).await;
        };
        self.client
            .verifybmm(&main_hash, bmm_bytes, self.sidechain_number)
            .await?;
        Ok(())
    }

    pub async fn get_mainchain_tip(&self) -> Result<bitcoin::BlockHash, Error> {
        Ok(self.client.getbestblockhash().await?)
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
            .await?;
        Ok(())
    }

    async fn get_deposit_outputs(
        &self,
        end: bitcoin::BlockHash,
        start: Option<bitcoin::BlockHash>,
    ) -> Result<
        (
            HashMap<bitcoin::OutPoint, Output>,
            Option<bitcoin::BlockHash>,
        ),
        Error,
    > {
        let deposits = self
            .client
            .listsidechaindepositsbyblock(self.sidechain_number, Some(end), start)
            .await?;
        let mut last_block_hash = None;
        let mut last_total = 0;
        let mut outputs = HashMap::new();
        for deposit in &deposits {
            let transaction = hex::decode(&deposit.txhex)?;
            let transaction =
                bitcoin::Transaction::consensus_decode(&mut std::io::Cursor::new(transaction))?;
            if let Some(start) = start {
                if deposit.hashblock == start {
                    last_total = transaction.output[deposit.nburnindex].value;
                    continue;
                }
            }
            let total = transaction.output[deposit.nburnindex].value;
            if total < last_total {
                last_total = total;
                continue;
            }
            let value = total - last_total;
            let outpoint = bitcoin::OutPoint {
                txid: transaction.txid(),
                vout: deposit.nburnindex as u32,
            };
            last_total = total;
            last_block_hash = Some(deposit.hashblock);
            let output = Output {
                address: deposit.strdest.clone(),
                value,
            };
            outputs.insert(outpoint, output);
        }
        Ok((outputs, last_block_hash))
    }

    async fn get_withdrawal_bundle_statuses(
        &self,
    ) -> Result<HashMap<bitcoin::Txid, WithdrawalBundleStatus>, Error> {
        let mut statuses = HashMap::new();
        for spent in &self.client.listspentwithdrawals().await? {
            if spent.nsidechain == self.sidechain_number {
                statuses.insert(spent.hash, WithdrawalBundleStatus::Confirmed);
            }
        }
        for failed in &self.client.listfailedwithdrawals().await? {
            statuses.insert(failed.hash, WithdrawalBundleStatus::Failed);
        }
        Ok(statuses)
    }

    pub fn new(
        sidechain_number: u8,
        main_addr: SocketAddr,
        user: &str,
        password: &str,
    ) -> Result<Self, Error> {
        let mut headers = HeaderMap::new();
        let auth = format!("{user}:{password}");
        let header_value = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD_NO_PAD.encode(auth)
        )
        .parse()?;
        headers.insert("authorization", header_value);
        let client = HttpClientBuilder::default()
            .set_headers(headers.clone())
            .build(format!("http://{main_addr}"))?;
        Ok(Drivechain {
            sidechain_number,
            client,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("jsonrpsee error")]
    Jsonrpsee(#[from] jsonrpsee::core::Error),
    #[error("header error")]
    InvalidHeaderValue(#[from] http::header::InvalidHeaderValue),
    #[error("bitcoin consensus encode error")]
    BitcoinConsensusEncode(#[from] bitcoin::consensus::encode::Error),
    #[error("bitcoin hex error")]
    BitcoinHex(#[from] bitcoin::hashes::hex::Error),
    #[error("hex error")]
    Hex(#[from] hex::FromHexError),
    #[error("no next block for prev_main_hash = {prev_main_hash}")]
    NoNextBlock { prev_main_hash: bitcoin::BlockHash },
    #[error("io error")]
    Io(#[from] std::io::Error),
}
