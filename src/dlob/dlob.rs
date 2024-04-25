#![allow(clippy::module_inception)]

use dashmap::DashSet;
use drift::state::oracle::OraclePriceData;
use drift::state::state::{ExchangeStatus, State};
use drift::state::user::{MarketType, Order, OrderStatus};
use mut_binary_heap::BinaryHeap;
use solana_sdk::pubkey::Pubkey;
use std::any::Any;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::dlob::dlob_node::{
    create_node, get_order_signature, DLOBNode, DirectionalNode, Node, NodeToFill, NodeToTrigger,
    NodeType,
};
use crate::dlob::market::{get_node_subtype_and_type, Exchange, OpenOrders, SubType};
use crate::dlob::utils::determine_maker_and_taker;
use crate::event_emitter::Event;
use crate::market_operations::MarketOperations;
use crate::math::auction::is_fallback_available;
use crate::math::order::{get_limit_price, is_order_expired, is_resting_limit_order};
use crate::usermap::UserMap;
use crate::utils::market_type_to_string;

use super::utils::merge_and_deduplicate_nodes;

#[derive(Clone)]
pub struct DLOB {
    exchange: Exchange,
    _open_orders: OpenOrders,
    _initialized: bool,
    _max_slot_for_resting_limit_orders: Arc<AtomicU64>,
}

impl DLOB {
    pub fn new() -> DLOB {
        let exchange = Exchange::new();

        let open_orders = OpenOrders::new();
        open_orders.insert("perp".to_string(), DashSet::new());
        open_orders.insert("spot".to_string(), DashSet::new());

        DLOB {
            exchange,
            _open_orders: open_orders,
            _initialized: true,
            _max_slot_for_resting_limit_orders: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn build_from_usermap(&mut self, usermap: &UserMap, slot: u64) {
        self.clear();
        usermap.usermap.iter().for_each(|user_ref| {
            let user = user_ref.value();
            let user_key = user_ref.key();
            let user_pubkey = Pubkey::from_str(user_key).expect("Valid pubkey");
            for order in user.orders.iter() {
                if order.status == OrderStatus::Init {
                    continue;
                }
                self.insert_order(order, user_pubkey, slot);
            }
        });
        self._initialized = true;
    }

    pub fn size(&self) -> (usize, usize) {
        (self.exchange.perp_size(), self.exchange.spot_size())
    }

    /// for debugging
    pub fn print_all_spot_orders(&self) {
        for market in self.exchange.spot.iter() {
            println!("market index: {}", market.key());
            market.value().print_all_orders();
        }
    }

    pub fn clear(&mut self) {
        self.exchange.clear();
        self._open_orders.clear();
        self._initialized = false;
        self._max_slot_for_resting_limit_orders
            .store(0, Ordering::Relaxed);
    }

    pub fn insert_order(&self, order: &Order, user_account: Pubkey, slot: u64) {
        let market_type = market_type_to_string(&order.market_type);
        let market_index = order.market_index;

        let (subtype, node_type) = get_node_subtype_and_type(order, slot);
        let node = create_node(node_type, *order, user_account);

        self.exchange
            .add_market_indempotent(&market_type, market_index);

        let mut market = match order.market_type {
            MarketType::Perp => self.exchange.perp.get_mut(&market_index).expect("market"),
            MarketType::Spot => self.exchange.spot.get_mut(&market_index).expect("market"),
        };

        let order_list = market.get_order_list_for_node_insert(node_type);

        match subtype {
            SubType::Bid => order_list.insert_bid(node),
            SubType::Ask => order_list.insert_ask(node),
            _ => {}
        }
    }

    pub fn get_order(&self, order_id: u32, user_account: Pubkey) -> Option<Order> {
        let order_signature = get_order_signature(order_id, user_account);
        for order_list in self.exchange.get_order_lists() {
            if let Some(node) = order_list.get_node(&order_signature) {
                return Some(*node.get_order());
            }
        }

        None
    }

    fn update_resting_limit_orders_for_market_type(&self, slot: u64, market_type: MarketType) {
        let mut new_taking_asks: BinaryHeap<String, DirectionalNode> = BinaryHeap::new();
        let mut new_taking_bids: BinaryHeap<String, DirectionalNode> = BinaryHeap::new();

        let markets_for_market_type = match market_type {
            MarketType::Perp => &self.exchange.perp,
            MarketType::Spot => &self.exchange.spot,
        };

        // we have to clone in this loop or else the insert calls will deadlock
        for mut market_ref in markets_for_market_type.clone().iter_mut() {
            let market_index = market_ref.key().clone();
            let market = market_ref.value_mut();

            for (order_sig, directional_node) in market.taking_limit_orders.bids.iter() {
                if is_resting_limit_order(directional_node.node.get_order(), slot) {
                    market
                        .resting_limit_orders
                        .insert_bid(directional_node.node)
                } else {
                    new_taking_bids.push(order_sig.clone(), *directional_node);
                }
            }

            for (order_sig, directional_node) in market.taking_limit_orders.asks.iter() {
                if is_resting_limit_order(directional_node.node.get_order(), slot) {
                    market
                        .resting_limit_orders
                        .insert_ask(directional_node.node);
                } else {
                    new_taking_asks.push(order_sig.clone(), *directional_node);
                }
            }

            match market_type {
                MarketType::Perp => {
                    let mut market_clone = market_ref.clone();
                    market_clone.taking_limit_orders.bids = new_taking_bids.clone();
                    market_clone.taking_limit_orders.asks = new_taking_asks.clone();
                    markets_for_market_type.insert(market_index, market_clone);
                }
                MarketType::Spot => {
                    let mut market_clone = market_ref.clone();
                    market_clone.taking_limit_orders.bids = new_taking_bids.clone();
                    market_clone.taking_limit_orders.asks = new_taking_asks.clone();
                    markets_for_market_type.insert(market_index, market_clone);
                }
            }
        }
    }

    pub fn update_resting_limit_orders(&self, slot: u64) {
        if slot
            <= self
                ._max_slot_for_resting_limit_orders
                .load(Ordering::Relaxed)
        {
            return;
        }

        self._max_slot_for_resting_limit_orders
            .store(slot, Ordering::Relaxed);

        self.update_resting_limit_orders_for_market_type(slot, MarketType::Perp);
        self.update_resting_limit_orders_for_market_type(slot, MarketType::Spot);
    }

    pub fn get_best_orders(
        &self,
        market_type: MarketType,
        sub_type: SubType,
        node_type: NodeType,
        market_index: u16,
    ) -> Vec<Node> {
        let market = match market_type {
            MarketType::Perp => self.exchange.perp.get_mut(&market_index).expect("market"),
            MarketType::Spot => self.exchange.spot.get_mut(&market_index).expect("market"),
        };
        let mut order_list = market.get_order_list_for_node_type(node_type);

        let mut best_orders: Vec<Node> = vec![];

        match sub_type {
            SubType::Bid => {
                while !order_list.bids_empty() {
                    if let Some(node) = order_list.get_best_bid() {
                        best_orders.push(node);
                    }
                }
            }
            SubType::Ask => {
                while !order_list.asks_empty() {
                    if let Some(node) = order_list.get_best_ask() {
                        best_orders.push(node);
                    }
                }
            }
            _ => unimplemented!(),
        }

        best_orders
    }

    pub fn get_resting_limit_asks(
        &self,
        slot: u64,
        market_type: MarketType,
        market_index: u16,
        oracle_price_data: OraclePriceData,
    ) -> Vec<Node> {
        self.update_resting_limit_orders(slot);

        let mut resting_limit_orders = self.get_best_orders(
            market_type,
            SubType::Ask,
            NodeType::RestingLimit,
            market_index,
        );
        let mut floating_limit_orders = self.get_best_orders(
            market_type,
            SubType::Ask,
            NodeType::FloatingLimit,
            market_index,
        );

        let comparative = Box::new(
            |node_a: &Node, node_b: &Node, slot: u64, oracle_price_data: OraclePriceData| {
                node_a.get_price(oracle_price_data, slot)
                    > node_b.get_price(oracle_price_data, slot)
            },
        );

        let mut all_orders = vec![];
        all_orders.append(&mut resting_limit_orders);
        all_orders.append(&mut floating_limit_orders);

        all_orders.sort_by(|a, b| {
            if comparative(a, b, slot, oracle_price_data) {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Less
            }
        });

        all_orders
    }

    pub fn get_resting_limit_bids(
        &self,
        slot: u64,
        market_type: MarketType,
        market_index: u16,
        oracle_price_data: OraclePriceData,
    ) -> Vec<Node> {
        self.update_resting_limit_orders(slot);

        let mut resting_limit_orders = self.get_best_orders(
            market_type,
            SubType::Bid,
            NodeType::RestingLimit,
            market_index,
        );
        let mut floating_limit_orders = self.get_best_orders(
            market_type,
            SubType::Bid,
            NodeType::FloatingLimit,
            market_index,
        );

        let comparative = Box::new(
            |node_a: &Node, node_b: &Node, slot: u64, oracle_price_data: OraclePriceData| {
                node_a.get_price(oracle_price_data, slot)
                    < node_b.get_price(oracle_price_data, slot)
            },
        );

        let mut all_orders = vec![];
        all_orders.append(&mut resting_limit_orders);
        all_orders.append(&mut floating_limit_orders);

        all_orders.sort_by(|a, b| {
            if comparative(a, b, slot, oracle_price_data) {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Less
            }
        });

        all_orders
    }

    pub fn get_taking_orders(
        &mut self,
        slot: u64,
        market_type: MarketType,
        market_index: u16,
        subtype: SubType,
    ) -> Vec<Node> {
        self.update_resting_limit_orders(slot);

        let mut taking_limit_orders =
            self.get_best_orders(market_type, subtype, NodeType::TakingLimit, market_index);

        let mut market_orders =
            self.get_best_orders(market_type, subtype, NodeType::Market, market_index);

        let comparative = Box::new(|node_a: &Node, node_b: &Node| {
            node_a.get_order().slot < node_b.get_order().slot
        });

        let mut all_orders = vec![];
        all_orders.append(&mut taking_limit_orders);
        all_orders.append(&mut market_orders);

        all_orders.sort_by(|a, b| {
            if comparative(a, b) {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Less
            }
        });

        all_orders
    }

    fn find_nodes_to_trigger(
        self,
        market_index: u16,
        market_type: MarketType,
        oracle_price: u64,
        state: State,
    ) {
        if state.exchange_status != ExchangeStatus::active() {
            return;
        }

        let mut nodes_to_trigger = vec![];

        let market = match market_type {
            MarketType::Perp => self.exchange.perp.get_mut(&market_index).expect("market"),
            MarketType::Spot => self.exchange.spot.get_mut(&market_index).expect("market"),
        };

        let mut trigger_above_list = market.trigger_orders.bids.clone();
        let mut trigger_below_list = market.trigger_orders.asks.clone();

        while !trigger_above_list.is_empty() {
            let node = trigger_above_list.pop().unwrap();
            if node.node.get_order().trigger_price < oracle_price {
                nodes_to_trigger.push(NodeToTrigger { node: node.node });
            } else {
                break;
            }
        }

        while !trigger_below_list.is_empty() {
            let node = trigger_below_list.pop().unwrap();
            if node.node.get_order().trigger_price > oracle_price {
                nodes_to_trigger.push(NodeToTrigger { node: node.node });
            } else {
                break;
            }
        }
    }

    fn find_nodes_to_fill(
        &mut self,
        market: Box<&dyn MarketOperations>,
        fallback_bid: Option<u64>,
        fallback_ask: Option<u64>,
        slot: u64,
        ts: i64,
        oracle_price_data: OraclePriceData,
        state: State,
    ) -> Option<Vec<NodeToFill>> {
        if market.is_fill_paused(&state) {
            return None;
        }

        let is_amm_paused = market.is_amm_paused(&state);

        let min_auction_duration = match market.market_type() {
            MarketType::Perp => state.min_perp_auction_duration,
            MarketType::Spot => 0_u8,
        };

        let (maker_rebate_numerator, maker_rebate_denominator) =
            market.calculate_maker_rebate(&state);

        let expired_nodes_to_fill =
            self.find_expired_orders_to_fill(market.market_index(), market.market_type(), ts);

        let resting_orders_to_fill = self.find_resting_orders_to_fill(
            market.clone(),
            slot,
            oracle_price_data,
            is_amm_paused,
            min_auction_duration,
            maker_rebate_numerator,
            maker_rebate_denominator,
            fallback_bid,
            fallback_ask,
        );

        let taking_orders_to_fill = self.find_taking_orders_to_fill(
            market.clone(),
            slot,
            oracle_price_data,
            is_amm_paused,
            min_auction_duration,
            fallback_bid,
            fallback_ask,
        );

        let mut nodes_to_fill =
            merge_and_deduplicate_nodes(resting_orders_to_fill, taking_orders_to_fill);

        nodes_to_fill.extend(expired_nodes_to_fill);

        Some(nodes_to_fill)
    }

    fn find_resting_orders_to_fill(
        &mut self,
        market: Box<&dyn MarketOperations>,
        slot: u64,
        oracle_price_data: OraclePriceData,
        is_amm_paused: bool,
        min_auction_duration: u8,
        maker_rebate_numerator: u64,
        maker_rebate_denominator: u64,
        fallback_bid: Option<u64>,
        fallback_ask: Option<u64>,
    ) -> Vec<NodeToFill> {
        let mut nodes_to_fill: Vec<NodeToFill> = vec![];

        let crossing_nodes =
            self.find_crossing_resting_limit_orders(market.clone(), slot, oracle_price_data);

        nodes_to_fill.extend(crossing_nodes);

        if fallback_bid.is_some() && !is_amm_paused {
            let fallback_bid = fallback_bid.unwrap();
            let resting_limit_asks = self.get_resting_limit_asks(
                slot,
                market.market_type(),
                market.market_index(),
                oracle_price_data,
            );

            let fallback_bid_with_buffer =
                fallback_bid - (fallback_bid * maker_rebate_numerator / maker_rebate_denominator);

            let asks_crossing = self.find_nodes_crossing_fallback_liquidity(
                market.market_type(),
                slot,
                oracle_price_data,
                resting_limit_asks,
                Box::new(|price| price.unwrap() < fallback_bid_with_buffer),
                min_auction_duration,
            );

            nodes_to_fill.extend(asks_crossing);
        }

        if fallback_ask.is_some() && !is_amm_paused {
            let fallback_ask = fallback_ask.unwrap();

            let resting_limit_bids = self.get_resting_limit_bids(
                slot,
                market.market_type(),
                market.market_index(),
                oracle_price_data,
            );

            let fallback_ask_with_buffer =
                fallback_ask - (fallback_ask * maker_rebate_numerator / maker_rebate_denominator);

            let bids_crossing = self.find_nodes_crossing_fallback_liquidity(
                market.market_type(),
                slot,
                oracle_price_data,
                resting_limit_bids,
                Box::new(|price| price.unwrap() > fallback_ask_with_buffer),
                min_auction_duration,
            );

            nodes_to_fill.extend(bids_crossing);
        }

        nodes_to_fill
    }

    fn find_taking_orders_to_fill(
        &mut self,
        market: Box<&dyn MarketOperations>,
        slot: u64,
        oracle_price_data: OraclePriceData,
        is_amm_paused: bool,
        min_auction_duration: u8,
        fallback_bid: Option<u64>,
        fallback_ask: Option<u64>,
    ) -> Vec<NodeToFill> {
        let mut nodes_to_fill: Vec<NodeToFill> = vec![];

        let taking_asks = self.get_taking_orders(
            slot,
            market.market_type(),
            market.market_index(),
            SubType::Ask,
        );

        let taking_asks_crossing_bids = self.find_taking_nodes_crossing_maker_nodes(
            market.clone(),
            slot,
            oracle_price_data,
            taking_asks,
            Box::new(|slot, market_type, market_index, oracle_price_data| {
                self.get_resting_limit_bids(slot, market_type, market_index, oracle_price_data)
            }),
            Box::new(|market_type, taker_price, maker_price| {
                if market_type == MarketType::Spot {
                    if taker_price.is_none() {
                        return false;
                    }

                    if fallback_bid.is_some() && maker_price < fallback_bid.unwrap() {
                        return false;
                    }
                }
                taker_price.is_none() || taker_price.unwrap() <= maker_price
            }),
        );

        nodes_to_fill.extend(taking_asks_crossing_bids);

        let taking_bids = self.get_taking_orders(
            slot,
            market.market_type(),
            market.market_index(),
            SubType::Bid,
        );

        let taking_bids_crossing_asks = self.find_taking_nodes_crossing_maker_nodes(
            market.clone(),
            slot,
            oracle_price_data,
            taking_bids,
            Box::new(|slot, market_type, market_index, oracle_price_data| {
                self.get_resting_limit_asks(slot, market_type, market_index, oracle_price_data)
            }),
            Box::new(|market_type, taker_price, maker_price| {
                if market_type == MarketType::Spot {
                    if taker_price.is_none() {
                        return false;
                    }

                    if fallback_ask.is_some() && maker_price > fallback_ask.unwrap() {
                        return false;
                    }
                }

                taker_price.is_none() || taker_price.unwrap() >= maker_price
            }),
        );

        nodes_to_fill.extend(taking_bids_crossing_asks);

        if fallback_bid.is_some() && !is_amm_paused {
            let taking_asks = self.get_taking_orders(
                slot,
                market.market_type(),
                market.market_index(),
                SubType::Ask,
            );

            let taking_asks_crossing_fallback = self.find_nodes_crossing_fallback_liquidity(
                market.market_type(),
                slot,
                oracle_price_data,
                taking_asks,
                Box::new(|taker_price| taker_price.is_none() || taker_price <= fallback_bid),
                min_auction_duration,
            );

            nodes_to_fill.extend(taking_asks_crossing_fallback);
        }

        if fallback_ask.is_some() && !is_amm_paused {
            let taking_bids = self.get_taking_orders(
                slot,
                market.market_type(),
                market.market_index(),
                SubType::Bid,
            );

            let taking_bids_crossing_fallback = self.find_nodes_crossing_fallback_liquidity(
                market.market_type(),
                slot,
                oracle_price_data,
                taking_bids,
                Box::new(|taker_price| taker_price.is_none() || taker_price > fallback_ask),
                min_auction_duration,
            );

            nodes_to_fill.extend(taking_bids_crossing_fallback);
        }

        nodes_to_fill
    }

    fn find_expired_orders_to_fill(
        &self,
        market_index: u16,
        market_type: MarketType,
        ts: i64,
    ) -> Vec<NodeToFill> {
        let mut nodes_to_fill: Vec<NodeToFill> = vec![];

        let markets = match market_type {
            MarketType::Perp => self.exchange.perp.get_mut(&market_index).expect("market"),
            MarketType::Spot => self.exchange.spot.get_mut(&market_index).expect("market"),
        };

        let mut bids = [
            markets.taking_limit_orders.bids.clone(),
            markets.resting_limit_orders.bids.clone(),
            markets.floating_limit_orders.bids.clone(),
            markets.market_orders.bids.clone(),
        ];

        let mut asks = [
            markets.taking_limit_orders.asks.clone(),
            markets.resting_limit_orders.asks.clone(),
            markets.floating_limit_orders.asks.clone(),
            markets.market_orders.asks.clone(),
        ];

        for bid_list in bids.iter_mut() {
            while !bid_list.is_empty() {
                let bid = bid_list.pop().unwrap();
                if is_order_expired(bid.node.get_order(), ts, true) {
                    nodes_to_fill.push(NodeToFill {
                        node: bid.node,
                        maker_nodes: vec![],
                    });
                }
            }
        }

        for ask_list in asks.iter_mut() {
            while !ask_list.is_empty() {
                let ask = ask_list.pop().unwrap();
                if is_order_expired(ask.node.get_order(), ts, true) {
                    nodes_to_fill.push(NodeToFill {
                        node: ask.node,
                        maker_nodes: vec![],
                    });
                }
            }
        }

        nodes_to_fill
    }

    fn find_crossing_resting_limit_orders(
        &mut self,
        market: Box<&dyn MarketOperations>,
        slot: u64,
        oracle_price_data: OraclePriceData,
    ) -> Vec<NodeToFill> {
        let mut nodes_to_fill: Vec<NodeToFill> = vec![];

        let resting_limit_asks = self.get_resting_limit_asks(
            slot,
            market.market_type(),
            market.market_index(),
            oracle_price_data,
        );

        for ask in resting_limit_asks.iter() {
            let ask_price = ask.get_price(oracle_price_data, slot);
            let ask_order = ask.get_order();

            let resting_limit_bids = self.get_resting_limit_bids(
                slot,
                market.market_type(),
                market.market_index(),
                oracle_price_data,
            );

            for bid in resting_limit_bids.iter() {
                let bid_price = bid.get_price(oracle_price_data, slot);

                if bid_price < ask_price {
                    break;
                }

                let bid_order = bid.get_order();

                if bid.get_user_account() == ask.get_user_account() {
                    continue;
                }

                if let Some((maker, taker)) = determine_maker_and_taker(ask, bid) {
                    let bid_base_remaining =
                        bid_order.base_asset_amount - bid_order.base_asset_amount_filled;
                    let ask_base_remaining =
                        ask_order.base_asset_amount - ask_order.base_asset_amount_filled;

                    let base_filled = std::cmp::min(bid_base_remaining, ask_base_remaining);

                    let mut new_bid_order = bid_order.clone();
                    new_bid_order.base_asset_amount_filled =
                        bid_order.base_asset_amount_filled + base_filled;

                    let (subtype, node_type) = get_node_subtype_and_type(&new_bid_order, slot);
                    let mut market = match new_bid_order.market_type {
                        MarketType::Perp => self
                            .exchange
                            .perp
                            .get_mut(&new_bid_order.market_index)
                            .expect("market"),
                        MarketType::Spot => self
                            .exchange
                            .spot
                            .get_mut(&new_bid_order.market_index)
                            .expect("market"),
                    };

                    let order_list = market.get_order_list_for_node_insert(node_type);

                    order_list.update(&new_bid_order, &bid.get_user_account(), subtype);

                    let mut new_ask_order = ask_order.clone();
                    new_ask_order.base_asset_amount_filled =
                        ask_order.base_asset_amount_filled + base_filled;

                    let (subtype, node_type) = get_node_subtype_and_type(&new_ask_order, slot);
                    let mut market = match new_ask_order.market_type {
                        MarketType::Perp => self
                            .exchange
                            .perp
                            .get_mut(&new_ask_order.market_index)
                            .expect("market"),
                        MarketType::Spot => self
                            .exchange
                            .spot
                            .get_mut(&new_ask_order.market_index)
                            .expect("market"),
                    };

                    let order_list = market.get_order_list_for_node_insert(node_type);

                    order_list.update(&new_ask_order, &ask.get_user_account(), subtype);

                    nodes_to_fill.push(NodeToFill {
                        node: taker,
                        maker_nodes: vec![maker],
                    });

                    if new_ask_order.base_asset_amount == new_ask_order.base_asset_amount_filled {
                        break;
                    }
                } else {
                    continue;
                }
            }
        }

        nodes_to_fill
    }

    fn find_nodes_crossing_fallback_liquidity<'a>(
        &self,
        market_type: MarketType,
        slot: u64,
        oracle_price_data: OraclePriceData,
        nodes: Vec<Node>,
        does_cross: Box<dyn Fn(Option<u64>) -> bool + 'a>,
        min_auction_duration: u8,
    ) -> Vec<NodeToFill> {
        let mut nodes_to_fill: Vec<NodeToFill> = vec![];

        for node in nodes.iter() {
            if market_type == MarketType::Spot && node.get_order().post_only {
                continue;
            }

            let node_price = get_limit_price(node.get_order(), &oracle_price_data, slot, None);

            let crosses = does_cross(Some(node_price));

            let fallback_available = market_type == MarketType::Spot
                || is_fallback_available(node.get_order(), min_auction_duration, slot);

            if crosses && fallback_available {
                nodes_to_fill.push(NodeToFill {
                    node: node.clone(),
                    maker_nodes: vec![], // filled by fallback liquidity
                });
            }
        }

        nodes_to_fill
    }

    fn find_taking_nodes_crossing_maker_nodes<'a>(
        &self,
        market: Box<&dyn MarketOperations>,
        slot: u64,
        oracle_price_data: OraclePriceData,
        taker_nodes: Vec<Node>,
        mut maker_node_function: Box<
            dyn FnMut(u64, MarketType, u16, OraclePriceData) -> Vec<Node> + 'a,
        >,
        does_cross: Box<dyn Fn(MarketType, Option<u64>, u64) -> bool + 'a>,
    ) -> Vec<NodeToFill> {
        let mut nodes_to_fill: Vec<NodeToFill> = vec![];

        for taker in taker_nodes.iter() {
            let maker_nodes = maker_node_function(
                slot,
                market.market_type(),
                market.market_index(),
                oracle_price_data,
            );

            for maker in maker_nodes.iter() {
                if taker.get_user_account() == maker.get_user_account() {
                    continue;
                }

                let taker_price = taker.get_price(oracle_price_data, slot);
                let maker_price = maker.get_price(oracle_price_data, slot);

                let crossing = does_cross(market.market_type(), Some(taker_price), maker_price);

                if !crossing {
                    continue;
                }

                nodes_to_fill.push(NodeToFill {
                    node: taker.clone(),
                    maker_nodes: vec![maker.clone()],
                });

                let maker_order = maker.get_order();
                let taker_order = taker.get_order();

                let maker_base_remaining =
                    maker_order.base_asset_amount - maker_order.base_asset_amount_filled;
                let taker_base_remaining =
                    taker_order.base_asset_amount - taker_order.base_asset_amount_filled;

                let base_filled = std::cmp::min(maker_base_remaining, taker_base_remaining);

                let mut new_maker_order = maker_order.clone();
                new_maker_order.base_asset_amount_filled =
                    maker_order.base_asset_amount_filled + base_filled;

                let (subtype, node_type) = get_node_subtype_and_type(&new_maker_order, slot);
                let mut market = match new_maker_order.market_type {
                    MarketType::Perp => self
                        .exchange
                        .perp
                        .get_mut(&new_maker_order.market_index)
                        .expect("market"),
                    MarketType::Spot => self
                        .exchange
                        .spot
                        .get_mut(&new_maker_order.market_index)
                        .expect("market"),
                };

                let order_list = market.get_order_list_for_node_insert(node_type);

                order_list.update(&new_maker_order, &maker.get_user_account(), subtype);

                let mut new_taker_order = taker_order.clone();
                new_taker_order.base_asset_amount_filled =
                    taker_order.base_asset_amount_filled + base_filled;

                let (subtype, node_type) = get_node_subtype_and_type(&new_taker_order, slot);
                let mut market = match new_taker_order.market_type {
                    MarketType::Perp => self
                        .exchange
                        .perp
                        .get_mut(&new_taker_order.market_index)
                        .expect("market"),
                    MarketType::Spot => self
                        .exchange
                        .spot
                        .get_mut(&new_taker_order.market_index)
                        .expect("market"),
                };

                let order_list = market.get_order_list_for_node_insert(node_type);

                order_list.update(&new_taker_order, &taker.get_user_account(), subtype);

                if new_taker_order.base_asset_amount == new_taker_order.base_asset_amount_filled {
                    break;
                }
            }
        }

        nodes_to_fill
    }
}

impl Default for DLOB {
    fn default() -> Self {
        Self::new()
    }
}

impl Event for DLOB {
    fn box_clone(&self) -> Box<dyn Event> {
        Box::new((*self).clone())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drift::{
        math::constants::PRICE_PRECISION_U64,
        state::user::{Order, OrderType},
    };
    use solana_sdk::pubkey::Pubkey;

    #[test]
    fn test_dlob_insert() {
        let dlob = DLOB::new();
        let user_account = Pubkey::new_unique();
        let taking_limit_order = Order {
            order_id: 1,
            slot: 1,
            market_index: 0,
            market_type: MarketType::Perp,
            ..Order::default()
        };
        let floating_limit_order = Order {
            order_id: 2,
            oracle_price_offset: 1,
            market_index: 0,
            market_type: MarketType::Perp,
            ..Order::default()
        };
        let resting_limit_order = Order {
            order_id: 3,
            slot: 3,
            market_index: 0,
            market_type: MarketType::Perp,
            ..Order::default()
        };
        let market_order = Order {
            order_id: 4,
            slot: 4,
            market_index: 0,
            market_type: MarketType::Perp,
            ..Order::default()
        };
        let trigger_order = Order {
            order_id: 5,
            slot: 5,
            market_index: 0,
            market_type: MarketType::Perp,
            ..Order::default()
        };

        dlob.insert_order(&taking_limit_order, user_account, 1);
        dlob.insert_order(&floating_limit_order, user_account, 0);
        dlob.insert_order(&resting_limit_order, user_account, 3);
        dlob.insert_order(&market_order, user_account, 4);
        dlob.insert_order(&trigger_order, user_account, 5);

        assert!(dlob.get_order(1, user_account).is_some());
        assert!(dlob.get_order(2, user_account).is_some());
        assert!(dlob.get_order(3, user_account).is_some());
        assert!(dlob.get_order(4, user_account).is_some());
        assert!(dlob.get_order(5, user_account).is_some());
    }

    #[test]
    fn test_dlob_ordering() {
        let dlob = DLOB::new();

        let user_account = Pubkey::new_unique();
        let order_1 = Order {
            order_id: 1,
            slot: 1,
            market_index: 0,
            direction: drift::controller::position::PositionDirection::Long,
            market_type: MarketType::Perp,
            auction_duration: 1,
            ..Order::default()
        };
        let order_2 = Order {
            order_id: 2,
            slot: 2,
            market_index: 0,
            direction: drift::controller::position::PositionDirection::Long,
            market_type: MarketType::Perp,
            auction_duration: 1,
            ..Order::default()
        };
        let order_3 = Order {
            order_id: 3,
            slot: 3,
            market_index: 0,
            direction: drift::controller::position::PositionDirection::Long,
            market_type: MarketType::Perp,
            auction_duration: 1,
            ..Order::default()
        };
        let order_4 = Order {
            order_id: 4,
            slot: 4,
            market_index: 0,
            direction: drift::controller::position::PositionDirection::Long,
            market_type: MarketType::Perp,
            auction_duration: 1,
            ..Order::default()
        };
        let order_5 = Order {
            order_id: 5,
            slot: 5,
            market_index: 0,
            direction: drift::controller::position::PositionDirection::Long,
            market_type: MarketType::Perp,
            auction_duration: 1,
            ..Order::default()
        };

        dlob.insert_order(&order_1, user_account, 1);
        dlob.insert_order(&order_2, user_account, 2);
        dlob.insert_order(&order_3, user_account, 3);
        dlob.insert_order(&order_4, user_account, 4);
        dlob.insert_order(&order_5, user_account, 5);

        assert!(dlob.get_order(1, user_account).is_some());
        assert!(dlob.get_order(2, user_account).is_some());
        assert!(dlob.get_order(3, user_account).is_some());
        assert!(dlob.get_order(4, user_account).is_some());
        assert!(dlob.get_order(5, user_account).is_some());

        let best_orders =
            dlob.get_best_orders(MarketType::Perp, SubType::Bid, NodeType::TakingLimit, 0);

        assert_eq!(best_orders[0].get_order().slot, 1);
        assert_eq!(best_orders[1].get_order().slot, 2);
        assert_eq!(best_orders[2].get_order().slot, 3);
        assert_eq!(best_orders[3].get_order().slot, 4);
        assert_eq!(best_orders[4].get_order().slot, 5);
    }

    #[test]
    fn test_update_resting_limit_orders() {
        let mut dlob = DLOB::new();

        let user_account = Pubkey::new_unique();
        let order_1 = Order {
            order_id: 1,
            slot: 1,
            market_index: 0,
            direction: drift::controller::position::PositionDirection::Long,
            market_type: MarketType::Perp,
            auction_duration: 1,
            ..Order::default()
        };

        dlob.insert_order(&order_1, user_account, 1);

        let markets_for_market_type = dlob.exchange.perp.clone();
        let market = markets_for_market_type.get(&0).unwrap();

        assert_eq!(market.taking_limit_orders.bids.len(), 1);

        let slot = 5;

        drop(market);
        drop(markets_for_market_type);

        dlob.update_resting_limit_orders(slot);

        let markets_for_market_type = dlob.exchange.perp.clone();
        let market = markets_for_market_type.get(&0).unwrap();

        assert_eq!(market.taking_limit_orders.bids.len(), 0);
        assert_eq!(market.resting_limit_orders.bids.len(), 1);
    }

    #[test]
    fn test_get_resting_limit_asks() {
        let mut dlob = DLOB::new();

        let v_ask = 15;
        let v_bid = 10;

        let oracle_price_data = OraclePriceData {
            price: (v_bid + v_ask) / 2,
            confidence: 1,
            delay: 0,
            has_sufficient_number_of_data_points: true,
        };

        let user_account = Pubkey::new_unique();
        let order_1 = Order {
            order_id: 1,
            slot: 1,
            market_index: 0,
            direction: drift::controller::position::PositionDirection::Short,
            market_type: MarketType::Perp,
            order_type: OrderType::Limit,
            auction_duration: 10,
            price: 11 * PRICE_PRECISION_U64,
            ..Order::default()
        };

        let order_2 = Order {
            order_id: 2,
            slot: 11,
            market_index: 0,
            direction: drift::controller::position::PositionDirection::Short,
            market_type: MarketType::Perp,
            order_type: OrderType::Limit,
            auction_duration: 10,
            price: 12 * PRICE_PRECISION_U64,
            ..Order::default()
        };

        let order_3 = Order {
            order_id: 3,
            slot: 21,
            market_index: 0,
            direction: drift::controller::position::PositionDirection::Short,
            market_type: MarketType::Perp,
            order_type: OrderType::Limit,
            auction_duration: 10,
            price: 13 * PRICE_PRECISION_U64,
            ..Order::default()
        };

        dlob.insert_order(&order_1, user_account, 1);
        dlob.insert_order(&order_2, user_account, 11);
        dlob.insert_order(&order_3, user_account, 21);

        let mut slot = 1;

        dbg!("expecting 0");
        let resting_limit_asks =
            dlob.get_resting_limit_asks(slot, MarketType::Perp, 0, oracle_price_data);

        assert_eq!(resting_limit_asks.len(), 0);

        slot += 11;

        dbg!("expecting 1");
        let resting_limit_asks =
            dlob.get_resting_limit_asks(slot, MarketType::Perp, 0, oracle_price_data);

        assert_eq!(resting_limit_asks.len(), 1);
        assert_eq!(resting_limit_asks[0].get_order().order_id, 1);

        slot += 11;

        dbg!("expecting 2");
        let resting_limit_asks =
            dlob.get_resting_limit_asks(slot, MarketType::Perp, 0, oracle_price_data);

        assert_eq!(resting_limit_asks.len(), 2);
        assert_eq!(resting_limit_asks[0].get_order().order_id, 1);
        assert_eq!(resting_limit_asks[1].get_order().order_id, 2);

        slot += 11;

        dbg!("expecting 3");
        let resting_limit_asks =
            dlob.get_resting_limit_asks(slot, MarketType::Perp, 0, oracle_price_data);

        assert_eq!(resting_limit_asks.len(), 3);
        assert_eq!(resting_limit_asks[0].get_order().order_id, 1);
        assert_eq!(resting_limit_asks[1].get_order().order_id, 2);
        assert_eq!(resting_limit_asks[2].get_order().order_id, 3);
    }

    #[test]
    fn test_get_resting_limit_bids() {
        let mut dlob = DLOB::new();

        let v_ask = 15;
        let v_bid = 10;

        let oracle_price_data = OraclePriceData {
            price: (v_bid + v_ask) / 2,
            confidence: 1,
            delay: 0,
            has_sufficient_number_of_data_points: true,
        };

        let user_account = Pubkey::new_unique();
        let order_1 = Order {
            order_id: 1,
            slot: 1,
            market_index: 0,
            direction: drift::controller::position::PositionDirection::Long,
            market_type: MarketType::Perp,
            order_type: OrderType::Limit,
            auction_duration: 10,
            price: 11,
            ..Order::default()
        };

        let order_2 = Order {
            order_id: 2,
            slot: 11,
            market_index: 0,
            direction: drift::controller::position::PositionDirection::Long,
            market_type: MarketType::Perp,
            order_type: OrderType::Limit,
            auction_duration: 10,
            price: 12,
            ..Order::default()
        };

        let order_3 = Order {
            order_id: 3,
            slot: 21,
            market_index: 0,
            direction: drift::controller::position::PositionDirection::Long,
            market_type: MarketType::Perp,
            order_type: OrderType::Limit,
            auction_duration: 10,
            price: 13,
            ..Order::default()
        };

        dlob.insert_order(&order_1, user_account, 1);
        dlob.insert_order(&order_2, user_account, 11);
        dlob.insert_order(&order_3, user_account, 21);

        let mut slot = 1;

        dbg!("expecting 0");
        let resting_limit_bids =
            dlob.get_resting_limit_bids(slot, MarketType::Perp, 0, oracle_price_data);

        assert_eq!(resting_limit_bids.len(), 0);

        slot += 11;

        dbg!("expecting 1");
        let resting_limit_bids =
            dlob.get_resting_limit_bids(slot, MarketType::Perp, 0, oracle_price_data);

        assert_eq!(resting_limit_bids.len(), 1);
        assert_eq!(resting_limit_bids[0].get_order().order_id, 1);

        slot += 11;

        dbg!("expecting 2");
        let resting_limit_bids =
            dlob.get_resting_limit_bids(slot, MarketType::Perp, 0, oracle_price_data);

        assert_eq!(resting_limit_bids.len(), 2);
        assert_eq!(resting_limit_bids[0].get_order().order_id, 2);
        assert_eq!(resting_limit_bids[1].get_order().order_id, 1);

        slot += 11;

        dbg!("expecting 3");
        let resting_limit_bids =
            dlob.get_resting_limit_bids(slot, MarketType::Perp, 0, oracle_price_data);

        assert_eq!(resting_limit_bids.len(), 3);
        assert_eq!(resting_limit_bids[0].get_order().order_id, 3);
        assert_eq!(resting_limit_bids[1].get_order().order_id, 2);
        assert_eq!(resting_limit_bids[2].get_order().order_id, 1);
    }
}
