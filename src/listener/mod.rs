mod events;

use crate::{
    db,
    {error::Error, AppState},
};
use anchor_client::{
    solana_client::rpc_config::{
        RpcAccountInfoConfig, RpcTransactionLogsConfig,
        RpcTransactionLogsFilter,
    },
    EventContext,
};
use futures::StreamExt;
use jsonrpc_core_client::transports::ws;
use solana_account_decoder::{UiAccountData, UiAccountEncoding};
use solana_rpc::rpc_pubsub::RpcSolPubSubClient;
use std::{
    collections::HashMap,
    env,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use tracing::{debug, error_span, info, Instrument};

pub async fn run(st: &'static AppState) -> Result<(), Error> {
    let db_client =
        mongodb::Client::with_uri_str(env::var("DATABASE_URL")?).await?;

    let db = db_client.database("main");
    let db: &'static _ = Box::leak(Box::new(db));

    futures::join!(
        listen_oracle_failures(st),
        listen_event_queue(st, db),
        poll_update_funding(st, db),
        listen_rpnl(st, db),
        listen_liq(st, db),
        listen_bankruptcy(st, db),
    );

    Ok(())
}

async fn listen_oracle_failures(st: &'static AppState) {
    let span = error_span!("oracle_failures");

    let re = regex::Regex::new(r"NOOPS/CACHE_ORACLE/SYM/(\w+)").unwrap();

    loop {
        let sub = ws::try_connect::<RpcSolPubSubClient>(st.cluster.ws_url())
            .unwrap()
            .await
            .and_then(|p| {
                p.logs_subscribe(
                    RpcTransactionLogsFilter::Mentions(vec![
                        zo_abi::ID.to_string()
                    ]),
                    Some(RpcTransactionLogsConfig { commitment: None }),
                )
            });

        let mut sub = match sub {
            Ok(x) => x,
            Err(e) => {
                st.error(span.clone(), e).await;
                continue;
            }
        };

        while let Some(resp) = sub.next().await {
            if let Ok(resp) = resp {
                let skipped = resp
                    .value
                    .logs
                    .into_iter()
                    .filter_map(|s| {
                        re.captures(&s)
                            .map(|c| c.get(1).unwrap().as_str().to_owned())
                    })
                    .collect::<Vec<_>>();

                if !skipped.is_empty() {
                    st.error(
                        span.clone(),
                        crate::error::Error::OraclesSkipped(skipped),
                    )
                    .await;
                }
            }
        }
    }
}

/// Listens and logs liquidation events
async fn listen_liq(st: &'static AppState, db: &'static mongodb::Database) {
    let span = error_span!("liquidation");

    loop {
        let sub = ws::try_connect::<RpcSolPubSubClient>(st.cluster.ws_url())
            .unwrap()
            .await
            .and_then(|p| {
                p.logs_subscribe(
                    RpcTransactionLogsFilter::Mentions(vec![
                        zo_abi::ID.to_string()
                    ]),
                    Some(RpcTransactionLogsConfig { commitment: None }),
                )
            });

        let mut sub = match sub {
            Ok(x) => x,
            Err(e) => {
                st.error(span.clone(), e).await;
                continue;
            }
        };

        while let Some(resp) = sub.next().await {
            if let Ok(resp) = resp {
                if resp.value.err.is_some() {
                    continue;
                }

                let ctx = EventContext {
                    signature: resp.value.signature.parse().unwrap(),
                    slot: resp.context.slot,
                };

                let events: Vec<zo_abi::events::LiquidationLog> =
                    self::events::parse(resp.value.logs.into_iter(), st).await;

                let docs: Vec<_> = events
                    .iter()
                    .map(|e| {
                        let copy = (*e).clone();
                        db::Liquidation {
                            sig: ctx.signature.to_string(),
                            slot: ctx.slot as i64,
                            liquidation_event: e.liquidation_event.to_string(),
                            base_symbol: e.base_symbol.to_string(),
                            quote_symbol: copy
                                .quote_symbol
                                .unwrap_or_else(|| "".to_string()),
                            liqor_margin: e.liqor_margin.to_string(),
                            liqee_margin: e.liqee_margin.to_string(),
                            assets_to_liqor: e.assets_to_liqor,
                            quote_to_liqor: e.quote_to_liqor,
                        }
                    })
                    .collect();

                if let Err(e) = db::Liquidation::update(db, &docs).await {
                    st.error(span.clone(), e).await;
                    continue;
                }
            }
        }
    }
}

/// Listens and logs bankruptcy events
async fn listen_bankruptcy(
    st: &'static AppState,
    db: &'static mongodb::Database,
) {
    let span = error_span!("bankruptcy");

    loop {
        let sub = ws::try_connect::<RpcSolPubSubClient>(st.cluster.ws_url())
            .unwrap()
            .await
            .and_then(|p| {
                p.logs_subscribe(
                    RpcTransactionLogsFilter::Mentions(vec![
                        zo_abi::ID.to_string()
                    ]),
                    Some(RpcTransactionLogsConfig { commitment: None }),
                )
            });

        let mut sub = match sub {
            Ok(x) => x,
            Err(e) => {
                st.error(span.clone(), e).await;
                continue;
            }
        };

        while let Some(resp) = sub.next().await {
            if let Ok(resp) = resp {
                if resp.value.err.is_some() {
                    continue;
                }

                let ctx = EventContext {
                    signature: resp.value.signature.parse().unwrap(),
                    slot: resp.context.slot,
                };

                let events: Vec<zo_abi::events::BankruptcyLog> =
                    self::events::parse(resp.value.logs.into_iter(), st).await;

                let docs: Vec<_> = events
                    .iter()
                    .map(|e| db::Bankruptcy {
                        sig: ctx.signature.to_string(),
                        slot: ctx.slot as i64,
                        base_symbol: e.base_symbol.to_string(),
                        liqor_margin: e.liqor_margin.to_string(),
                        liqee_margin: e.liqee_margin.to_string(),
                        assets_to_liqor: e.assets_to_liqor,
                        quote_to_liqor: e.quote_to_liqor,
                        insurance_loss: e.insurance_loss,
                        socialized_loss: e.socialized_loss,
                    })
                    .collect();

                if let Err(e) = db::Bankruptcy::update(db, &docs).await {
                    st.error(span.clone(), e).await;
                    continue;
                }
            }
        }
    }
}

/// Listens and logs realized pnl events
async fn listen_rpnl(st: &'static AppState, db: &'static mongodb::Database) {
    let span = error_span!("rpnl");

    loop {
        let sub = ws::try_connect::<RpcSolPubSubClient>(st.cluster.ws_url())
            .unwrap()
            .await
            .and_then(|p| {
                p.logs_subscribe(
                    RpcTransactionLogsFilter::Mentions(vec![
                        zo_abi::ID.to_string()
                    ]),
                    Some(RpcTransactionLogsConfig { commitment: None }),
                )
            });

        let mut sub = match sub {
            Ok(x) => x,
            Err(e) => {
                st.error(span.clone(), e).await;
                continue;
            }
        };

        while let Some(resp) = sub.next().await {
            if let Ok(resp) = resp {
                if resp.value.err.is_some() {
                    continue;
                }

                let ctx = EventContext {
                    signature: resp.value.signature.parse().unwrap(),
                    slot: resp.context.slot,
                };

                let events: Vec<zo_abi::events::RealizedPnlLog> =
                    self::events::parse(resp.value.logs.into_iter(), st).await;

                let mut docs = Vec::new();

                for e in events.into_iter() {
                    let doc = st
                        .load_dex_markets()
                        .find(|(_symbol, m)| m.own_address == e.market_key)
                        .map(|(symbol, _m)| db::RealizedPnl {
                            symbol,
                            sig: ctx.signature.to_string(),
                            slot: ctx.slot as i64,
                            margin: e.margin.to_string(),
                            is_long: e.is_long,
                            pnl: e.pnl,
                            qty_paid: e.qty_paid,
                            qty_received: e.qty_received,
                        })
                        .unwrap();
                    docs.push(doc);
                }

                if let Err(e) = db::RealizedPnl::update(db, &docs).await {
                    st.error(span.clone(), e).await;
                    continue;
                }
            }
        }
    }
}

async fn listen_event_queue(
    st: &'static AppState,
    db: &'static mongodb::Database,
) {
    let handles: Vec<_> = st
        .load_dex_markets()
        .map(|(symbol, dex_market)| {
            let base_decimals = dex_market.coin_decimals as u8;
            let quote_decimals = 6u8;
            let event_q = dex_market.event_q.to_string();

            tokio::spawn(async move {
                let span = error_span!("event_queue", symbol = symbol.as_str());

                loop {
                    let event_q = event_q.clone();

                    let sub = ws::try_connect::<RpcSolPubSubClient>(
                        st.cluster.ws_url(),
                    )
                    .unwrap()
                    .await
                    .and_then(|p| {
                        p.account_subscribe(
                            event_q,
                            Some(RpcAccountInfoConfig {
                                encoding: Some(UiAccountEncoding::Base64),
                                data_slice: None,
                                commitment: None,
                            }),
                        )
                    });

                    let mut sub = match sub {
                        Err(e) => {
                            st.error(span.clone(), e).await;
                            continue;
                        }
                        Ok(x) => x,
                    };

                    while let Some(resp) = sub.next().await {
                        span.in_scope(|| info!("got update"));

                        let resp = match resp {
                            Ok(x) => x,
                            Err(e) => {
                                st.error(span.clone(), e).await;
                                continue;
                            }
                        };

                        let buf = match resp.value.data {
                            UiAccountData::Binary(b, _) => {
                                base64::decode(b).unwrap()
                            }
                            _ => panic!(),
                        };

                        let db_res = db::Trade::update(
                            db,
                            &symbol,
                            base_decimals,
                            quote_decimals,
                            &buf,
                        )
                        .instrument(span.clone())
                        .await;

                        if let Err(e) = db_res {
                            st.error(span.clone(), e).await;
                            continue;
                        }
                    }
                }
            })
        })
        .collect();

    let _ = futures::future::join_all(handles).await;
}

async fn poll_update_funding(
    st: &'static AppState,
    db: &'static mongodb::Database,
) {
    let span = error_span!("update_funding");

    let mut interval = tokio::time::interval(Duration::from_secs(10));

    // Previous update funding time. The funding is only
    // inserted into the DB if the funding time increases.
    let prev: HashMap<String, AtomicU64> = st
        .load_dex_markets()
        .map(|(s, _)| (s, AtomicU64::new(0)))
        .collect();

    let prev: &'static _ = Box::leak(Box::new(prev));

    loop {
        interval.tick().await;

        let to_update: Vec<_> = st
            .load_dex_markets()
            .filter(|(symbol, m)| {
                let prev_update = prev
                    .get(symbol)
                    .map(|x| x.load(Ordering::Relaxed))
                    .unwrap();

                m.last_updated > prev_update
            })
            .collect();

        if to_update.is_empty() {
            span.in_scope(|| debug!("nothing to update"));
            continue;
        }

        let new_entries: Vec<_> = to_update
            .iter()
            .map(|(symbol, m)| db::Funding {
                symbol: symbol.clone(),
                funding_index: { m.funding_index }.to_string(),
                last_updated: m.last_updated as i64,
            })
            .collect();

        let res = db::Funding::update(db, &new_entries).await;

        if let Err(e) = res {
            st.error(span.clone(), e).await;
            continue;
        }

        let updated: Vec<_> =
            to_update.iter().map(|(s, _)| s).cloned().collect();

        for (s, m) in to_update.into_iter() {
            prev.get(&s)
                .unwrap()
                .store(m.last_updated, Ordering::Relaxed);
        }

        span.in_scope(|| info!("inserted {}", updated.join(", ")));
    }
}