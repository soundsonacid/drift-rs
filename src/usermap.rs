use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::event_emitter::EventEmitter;
use crate::memcmp::{get_non_idle_user_filter, get_user_filter};
use crate::utils::{decode, get_ws_url};
use crate::websocket_program_account_subscriber::{
    ProgramAccountUpdate, WebsocketProgramAccountOptions, WebsocketProgramAccountSubscriber,
};
use crate::{DataAndSlot, SdkResult};
use anchor_lang::AccountDeserialize;
use dashmap::DashMap;
use drift::state::user::User;
use serde_json::json;
use solana_account_decoder::UiAccountEncoding;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig};
use solana_client::rpc_filter::RpcFilterType;
use solana_client::rpc_request::RpcRequest;
use solana_client::rpc_response::{OptionalContext, RpcKeyedAccount};
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;

pub struct UserMap {
    pub(crate) subscribed: bool,
    subscription: WebsocketProgramAccountSubscriber,
    pub(crate) usermap: Arc<DashMap<String, User>>,
    sync_lock: Option<Mutex<()>>,
    latest_slot: Arc<AtomicU64>,
    commitment: CommitmentConfig,
    rpc: RpcClient,
    optional_callback: Option<Arc<dyn Fn(&ProgramAccountUpdate<User>) + Send + Sync>>,
}

impl UserMap {
    pub const SUBSCRIPTION_ID: &'static str = "usermap";

    pub fn new(
        commitment: CommitmentConfig,
        endpoint: String,
        sync: bool,
        additional_filters: Option<Vec<RpcFilterType>>,
        optional_callback: Option<Arc<dyn Fn(&ProgramAccountUpdate<User>) + Send + Sync>>,
    ) -> Self {
        let mut filters = vec![get_user_filter(), get_non_idle_user_filter()];
        filters.extend(additional_filters.unwrap_or_default());
        let options = WebsocketProgramAccountOptions {
            filters,
            commitment,
            encoding: UiAccountEncoding::Base64,
        };
        let event_emitter = EventEmitter::new();

        let url = get_ws_url(&endpoint.clone()).unwrap();

        let subscription = WebsocketProgramAccountSubscriber::new(
            UserMap::SUBSCRIPTION_ID,
            url,
            options,
            event_emitter,
        );

        let usermap = Arc::new(DashMap::new());

        let rpc = RpcClient::new_with_commitment(endpoint.clone(), commitment);

        let sync_lock = if sync { Some(Mutex::new(())) } else { None };

        Self {
            subscribed: false,
            subscription,
            usermap,
            sync_lock,
            latest_slot: Arc::new(AtomicU64::new(0)),
            commitment,
            rpc,
            optional_callback,
        }
    }

    pub async fn subscribe(&mut self) -> SdkResult<()> {
        if self.sync_lock.is_some() {
            self.sync().await?;
        }

        if !self.subscribed {
            self.subscription.subscribe::<User>().await?;
            self.subscribed = true;

            let usermap = self.usermap.clone();
            let latest_slot = self.latest_slot.clone();
            let callback = match self.optional_callback {
                Some(ref callback) => Some(callback.clone()),
                None => None,
            };

            self.subscription
                .event_emitter
                .subscribe(UserMap::SUBSCRIPTION_ID, move |event| {
                    if let Some(update) =
                        event.as_any().downcast_ref::<ProgramAccountUpdate<User>>()
                    {
                        let user_data_and_slot = update.data_and_slot.clone();
                        let user_pubkey = update.pubkey.to_string();
                        if update.data_and_slot.slot > latest_slot.load(Ordering::Relaxed) {
                            latest_slot.store(update.data_and_slot.slot, Ordering::Relaxed);
                        }
                        usermap.insert(user_pubkey, user_data_and_slot.data);
                        if callback.is_some() {
                            callback.as_ref().unwrap()(update);
                        }
                    }
                });
        }

        Ok(())
    }

    pub async fn unsubscribe(&mut self) -> SdkResult<()> {
        if self.subscribed {
            self.subscription.unsubscribe().await?;
            self.subscribed = false;
            self.usermap.clear();
            self.latest_slot.store(0, Ordering::Relaxed);
        }
        Ok(())
    }

    pub fn size(&self) -> usize {
        self.usermap.len()
    }

    pub fn contains(&self, pubkey: &str) -> bool {
        self.usermap.contains_key(pubkey)
    }

    pub fn get(&self, pubkey: &str) -> Option<User> {
        self.usermap.get(pubkey).map(|user| user.value().clone())
    }

    pub async fn must_get(&self, pubkey: &str) -> SdkResult<User> {
        if let Some(user) = self.get(pubkey) {
            Ok(user)
        } else {
            let user_data = self
                .rpc
                .get_account_data(&Pubkey::from_str(pubkey).unwrap())
                .await?;
            let user = User::try_deserialize(&mut user_data.as_slice()).unwrap();
            let callback = match self.optional_callback {
                Some(ref callback) => Some(callback.clone()),
                None => None,
            };
            self.usermap.insert(pubkey.to_string(), user);
            if callback.is_some() {
                callback.as_ref().unwrap()(&ProgramAccountUpdate::new(
                    pubkey.to_string().clone(),
                    DataAndSlot {
                        data: user,
                        slot: self.get_latest_slot(),
                    },
                    Instant::now(),
                ));
            }
            Ok(self.get(pubkey).unwrap())
        }
    }

    async fn sync(&mut self) -> SdkResult<()> {
        dbg!("syncing");
        let sync_lock = self.sync_lock.as_ref().expect("expected sync lock");

        let callback = match self.optional_callback {
            Some(ref callback) => Some(callback.clone()),
            None => None,
        };

        let lock = match sync_lock.try_lock() {
            Ok(lock) => lock,
            Err(_) => return Ok(()),
        };

        let account_config = RpcAccountInfoConfig {
            commitment: Some(self.commitment),
            encoding: Some(self.subscription.options.encoding),
            ..RpcAccountInfoConfig::default()
        };

        let gpa_config = RpcProgramAccountsConfig {
            filters: Some(self.subscription.options.filters.clone()),
            account_config,
            with_context: Some(true),
        };

        let response = self
            .rpc
            .send::<OptionalContext<Vec<RpcKeyedAccount>>>(
                RpcRequest::GetProgramAccounts,
                json!([drift::id().to_string(), gpa_config]),
            )
            .await?;

        if let OptionalContext::Context(accounts) = response {
            for account in accounts.value {
                let pubkey = account.pubkey;
                let user_data = account.account.data;
                let data = decode::<User>(user_data)?;
                self.usermap.insert(pubkey.clone(), data);
                if callback.is_some() {
                    callback.as_ref().unwrap()(&ProgramAccountUpdate::new(
                        pubkey,
                        DataAndSlot {
                            data,
                            slot: accounts.context.slot,
                        },
                        Instant::now(),
                    ));
                }
            }

            self.latest_slot
                .store(accounts.context.slot, Ordering::Relaxed);
        }

        dbg!("sync over");

        drop(lock);
        Ok(())
    }

    pub fn get_latest_slot(&self) -> u64 {
        self.latest_slot.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use solana_sdk::commitment_config::CommitmentConfig;
    use solana_sdk::commitment_config::CommitmentLevel;

    #[tokio::test]
    #[cfg(rpc_tests)]
    async fn test_usermap() {
        let endpoint = "rpc".to_string();
        let commitment = CommitmentConfig {
            commitment: CommitmentLevel::Processed,
        };

        let mut usermap = UserMap::new(commitment, endpoint, true, None, None);
        usermap.subscribe().await.unwrap();

        tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;

        dbg!(usermap.size());
        assert!(usermap.size() > 50000);

        dbg!(usermap.get_latest_slot());

        usermap.unsubscribe().await.unwrap();

        tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;

        assert_eq!(usermap.size(), 0);
        assert_eq!(usermap.subscribed, false);
    }

    #[tokio::test]
    #[cfg(rpc_tests)]
    async fn test_callback() {
        use super::*;

        let event_emitter = Box::leak(Box::new(EventEmitter::new()));

        let endpoint = "rpc".to_string();
        let commitment = CommitmentConfig {
            commitment: CommitmentLevel::Processed,
        };

        let callback = Arc::new(|event: &ProgramAccountUpdate<User>| {
            event_emitter.emit("test", Box::new(event.clone()));
        });

        event_emitter.subscribe("test", move |event| {
            if let Some(event) = event.as_any().downcast_ref::<ProgramAccountUpdate<User>>() {
                dbg!(event);
            }
        });

        let mut usermap = UserMap::new(commitment, endpoint, true, None, Some(callback));
        usermap.subscribe().await.unwrap();

        tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;

        dbg!(usermap.size());
        assert!(usermap.size() > 50000);

        dbg!(usermap.get_latest_slot());

        usermap.unsubscribe().await.unwrap();

        tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;

        assert_eq!(usermap.size(), 0);
        assert_eq!(usermap.subscribed, false);
    }
}
