use crate::sql::Connection;

pub(crate) const NEW_ORDER: &str = "tpcc_compare.new_order";
pub(crate) const PAYMENT: &str = "tpcc_compare.payment";
pub(crate) const ORDER_STATUS: &str = "tpcc_compare.order_status";
pub(crate) const DELIVERY: &str = "tpcc_compare.delivery";
pub(crate) const STOCK_LEVEL: &str = "tpcc_compare.stock_level";
pub(crate) const TRANSACTIONS: [&str; 5] = [NEW_ORDER, PAYMENT, ORDER_STATUS, DELIVERY, STOCK_LEVEL];

const DISTRICTS: u64 = 10;
const BATCH: u64 = 500;
const LAST_NAMES: [&str; 10] = ["BAR", "OUGHT", "ABLE", "PRI", "PRES", "ESE", "ANTI", "CALLY", "ATION", "EING"];

#[derive(Debug, Clone, Copy)]
pub(crate) struct Scale {
    pub(crate) warehouses: u64,
    pub(crate) customers: u64,
    pub(crate) items: u64,
}

const SCHEMA: [&str; 9] = [
    "CREATE TABLE warehouse (w_id integer PRIMARY KEY, w_name varchar(10), w_tax decimal(4,4), w_ytd decimal(12,2))",
    "CREATE TABLE district (d_w_id integer, d_id integer, d_name varchar(10), d_tax decimal(4,4), d_ytd decimal(12,2), d_next_o_id integer, PRIMARY KEY (d_w_id, d_id))",
    "CREATE TABLE customer (c_w_id integer, c_d_id integer, c_id integer, c_last varchar(16), c_discount decimal(4,4), c_balance decimal(12,2), c_ytd_payment decimal(12,2), c_payment_cnt integer, c_delivery_cnt integer, PRIMARY KEY (c_w_id, c_d_id, c_id))",
    "CREATE TABLE history (h_id integer PRIMARY KEY, h_c_id integer, h_c_d_id integer, h_c_w_id integer, h_amount decimal(6,2))",
    "CREATE TABLE orders (o_w_id integer, o_d_id integer, o_id integer, o_c_id integer, o_carrier_id integer, o_ol_cnt integer, PRIMARY KEY (o_w_id, o_d_id, o_id))",
    "CREATE TABLE new_order (no_w_id integer, no_d_id integer, no_o_id integer, PRIMARY KEY (no_w_id, no_d_id, no_o_id))",
    "CREATE TABLE order_line (ol_w_id integer, ol_d_id integer, ol_o_id integer, ol_number integer, ol_i_id integer, ol_quantity integer, ol_amount decimal(6,2), ol_delivered integer, PRIMARY KEY (ol_w_id, ol_d_id, ol_o_id, ol_number))",
    "CREATE TABLE item (i_id integer PRIMARY KEY, i_name varchar(24), i_price decimal(5,2))",
    "CREATE TABLE stock (s_w_id integer, s_i_id integer, s_quantity integer, s_ytd integer, s_order_cnt integer, PRIMARY KEY (s_w_id, s_i_id))",
];

const DROP: [&str; 9] = [
    "DROP TABLE IF EXISTS order_line",
    "DROP TABLE IF EXISTS new_order",
    "DROP TABLE IF EXISTS orders",
    "DROP TABLE IF EXISTS history",
    "DROP TABLE IF EXISTS stock",
    "DROP TABLE IF EXISTS item",
    "DROP TABLE IF EXISTS customer",
    "DROP TABLE IF EXISTS district",
    "DROP TABLE IF EXISTS warehouse",
];

fn mixed(seed: u64) -> u64 {
    let mut x = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

fn between(seed: u64, low: u64, high: u64) -> u64 {
    low + mixed(seed) % (high - low + 1)
}

fn insert_batches(connection: &mut Connection, table: &str, rows: impl Iterator<Item = String>) -> Result<(), String> {
    let mut batch = Vec::new();
    for row in rows {
        batch.push(row);
        if batch.len() as u64 == BATCH {
            connection.run(&format!("INSERT INTO {table} VALUES {}", batch.join(", ")))?;
            batch.clear();
        }
    }
    if !batch.is_empty() {
        connection.run(&format!("INSERT INTO {table} VALUES {}", batch.join(", ")))?;
    }
    Ok(())
}

/// Creates the nine tables and fills them as TPC-C's initial population does, at a smaller,
/// configurable size: per warehouse 10 districts, `customers` customers each with one order
/// already placed, the last third of them still undelivered, and stock for every item.
pub(crate) fn load(connection: &mut Connection, scale: Scale) -> Result<(), String> {
    for statement in DROP.iter().chain(SCHEMA.iter()) {
        connection.run(statement)?;
    }
    insert_batches(
        connection,
        "item",
        (1..=scale.items).map(|id| format!("({id}, 'item-{id}', {}.{:02})", between(id, 1, 99), between(id ^ 7, 0, 99))),
    )?;
    for warehouse in 1..=scale.warehouses {
        connection.run(&format!("INSERT INTO warehouse VALUES ({warehouse}, 'w{warehouse}', 0.{:04}, 300000.00)", between(warehouse, 0, 2000)))?;
        insert_batches(
            connection,
            "stock",
            (1..=scale.items).map(|item| format!("({warehouse}, {item}, {}, 0, 0)", between(warehouse * 1_000_003 + item, 10, 100))),
        )?;
        for district in 1..=DISTRICTS {
            connection.run(&format!(
                "INSERT INTO district VALUES ({warehouse}, {district}, 'd{district}', 0.{:04}, 30000.00, {})",
                between(warehouse * 31 + district, 0, 2000),
                scale.customers + 1
            ))?;
            insert_batches(
                connection,
                "customer",
                (1..=scale.customers).map(|customer| {
                    format!(
                        "({warehouse}, {district}, {customer}, '{}', 0.{:04}, -10.00, 10.00, 1, 0)",
                        LAST_NAMES[(customer % 10) as usize],
                        between(customer * 13 + district, 0, 5000)
                    )
                }),
            )?;
            let undelivered_from = scale.customers - scale.customers / 3;
            insert_batches(
                connection,
                "orders",
                (1..=scale.customers).map(|order| {
                    let carrier = if order > undelivered_from { "NULL".to_owned() } else { between(order, 1, 10).to_string() };
                    format!("({warehouse}, {district}, {order}, {}, {carrier}, 5)", between(order ^ district, 1, scale.customers))
                }),
            )?;
            insert_batches(
                connection,
                "order_line",
                (1..=scale.customers).flat_map(|order| {
                    (1..=5).map(move |line| {
                        format!(
                            "({warehouse}, {district}, {order}, {line}, {}, 5, {}.00, {})",
                            between(order * 7 + line, 1, scale.items),
                            between(order + line, 1, 99),
                            u8::from(order <= undelivered_from)
                        )
                    })
                }),
            )?;
            insert_batches(
                connection,
                "new_order",
                (undelivered_from + 1..=scale.customers).map(|order| format!("({warehouse}, {district}, {order})")),
            )?;
        }
    }
    Ok(())
}

fn new_order(connection: &mut Connection, scale: Scale, step: u64) -> Result<(), String> {
    let warehouse = between(step, 1, scale.warehouses);
    let district = between(step ^ 1, 1, DISTRICTS);
    let customer = between(step ^ 2, 1, scale.customers);
    let lines = between(step ^ 3, 5, 15);
    connection.run("BEGIN")?;
    connection.run(&format!("SELECT w_tax FROM warehouse WHERE w_id = {warehouse}"))?;
    let order = connection.number(&format!("SELECT d_next_o_id FROM district WHERE d_w_id = {warehouse} AND d_id = {district}"))?;
    connection.run(&format!("UPDATE district SET d_next_o_id = d_next_o_id + 1 WHERE d_w_id = {warehouse} AND d_id = {district}"))?;
    connection.run(&format!("SELECT c_discount, c_last FROM customer WHERE c_w_id = {warehouse} AND c_d_id = {district} AND c_id = {customer}"))?;
    connection.run(&format!("INSERT INTO orders VALUES ({warehouse}, {district}, {order}, {customer}, NULL, {lines})"))?;
    connection.run(&format!("INSERT INTO new_order VALUES ({warehouse}, {district}, {order})"))?;
    for line in 1..=lines {
        let item = between(step * 31 + line, 1, scale.items);
        let quantity = between(step * 17 + line, 1, 10);
        connection.run(&format!("SELECT i_price FROM item WHERE i_id = {item}"))?;
        let stock = connection.number(&format!("SELECT s_quantity FROM stock WHERE s_w_id = {warehouse} AND s_i_id = {item}"))?;
        let remaining = if stock >= i64::try_from(quantity).unwrap_or(0) + 10 { stock - quantity as i64 } else { stock - quantity as i64 + 91 };
        connection.run(&format!(
            "UPDATE stock SET s_quantity = {remaining}, s_ytd = s_ytd + {quantity}, s_order_cnt = s_order_cnt + 1 WHERE s_w_id = {warehouse} AND s_i_id = {item}"
        ))?;
        connection.run(&format!(
            "INSERT INTO order_line VALUES ({warehouse}, {district}, {order}, {line}, {item}, {quantity}, {}.00, 0)",
            quantity * between(item, 1, 99)
        ))?;
    }
    connection.run("COMMIT")?;
    Ok(())
}

fn payment(connection: &mut Connection, scale: Scale, step: u64, history: u64) -> Result<(), String> {
    let warehouse = between(step, 1, scale.warehouses);
    let district = between(step ^ 5, 1, DISTRICTS);
    let customer = between(step ^ 6, 1, scale.customers);
    let amount = between(step ^ 7, 1, 5000);
    connection.run("BEGIN")?;
    connection.run(&format!("UPDATE warehouse SET w_ytd = w_ytd + {amount} WHERE w_id = {warehouse}"))?;
    connection.run(&format!("UPDATE district SET d_ytd = d_ytd + {amount} WHERE d_w_id = {warehouse} AND d_id = {district}"))?;
    connection.run(&format!(
        "UPDATE customer SET c_balance = c_balance - {amount}, c_ytd_payment = c_ytd_payment + {amount}, c_payment_cnt = c_payment_cnt + 1 WHERE c_w_id = {warehouse} AND c_d_id = {district} AND c_id = {customer}"
    ))?;
    connection.run(&format!("INSERT INTO history VALUES ({history}, {customer}, {district}, {warehouse}, {amount}.00)"))?;
    connection.run("COMMIT")?;
    Ok(())
}

fn order_status(connection: &mut Connection, scale: Scale, step: u64) -> Result<(), String> {
    let warehouse = between(step, 1, scale.warehouses);
    let district = between(step ^ 9, 1, DISTRICTS);
    let customer = between(step ^ 10, 1, scale.customers);
    connection.run(&format!("SELECT c_balance, c_last FROM customer WHERE c_w_id = {warehouse} AND c_d_id = {district} AND c_id = {customer}"))?;
    let latest = connection.run(&format!(
        "SELECT o_id FROM orders WHERE o_w_id = {warehouse} AND o_d_id = {district} AND o_c_id = {customer} ORDER BY o_id DESC LIMIT 1"
    ))?;
    if let Some(order) = latest.first().and_then(|row| row.first()).and_then(|value| value.clone()) {
        connection.run(&format!(
            "SELECT ol_i_id, ol_quantity, ol_amount FROM order_line WHERE ol_w_id = {warehouse} AND ol_d_id = {district} AND ol_o_id = {order}"
        ))?;
    }
    Ok(())
}

fn delivery(connection: &mut Connection, scale: Scale, step: u64) -> Result<(), String> {
    let warehouse = between(step, 1, scale.warehouses);
    let carrier = between(step ^ 11, 1, 10);
    connection.run("BEGIN")?;
    for district in 1..=DISTRICTS {
        let oldest = connection.run(&format!("SELECT min(no_o_id) FROM new_order WHERE no_w_id = {warehouse} AND no_d_id = {district}"))?;
        let Some(order) = oldest.first().and_then(|row| row.first()).and_then(|value| value.clone()) else {
            continue;
        };
        connection.run(&format!("DELETE FROM new_order WHERE no_w_id = {warehouse} AND no_d_id = {district} AND no_o_id = {order}"))?;
        connection.run(&format!("UPDATE orders SET o_carrier_id = {carrier} WHERE o_w_id = {warehouse} AND o_d_id = {district} AND o_id = {order}"))?;
        connection.run(&format!("UPDATE order_line SET ol_delivered = 1 WHERE ol_w_id = {warehouse} AND ol_d_id = {district} AND ol_o_id = {order}"))?;
        let total = connection.run(&format!(
            "SELECT sum(ol_amount) FROM order_line WHERE ol_w_id = {warehouse} AND ol_d_id = {district} AND ol_o_id = {order}"
        ))?;
        let amount = total
            .first()
            .and_then(|row| row.first())
            .and_then(|value| value.clone())
            .unwrap_or_else(|| "0".to_owned());
        let customer = connection.number(&format!("SELECT o_c_id FROM orders WHERE o_w_id = {warehouse} AND o_d_id = {district} AND o_id = {order}"))?;
        connection.run(&format!(
            "UPDATE customer SET c_balance = c_balance + {amount}, c_delivery_cnt = c_delivery_cnt + 1 WHERE c_w_id = {warehouse} AND c_d_id = {district} AND c_id = {customer}"
        ))?;
    }
    connection.run("COMMIT")?;
    Ok(())
}

fn stock_level(connection: &mut Connection, scale: Scale, step: u64) -> Result<(), String> {
    let warehouse = between(step, 1, scale.warehouses);
    let district = between(step ^ 13, 1, DISTRICTS);
    let threshold = between(step ^ 14, 10, 20);
    let next = connection.number(&format!("SELECT d_next_o_id FROM district WHERE d_w_id = {warehouse} AND d_id = {district}"))?;
    connection.run(&format!(
        "SELECT count(DISTINCT s_i_id) FROM order_line, stock WHERE ol_w_id = {warehouse} AND ol_d_id = {district} AND ol_o_id >= {} AND ol_o_id < {next} AND s_w_id = {warehouse} AND s_i_id = ol_i_id AND s_quantity < {threshold}",
        next - 20
    ))?;
    Ok(())
}

/// Runs one transaction of the named kind with parameters drawn from `step`.
pub(crate) fn run(connection: &mut Connection, name: &str, scale: Scale, step: u64) -> Result<(), String> {
    match name {
        NEW_ORDER => new_order(connection, scale, step),
        PAYMENT => payment(connection, scale, step, step + 1),
        ORDER_STATUS => order_status(connection, scale, step),
        DELIVERY => delivery(connection, scale, step),
        STOCK_LEVEL => stock_level(connection, scale, step),
        _ => Err(format!("no transaction named {name}")),
    }
}
