use std::{
    collections::{BTreeSet, HashMap},
    future::Future,
    sync::Arc,
    time::Duration,
};

use tokio::sync::{watch, RwLock};
use zksync_dal::ConnectionPool;
use zksync_types::{
    api::{BlockId, Transaction, TransactionDetails, TransactionId},
    l2::L2Tx,
    Address, Nonce, H256,
};
use zksync_web3_decl::{
    error::{ClientRpcContext, EnrichedClientResult},
    jsonrpsee::http_client::{HttpClient, HttpClientBuilder},
    namespaces::{EthNamespaceClient, ZksNamespaceClient},
};

#[derive(Debug, Clone, Default)]
pub(crate) struct TxCache {
    inner: Arc<RwLock<TxCacheInner>>,
}

#[derive(Debug, Default)]
struct TxCacheInner {
    tx_cache: HashMap<H256, L2Tx>,
    nonces_by_account: HashMap<Address, BTreeSet<Nonce>>,
}

impl TxCache {
    async fn push(&self, tx: L2Tx) {
        let mut inner = self.inner.write().await;
        inner
            .nonces_by_account
            .entry(tx.initiator_account())
            .or_default()
            .insert(tx.nonce());
        inner.tx_cache.insert(tx.hash(), tx);
    }

    async fn get_tx(&self, tx_hash: H256) -> Option<L2Tx> {
        self.inner.read().await.tx_cache.get(&tx_hash).cloned()
    }

    async fn get_nonces_for_account(&self, account_address: Address) -> BTreeSet<Nonce> {
        let inner = self.inner.read().await;
        if let Some(nonces) = inner.nonces_by_account.get(&account_address) {
            nonces.clone()
        } else {
            BTreeSet::new()
        }
    }

    async fn remove_tx(&self, tx_hash: H256) {
        self.inner.write().await.tx_cache.remove(&tx_hash);
        // We intentionally don't change `nonces_by_account`; they should only be changed in response to new miniblocks
    }

    async fn run_updates(
        self,
        pool: ConnectionPool,
        stop_receiver: watch::Receiver<bool>,
    ) -> anyhow::Result<()> {
        const UPDATE_INTERVAL: Duration = Duration::from_secs(1);

        loop {
            if *stop_receiver.borrow() {
                return Ok(());
            }

            let addresses: Vec<_> = {
                // Split into 2 statements for readability.
                let inner = self.inner.read().await;
                inner.nonces_by_account.keys().copied().collect()
            };
            let mut storage = pool.access_storage_tagged("api").await?;
            let nonces_for_accounts = storage
                .storage_web3_dal()
                .get_nonces_for_addresses(&addresses)
                .await?;
            drop(storage); // Don't hold both `storage` and lock on `inner` at the same time.

            let mut inner = self.inner.write().await;
            inner.nonces_by_account.retain(|address, account_nonces| {
                let stored_nonce = nonces_for_accounts
                    .get(address)
                    .copied()
                    .unwrap_or(Nonce(0));
                // Retain only nonces starting from the stored one.
                *account_nonces = account_nonces.split_off(&stored_nonce);
                // If we've removed all nonces, drop the account entry so we don't request stored nonces for it later.
                !account_nonces.is_empty()
            });
            drop(inner);

            tokio::time::sleep(UPDATE_INTERVAL).await;
        }
    }
}

/// Used by external node to proxy transaction to the main node
/// and store them while they're not synced back yet
#[derive(Debug)]
pub struct TxProxy {
    tx_cache: TxCache,
    client: HttpClient,
}

impl TxProxy {
    pub fn new(main_node_url: &str) -> Self {
        let client = HttpClientBuilder::default().build(main_node_url).unwrap();
        Self {
            client,
            tx_cache: TxCache::default(),
        }
    }

    pub async fn find_tx(&self, tx_hash: H256) -> Option<L2Tx> {
        self.tx_cache.get_tx(tx_hash).await
    }

    pub async fn forget_tx(&self, tx_hash: H256) {
        self.tx_cache.remove_tx(tx_hash).await;
    }

    pub async fn save_tx(&self, tx: L2Tx) {
        self.tx_cache.push(tx).await;
    }

    pub async fn get_nonces_by_account(&self, account_address: Address) -> BTreeSet<Nonce> {
        self.tx_cache.get_nonces_for_account(account_address).await
    }

    pub async fn next_nonce_by_initiator_account(
        &self,
        account_address: Address,
        current_nonce: u32,
    ) -> Nonce {
        let mut pending_nonce = Nonce(current_nonce);
        let nonces = self.get_nonces_by_account(account_address).await;
        for nonce in nonces.range(pending_nonce + 1..) {
            // If nonce is not sequential, then we should not increment nonce.
            if nonce == &pending_nonce {
                pending_nonce += 1;
            } else {
                break;
            }
        }

        pending_nonce
    }

    pub async fn submit_tx(&self, tx: &L2Tx) -> EnrichedClientResult<H256> {
        let input_data = tx.common_data.input_data().expect("raw tx is absent");
        let raw_tx = zksync_types::Bytes(input_data.to_vec());
        let tx_hash = tx.hash();
        tracing::info!("Proxying tx {tx_hash:?}");
        self.client
            .send_raw_transaction(raw_tx)
            .rpc_context("send_raw_transaction")
            .with_arg("tx_hash", &tx_hash)
            .await
    }

    pub async fn request_tx(&self, id: TransactionId) -> EnrichedClientResult<Option<Transaction>> {
        match id {
            TransactionId::Block(BlockId::Hash(block), index) => {
                self.client
                    .get_transaction_by_block_hash_and_index(block, index)
                    .rpc_context("get_transaction_by_block_hash_and_index")
                    .with_arg("block", &block)
                    .with_arg("index", &index)
                    .await
            }
            TransactionId::Block(BlockId::Number(block), index) => {
                self.client
                    .get_transaction_by_block_number_and_index(block, index)
                    .rpc_context("get_transaction_by_block_number_and_index")
                    .with_arg("block", &block)
                    .with_arg("index", &index)
                    .await
            }
            TransactionId::Hash(hash) => {
                self.client
                    .get_transaction_by_hash(hash)
                    .rpc_context("get_transaction_by_hash")
                    .with_arg("hash", &hash)
                    .await
            }
        }
    }

    pub async fn request_tx_details(
        &self,
        hash: H256,
    ) -> EnrichedClientResult<Option<TransactionDetails>> {
        self.client
            .get_transaction_details(hash)
            .rpc_context("get_transaction_details")
            .with_arg("hash", &hash)
            .await
    }

    pub fn run_account_nonce_sweeper(
        &self,
        pool: ConnectionPool,
        stop_receiver: watch::Receiver<bool>,
    ) -> impl Future<Output = anyhow::Result<()>> {
        let tx_cache = self.tx_cache.clone();
        tx_cache.run_updates(pool, stop_receiver)
    }
}
