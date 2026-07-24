use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use alloy::{
    network::ReceiptResponse,
    primitives::{Address, B256, U256, keccak256},
    providers::{DynProvider, PendingTransactionBuilder, Provider, ProviderBuilder},
};
use async_trait::async_trait;
use mpp::client::tempo::wallet::TempoWallet;
use tempo_alloy::{TempoNetwork, contracts::precompiles::ITIP20, provider::TempoProviderExt};
use tempo_primitives::SignatureType;

use crate::{db::Fulfillment, tempo_wallet::TempoAccessKeyWallet};

#[async_trait]
pub(crate) trait Issuer: Send + Sync {
    async fn balance(&self, wallet: Address) -> Result<u64, IssuerError>;
    async fn submit(&self, order: &Fulfillment) -> Result<String, IssuerError>;
    async fn confirm(&self, transaction_hash: &str) -> Result<(), IssuerError>;
}

#[derive(Default)]
pub(crate) struct MockIssuer {
    balances: Mutex<HashMap<Address, u64>>,
    fulfilled: Mutex<HashSet<String>>,
}

#[async_trait]
impl Issuer for MockIssuer {
    async fn balance(&self, wallet: Address) -> Result<u64, IssuerError> {
        Ok(*self
            .balances
            .lock()
            .map_err(|_| IssuerError::Poisoned)?
            .get(&wallet)
            .unwrap_or(&0))
    }

    async fn submit(&self, order: &Fulfillment) -> Result<String, IssuerError> {
        let hash = order
            .transaction_hash
            .clone()
            .unwrap_or_else(|| format!("{:#x}", keccak256(order.id.as_bytes())));
        let mut fulfilled = self.fulfilled.lock().map_err(|_| IssuerError::Poisoned)?;
        if fulfilled.insert(order.id.clone()) {
            let mut balances = self.balances.lock().map_err(|_| IssuerError::Poisoned)?;
            let balance = balances.entry(order.wallet).or_default();
            *balance = balance.saturating_add(order.amount);
        }
        Ok(hash)
    }

    async fn confirm(&self, _transaction_hash: &str) -> Result<(), IssuerError> {
        Ok(())
    }
}

pub(crate) struct AlloyIssuer {
    provider: DynProvider<TempoNetwork>,
    token: Address,
    fee_token: Address,
    account: Address,
    access_key: Address,
    key_authorization: Option<tempo_primitives::transaction::SignedKeyAuthorization>,
}

impl AlloyIssuer {
    pub fn new(
        rpc_url: &str,
        token: Address,
        fee_token: Address,
        wallet_store: Option<&Path>,
    ) -> Result<Self, IssuerError> {
        let wallet = wallet_store.map_or_else(TempoWallet::load_default, TempoWallet::load)?;
        if wallet.chain_id != nanousd::TEMPO_MAINNET_CHAIN_ID {
            return Err(IssuerError::WrongChain(wallet.chain_id));
        }
        let account = wallet.account;
        let access_key = wallet.access_key;
        let key_authorization = wallet.key_authorization.as_deref().cloned();
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .wallet(TempoAccessKeyWallet::from(&wallet))
            .connect_http(rpc_url.parse().map_err(alloy_error)?)
            .erased();
        tracing::info!(%account, %access_key, "loaded NanoUSD Alloy issuer");
        Ok(Self {
            provider,
            token,
            fee_token,
            account,
            access_key,
            key_authorization,
        })
    }

    async fn pending_key_authorization(
        &self,
    ) -> Result<Option<tempo_primitives::transaction::SignedKeyAuthorization>, IssuerError> {
        let Some(authorization) = &self.key_authorization else {
            return Ok(None);
        };
        let key = self
            .provider
            .get_keychain_key(self.account, self.access_key)
            .await
            .map_err(alloy_error)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| IssuerError::Clock)?
            .as_secs();
        Ok(
            (key.keyId != self.access_key || key.isRevoked || key.expiry <= now)
                .then(|| authorization.clone()),
        )
    }
}

#[async_trait]
impl Issuer for AlloyIssuer {
    async fn balance(&self, wallet: Address) -> Result<u64, IssuerError> {
        let balance = ITIP20::new(self.token, &self.provider)
            .balanceOf(wallet)
            .call()
            .await
            .map_err(alloy_error)?;
        u64::try_from(balance).map_err(|_| IssuerError::BalanceOverflow)
    }

    async fn submit(&self, order: &Fulfillment) -> Result<String, IssuerError> {
        if let Some(hash) = &order.transaction_hash {
            return Ok(hash.clone());
        }

        let nonce_key =
            U256::from_be_bytes(keccak256(format!("nanousd-order:{}", order.id).as_bytes()).0);
        let mut request = ITIP20::new(self.token, &self.provider)
            .mint(order.wallet, U256::from(order.amount))
            .into_transaction_request()
            .with_fee_token(self.fee_token)
            .with_nonce_key(nonce_key)
            .with_key_type(SignatureType::P256)
            .with_key_id(self.access_key);
        request.inner.from = Some(self.account);
        request.inner.nonce = Some(0);
        request.key_authorization = self.pending_key_authorization().await?;

        let pending = self
            .provider
            .send_transaction(request)
            .await
            .map_err(alloy_error)?;
        Ok(format!("{:#x}", pending.tx_hash()))
    }

    async fn confirm(&self, transaction_hash: &str) -> Result<(), IssuerError> {
        let hash: B256 = transaction_hash.parse().map_err(alloy_error)?;
        let receipt = PendingTransactionBuilder::new(self.provider.root().clone(), hash)
            .get_receipt()
            .await
            .map_err(alloy_error)?;
        if receipt.status() {
            Ok(())
        } else {
            Err(IssuerError::TransactionReverted(
                transaction_hash.to_owned(),
            ))
        }
    }
}

fn alloy_error(error: impl std::fmt::Display) -> IssuerError {
    IssuerError::Alloy(error.to_string())
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum IssuerError {
    #[error("failed to load the Tempo issuer wallet: {0}")]
    Wallet(#[from] mpp::client::tempo::wallet::TempoWalletError),
    #[error("issuer wallet is configured for chain {0}, expected Tempo mainnet")]
    WrongChain(u64),
    #[error("Tempo Alloy operation failed: {0}")]
    Alloy(String),
    #[error("NanoUSD balance does not fit in the API representation")]
    BalanceOverflow,
    #[error("Tempo mint transaction {0} reverted")]
    TransactionReverted(String),
    #[error("system clock is before the Unix epoch")]
    Clock,
    #[error("mock issuer balance lock was poisoned")]
    Poisoned,
}
