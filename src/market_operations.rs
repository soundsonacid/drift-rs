use drift::state::{
    paused_operations::{PerpOperation, SpotOperation},
    perp_market::PerpMarket,
    spot_market::SpotMarket,
    state::{ExchangeStatus, State},
    user::MarketType,
};

pub trait MarketOperations {
    fn market_type(&self) -> MarketType;
    fn market_index(&self) -> u16;
    fn calculate_maker_rebate(&self, state: &State) -> (u64, u64);
    fn is_amm_paused(&self, state: &State) -> bool;
    fn is_fill_paused(&self, state: &State) -> bool;
}

impl MarketOperations for PerpMarket {
    fn market_type(&self) -> MarketType {
        MarketType::Perp
    }

    fn market_index(&self) -> u16 {
        self.market_index
    }

    fn calculate_maker_rebate(&self, state: &State) -> (u64, u64) {
        let mut maker_rebate_numerator =
            state.perp_fee_structure.fee_tiers[0].maker_rebate_numerator as i64;
        let maker_rebate_denominator =
            state.perp_fee_structure.fee_tiers[0].maker_rebate_denominator as i64;

        let fee_adjustment = self.fee_adjustment;
        if fee_adjustment != 0 {
            maker_rebate_numerator +=
                (maker_rebate_numerator as i64 * fee_adjustment as i64) / 100_i64;
        }

        (
            maker_rebate_numerator as u64,
            maker_rebate_denominator as u64,
        )
    }

    fn is_amm_paused(&self, state: &State) -> bool {
        if state.exchange_status & (ExchangeStatus::AmmPaused as u8)
            == (ExchangeStatus::AmmPaused as u8)
        {
            return true;
        }

        let is_op_paused = self.is_operation_paused(PerpOperation::AmmFill);
        let is_drawdown_pause = self.has_too_much_drawdown().expect("ok");

        is_op_paused || is_drawdown_pause
    }

    fn is_fill_paused(&self, state: &State) -> bool {
        if state.exchange_status & (ExchangeStatus::FillPaused as u8)
            == (ExchangeStatus::FillPaused as u8)
        {
            return true;
        }

        let is_op_paused = self.is_operation_paused(PerpOperation::Fill);

        is_op_paused
    }
}

impl MarketOperations for SpotMarket {
    fn market_type(&self) -> MarketType {
        MarketType::Spot
    }

    fn market_index(&self) -> u16 {
        self.market_index
    }

    fn calculate_maker_rebate(&self, state: &State) -> (u64, u64) {
        let maker_rebate_numerator = state.spot_fee_structure.fee_tiers[0].maker_rebate_numerator;
        let maker_rebate_denominator =
            state.spot_fee_structure.fee_tiers[0].maker_rebate_denominator;

        (
            maker_rebate_numerator as u64,
            maker_rebate_denominator as u64,
        )
    }

    fn is_amm_paused(&self, state: &State) -> bool {
        state.exchange_status & (ExchangeStatus::AmmPaused as u8)
            == (ExchangeStatus::AmmPaused as u8)
    }

    fn is_fill_paused(&self, state: &State) -> bool {
        if state.exchange_status & (ExchangeStatus::FillPaused as u8)
            == (ExchangeStatus::FillPaused as u8)
        {
            return true;
        }

        let is_op_paused = self.is_operation_paused(SpotOperation::Fill);

        is_op_paused
    }
}
