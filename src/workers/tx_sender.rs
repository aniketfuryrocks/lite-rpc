use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::bail;
use dashmap::DashMap;
use log::{info, warn};

use solana_transaction_status::TransactionStatus;
use tokio::{
    sync::mpsc::{error::TryRecvError, UnboundedReceiver},
    task::JoinHandle,
};

use crate::{
    tpu_manager::TpuManager,
    workers::{PostgresMsg, PostgresTx},
};

use super::PostgresMpscSend;

pub type WireTransaction = Vec<u8>;

/// Retry transactions to a maximum of `u16` times, keep a track of confirmed transactions
#[derive(Clone)]
pub struct TxSender {
    /// Tx(s) forwarded to tpu
    pub txs_sent: Arc<DashMap<String, TxProps>>,
    /// TpuClient to call the tpu port
    tpu_manager: Arc<TpuManager>,
}

/// Transaction Properties
pub struct TxProps {
    pub status: Option<TransactionStatus>,
    /// Time at which transaction was forwarded
    pub sent_at: Instant,
}

impl Default for TxProps {
    fn default() -> Self {
        Self {
            status: Default::default(),
            sent_at: Instant::now(),
        }
    }
}

impl TxSender {
    pub fn new(tpu_manager: Arc<TpuManager>) -> Self {
        Self {
            tpu_manager,
            txs_sent: Default::default(),
        }
    }

    /// retry enqued_tx(s)
    async fn forward_txs(
        &self,
        sigs_and_slots: Vec<(String, u64)>,
        txs: Vec<WireTransaction>,
        postgres: Option<PostgresMpscSend>,
    ) {
        assert_eq!(sigs_and_slots.len(), txs.len());

        if sigs_and_slots.is_empty() {
            return;
        }

        let tpu_client = self.tpu_manager.clone();
        let txs_sent = self.txs_sent.clone();

        tokio::spawn(async move {
            let quic_response = match tpu_client.try_send_wire_transaction_batch(txs).await {
                Ok(_) => {
                    for (sig, _) in &sigs_and_slots {
                        txs_sent.insert(sig.to_owned(), TxProps::default());
                    }
                    1
                }
                Err(err) => {
                    warn!("{err}");
                    0
                }
            };

            if let Some(postgres) = postgres {
                for (sig, recent_slot) in sigs_and_slots {
                    postgres
                        .send(PostgresMsg::PostgresTx(PostgresTx {
                            signature: sig.clone(),
                            recent_slot: recent_slot as i64,
                            forwarded_slot: 0,    // FIX: figure this out
                            processed_slot: None, // FIX: figure this out
                            cu_consumed: None,    // FIX: figure this out
                            cu_requested: None,   // FIX: figure this out
                            quic_response,
                        }))
                        .expect("Error writing to postgres service");
                }
            }
        });
    }

    /// retry and confirm transactions every 2ms (avg time to confirm tx)
    pub fn execute(
        self,
        mut recv: UnboundedReceiver<(String, WireTransaction, u64)>,
        tx_batch_size: usize,
        tx_send_interval: Duration,
        postgres_send: Option<PostgresMpscSend>,
    ) -> JoinHandle<anyhow::Result<()>> {
        tokio::spawn(async move {
            info!(
                "Batching tx(s) with batch size of {tx_batch_size} every {}ms",
                tx_send_interval.as_millis()
            );

            loop {
                let prev_inst = tokio::time::Instant::now();

                let mut sigs_and_slots = Vec::with_capacity(tx_batch_size);
                let mut txs = Vec::with_capacity(tx_batch_size);

                while (prev_inst.elapsed() < tx_send_interval) || txs.len() == tx_batch_size {
                    match recv.try_recv() {
                        Ok((sig, tx, slot)) => {
                            sigs_and_slots.push((sig, slot));
                            txs.push(tx);
                        }
                        Err(TryRecvError::Disconnected) => {
                            bail!("Channel Disconnected");
                        }
                        _ => {}
                    }
                }

                self.forward_txs(sigs_and_slots, txs, postgres_send.clone())
                    .await;
            }
        })
    }
}