select placed_on, count(*) as orders, sum(total) as revenue
from public.orders
group by placed_on
