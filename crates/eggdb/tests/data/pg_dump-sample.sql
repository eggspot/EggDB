--
-- PostgreSQL database dump
--

\restrict s5VYFAIuKvUxLIMI6YEutFMwDUGcxrZyoOwiet3uQuLnWmcMdE6nqoa0fJdcaWr

-- Dumped from database version 18.6 (Homebrew)
-- Dumped by pg_dump version 18.6 (Homebrew)

SET statement_timeout = 0;
SET lock_timeout = 0;
SET idle_in_transaction_session_timeout = 0;
SET transaction_timeout = 0;
SET client_encoding = 'UTF8';
SET standard_conforming_strings = on;
SELECT pg_catalog.set_config('search_path', '', false);
SET check_function_bodies = false;
SET xmloption = content;
SET client_min_messages = warning;
SET row_security = off;

--
-- Name: bakery; Type: SCHEMA; Schema: -; Owner: postgres
--

CREATE SCHEMA bakery;


ALTER SCHEMA bakery OWNER TO postgres;

--
-- Name: SCHEMA bakery; Type: COMMENT; Schema: -; Owner: postgres
--

COMMENT ON SCHEMA bakery IS 'the bakery';


--
-- Name: touch_order(); Type: FUNCTION; Schema: bakery; Owner: postgres
--

CREATE FUNCTION bakery.touch_order() RETURNS trigger
    LANGUAGE plpgsql
    AS $$ BEGIN NEW.note := coalesce(NEW.note, ''); RETURN NEW; END $$;


ALTER FUNCTION bakery.touch_order() OWNER TO postgres;

SET default_tablespace = '';

SET default_table_access_method = heap;

--
-- Name: breads; Type: TABLE; Schema: bakery; Owner: postgres
--

CREATE TABLE bakery.breads (
    id integer NOT NULL,
    name text NOT NULL,
    price numeric(6,2) NOT NULL,
    baked date,
    CONSTRAINT breads_price_check CHECK ((price > (0)::numeric))
);


ALTER TABLE bakery.breads OWNER TO postgres;

--
-- Name: TABLE breads; Type: COMMENT; Schema: bakery; Owner: postgres
--

COMMENT ON TABLE bakery.breads IS 'what we bake';


--
-- Name: orders; Type: TABLE; Schema: bakery; Owner: postgres
--

CREATE TABLE bakery.orders (
    id integer NOT NULL,
    bread_id integer NOT NULL,
    quantity integer NOT NULL,
    note text,
    placed timestamp with time zone,
    CONSTRAINT orders_quantity_check CHECK ((quantity > 0))
);


ALTER TABLE bakery.orders OWNER TO postgres;

--
-- Name: best_sellers; Type: VIEW; Schema: bakery; Owner: postgres
--

CREATE VIEW bakery.best_sellers AS
 SELECT b.name,
    sum(o.quantity) AS sold
   FROM (bakery.breads b
     JOIN bakery.orders o ON ((o.bread_id = b.id)))
  GROUP BY b.name;


ALTER VIEW bakery.best_sellers OWNER TO postgres;

--
-- Name: breads_id_seq; Type: SEQUENCE; Schema: bakery; Owner: postgres
--

CREATE SEQUENCE bakery.breads_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


ALTER SEQUENCE bakery.breads_id_seq OWNER TO postgres;

--
-- Name: breads_id_seq; Type: SEQUENCE OWNED BY; Schema: bakery; Owner: postgres
--

ALTER SEQUENCE bakery.breads_id_seq OWNED BY bakery.breads.id;


--
-- Name: orders_id_seq; Type: SEQUENCE; Schema: bakery; Owner: postgres
--

ALTER TABLE bakery.orders ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME bakery.orders_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);


--
-- Name: breads id; Type: DEFAULT; Schema: bakery; Owner: postgres
--

ALTER TABLE ONLY bakery.breads ALTER COLUMN id SET DEFAULT nextval('bakery.breads_id_seq'::regclass);


--
-- Data for Name: breads; Type: TABLE DATA; Schema: bakery; Owner: postgres
--

COPY bakery.breads (id, name, price, baked) FROM stdin;
1	rye	3.50	2026-10-01
2	sourdough	4.25	2026-10-02
\.


--
-- Data for Name: orders; Type: TABLE DATA; Schema: bakery; Owner: postgres
--

COPY bakery.orders (id, bread_id, quantity, note, placed) FROM stdin;
1	1	3	tab\tinside	2026-10-03 08:00:00+00
2	1	4		2026-10-03 09:30:00+00
3	2	1	línea\ndos	2026-10-04 10:00:00+00
4	2	2	back\\slash	2026-10-04 11:15:00+00
\.


--
-- Name: breads_id_seq; Type: SEQUENCE SET; Schema: bakery; Owner: postgres
--

SELECT pg_catalog.setval('bakery.breads_id_seq', 2, true);


--
-- Name: orders_id_seq; Type: SEQUENCE SET; Schema: bakery; Owner: postgres
--

SELECT pg_catalog.setval('bakery.orders_id_seq', 4, true);


--
-- Name: breads breads_name_key; Type: CONSTRAINT; Schema: bakery; Owner: postgres
--

ALTER TABLE ONLY bakery.breads
    ADD CONSTRAINT breads_name_key UNIQUE (name);


--
-- Name: breads breads_pkey; Type: CONSTRAINT; Schema: bakery; Owner: postgres
--

ALTER TABLE ONLY bakery.breads
    ADD CONSTRAINT breads_pkey PRIMARY KEY (id);


--
-- Name: orders orders_pkey; Type: CONSTRAINT; Schema: bakery; Owner: postgres
--

ALTER TABLE ONLY bakery.orders
    ADD CONSTRAINT orders_pkey PRIMARY KEY (id);


--
-- Name: orders_bread_idx; Type: INDEX; Schema: bakery; Owner: postgres
--

CREATE INDEX orders_bread_idx ON bakery.orders USING btree (bread_id);


--
-- Name: orders orders_touch; Type: TRIGGER; Schema: bakery; Owner: postgres
--

CREATE TRIGGER orders_touch BEFORE INSERT ON bakery.orders FOR EACH ROW EXECUTE FUNCTION bakery.touch_order();


--
-- Name: orders orders_bread_id_fkey; Type: FK CONSTRAINT; Schema: bakery; Owner: postgres
--

ALTER TABLE ONLY bakery.orders
    ADD CONSTRAINT orders_bread_id_fkey FOREIGN KEY (bread_id) REFERENCES bakery.breads(id) ON DELETE CASCADE;


--
-- PostgreSQL database dump complete
--

\unrestrict s5VYFAIuKvUxLIMI6YEutFMwDUGcxrZyoOwiet3uQuLnWmcMdE6nqoa0fJdcaWr

