ALTER TABLE orders ADD COLUMN status varchar(20) NOT NULL DEFAULT 'new';
CREATE INDEX orders_customer_idx ON orders (customer_id);
UPDATE orders SET status = 'shipped' WHERE placed_on < '2026-02-01';
