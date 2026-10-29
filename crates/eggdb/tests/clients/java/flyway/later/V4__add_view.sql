CREATE VIEW customer_totals AS
SELECT c.id, c.name, count(o.id) AS order_count, coalesce(sum(o.total), 0) AS spent
FROM customers c
LEFT JOIN orders o ON o.customer_id = c.id
GROUP BY c.id, c.name;
