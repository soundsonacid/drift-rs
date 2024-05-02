use drift::state::user::User;
use solana_sdk::commitment_config::CommitmentConfig;

use crate::{
    event_emitter::EventEmitter, memcmp::get_user_with_order_filter, usermap::UserMap,
    websocket_program_account_subscriber::ProgramAccountUpdate, SdkResult,
};

pub struct OrderSubscriberConfig {
    pub commitment: CommitmentConfig,
    pub url: String,
}

pub struct OrderSubscriber {
    pub subscriber: UserMap,
    pub event_emitter: &'static EventEmitter,
}

impl OrderSubscriber {
    pub const SUBSCRIPTION_ID: &'static str = "order";

    pub fn new(config: OrderSubscriberConfig) -> Self {
        let event_emitter = Box::leak(Box::new(EventEmitter::new()));

        let callback = std::sync::Arc::new(|event: &ProgramAccountUpdate<User>| {
            event_emitter.emit(Self::SUBSCRIPTION_ID, Box::new(event.clone()));
        });

        let subscriber = UserMap::new(
            config.commitment,
            config.url.clone(),
            true,
            Some(vec![get_user_with_order_filter()]),
            Some(callback),
        );

        OrderSubscriber {
            subscriber,
            event_emitter,
        }
    }

    pub async fn subscribe(&mut self) -> SdkResult<()> {
        if self.subscriber.subscribed {
            return Ok(());
        }

        self.subscriber.subscribe().await?;

        Ok(())
    }

    pub async fn unsubscribe(&mut self) -> SdkResult<()> {
        self.subscriber.unsubscribe().await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::websocket_program_account_subscriber::ProgramAccountUpdate;
    use env_logger;

    #[cfg(feature = "rpc_tests")]
    #[tokio::test]
    async fn test_order_subscriber() {
        env_logger::init();
        let endpoint = "rpc".to_string();

        let config = OrderSubscriberConfig {
            commitment: CommitmentConfig::confirmed(),
            url: endpoint,
        };

        let mut order_subscriber = OrderSubscriber::new(config);

        let emitter = order_subscriber.event_emitter;

        emitter.subscribe(OrderSubscriber::SUBSCRIPTION_ID, move |event| {
            let mut count: i32 = 0;
            if let Some(event) = event.as_any().downcast_ref::<ProgramAccountUpdate<User>>() {
                count += 1;
                dbg!(count);
            }
        });

        order_subscriber.subscribe().await.unwrap();

        tokio::time::sleep(tokio::time::Duration::from_secs(120)).await;
        dbg!(order_subscriber.subscriber.size());

        let _ = order_subscriber.unsubscribe().await;

        tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
    }
}
