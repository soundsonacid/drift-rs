use std::cmp::min;

use drift::{
    controller::position::PositionDirection,
    state::user::{Order, OrderType},
};

use crate::is_one_of_variant;

pub fn is_auction_complete(order: &Order, slot: u64) -> bool {
    if order.auction_duration == 0 {
        return true;
    }

    (order.slot + order.auction_duration as u64) < slot
}

#[track_caller]
pub fn get_auction_price(order: &Order, slot: u64, price: i64) -> i128 {
    if is_one_of_variant(
        &order.order_type,
        &[
            OrderType::Market,
            OrderType::TriggerMarket,
            OrderType::Limit,
            OrderType::TriggerLimit,
        ],
    ) {
        get_auction_price_for_fixed_auction(order, slot)
    } else if order.order_type == OrderType::Oracle {
        get_auction_price_for_oracle_offset_auction(order, slot, price)
    } else {
        panic!("Invalid order type")
    }
}

fn get_auction_price_for_fixed_auction(order: &Order, slot: u64) -> i128 {
    let slots_elapsed = slot as i64 - order.slot as i64;

    let auction_start_price = order.auction_start_price as i128;
    let auction_end_price = order.auction_end_price as i128;
    let delta_denominator: i128 = order.auction_duration.into();
    let delta_numerator: i128 = min(slots_elapsed, order.auction_duration as i64).into();

    if delta_denominator == 0 {
        return auction_start_price;
    }

    match order.direction {
        PositionDirection::Long => {
            let price_delta =
                auction_end_price - auction_start_price * delta_numerator / delta_denominator;
            auction_start_price + price_delta
        }
        PositionDirection::Short => {
            let price_delta =
                auction_start_price - auction_end_price * delta_numerator / delta_denominator;
            auction_start_price - price_delta
        }
    }
}

fn get_auction_price_for_oracle_offset_auction(
    order: &Order,
    slot: u64,
    oracle_price: i64,
) -> i128 {
    let slots_elapsed = slot as i64 - order.slot as i64;

    let auction_start_price = order.auction_start_price as i128;
    let auction_end_price = order.auction_end_price as i128;
    let delta_denominator: i128 = order.auction_duration.into();
    let delta_numerator: i128 = min(slots_elapsed, order.auction_duration as i64).into();

    if delta_denominator == 0 {
        return auction_start_price;
    }

    let price_offset = match order.direction {
        PositionDirection::Long => {
            let price_delta =
                auction_end_price - auction_start_price * delta_numerator / delta_denominator;
            auction_start_price + price_delta
        }
        PositionDirection::Short => {
            let price_delta =
                auction_start_price - auction_end_price * delta_numerator / delta_denominator;
            auction_start_price - price_delta
        }
    };

    oracle_price as i128 + price_offset
}

pub fn is_fallback_available(order: &Order, min_auction_duration: u8, slot: u64) -> bool {
    if min_auction_duration == 0 {
        return true;
    }

    (order.slot + min_auction_duration as u64) < slot
}
