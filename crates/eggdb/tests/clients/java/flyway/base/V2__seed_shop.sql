INSERT INTO customers (email, name) VALUES ('ada@example.com', 'Ada'), ('linus@example.com', 'Linus');
INSERT INTO customers (email, name) VALUES ('grace@example.com', 'Grace');

INSERT INTO orders (customer_id, total, placed_on) VALUES (1, 19.99, '2026-01-05'), (1, 5.00, '2026-01-06');
INSERT INTO orders (customer_id, total, placed_on) VALUES (2, 120.50, '2026-02-01');
INSERT INTO orders (customer_id, total, placed_on) VALUES (3, 0.00, '2026-03-15');
