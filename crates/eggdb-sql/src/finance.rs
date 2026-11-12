/// The statements `CREATE EXTENSION eggdb_finance` runs: a double-entry ledger whose entries
/// must balance per currency and never change, and a limit order book matched by price, then
/// time (D50).
pub(crate) const FINANCE_SCRIPT: &str = r#"
CREATE SCHEMA eggdb_finance;
CREATE TABLE eggdb_finance.accounts (account text PRIMARY KEY, kind text NOT NULL CHECK (kind IN ('asset', 'liability', 'equity', 'income', 'expense')), currency text NOT NULL DEFAULT 'USD', opened_at timestamptz NOT NULL DEFAULT now());
CREATE TABLE eggdb_finance.entries (entry_id bigserial PRIMARY KEY, posted_at timestamptz NOT NULL DEFAULT now(), description text NOT NULL, reverses bigint UNIQUE REFERENCES eggdb_finance.entries (entry_id));
CREATE TABLE eggdb_finance.postings (entry_id bigint NOT NULL REFERENCES eggdb_finance.entries (entry_id), line integer NOT NULL, account text NOT NULL REFERENCES eggdb_finance.accounts (account), amount numeric NOT NULL CHECK (amount <> 0), PRIMARY KEY (entry_id, line));
CREATE INDEX postings_by_account ON eggdb_finance.postings (account, entry_id);
CREATE FUNCTION eggdb_finance.refuse_change() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'posted entries never change: post a reversal instead' USING ERRCODE = '55000';
END
$$;
CREATE TRIGGER entries_never_change BEFORE UPDATE OR DELETE ON eggdb_finance.entries FOR EACH ROW EXECUTE FUNCTION eggdb_finance.refuse_change();
CREATE TRIGGER postings_never_change BEFORE UPDATE OR DELETE ON eggdb_finance.postings FOR EACH ROW EXECUTE FUNCTION eggdb_finance.refuse_change();
CREATE FUNCTION eggdb_finance.post(entry_description text, lines jsonb, reversed_entry bigint DEFAULT NULL) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    created bigint;
    item record;
    line_number integer := 0;
    unbalanced text;
BEGIN
    IF jsonb_typeof(lines) IS DISTINCT FROM 'array' OR jsonb_array_length(lines) < 2 THEN
        RAISE EXCEPTION 'an entry needs at least two postings' USING ERRCODE = '22023';
    END IF;
    INSERT INTO eggdb_finance.entries (description, reverses) VALUES (entry_description, reversed_entry) RETURNING entries.entry_id INTO created;
    FOR item IN SELECT l.value AS posting FROM jsonb_array_elements(lines) l LOOP
        line_number := line_number + 1;
        INSERT INTO eggdb_finance.postings (entry_id, line, account, amount) VALUES (created, line_number, item.posting ->> 'account', (item.posting ->> 'amount')::numeric);
    END LOOP;
    SELECT string_agg(format('%s %s', t.currency, t.total), ', ') INTO unbalanced
        FROM (SELECT a.currency, sum(p.amount) AS total FROM eggdb_finance.postings p JOIN eggdb_finance.accounts a ON a.account = p.account WHERE p.entry_id = created GROUP BY a.currency HAVING sum(p.amount) <> 0) t;
    IF unbalanced IS NOT NULL THEN
        RAISE EXCEPTION 'the entry does not balance: %', unbalanced USING ERRCODE = '23514';
    END IF;
    RETURN created;
END
$$;
CREATE FUNCTION eggdb_finance.reverse(original_entry bigint) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    opposite jsonb;
    wording text;
BEGIN
    SELECT e.description INTO wording FROM eggdb_finance.entries e WHERE e.entry_id = original_entry;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'entry % does not exist', original_entry USING ERRCODE = '42704';
    END IF;
    SELECT jsonb_agg(jsonb_build_object('account', p.account, 'amount', -p.amount) ORDER BY p.line) INTO opposite FROM eggdb_finance.postings p WHERE p.entry_id = original_entry;
    RETURN eggdb_finance.post('Reversal of ' || wording, opposite, original_entry);
END
$$;
CREATE FUNCTION eggdb_finance.balance(account_name text, at_time timestamptz DEFAULT now()) RETURNS numeric LANGUAGE plpgsql AS $$
DECLARE
    total numeric;
BEGIN
    SELECT coalesce(sum(p.amount), 0) INTO total FROM eggdb_finance.postings p JOIN eggdb_finance.entries e ON e.entry_id = p.entry_id WHERE p.account = account_name AND e.posted_at <= at_time;
    RETURN total;
END
$$;
CREATE FUNCTION eggdb_finance.trial_balance(at_time timestamptz DEFAULT now()) RETURNS TABLE (account text, kind text, currency text, debit numeric, credit numeric) LANGUAGE plpgsql AS $$
BEGIN
    RETURN QUERY SELECT a.account, a.kind, a.currency,
            greatest(coalesce(sum(p.amount), 0), 0), greatest(-coalesce(sum(p.amount), 0), 0)
        FROM eggdb_finance.accounts a
        LEFT JOIN eggdb_finance.postings p ON p.account = a.account AND p.entry_id IN (SELECT e.entry_id FROM eggdb_finance.entries e WHERE e.posted_at <= at_time)
        GROUP BY a.account, a.kind, a.currency
        ORDER BY a.account;
END
$$;
CREATE TABLE eggdb_finance.orders (order_id bigserial PRIMARY KEY, instrument text NOT NULL, side text NOT NULL CHECK (side IN ('buy', 'sell')), limit_price numeric CHECK (limit_price > 0), quantity numeric NOT NULL CHECK (quantity > 0), remaining numeric NOT NULL CHECK (remaining >= 0), status text NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'filled', 'cancelled')), owner text, placed_at timestamptz NOT NULL DEFAULT now());
CREATE INDEX orders_on_the_book ON eggdb_finance.orders (instrument, side, status, limit_price, order_id);
CREATE TABLE eggdb_finance.trades (trade_id bigserial PRIMARY KEY, instrument text NOT NULL, buy_order bigint NOT NULL REFERENCES eggdb_finance.orders (order_id), sell_order bigint NOT NULL REFERENCES eggdb_finance.orders (order_id), price numeric NOT NULL, quantity numeric NOT NULL, traded_at timestamptz NOT NULL DEFAULT now());
CREATE FUNCTION eggdb_finance.place_order(instrument_name text, order_side text, order_quantity numeric, order_price numeric DEFAULT NULL, order_owner text DEFAULT NULL, time_in_force text DEFAULT 'gtc') RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    placed bigint;
    left_over numeric := order_quantity;
    resting record;
    filled numeric;
BEGIN
    IF order_side NOT IN ('buy', 'sell') THEN
        RAISE EXCEPTION 'an order is a buy or a sell, not %', order_side USING ERRCODE = '22023';
    END IF;
    IF time_in_force NOT IN ('gtc', 'ioc', 'fok') THEN
        RAISE EXCEPTION 'time in force is gtc, ioc or fok, not %', time_in_force USING ERRCODE = '22023';
    END IF;
    INSERT INTO eggdb_finance.orders (instrument, side, limit_price, quantity, remaining, owner)
        VALUES (instrument_name, order_side, order_price, order_quantity, order_quantity, order_owner) RETURNING orders.order_id INTO placed;
    IF time_in_force = 'fok' AND (SELECT coalesce(sum(o.remaining), 0) FROM eggdb_finance.orders o
            WHERE o.instrument = instrument_name AND o.status = 'open' AND o.side <> order_side AND o.order_id <> placed
              AND (order_price IS NULL OR (order_side = 'buy' AND o.limit_price <= order_price) OR (order_side = 'sell' AND o.limit_price >= order_price))) < order_quantity THEN
        UPDATE eggdb_finance.orders SET status = 'cancelled' WHERE order_id = placed;
        RETURN placed;
    END IF;
    FOR resting IN SELECT o.order_id AS resting_id, o.limit_price AS resting_price, o.remaining AS resting_remaining FROM eggdb_finance.orders o
            WHERE o.instrument = instrument_name AND o.status = 'open' AND o.side <> order_side AND o.order_id <> placed AND o.limit_price IS NOT NULL
              AND (order_price IS NULL OR (order_side = 'buy' AND o.limit_price <= order_price) OR (order_side = 'sell' AND o.limit_price >= order_price))
            ORDER BY CASE WHEN order_side = 'buy' THEN o.limit_price ELSE -o.limit_price END, o.order_id LOOP
        EXIT WHEN left_over <= 0;
        filled := least(left_over, resting.resting_remaining);
        INSERT INTO eggdb_finance.trades (instrument, buy_order, sell_order, price, quantity)
            VALUES (instrument_name, CASE WHEN order_side = 'buy' THEN placed ELSE resting.resting_id END, CASE WHEN order_side = 'buy' THEN resting.resting_id ELSE placed END, resting.resting_price, filled);
        UPDATE eggdb_finance.orders SET remaining = remaining - filled, status = CASE WHEN remaining - filled = 0 THEN 'filled' ELSE 'open' END WHERE order_id = resting.resting_id;
        left_over := left_over - filled;
    END LOOP;
    UPDATE eggdb_finance.orders SET remaining = left_over,
            status = CASE WHEN left_over = 0 THEN 'filled' WHEN order_price IS NULL OR time_in_force <> 'gtc' THEN 'cancelled' ELSE 'open' END
        WHERE order_id = placed;
    RETURN placed;
END
$$;
CREATE FUNCTION eggdb_finance.cancel_order(cancelled_order bigint) RETURNS boolean LANGUAGE plpgsql AS $$
DECLARE
    changed bigint;
BEGIN
    UPDATE eggdb_finance.orders SET status = 'cancelled' WHERE order_id = cancelled_order AND status = 'open';
    GET DIAGNOSTICS changed = ROW_COUNT;
    RETURN changed > 0;
END
$$;
CREATE FUNCTION eggdb_finance.book(instrument_name text, depth integer DEFAULT 10) RETURNS TABLE (side text, price numeric, quantity numeric, orders bigint) LANGUAGE plpgsql AS $$
BEGIN
    RETURN QUERY SELECT b.side, b.price, b.quantity, b.orders FROM (
            (SELECT o.side, o.limit_price AS price, sum(o.remaining) AS quantity, count(*) AS orders FROM eggdb_finance.orders o
                WHERE o.instrument = instrument_name AND o.status = 'open' AND o.side = 'sell' GROUP BY o.side, o.limit_price ORDER BY o.limit_price LIMIT depth)
            UNION ALL
            (SELECT o.side, o.limit_price, sum(o.remaining), count(*) FROM eggdb_finance.orders o
                WHERE o.instrument = instrument_name AND o.status = 'open' AND o.side = 'buy' GROUP BY o.side, o.limit_price ORDER BY o.limit_price DESC LIMIT depth)
        ) b ORDER BY b.price DESC;
END
$$;
"#;

#[cfg(test)]
mod tests {
    use crate::memory_store::MemoryStore;
    use crate::tests::render;
    use crate::{Session, SqlError};

    fn run(session: &mut Session<'_>, sql: &str) -> Result<String, SqlError> {
        let mut outputs = Vec::new();
        session.execute(sql, &mut |result| outputs.push(render(&result)))?;
        Ok(outputs.pop().unwrap_or_default())
    }

    #[test]
    fn entries_balance_and_never_change() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for sql in [
            "CREATE EXTENSION eggdb_finance",
            "INSERT INTO eggdb_finance.accounts (account, kind) VALUES ('cash', 'asset'), ('sales', 'income')",
        ] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        assert_eq!(
            run(&mut session, "SELECT eggdb_finance.post('Egg sale', '[{\"account\": \"cash\", \"amount\": 30}, {\"account\": \"sales\", \"amount\": -30}]') AS entry").unwrap(),
            "entry\n1\nSELECT 1"
        );
        assert_eq!(
            run(&mut session, "SELECT eggdb_finance.post('Typo', '[{\"account\": \"cash\", \"amount\": 30}, {\"account\": \"sales\", \"amount\": -3}]')").unwrap_err().state().code(),
            "23514"
        );
        assert_eq!(run(&mut session, "UPDATE eggdb_finance.postings SET amount = 31 WHERE line = 1").unwrap_err().state().code(), "55000");
        assert_eq!(run(&mut session, "SELECT eggdb_finance.balance('cash') AS cash").unwrap(), "cash\n30\nSELECT 1");
        run(&mut session, "SELECT eggdb_finance.reverse(1)").unwrap();
        assert_eq!(
            run(&mut session, "SELECT account, debit, credit FROM eggdb_finance.trial_balance()").unwrap(),
            "account|debit|credit\ncash|0|0\nsales|0|0\nSELECT 2"
        );
    }

    #[test]
    fn orders_match_by_price_then_time() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for sql in [
            "CREATE EXTENSION eggdb_finance",
            "SELECT eggdb_finance.place_order('EGG', 'sell', 5, 101)",
            "SELECT eggdb_finance.place_order('EGG', 'sell', 5, 100)",
            "SELECT eggdb_finance.place_order('EGG', 'sell', 5, 100)",
            "SELECT eggdb_finance.place_order('EGG', 'buy', 4, 99)",
            "SELECT eggdb_finance.place_order('EGG', 'buy', 8, 100.5)",
        ] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        assert_eq!(
            run(&mut session, "SELECT buy_order, sell_order, price, quantity FROM eggdb_finance.trades ORDER BY trade_id").unwrap(),
            "buy_order|sell_order|price|quantity\n5|2|100|5\n5|3|100|3\nSELECT 2"
        );
        assert_eq!(
            run(&mut session, "SELECT side, price, quantity, orders FROM eggdb_finance.book('EGG')").unwrap(),
            "side|price|quantity|orders\nsell|101|5|1\nsell|100|2|1\nbuy|99|4|1\nSELECT 3"
        );
        run(&mut session, "SELECT eggdb_finance.place_order('EGG', 'buy', 10, 101, NULL, 'fok')").unwrap();
        assert_eq!(run(&mut session, "SELECT status FROM eggdb_finance.orders WHERE order_id = 6").unwrap(), "status\ncancelled\nSELECT 1");
    }
}
