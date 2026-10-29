--
-- PostgreSQL database dump
--

\restrict zEHXhsccvIZyflqhBrAY3g8v0EyOIaC7fH6mTc4MN9VLGeKZX6Fbf3QNEuKlQH6

-- Dumped from database version 16.13 (Homebrew)
-- Dumped by pg_dump version 16.13 (Homebrew)

SET statement_timeout = 0;
SET lock_timeout = 0;
SET idle_in_transaction_session_timeout = 0;
SET client_encoding = 'UTF8';
SET standard_conforming_strings = on;
SELECT pg_catalog.set_config('search_path', '', false);
SET check_function_bodies = false;
SET xmloption = content;
SET client_min_messages = warning;
SET row_security = off;

--
-- Name: shop; Type: SCHEMA; Schema: -; Owner: eggdb
--

CREATE SCHEMA shop;


ALTER SCHEMA shop OWNER TO eggdb;

--
-- Name: SCHEMA shop; Type: COMMENT; Schema: -; Owner: eggdb
--

COMMENT ON SCHEMA shop IS 'warehouses and their stock';


SET default_tablespace = '';

SET default_table_access_method = heap;

--
-- Name: customers; Type: TABLE; Schema: public; Owner: eggdb
--

CREATE TABLE public.customers (
    id integer NOT NULL,
    name text NOT NULL,
    email character varying(120),
    country character(2),
    active boolean DEFAULT true NOT NULL,
    created_at timestamp with time zone,
    note text
);


ALTER TABLE public.customers OWNER TO eggdb;

--
-- Name: TABLE customers; Type: COMMENT; Schema: public; Owner: eggdb
--

COMMENT ON TABLE public.customers IS 'people who buy';


--
-- Name: COLUMN customers.email; Type: COMMENT; Schema: public; Owner: eggdb
--

COMMENT ON COLUMN public.customers.email IS 'unique when present';


--
-- Name: active_customers; Type: VIEW; Schema: public; Owner: eggdb
--

CREATE VIEW public.active_customers AS
 SELECT id,
    name,
    email
   FROM public.customers
  WHERE active;


ALTER VIEW public.active_customers OWNER TO eggdb;

--
-- Name: VIEW active_customers; Type: COMMENT; Schema: public; Owner: eggdb
--

COMMENT ON VIEW public.active_customers IS 'customers who can order';


--
-- Name: blobs; Type: TABLE; Schema: public; Owner: eggdb
--

CREATE TABLE public.blobs (
    id integer NOT NULL,
    payload bytea,
    digest bytea
);


ALTER TABLE public.blobs OWNER TO eggdb;

--
-- Name: customers_id_seq; Type: SEQUENCE; Schema: public; Owner: eggdb
--

CREATE SEQUENCE public.customers_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


ALTER SEQUENCE public.customers_id_seq OWNER TO eggdb;

--
-- Name: customers_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: eggdb
--

ALTER SEQUENCE public.customers_id_seq OWNED BY public.customers.id;


--
-- Name: events; Type: TABLE; Schema: public; Owner: eggdb
--

CREATE TABLE public.events (
    id bigint NOT NULL,
    happened timestamp with time zone NOT NULL,
    duration interval,
    payload jsonb,
    tag text
);


ALTER TABLE public.events OWNER TO eggdb;

--
-- Name: events_id_seq; Type: SEQUENCE; Schema: public; Owner: eggdb
--

CREATE SEQUENCE public.events_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


ALTER SEQUENCE public.events_id_seq OWNER TO eggdb;

--
-- Name: events_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: eggdb
--

ALTER SEQUENCE public.events_id_seq OWNED BY public.events.id;


--
-- Name: invoice_number_seq; Type: SEQUENCE; Schema: public; Owner: eggdb
--

CREATE SEQUENCE public.invoice_number_seq
    START WITH 1000
    INCREMENT BY 7
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


ALTER SEQUENCE public.invoice_number_seq OWNER TO eggdb;

--
-- Name: ledger; Type: TABLE; Schema: public; Owner: eggdb
--

CREATE TABLE public.ledger (
    id integer NOT NULL,
    account bigint NOT NULL,
    amount numeric(20,6) NOT NULL,
    invoice integer DEFAULT nextval('public.invoice_number_seq'::regclass),
    memo text,
    posted date
);


ALTER TABLE public.ledger OWNER TO eggdb;

--
-- Name: ledger_id_seq; Type: SEQUENCE; Schema: public; Owner: eggdb
--

CREATE SEQUENCE public.ledger_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


ALTER SEQUENCE public.ledger_id_seq OWNER TO eggdb;

--
-- Name: ledger_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: eggdb
--

ALTER SEQUENCE public.ledger_id_seq OWNED BY public.ledger.id;


--
-- Name: log_lines; Type: TABLE; Schema: public; Owner: eggdb
--

CREATE TABLE public.log_lines (
    logged_at timestamp with time zone NOT NULL,
    level character varying(10) NOT NULL,
    message text,
    CONSTRAINT log_lines_level_check CHECK (((level)::text = ANY ((ARRAY['debug'::character varying, 'info'::character varying, 'warn'::character varying, 'error'::character varying])::text[])))
);


ALTER TABLE public.log_lines OWNER TO eggdb;

--
-- Name: measurements; Type: TABLE; Schema: public; Owner: eggdb
--

CREATE TABLE public.measurements (
    sensor smallint NOT NULL,
    taken timestamp without time zone NOT NULL,
    value double precision,
    ok boolean
);


ALTER TABLE public.measurements OWNER TO eggdb;

--
-- Name: order_items; Type: TABLE; Schema: public; Owner: eggdb
--

CREATE TABLE public.order_items (
    order_id bigint NOT NULL,
    line smallint NOT NULL,
    product_id integer NOT NULL,
    quantity integer NOT NULL,
    unit_price numeric(10,2) NOT NULL,
    CONSTRAINT order_items_quantity_check CHECK ((quantity > 0))
);


ALTER TABLE public.order_items OWNER TO eggdb;

--
-- Name: orders; Type: TABLE; Schema: public; Owner: eggdb
--

CREATE TABLE public.orders (
    id bigint NOT NULL,
    customer_id integer NOT NULL,
    ordered_on date NOT NULL,
    ordered_at timestamp without time zone,
    status text NOT NULL,
    total numeric,
    CONSTRAINT orders_status_check CHECK ((status = ANY (ARRAY['new'::text, 'paid'::text, 'shipped'::text, 'cancelled'::text])))
);


ALTER TABLE public.orders OWNER TO eggdb;

--
-- Name: order_totals; Type: VIEW; Schema: public; Owner: eggdb
--

CREATE VIEW public.order_totals AS
 SELECT c.id AS customer_id,
    c.name,
    count(o.id) AS orders,
    sum(o.total) AS total
   FROM (public.customers c
     JOIN public.orders o ON ((o.customer_id = c.id)))
  GROUP BY c.id, c.name;


ALTER VIEW public.order_totals OWNER TO eggdb;

--
-- Name: orders_id_seq; Type: SEQUENCE; Schema: public; Owner: eggdb
--

CREATE SEQUENCE public.orders_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


ALTER SEQUENCE public.orders_id_seq OWNER TO eggdb;

--
-- Name: orders_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: eggdb
--

ALTER SEQUENCE public.orders_id_seq OWNED BY public.orders.id;


--
-- Name: products; Type: TABLE; Schema: public; Owner: eggdb
--

CREATE TABLE public.products (
    id integer NOT NULL,
    sku uuid NOT NULL,
    title character varying(200) NOT NULL,
    price numeric(12,2) NOT NULL,
    weight real,
    rating double precision,
    attrs jsonb,
    raw json,
    CONSTRAINT products_price_check CHECK ((price >= (0)::numeric))
);


ALTER TABLE public.products OWNER TO eggdb;

--
-- Name: products_id_seq; Type: SEQUENCE; Schema: public; Owner: eggdb
--

CREATE SEQUENCE public.products_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


ALTER SEQUENCE public.products_id_seq OWNER TO eggdb;

--
-- Name: products_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: eggdb
--

ALTER SEQUENCE public.products_id_seq OWNED BY public.products.id;


--
-- Name: texts; Type: TABLE; Schema: public; Owner: eggdb
--

CREATE TABLE public.texts (
    id integer NOT NULL,
    body text,
    label character varying(40)
);


ALTER TABLE public.texts OWNER TO eggdb;

--
-- Name: stock; Type: TABLE; Schema: shop; Owner: eggdb
--

CREATE TABLE shop.stock (
    warehouse character(4) NOT NULL,
    product_id integer NOT NULL,
    on_hand integer DEFAULT 0 NOT NULL,
    updated timestamp with time zone
);


ALTER TABLE shop.stock OWNER TO eggdb;

--
-- Name: stock_by_product; Type: VIEW; Schema: shop; Owner: eggdb
--

CREATE VIEW shop.stock_by_product AS
 SELECT p.sku,
    p.title,
    s.warehouse,
    s.on_hand
   FROM (shop.stock s
     JOIN public.products p ON ((p.id = s.product_id)))
  WHERE (s.on_hand > 0);


ALTER VIEW shop.stock_by_product OWNER TO eggdb;

--
-- Name: warehouses; Type: TABLE; Schema: shop; Owner: eggdb
--

CREATE TABLE shop.warehouses (
    code character(4) NOT NULL,
    name text NOT NULL,
    opens time without time zone,
    closes time with time zone
);


ALTER TABLE shop.warehouses OWNER TO eggdb;

--
-- Name: TABLE warehouses; Type: COMMENT; Schema: shop; Owner: eggdb
--

COMMENT ON TABLE shop.warehouses IS 'where stock is kept';


--
-- Name: customers id; Type: DEFAULT; Schema: public; Owner: eggdb
--

ALTER TABLE ONLY public.customers ALTER COLUMN id SET DEFAULT nextval('public.customers_id_seq'::regclass);


--
-- Name: events id; Type: DEFAULT; Schema: public; Owner: eggdb
--

ALTER TABLE ONLY public.events ALTER COLUMN id SET DEFAULT nextval('public.events_id_seq'::regclass);


--
-- Name: ledger id; Type: DEFAULT; Schema: public; Owner: eggdb
--

ALTER TABLE ONLY public.ledger ALTER COLUMN id SET DEFAULT nextval('public.ledger_id_seq'::regclass);


--
-- Name: orders id; Type: DEFAULT; Schema: public; Owner: eggdb
--

ALTER TABLE ONLY public.orders ALTER COLUMN id SET DEFAULT nextval('public.orders_id_seq'::regclass);


--
-- Name: products id; Type: DEFAULT; Schema: public; Owner: eggdb
--

ALTER TABLE ONLY public.products ALTER COLUMN id SET DEFAULT nextval('public.products_id_seq'::regclass);


--
-- Data for Name: blobs; Type: TABLE DATA; Schema: public; Owner: eggdb
--

COPY public.blobs (id, payload, digest) FROM stdin;
1	\\x0102	\N
\.


--
-- Data for Name: customers; Type: TABLE DATA; Schema: public; Owner: eggdb
--

COPY public.customers (id, name, email, country, active, created_at, note) FROM stdin;
1	customer 1	c1@example.com	VN	t	2024-01-01 03:00:00+00	\N
2	customer 2	c2@example.com	VN	t	2024-01-01 06:00:00+00	\N
3	customer 3	c3@example.com	VN	f	2024-01-01 09:00:00+00	\N
4	customer 4	c4@example.com	VN	t	2024-01-01 12:00:00+00	\N
5	customer 5	c5@example.com	VN	t	2024-01-01 15:00:00+00	\N
6	customer 6	c6@example.com	VN	f	2024-01-01 18:00:00+00	\N
7	customer 7	c7@example.com	VN	t	2024-01-01 21:00:00+00	\N
8	customer 8	c8@example.com	VN	t	2024-01-02 00:00:00+00	\N
9	customer 9	c9@example.com	VN	f	2024-01-02 03:00:00+00	\N
10	customer 10	c10@example.com	VN	t	2024-01-02 06:00:00+00	\N
11	customer 11	c11@example.com	VN	t	2024-01-02 09:00:00+00	\N
12	customer 12	c12@example.com	VN	f	2024-01-02 12:00:00+00	\N
13	customer 13	c13@example.com	VN	t	2024-01-02 15:00:00+00	\N
14	customer 14	c14@example.com	VN	t	2024-01-02 18:00:00+00	\N
15	customer 15	c15@example.com	VN	f	2024-01-02 21:00:00+00	\N
16	customer 16	c16@example.com	VN	t	2024-01-03 00:00:00+00	\N
17	customer 17	c17@example.com	VN	t	2024-01-03 03:00:00+00	\N
18	customer 18	c18@example.com	VN	f	2024-01-03 06:00:00+00	\N
19	customer 19	c19@example.com	VN	t	2024-01-03 09:00:00+00	\N
20	customer 20	c20@example.com	VN	t	2024-01-03 12:00:00+00	\N
\.


--
-- Data for Name: events; Type: TABLE DATA; Schema: public; Owner: eggdb
--

COPY public.events (id, happened, duration, payload, tag) FROM stdin;
1	2025-01-01 00:00:00+00	1 day	\N	deploy
\.


--
-- Data for Name: ledger; Type: TABLE DATA; Schema: public; Owner: eggdb
--

COPY public.ledger (id, account, amount, invoice, memo, posted) FROM stdin;
1	1000000000000	1.250000	1000	entry	2020-02-29
\.


--
-- Data for Name: log_lines; Type: TABLE DATA; Schema: public; Owner: eggdb
--

COPY public.log_lines (logged_at, level, message) FROM stdin;
2025-02-01 00:00:00+00	info	ok
\.


--
-- Data for Name: measurements; Type: TABLE DATA; Schema: public; Owner: eggdb
--

COPY public.measurements (sensor, taken, value, ok) FROM stdin;
1	2025-01-01 00:00:00	1.5	t
\.


--
-- Data for Name: order_items; Type: TABLE DATA; Schema: public; Owner: eggdb
--

COPY public.order_items (order_id, line, product_id, quantity, unit_price) FROM stdin;
2	1	2	2	0.50
3	1	3	3	1.00
4	1	4	4	1.50
5	1	5	5	2.00
6	1	6	6	2.50
7	1	7	7	3.00
8	1	8	8	3.50
9	1	9	9	4.00
10	1	10	1	4.50
11	1	1	2	5.00
12	1	2	3	5.50
13	1	3	4	6.00
14	1	4	5	6.50
15	1	5	6	7.00
16	1	6	7	7.50
17	1	7	8	8.00
18	1	8	9	8.50
19	1	9	1	9.00
20	1	10	2	9.50
21	1	1	3	10.00
22	1	2	4	10.50
23	1	3	5	11.00
24	1	4	6	11.50
25	1	5	7	12.00
26	1	6	8	12.50
27	1	7	9	13.00
28	1	8	1	13.50
29	1	9	2	14.00
30	1	10	3	14.50
1	2	1	4	15.00
2	2	2	5	15.50
3	2	3	6	16.00
4	2	4	7	16.50
5	2	5	8	17.00
6	2	6	9	17.50
7	2	7	1	18.00
8	2	8	2	18.50
9	2	9	3	19.00
10	2	10	4	19.50
11	2	1	5	20.00
\.


--
-- Data for Name: orders; Type: TABLE DATA; Schema: public; Owner: eggdb
--

COPY public.orders (id, customer_id, ordered_on, ordered_at, status, total) FROM stdin;
1	2	2024-01-02	2024-01-01 08:41:00	paid	3.25
2	3	2024-01-03	2024-01-01 09:22:00	shipped	6.50
3	4	2024-01-04	2024-01-01 10:03:00	cancelled	9.75
4	5	2024-01-05	2024-01-01 10:44:00	new	13.00
5	6	2024-01-06	2024-01-01 11:25:00	paid	16.25
6	7	2024-01-07	2024-01-01 12:06:00	shipped	19.50
7	8	2024-01-08	2024-01-01 12:47:00	cancelled	22.75
8	9	2024-01-09	2024-01-01 13:28:00	new	26.00
9	10	2024-01-10	2024-01-01 14:09:00	paid	29.25
10	11	2024-01-11	2024-01-01 14:50:00	shipped	32.50
11	12	2024-01-12	2024-01-01 15:31:00	cancelled	35.75
12	13	2024-01-13	2024-01-01 16:12:00	new	39.00
13	14	2024-01-14	2024-01-01 16:53:00	paid	42.25
14	15	2024-01-15	2024-01-01 17:34:00	shipped	45.50
15	16	2024-01-16	2024-01-01 18:15:00	cancelled	48.75
16	17	2024-01-17	2024-01-01 18:56:00	new	52.00
17	18	2024-01-18	2024-01-01 19:37:00	paid	55.25
18	19	2024-01-19	2024-01-01 20:18:00	shipped	58.50
19	20	2024-01-20	2024-01-01 20:59:00	cancelled	61.75
20	1	2024-01-21	2024-01-01 21:40:00	new	65.00
21	2	2024-01-22	2024-01-01 22:21:00	paid	68.25
22	3	2024-01-23	2024-01-01 23:02:00	shipped	71.50
23	4	2024-01-24	2024-01-01 23:43:00	cancelled	74.75
24	5	2024-01-25	2024-01-02 00:24:00	new	78.00
25	6	2024-01-26	2024-01-02 01:05:00	paid	81.25
26	7	2024-01-27	2024-01-02 01:46:00	shipped	84.50
27	8	2024-01-28	2024-01-02 02:27:00	cancelled	87.75
28	9	2024-01-29	2024-01-02 03:08:00	new	91.00
29	10	2024-01-30	2024-01-02 03:49:00	paid	94.25
30	11	2024-01-31	2024-01-02 04:30:00	shipped	97.50
\.


--
-- Data for Name: products; Type: TABLE DATA; Schema: public; Owner: eggdb
--

COPY public.products (id, sku, title, price, weight, rating, attrs, raw) FROM stdin;
1	8402e99c-5b52-5201-6079-4615a8161683	product 1	1.50	0.125	0.25	\N	\N
2	815b505d-9619-dfa9-a7c1-73acd8897fce	product 2	3.00	0.25	0.5	\N	\N
3	8a203a0d-515d-eb1c-42a1-d176e7da3b92	product 3	4.50	0.375	0.75	\N	\N
4	90d6d188-6f4f-ba57-a67f-f5c326fdecee	product 4	6.00	0.5	1	\N	\N
5	cd606c61-d9fb-1c37-599f-1f483621e929	product 5	7.50	0.625	1.25	\N	\N
6	ef329fbc-031f-8a6e-ecc7-1f2ba8f99f58	product 6	9.00	0.75	1.5	\N	\N
7	21801766-4817-f076-0936-3db9d742de97	product 7	10.50	0.875	1.75	\N	\N
8	57502f18-6469-ca7a-c9d1-c0585b163bdf	product 8	12.00	1	2	\N	\N
9	f60dbded-2db5-717a-c37c-bcacaa95316a	product 9	13.50	1.125	2.25	\N	\N
10	19ab8a31-629d-db5d-7723-64c1c96704d3	product 10	15.00	1.25	2.5	\N	\N
\.


--
-- Data for Name: texts; Type: TABLE DATA; Schema: public; Owner: eggdb
--

COPY public.texts (id, body, label) FROM stdin;
1	body	label
\.


--
-- Data for Name: stock; Type: TABLE DATA; Schema: shop; Owner: eggdb
--

COPY shop.stock (warehouse, product_id, on_hand, updated) FROM stdin;
HAN1	1	3	2025-06-01 12:00:00+00
HAN1	2	6	2025-06-01 12:00:00+00
HAN1	3	9	2025-06-01 12:00:00+00
HAN1	4	12	2025-06-01 12:00:00+00
HAN1	5	15	2025-06-01 12:00:00+00
HAN1	6	18	2025-06-01 12:00:00+00
HAN1	7	21	2025-06-01 12:00:00+00
HAN1	8	24	2025-06-01 12:00:00+00
HAN1	9	27	2025-06-01 12:00:00+00
HAN1	10	30	2025-06-01 12:00:00+00
SGN1	1	3	2025-06-01 12:00:00+00
SGN1	2	6	2025-06-01 12:00:00+00
SGN1	3	9	2025-06-01 12:00:00+00
SGN1	4	12	2025-06-01 12:00:00+00
SGN1	5	15	2025-06-01 12:00:00+00
SGN1	6	18	2025-06-01 12:00:00+00
SGN1	7	21	2025-06-01 12:00:00+00
SGN1	8	24	2025-06-01 12:00:00+00
SGN1	9	27	2025-06-01 12:00:00+00
SGN1	10	30	2025-06-01 12:00:00+00
\.


--
-- Data for Name: warehouses; Type: TABLE DATA; Schema: shop; Owner: eggdb
--

COPY shop.warehouses (code, name, opens, closes) FROM stdin;
HAN1	Hà Nội	07:30:00	18:00:00+07
SGN1	Sài Gòn	08:00:00.5	20:30:00+07
\.


--
-- Name: customers_id_seq; Type: SEQUENCE SET; Schema: public; Owner: eggdb
--

SELECT pg_catalog.setval('public.customers_id_seq', 20, true);


--
-- Name: events_id_seq; Type: SEQUENCE SET; Schema: public; Owner: eggdb
--

SELECT pg_catalog.setval('public.events_id_seq', 1, true);


--
-- Name: invoice_number_seq; Type: SEQUENCE SET; Schema: public; Owner: eggdb
--

SELECT pg_catalog.setval('public.invoice_number_seq', 5000, true);


--
-- Name: ledger_id_seq; Type: SEQUENCE SET; Schema: public; Owner: eggdb
--

SELECT pg_catalog.setval('public.ledger_id_seq', 1, true);


--
-- Name: orders_id_seq; Type: SEQUENCE SET; Schema: public; Owner: eggdb
--

SELECT pg_catalog.setval('public.orders_id_seq', 30, true);


--
-- Name: products_id_seq; Type: SEQUENCE SET; Schema: public; Owner: eggdb
--

SELECT pg_catalog.setval('public.products_id_seq', 10, true);


--
-- Name: blobs blobs_pkey; Type: CONSTRAINT; Schema: public; Owner: eggdb
--

ALTER TABLE ONLY public.blobs
    ADD CONSTRAINT blobs_pkey PRIMARY KEY (id);


--
-- Name: customers customers_email_key; Type: CONSTRAINT; Schema: public; Owner: eggdb
--

ALTER TABLE ONLY public.customers
    ADD CONSTRAINT customers_email_key UNIQUE (email);


--
-- Name: customers customers_pkey; Type: CONSTRAINT; Schema: public; Owner: eggdb
--

ALTER TABLE ONLY public.customers
    ADD CONSTRAINT customers_pkey PRIMARY KEY (id);


--
-- Name: events events_pkey; Type: CONSTRAINT; Schema: public; Owner: eggdb
--

ALTER TABLE ONLY public.events
    ADD CONSTRAINT events_pkey PRIMARY KEY (id);


--
-- Name: ledger ledger_pkey; Type: CONSTRAINT; Schema: public; Owner: eggdb
--

ALTER TABLE ONLY public.ledger
    ADD CONSTRAINT ledger_pkey PRIMARY KEY (id);


--
-- Name: measurements measurements_pkey; Type: CONSTRAINT; Schema: public; Owner: eggdb
--

ALTER TABLE ONLY public.measurements
    ADD CONSTRAINT measurements_pkey PRIMARY KEY (sensor, taken);


--
-- Name: order_items order_items_pkey; Type: CONSTRAINT; Schema: public; Owner: eggdb
--

ALTER TABLE ONLY public.order_items
    ADD CONSTRAINT order_items_pkey PRIMARY KEY (order_id, line);


--
-- Name: orders orders_pkey; Type: CONSTRAINT; Schema: public; Owner: eggdb
--

ALTER TABLE ONLY public.orders
    ADD CONSTRAINT orders_pkey PRIMARY KEY (id);


--
-- Name: products products_pkey; Type: CONSTRAINT; Schema: public; Owner: eggdb
--

ALTER TABLE ONLY public.products
    ADD CONSTRAINT products_pkey PRIMARY KEY (id);


--
-- Name: products products_sku_key; Type: CONSTRAINT; Schema: public; Owner: eggdb
--

ALTER TABLE ONLY public.products
    ADD CONSTRAINT products_sku_key UNIQUE (sku);


--
-- Name: texts texts_pkey; Type: CONSTRAINT; Schema: public; Owner: eggdb
--

ALTER TABLE ONLY public.texts
    ADD CONSTRAINT texts_pkey PRIMARY KEY (id);


--
-- Name: stock stock_pkey; Type: CONSTRAINT; Schema: shop; Owner: eggdb
--

ALTER TABLE ONLY shop.stock
    ADD CONSTRAINT stock_pkey PRIMARY KEY (warehouse, product_id);


--
-- Name: warehouses warehouses_name_key; Type: CONSTRAINT; Schema: shop; Owner: eggdb
--

ALTER TABLE ONLY shop.warehouses
    ADD CONSTRAINT warehouses_name_key UNIQUE (name);


--
-- Name: warehouses warehouses_pkey; Type: CONSTRAINT; Schema: shop; Owner: eggdb
--

ALTER TABLE ONLY shop.warehouses
    ADD CONSTRAINT warehouses_pkey PRIMARY KEY (code);


--
-- Name: events_happened_idx; Type: INDEX; Schema: public; Owner: eggdb
--

CREATE INDEX events_happened_idx ON public.events USING btree (happened);


--
-- Name: ledger_account_invoice_idx; Type: INDEX; Schema: public; Owner: eggdb
--

CREATE INDEX ledger_account_invoice_idx ON public.ledger USING btree (account, invoice);


--
-- Name: log_lines_level_idx; Type: INDEX; Schema: public; Owner: eggdb
--

CREATE INDEX log_lines_level_idx ON public.log_lines USING btree (level);


--
-- Name: orders_customer_idx; Type: INDEX; Schema: public; Owner: eggdb
--

CREATE INDEX orders_customer_idx ON public.orders USING btree (customer_id);


--
-- Name: orders_placed_idx; Type: INDEX; Schema: public; Owner: eggdb
--

CREATE INDEX orders_placed_idx ON public.orders USING btree (ordered_on, status);


--
-- Name: stock_product_idx; Type: INDEX; Schema: shop; Owner: eggdb
--

CREATE INDEX stock_product_idx ON shop.stock USING btree (product_id);


--
-- Name: order_items order_items_order_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: eggdb
--

ALTER TABLE ONLY public.order_items
    ADD CONSTRAINT order_items_order_id_fkey FOREIGN KEY (order_id) REFERENCES public.orders(id) ON DELETE CASCADE;


--
-- Name: order_items order_items_product_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: eggdb
--

ALTER TABLE ONLY public.order_items
    ADD CONSTRAINT order_items_product_id_fkey FOREIGN KEY (product_id) REFERENCES public.products(id);


--
-- Name: orders orders_customer_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: eggdb
--

ALTER TABLE ONLY public.orders
    ADD CONSTRAINT orders_customer_id_fkey FOREIGN KEY (customer_id) REFERENCES public.customers(id) ON DELETE CASCADE;


--
-- Name: stock stock_product_id_fkey; Type: FK CONSTRAINT; Schema: shop; Owner: eggdb
--

ALTER TABLE ONLY shop.stock
    ADD CONSTRAINT stock_product_id_fkey FOREIGN KEY (product_id) REFERENCES public.products(id) ON DELETE CASCADE;


--
-- Name: stock stock_warehouse_fkey; Type: FK CONSTRAINT; Schema: shop; Owner: eggdb
--

ALTER TABLE ONLY shop.stock
    ADD CONSTRAINT stock_warehouse_fkey FOREIGN KEY (warehouse) REFERENCES shop.warehouses(code) ON UPDATE CASCADE;


--
-- PostgreSQL database dump complete
--

\unrestrict zEHXhsccvIZyflqhBrAY3g8v0EyOIaC7fH6mTc4MN9VLGeKZX6Fbf3QNEuKlQH6

