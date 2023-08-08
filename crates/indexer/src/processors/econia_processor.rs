use crate::{
    database::{clean_data_for_db, execute_with_better_error, get_chunks, PgDbPool},
    indexer::{
        errors::TransactionProcessingError, processing_result::ProcessingResult,
        transaction_processor::TransactionProcessor,
    },
    models::{
        coin_models::{
            coin_activities::CoinActivity,
            coin_infos::{CoinInfo, CoinInfoQuery},
        },
        events::EventModel,
        token_models::token_utils::TypeInfo,
        transactions::{TransactionDetail, TransactionModel},
    },
};
use anyhow::Context;
use aptos_api_types::Transaction;
use aptos_types::APTOS_COIN_TYPE;
use async_trait::async_trait;
use bigdecimal::{BigDecimal, ToPrimitive};
use chrono::{DateTime, Utc};
use crossbeam::channel;
use dashmap::DashMap;
use diesel::{result::Error, PgConnection};
use econia_db::{
    create_coin,
    error::DbError,
    models::{self, coin::Coin, market::MarketEventType, ToInsertable},
    add_market_registration_event,
};
use econia_types::{
    book::{OrderBook, PriceLevelWithId},
    message::Update,
    order::{Order, Side},
};
use field_count::FieldCount;
use once_cell::sync::Lazy;
use redis::Commands;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::HashMap, ops::Not};

pub const NAME: &str = "econia_processor";

const BOOK_PREFIX: &str = "book";
const ORDER_PREFIX: &str = "orders";
const FILL_PREFIX: &str = "fills";
const MARKETS_CHANNEL: &str = "markets";

static REDIS_URL: Lazy<String> =
    Lazy::new(|| std::env::var("REDIS_URL").expect("REDIS_URL not set"));

static ECONIA_ADDRESS: Lazy<String> =
    Lazy::new(|| std::env::var("ECONIA_ADDRESS").expect("ECONIA_ADDRESS not set"));

static EVENT_TYPES: Lazy<Vec<String>> = Lazy::new(|| {
    vec![
        format!("{}::market::TakerEvent", &*ECONIA_ADDRESS),
        format!("{}::market::MakerEvent", &*ECONIA_ADDRESS),
        format!("{}::registry::MarketRegistrationEvent", &*ECONIA_ADDRESS),
        format!("{}::registry::RecognizedMarketEvent", &*ECONIA_ADDRESS),
    ]
});

#[derive(Debug, Deserialize, Clone)]
struct MarketRegistrationEvent {
    market_id: String,
    base_type: TypeInfo,
    base_name_generic: String,
    quote_type: TypeInfo,
    lot_size: String,
    tick_size: String,
    min_size: String,
    underwriter_id: String,
    time: String,
}

// TODO remove the unwraps and use TryFrom instead of From.
impl From<MarketRegistrationEvent> for models::market::MarketRegistrationEvent {
    fn from(e: MarketRegistrationEvent) -> Self {
        Self {
            market_id: e.market_id.parse().unwrap(),
            time: e.time.parse().unwrap(),
            base_account_address: e
                .base_type
                .account_address
                .is_empty()
                .not()
                .then_some(e.base_type.account_address),
            base_module_name: e
                .base_type
                .module_name
                .is_empty()
                .not()
                .then_some(e.base_type.module_name),
            base_struct_name: e
                .base_type
                .struct_name
                .is_empty()
                .not()
                .then_some(e.base_type.struct_name),
            base_name_generic: e
                .base_name_generic
                .is_empty()
                .not()
                .then_some(e.base_name_generic),
            quote_account_address: e.quote_type.account_address,
            quote_module_name: e.quote_type.module_name,
            quote_struct_name: e.quote_type.struct_name,
            lot_size: e.lot_size.parse().unwrap(),
            tick_size: e.tick_size.parse().unwrap(),
            min_size: e.min_size.parse().unwrap(),
            underwriter_id: e.underwriter_id.parse().unwrap(),
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
struct TradingPair {
    base_type: TypeInfo,
    base_name_generic: String,
    quote_type: TypeInfo,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
struct RecognizedMarketInfo {
    market_id: u64,
    lot_size: u64,
    tick_size: u64,
    min_size: u64,
    underwriter_id: u64,
}

#[derive(Debug, Deserialize, Clone)]
struct RecognizedMarketEvent {
    trading_pair: TradingPair,
    recognized_market_info: Option<RecognizedMarketInfo>,
    time: DateTime<Utc>,
}

// #[derive(Debug, Deserialize, Clone)]
// struct MakerEvent {
//     custodian_id: String,
//     market_id: String,
//     market_order_id: String,
//     price: String,
//     side: bool,
//     size: String,
//     r#type: u8,
//     user: String,
//     time: String,
// }
//
// impl From<MakerEvent> for models::events::MakerEvent {
//     fn from(e: MakerEvent) -> Self {
//         Self {
//             custodian_id: e
//                 .custodian_id
//                 .is_empty()
//                 .not()
//                 .then_some(e.custodian_id.parse().unwrap()),
//             market_id: e.market_id.parse().unwrap(),
//             market_order_id: e.market_order_id.parse().unwrap(),
//             price: e.price.parse().unwrap(),
//             side: e.side.into(),
//             size: e.size.parse().unwrap(),
//             event_type: e.r#type.try_into().unwrap(),
//             user_address: e.user,
//             time: e.time.parse().unwrap(),
//         }
//     }
// }
//
// #[derive(Debug, Deserialize, Clone)]
// struct TakerEvent {
//     market_id: String,
//     side: bool,
//     market_order_id: String,
//     maker: String,
//     custodian_id: String,
//     size: String,
//     price: String,
//     time: String,
// }
//
// impl From<TakerEvent> for models::events::TakerEvent {
//     fn from(e: TakerEvent) -> Self {
//         Self {
//             custodian_id: e
//                 .custodian_id
//                 .is_empty()
//                 .not()
//                 .then_some(e.custodian_id.parse().unwrap()),
//             market_id: e.market_id.parse().unwrap(),
//             market_order_id: e.market_order_id.parse().unwrap(),
//             maker: e.maker.parse().unwrap(),
//             price: e.price.parse().unwrap(),
//             side: e.side.into(),
//             size: e.size.parse().unwrap(),
//             time: e.time.parse().unwrap(),
//         }
//     }
// }

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
enum EventWrapper {
    // Maker(MakerEvent),
    // Taker(TakerEvent),
    MarketRegistration(MarketRegistrationEvent),
    RecognizedMarket(RecognizedMarketEvent),
}

fn is_event_type_valid(e: &EventModel) -> bool {
    EVENT_TYPES.iter().any(|t| t == &e.type_)
}

fn fetch_all_markets(
    conn: &mut PgConnection,
) -> Result<Vec<models::market::MarketRegistrationEvent>, Error> {
    use diesel::prelude::*;
    use econia_db::schema::market_registration_events::dsl::*;
    market_registration_events.load::<models::market::MarketRegistrationEvent>(conn)
}

type BaseQuoteKey = (String, String, String, String, String, String);

fn create_base_quote_key(m: &models::market::MarketRegistrationEvent) -> BaseQuoteKey {
    (
        m.base_account_address.clone().unwrap_or_default(),
        m.base_module_name.clone().unwrap_or_default(),
        m.base_struct_name.clone().unwrap_or_default(),
        m.quote_account_address.clone(),
        m.quote_module_name.clone(),
        m.quote_struct_name.clone(),
    )
}

#[derive(Debug, Clone)]
enum MarketAction {
    Add(BigDecimal),
    Remove(BigDecimal),
}

struct EconiaRedisCacher {
    redis_client: redis::Client,
    // mkt_id => OrderBook
    books: HashMap<u64, OrderBook>,
    market_rx: channel::Receiver<MarketAction>,
    event_rx: channel::Receiver<EventWrapper>,
}

impl EconiaRedisCacher {
    fn new(
        redis_url: &str,
        market_rx: channel::Receiver<MarketAction>,
        event_rx: channel::Receiver<EventWrapper>,
    ) -> Self {
        let redis_client = redis::Client::open(redis_url).expect("failed to connect to redis");
        Self {
            redis_client,
            books: HashMap::new(),
            market_rx,
            event_rx,
        }
    }

    fn initialise_market(
        &mut self,
        conn: &mut redis::Connection,
        mkt_id: BigDecimal,
    ) -> anyhow::Result<()> {
        conn.hset(MARKETS_CHANNEL, mkt_id.to_string(), 1)?;
        let mkt_id = mkt_id.to_u64().expect("failed to convert to u64");

        if self.books.get(&mkt_id).is_none() {
            self.books.insert(mkt_id, OrderBook::new());
        }

        Ok(())
    }

    fn remove_market(
        &mut self,
        conn: &mut redis::Connection,
        mkt_id: &BigDecimal,
    ) -> anyhow::Result<()> {
        conn.hdel(MARKETS_CHANNEL, mkt_id.to_string())?;
        let mkt_id = mkt_id.to_u64().expect("failed to convert to u64");
        self.books.remove(&mkt_id);
        Ok(())
    }

    fn initialise_markets(&mut self, books: Vec<BigDecimal>) -> anyhow::Result<()> {
        let mut conn = self
            .redis_client
            .get_connection()
            .context("failed to connect to redis")?;
        for mkt_id in books.into_iter() {
            self.initialise_market(&mut conn, mkt_id)?;
        }
        Ok(())
    }

    fn send_order_update(&self, conn: &mut redis::Connection, order: &Order) -> anyhow::Result<()> {
        let channel_name = format!("{}:{}", ORDER_PREFIX, order.market_id);
        let update = Update::Orders(order.clone());
        let message = serde_json::to_string(&update)?;
        conn.publish(channel_name, message)?;
        Ok(())
    }

    // fn send_fill(&self, conn: &mut redis::Connection, fill: &Fill) -> anyhow::Result<()> {
    //     let channel_name = format!("{}:{}", FILL_PREFIX, fill.market_id);
    //     let update = Update::Fills(fill.clone());
    //     let message = serde_json::to_string(&update)?;
    //     conn.publish(channel_name, message)?;
    //     Ok(())
    // }

    fn send_price_level_update(
        &self,
        conn: &mut redis::Connection,
        mkt_id: u64,
        side: Side,
        price: u64,
        time: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        let book = self.books.get(&mkt_id).expect("book has gone missing?");
        let cum_size = book
            .get_side(side)
            .get(&price)
            .map_or(0, |v| v.iter().fold(0, |i, s: &Order| i + s.size));
        let channel_name = format!("{}:{}", BOOK_PREFIX, mkt_id);
        let update = Update::PriceLevels(PriceLevelWithId {
            market_id: mkt_id,
            side,
            price,
            size: cum_size,
            timestamp: time,
        });
        let message = serde_json::to_string(&update)?;
        conn.publish(channel_name, message)?;
        Ok(())
    }

    // fn handle_maker_event(
    //     &mut self,
    //     conn: &mut redis::Connection,
    //     e: MakerEvent,
    // ) -> anyhow::Result<()> {
    //     let e: models::events::MakerEvent = e.into();
    //     let e: econia_types::events::MakerEvent = e.try_into()?;
    //
    //     if !self.books.contains_key(&e.market_id) {
    //         panic!("invalid state, market is missing")
    //     };
    //
    //     let removed_order = match MakerEventType::try_from(e.event_type)? {
    //         MakerEventType::Cancel => {
    //             let book = self.books.get_mut(&e.market_id).unwrap();
    //             let mut order = book
    //                 .remove_order(e.market_order_id)
    //                 .expect("order not found");
    //             order.order_state = OrderState::Cancelled;
    //             Some(order)
    //         },
    //         MakerEventType::Change => {
    //             let pop_and_reinsert = {
    //                 let book = self.books.get(&e.market_id).unwrap();
    //                 let order = book.get_order(e.market_order_id).expect("order not found");
    //                 (order.price != e.price) || (e.size > order.size)
    //             };
    //             if pop_and_reinsert {
    //                 let book = self.books.get_mut(&e.market_id).unwrap();
    //                 let mut order = book
    //                     .remove_order(e.market_order_id)
    //                     .expect("order not found");
    //                 let old_price = order.price;
    //                 order.size = e.size;
    //                 order.price = e.price;
    //                 book.add_order(order);
    //                 self.send_price_level_update(conn, e.market_id, e.side, old_price, e.time)?;
    //             } else {
    //                 let book = self.books.get_mut(&e.market_id).unwrap();
    //                 let order = book
    //                     .get_order_mut(e.market_order_id)
    //                     .expect("order not found");
    //                 order.size = e.size;
    //                 order.price = e.price;
    //             }
    //
    //             None
    //         },
    //         MakerEventType::Evict => {
    //             let book = self.books.get_mut(&e.market_id).unwrap();
    //             let mut order = book
    //                 .remove_order(e.market_order_id)
    //                 .expect("order not found");
    //             order.order_state = OrderState::Evicted;
    //             Some(order)
    //         },
    //         MakerEventType::Place => {
    //             let o = Order {
    //                 market_order_id: e.market_order_id,
    //                 market_id: e.market_id,
    //                 side: e.side,
    //                 size: e.size,
    //                 price: e.price,
    //                 user_address: e.user_address,
    //                 custodian_id: e.custodian_id,
    //                 order_state: OrderState::Open,
    //                 created_at: e.time,
    //             };
    //
    //             let book = self.books.get_mut(&e.market_id).unwrap();
    //             book.add_order(o);
    //             None
    //         },
    //     };
    //
    //     let book = self.books.get(&e.market_id).unwrap();
    //     let order = removed_order
    //         .as_ref()
    //         .unwrap_or_else(|| book.get_order(e.market_order_id).expect("order not found"));
    //     self.send_order_update(conn, order)?;
    //     self.send_price_level_update(conn, e.market_id, order.side, order.price, e.time)
    // }
    //
    // fn handle_taker_event(
    //     &mut self,
    //     conn: &mut redis::Connection,
    //     e: TakerEvent,
    // ) -> anyhow::Result<()> {
    //     let e: models::events::TakerEvent = e.into();
    //     let e: econia_types::events::TakerEvent = e.try_into()?;
    //
    //     if !self.books.contains_key(&e.market_id) {
    //         panic!("invalid state, market is missing")
    //     };
    //
    //     let book = self.books.get_mut(&e.market_id).unwrap();
    //     let order = book
    //         .get_order_mut(e.market_order_id)
    //         .expect("order not found");
    //
    //     order.size = order.size.checked_sub(e.size).unwrap_or_default();
    //
    //     let fill = Fill {
    //         market_id: e.market_id,
    //         maker_order_id: e.market_order_id,
    //         maker: e.maker.clone(),
    //         maker_side: order.side,
    //         custodian_id: order.custodian_id,
    //         size: e.size,
    //         price: e.price,
    //         time: e.time,
    //     };
    //
    //     if order.size == 0 {
    //         order.order_state = OrderState::Filled;
    //         let order = book
    //             .remove_order(e.market_order_id)
    //             .expect("order not found");
    //         self.send_order_update(conn, &order)?;
    //     } else {
    //         let book = self.books.get(&e.market_id).unwrap();
    //         let order = book.get_order(e.market_order_id).expect("order not found");
    //         self.send_order_update(conn, order)?;
    //     }
    //
    //     self.send_fill(conn, &fill)?;
    //     self.send_price_level_update(conn, e.market_id, e.side, e.price, e.time)
    // }

    fn start(&mut self, books: Vec<BigDecimal>) {
        // initialise markets
        self.initialise_markets(books)
            .expect("failed to initialise markets");

        let mut conn = self
            .redis_client
            .get_connection()
            .expect("failed to connect to redis");

        loop {
            channel::select! {
                recv(self.market_rx) -> mkt => match mkt.unwrap() {
                    MarketAction::Add(m) => self.initialise_market(&mut conn, m).expect("failed to initialise market"),
                    MarketAction::Remove(m) => self.remove_market(&mut conn, &m).expect("failed to remove market"),
                },
                recv(self.event_rx) -> event => match event.unwrap() {
                    // EventWrapper::Maker(e) => self.handle_maker_event(&mut conn, e).expect("failed to handle maker event"),
                    // EventWrapper::Taker(e) => self.handle_taker_event(&mut conn, e).expect("failed to handle taker event"),
                    _ => panic!("received incorrect event in redis handler")
                }
            };
        }
    }
}

pub struct EconiaTransactionProcessor {
    connection_pool: PgDbPool,
    markets: DashMap<BigDecimal, models::market::MarketRegistrationEvent>,
    base_quote_to_market_id: DashMap<BaseQuoteKey, BigDecimal>,
    market_tx: channel::Sender<MarketAction>,
    event_tx: channel::Sender<EventWrapper>,
}

impl EconiaTransactionProcessor {
    pub fn new(connection_pool: PgDbPool) -> Self {
        let mut conn = connection_pool
            .get()
            .expect("failed connecting to db on startup");
        let mkts = fetch_all_markets(&mut conn).expect("failed loading markets on startup");
        let markets = DashMap::new();
        let base_quote_to_market_id = DashMap::new();
        for m in mkts.into_iter() {
            let key = create_base_quote_key(&m);
            base_quote_to_market_id.insert(key, m.market_id.clone());
            markets.insert(m.market_id.clone(), m);
        }

        let (event_tx, event_rx) = channel::unbounded();
        let (market_tx, market_rx) = channel::unbounded();

        // start redis task
        let books = markets
            .iter()
            .map(|m| m.key().clone())
            .collect::<Vec<BigDecimal>>();
        std::thread::spawn(move || {
            let mut cacher = EconiaRedisCacher::new(&REDIS_URL, market_rx, event_rx);
            cacher.start(books);
        });

        Self {
            connection_pool,
            markets,
            base_quote_to_market_id,
            market_tx,
            event_tx,
        }
    }

    fn insert_markets_in_cache(&self, ev: &[MarketRegistrationEvent]) {
        for e in ev.iter().cloned() {
            let m = models::market::MarketRegistrationEvent::from(e);
            let key = create_base_quote_key(&m);
            self.market_tx
                .send(MarketAction::Add(m.market_id.clone()))
                .expect("market add tx failed");
            self.base_quote_to_market_id
                .insert(key, m.market_id.clone());
            self.markets.insert(m.market_id.clone(), m);
        }
    }

    fn remove_market_from_cache(&self, market_id: &BigDecimal) {
        self.market_tx
            .send(MarketAction::Remove(market_id.clone()))
            .expect("market removal tx failed");
        self.markets.remove(market_id);
    }

    fn insert_to_db(
        &self,
        start_version: u64,
        end_version: u64,
        coins: Vec<Coin>,
        events: Vec<EventModel>,
        block_to_time: HashMap<i64, chrono::NaiveDateTime>,
    ) -> Result<ProcessingResult, Error> {
        aptos_logger::trace!(
            name = NAME,
            start_version = start_version,
            end_version = end_version,
            "Inserting to db",
        );
        if self.insert_data(&coins, &events, &block_to_time).is_err() {
            let events = clean_data_for_db(events, true);
            self.insert_data(&coins, &events, &block_to_time)?;
        }
        Ok(ProcessingResult::new(NAME, start_version, end_version))
    }

    fn insert_data(
        &self,
        coins: &[Coin],
        events: &[EventModel],
        block_to_time: &HashMap<i64, chrono::NaiveDateTime>,
    ) -> Result<(), Error> {
        let mut conn = self.get_conn();
        conn.build_transaction()
            .read_write()
            .run::<_, Error, _>(|pg_conn| {
                self.insert_coins(pg_conn, coins)?;
                self.insert_events(pg_conn, events, block_to_time)?;
                Ok(())
            })?;
        Ok(())
    }

    fn insert_coins(&self, conn: &mut PgConnection, coins: &[Coin]) -> Result<(), Error> {
        for c in coins.iter() {
            let insertable = c.to_insertable();
            let _ = create_coin(conn, &insertable);
        }
        Ok(())
    }

    fn insert_events(
        &self,
        conn: &mut PgConnection,
        events: &[EventModel],
        block_to_time: &HashMap<i64, chrono::NaiveDateTime>,
    ) -> Result<(), Error> {
        // let mut maker = vec![];
        // let mut taker = vec![];
        let mut market_registration = vec![];
        let mut recognized_market = vec![];

        for event in events.iter() {
            let current_time = block_to_time
                .get(&event.transaction_block_height)
                .expect("block height not found in block_to_time map");

            let utc_time = chrono::TimeZone::from_utc_datetime(&Utc, current_time);

            let mut event_wrapper = serde_json::from_value::<Value>(event.data.clone())
                .map_err(|e| Error::DeserializationError(Box::new(e)))?;
            let obj_map = event_wrapper.as_object_mut().unwrap();
            obj_map.insert("time".to_string(), json!(utc_time));
            let event_wrapper = serde_json::from_value::<EventWrapper>(event_wrapper)
                .map_err(|e| Error::DeserializationError(Box::new(e)))?;

            match event_wrapper {
                // EventWrapper::Maker(e) => {
                //     self.event_tx
                //         .send(EventWrapper::Maker(e.clone()))
                //         .expect("maker event tx failed");
                //     maker.push(e);
                // },
                // EventWrapper::Taker(e) => {
                //     self.event_tx
                //         .send(EventWrapper::Taker(e.clone()))
                //         .expect("taker event tx failed");
                //     taker.push(e);
                // },
                EventWrapper::MarketRegistration(e) => market_registration.push(e),
                EventWrapper::RecognizedMarket(e) => recognized_market.push(e),
            }
        }

        // update markets cache
        self.insert_markets_in_cache(&market_registration);

        // insert events
        self.insert_event_types::<_, models::market::MarketRegistrationEvent, _>(
            conn,
            market_registration,
            add_market_registration_event,
        )?;
        self.insert_recognized_market_events(conn, recognized_market)?;
        // self.insert_event_types::<_, models::events::MakerEvent, _>(conn, maker, add_maker_event)?;
        // self.insert_event_types::<_, models::events::TakerEvent, _>(conn, taker, add_taker_event)?;

        Ok(())
    }

    fn insert_event_types<E, M, F>(
        &self,
        conn: &mut PgConnection,
        ev: Vec<E>,
        insert_func: F,
    ) -> Result<(), Error>
    where
        M: From<E> + ToInsertable,
        F: Fn(&mut PgConnection, &<M as ToInsertable>::Insertable<'_>) -> Result<M, DbError>,
    {
        for e in ev.into_iter() {
            let model = M::from(e);
            let insertable = model.to_insertable();
            // TODO remove unwrap here
            insert_func(conn, &insertable).unwrap();
        }
        Ok(())
    }

    fn convert_recognized_market_events_to_db(
        &self,
        ev: Vec<RecognizedMarketEvent>,
    ) -> Result<Vec<models::market::RecognizedMarketEvent>, Error> {
        let mut events = vec![];
        for e in ev.into_iter() {
            if let Some(r) = e.recognized_market_info {
                let mkt = self
                    .markets
                    .get(&r.market_id.into())
                    .ok_or(Error::NotFound)?;
                let new_lot_size = BigDecimal::from(r.lot_size);
                let new_tick_size = BigDecimal::from(r.tick_size);
                let new_min_size = BigDecimal::from(r.min_size);
                events.push(models::market::RecognizedMarketEvent {
                    market_id: mkt.market_id.clone(),
                    time: e.time,
                    event_type: if mkt.lot_size == new_lot_size
                        && mkt.tick_size == new_tick_size
                        && mkt.min_size == new_min_size
                    {
                        MarketEventType::Add
                    } else {
                        MarketEventType::Update
                    },
                    lot_size: Some(new_lot_size),
                    tick_size: Some(new_tick_size),
                    min_size: Some(new_min_size),
                })
            } else {
                let key: BaseQuoteKey = (
                    e.trading_pair.base_type.account_address,
                    e.trading_pair.base_type.module_name,
                    e.trading_pair.base_type.struct_name,
                    e.trading_pair.quote_type.account_address,
                    e.trading_pair.quote_type.module_name,
                    e.trading_pair.quote_type.struct_name,
                );
                let mkt_id = self.base_quote_to_market_id.get(&key).unwrap();
                events.push(models::market::RecognizedMarketEvent {
                    market_id: mkt_id.clone(),
                    time: e.time,
                    event_type: MarketEventType::Remove,
                    lot_size: None,
                    tick_size: None,
                    min_size: None,
                });

                // update markets cache
                self.remove_market_from_cache(&mkt_id);
            }
        }
        Ok(events)
    }

    fn insert_recognized_market_events(
        &self,
        conn: &mut PgConnection,
        ev: Vec<RecognizedMarketEvent>,
    ) -> Result<(), Error> {
        let ev = self.convert_recognized_market_events_to_db(ev)?;
        let insertable = ev.iter().map(|e| e.to_insertable()).collect::<Vec<_>>();
        let chunks = get_chunks(
            ev.len(),
            models::market::NewRecognizedMarketEvent::field_count(),
        );
        let table = econia_db::schema::recognized_market_events::table;
        for (start_ind, end_ind) in chunks {
            execute_with_better_error(
                conn,
                diesel::insert_into(table)
                    .values(&insertable[start_ind..end_ind])
                    .on_conflict_do_nothing(),
                None,
            )?;
        }
        Ok(())
    }
}

impl std::fmt::Debug for EconiaTransactionProcessor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = &self.connection_pool.state();
        write!(
            f,
            "EconiaTransactionProcessor {{ connections: {:?}  idle_connections: {:?} }}",
            state.connections, state.idle_connections
        )
    }
}

fn get_next_block_time<'a>(
    details_iter: &mut impl Iterator<Item = &'a TransactionDetail>,
) -> (i64, chrono::NaiveDateTime) {
    if let Some(d) = details_iter.next() {
        match d {
            crate::models::transactions::TransactionDetail::User(t, _) => {
                (t.block_height, t.timestamp)
            },
            crate::models::transactions::TransactionDetail::BlockMetadata(t) => {
                (t.block_height, t.timestamp)
            },
        }
    } else {
        panic!("Transaction details missing")
    }
}

fn coin_info_to_coin(info: CoinInfo) -> Coin {
    let mut split_type = info
        .coin_type
        .splitn(3, "::")
        .map(|s| s.to_string())
        .collect::<Vec<String>>();
    Coin {
        account_address: std::mem::take(split_type.get_mut(0).unwrap()),
        module_name: std::mem::take(split_type.get_mut(1).unwrap()),
        struct_name: std::mem::take(split_type.get_mut(2).unwrap()),
        symbol: info.symbol,
        name: info.name,
        decimals: info.decimals.try_into().unwrap(),
    }
}

#[async_trait]
impl TransactionProcessor for EconiaTransactionProcessor {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn process_transactions(
        &self,
        transactions: Vec<Transaction>,
        start_version: u64,
        end_version: u64,
    ) -> Result<ProcessingResult, TransactionProcessingError> {
        let maybe_aptos_coin_info = {
            let mut conn = self.get_conn();
            &CoinInfoQuery::get_by_coin_type(APTOS_COIN_TYPE.to_string(), &mut conn).unwrap()
        };

        let mut all_coin_infos: HashMap<String, CoinInfo> = HashMap::new();

        for txn in &transactions {
            let (_, _, coin_infos, _, _) =
                CoinActivity::from_transaction(txn, maybe_aptos_coin_info);
            for (key, value) in coin_infos {
                all_coin_infos.entry(key).or_insert(value);
            }
        }

        let all_coins = all_coin_infos
            .into_values()
            .map(coin_info_to_coin)
            .collect::<Vec<Coin>>();

        let (_, details, events, _, _) = TransactionModel::from_transactions(&transactions);
        let mut details_iter = details.iter();
        let (mut cur_block, mut cur_time) = Default::default();
        let mut block_to_time = HashMap::new();
        let mut filtered_events = vec![];

        for e in events.into_iter().filter(is_event_type_valid) {
            while cur_block < e.transaction_block_height {
                let (block, time) = get_next_block_time(&mut details_iter);
                cur_block = block;
                cur_time = time;
            }

            if cur_block == e.transaction_block_height {
                block_to_time.insert(cur_block, cur_time);
            } else {
                panic!("Block height mismatch")
            }

            filtered_events.push(e);
        }

        self.insert_to_db(
            start_version,
            end_version,
            all_coins,
            filtered_events,
            block_to_time,
        )
        .map_err(|err| {
            TransactionProcessingError::TransactionCommitError((
                anyhow::Error::from(err),
                start_version,
                end_version,
                NAME,
            ))
        })
    }

    fn connection_pool(&self) -> &PgDbPool {
        &self.connection_pool
    }
}

