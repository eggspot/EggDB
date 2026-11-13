//! A TPC-C workload over SQL: the specification's nine tables (warehouse, district, customer,
//! history, item, stock, orders, new_order, order_line) at reduced cardinalities so a run loads
//! in seconds, and its five transactions in its mix, each one SQL transaction on the leader.

use crate::workload::{Latencies, Random, TpccTransaction, nurand, tpcc_mix};
use eggdb_pgwire::client::PgClient;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

const DISTRICTS: u64 = 10;
const CUSTOMERS: u64 = 300;
const ITEMS: u64 = 10_000;
const ROWS_PER_STATEMENT: u64 = 500;
const CUSTOMER_CONSTANT: u64 = 259;
const ITEM_CONSTANT: u64 = 7_911;

const SCHEMA: &str = "CREATE TABLE warehouse (w_id integer PRIMARY KEY, w_name text NOT NULL, w_tax numeric(4, 4) NOT NULL, w_ytd numeric(12, 2) NOT NULL);
CREATE TABLE district (d_w_id integer, d_id integer, d_name text NOT NULL, d_tax numeric(4, 4) NOT NULL, d_ytd numeric(12, 2) NOT NULL, d_next_o_id integer NOT NULL, PRIMARY KEY (d_w_id, d_id));
CREATE TABLE customer (c_w_id integer, c_d_id integer, c_id integer, c_last text NOT NULL, c_discount numeric(4, 4) NOT NULL, c_balance numeric(12, 2) NOT NULL, c_ytd_payment numeric(12, 2) NOT NULL, c_payment_cnt integer NOT NULL, c_delivery_cnt integer NOT NULL, PRIMARY KEY (c_w_id, c_d_id, c_id));
CREATE TABLE history (h_c_id integer, h_c_d_id integer, h_c_w_id integer, h_d_id integer, h_w_id integer, h_date timestamp, h_amount numeric(6, 2));
CREATE TABLE item (i_id integer PRIMARY KEY, i_name text NOT NULL, i_price numeric(5, 2) NOT NULL);
CREATE TABLE stock (s_w_id integer, s_i_id integer, s_quantity integer NOT NULL, s_ytd integer NOT NULL, s_order_cnt integer NOT NULL, PRIMARY KEY (s_w_id, s_i_id));
CREATE TABLE orders (o_w_id integer, o_d_id integer, o_id integer, o_c_id integer NOT NULL, o_entry_d timestamp NOT NULL, o_carrier_id integer, o_ol_cnt integer NOT NULL, PRIMARY KEY (o_w_id, o_d_id, o_id));
CREATE TABLE new_order (no_w_id integer, no_d_id integer, no_o_id integer, PRIMARY KEY (no_w_id, no_d_id, no_o_id));
CREATE TABLE order_line (ol_w_id integer, ol_d_id integer, ol_o_id integer, ol_number integer, ol_i_id integer NOT NULL, ol_quantity integer NOT NULL, ol_amount numeric(6, 2) NOT NULL, ol_delivery_d timestamp, PRIMARY KEY (ol_w_id, ol_d_id, ol_o_id, ol_number))";

fn run(client: &mut PgClient, sql: &str) -> Result<Vec<Vec<Option<String>>>, String> {
    client
        .simple_query(sql)
        .map(|results| results.into_iter().flat_map(|result| result.rows).collect())
        .map_err(|error| error.to_string())
}

fn insert_rows(client: &mut PgClient, table: &str, rows: impl Iterator<Item = String>) -> Result<(), String> {
    let mut batch = Vec::new();
    for row in rows {
        batch.push(row);
        if batch.len() as u64 == ROWS_PER_STATEMENT {
            run(client, &format!("INSERT INTO {table} VALUES {}", batch.join(", ")))?;
            batch.clear();
        }
    }
    if !batch.is_empty() {
        run(client, &format!("INSERT INTO {table} VALUES {}", batch.join(", ")))?;
    }
    Ok(())
}

/// Creates the schema and loads `warehouses` warehouses.
pub(crate) fn load(client: &mut PgClient, warehouses: u64, random: &mut Random) -> Result<(), String> {
    run(client, SCHEMA).map_err(|error| format!("cannot create the TPC-C schema: {error}"))?;
    let mut prices = Random::new(random.next());
    insert_rows(client, "item", (1..=ITEMS).map(|item| format!("({item}, 'item {item}', {}.{:02})", prices.between(1, 99), prices.below(100))))?;
    for warehouse in 1..=warehouses {
        run(client, &format!("INSERT INTO warehouse VALUES ({warehouse}, 'w{warehouse}', 0.{:04}, 300000)", random.below(2_000)))?;
        insert_rows(client, "district", (1..=DISTRICTS).map(|district| format!("({warehouse}, {district}, 'd{district}', 0.0500, 30000, 1)")))?;
        for district in 1..=DISTRICTS {
            insert_rows(
                client,
                "customer",
                (1..=CUSTOMERS).map(|customer| format!("({warehouse}, {district}, {customer}, 'last{}', 0.0100, -10, 10, 1, 0)", customer % 1000)),
            )?;
        }
        let mut quantities = Random::new(random.next());
        insert_rows(client, "stock", (1..=ITEMS).map(|item| format!("({warehouse}, {item}, {}, 0, 0)", quantities.between(10, 100))))?;
    }
    Ok(())
}

/// What one client measured.
#[derive(Debug, Default)]
pub(crate) struct ClientResult {
    pub(crate) new_orders: Latencies,
    pub(crate) all: Latencies,
    pub(crate) aborted: u64,
}

fn new_order(client: &mut PgClient, random: &mut Random, warehouse: u64) -> Result<(), String> {
    let district = random.between(1, DISTRICTS);
    let customer = nurand(random, 1023, CUSTOMER_CONSTANT, 1, CUSTOMERS);
    let lines = random.between(5, 15);
    run(client, "BEGIN")?;
    let order = run(client, &format!("UPDATE district SET d_next_o_id = d_next_o_id + 1 WHERE d_w_id = {warehouse} AND d_id = {district} RETURNING d_next_o_id - 1"))?
        .first()
        .and_then(|row| row.first().cloned().flatten())
        .ok_or("the district has no next order id")?;
    let mut script = format!(
        "INSERT INTO orders VALUES ({warehouse}, {district}, {order}, {customer}, now(), NULL, {lines}); INSERT INTO new_order VALUES ({warehouse}, {district}, {order});"
    );
    for number in 1..=lines {
        let item = nurand(random, 8191, ITEM_CONSTANT, 1, ITEMS);
        let quantity = random.between(1, 10);
        script.push_str(&format!(
            " UPDATE stock SET s_quantity = CASE WHEN s_quantity >= {quantity} + 10 THEN s_quantity - {quantity} ELSE s_quantity - {quantity} + 91 END, s_ytd = s_ytd + {quantity}, s_order_cnt = s_order_cnt + 1 WHERE s_w_id = {warehouse} AND s_i_id = {item};"
        ));
        script.push_str(&format!(
            " INSERT INTO order_line SELECT {warehouse}, {district}, {order}, {number}, i_id, {quantity}, i_price * {quantity}, NULL FROM item WHERE i_id = {item};"
        ));
    }
    script.push_str(" COMMIT");
    run(client, &script).map(drop)
}

fn payment(client: &mut PgClient, random: &mut Random, warehouse: u64) -> Result<(), String> {
    let district = random.between(1, DISTRICTS);
    let customer = nurand(random, 1023, CUSTOMER_CONSTANT, 1, CUSTOMERS);
    let amount = format!("{}.{:02}", random.between(1, 4_999), random.below(100));
    run(
        client,
        &format!(
            "BEGIN; UPDATE warehouse SET w_ytd = w_ytd + {amount} WHERE w_id = {warehouse}; UPDATE district SET d_ytd = d_ytd + {amount} WHERE d_w_id = {warehouse} AND d_id = {district}; UPDATE customer SET c_balance = c_balance - {amount}, c_ytd_payment = c_ytd_payment + {amount}, c_payment_cnt = c_payment_cnt + 1 WHERE c_w_id = {warehouse} AND c_d_id = {district} AND c_id = {customer}; INSERT INTO history VALUES ({customer}, {district}, {warehouse}, {district}, {warehouse}, now(), {amount}); COMMIT"
        ),
    )
    .map(drop)
}

fn order_status(client: &mut PgClient, random: &mut Random, warehouse: u64) -> Result<(), String> {
    let district = random.between(1, DISTRICTS);
    let customer = nurand(random, 1023, CUSTOMER_CONSTANT, 1, CUSTOMERS);
    run(
        client,
        &format!(
            "BEGIN; SELECT c_balance FROM customer WHERE c_w_id = {warehouse} AND c_d_id = {district} AND c_id = {customer}; SELECT ol_i_id, ol_quantity, ol_amount, ol_delivery_d FROM order_line WHERE ol_w_id = {warehouse} AND ol_d_id = {district} AND ol_o_id = (SELECT max(o_id) FROM orders WHERE o_w_id = {warehouse} AND o_d_id = {district} AND o_c_id = {customer}); COMMIT"
        ),
    )
    .map(drop)
}

fn delivery(client: &mut PgClient, random: &mut Random, warehouse: u64) -> Result<(), String> {
    let carrier = random.between(1, 10);
    run(client, "BEGIN")?;
    for district in 1..=DISTRICTS {
        let oldest = run(client, &format!("SELECT min(no_o_id) FROM new_order WHERE no_w_id = {warehouse} AND no_d_id = {district}"))?
            .first()
            .and_then(|row| row.first().cloned().flatten());
        let Some(order) = oldest else {
            continue;
        };
        run(
            client,
            &format!(
                "DELETE FROM new_order WHERE no_w_id = {warehouse} AND no_d_id = {district} AND no_o_id = {order}; UPDATE orders SET o_carrier_id = {carrier} WHERE o_w_id = {warehouse} AND o_d_id = {district} AND o_id = {order}; UPDATE order_line SET ol_delivery_d = now() WHERE ol_w_id = {warehouse} AND ol_d_id = {district} AND ol_o_id = {order}; UPDATE customer SET c_balance = c_balance + (SELECT coalesce(sum(ol_amount), 0) FROM order_line WHERE ol_w_id = {warehouse} AND ol_d_id = {district} AND ol_o_id = {order}), c_delivery_cnt = c_delivery_cnt + 1 WHERE c_w_id = {warehouse} AND c_d_id = {district} AND c_id = (SELECT o_c_id FROM orders WHERE o_w_id = {warehouse} AND o_d_id = {district} AND o_id = {order})"
            ),
        )?;
    }
    run(client, "COMMIT").map(drop)
}

fn stock_level(client: &mut PgClient, random: &mut Random, warehouse: u64) -> Result<(), String> {
    let district = random.between(1, DISTRICTS);
    let threshold = random.between(10, 20);
    run(
        client,
        &format!(
            "SELECT count(DISTINCT s_i_id) FROM order_line JOIN stock ON s_w_id = ol_w_id AND s_i_id = ol_i_id WHERE ol_w_id = {warehouse} AND ol_d_id = {district} AND ol_o_id >= (SELECT d_next_o_id - 20 FROM district WHERE d_w_id = {warehouse} AND d_id = {district}) AND s_quantity < {threshold}"
        ),
    )
    .map(drop)
}

/// Runs the TPC-C mix from one client against the leader until `stop` turns true, each client
/// at home in one warehouse as the specification's terminals are.
pub(crate) fn client(
    leader: usize,
    connect: fn(usize) -> Result<PgClient, String>,
    warehouses: u64,
    seed: u64,
    stop: &AtomicBool,
) -> Result<ClientResult, String> {
    let mut random = Random::new(seed);
    let mut client = connect(leader)?;
    let warehouse = random.between(1, warehouses.max(1));
    let mut result = ClientResult::default();
    while !stop.load(Ordering::Acquire) {
        let transaction = tpcc_mix(&mut random);
        let started = Instant::now();
        let outcome = match transaction {
            TpccTransaction::NewOrder => new_order(&mut client, &mut random, warehouse),
            TpccTransaction::Payment => payment(&mut client, &mut random, warehouse),
            TpccTransaction::OrderStatus => order_status(&mut client, &mut random, warehouse),
            TpccTransaction::Delivery => delivery(&mut client, &mut random, warehouse),
            TpccTransaction::StockLevel => stock_level(&mut client, &mut random, warehouse),
        };
        let micros = started.elapsed().as_micros() as u64;
        match outcome {
            Ok(()) => {
                result.all.record(micros);
                if transaction == TpccTransaction::NewOrder {
                    result.new_orders.record(micros);
                }
            }
            Err(_) => {
                let _ = run(&mut client, "ROLLBACK");
                result.aborted += 1;
            }
        }
    }
    Ok(result)
}
