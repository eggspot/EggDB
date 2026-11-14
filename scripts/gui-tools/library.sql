CREATE SCHEMA lending;

CREATE TABLE public.authors (
    id integer GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name text NOT NULL,
    born date
);

CREATE TABLE public.books (
    id serial PRIMARY KEY,
    author_id integer NOT NULL REFERENCES public.authors (id),
    title varchar(200) NOT NULL,
    price numeric(8, 2) DEFAULT 0 CHECK (price >= 0),
    tags text[]
);

CREATE INDEX books_title ON public.books (title);

CREATE TABLE lending.loans (
    book_id integer NOT NULL REFERENCES public.books (id),
    borrowed_at timestamptz NOT NULL DEFAULT now(),
    returned_at timestamptz
);

CREATE VIEW public.book_titles AS
    SELECT b.id, b.title, a.name AS author
    FROM public.books b JOIN public.authors a ON a.id = b.author_id;

CREATE SEQUENCE lending.ticket_numbers START 100;

CREATE FUNCTION public.book_count(author integer) RETURNS bigint
    LANGUAGE sql STABLE
    AS 'SELECT count(*) FROM public.books WHERE author_id = author';

COMMENT ON TABLE public.books IS 'Books on the shelves';

INSERT INTO public.authors (name, born) VALUES ('Ursula K. Le Guin', '1929-10-21'), ('Italo Calvino', '1923-10-15');
INSERT INTO public.books (author_id, title, price, tags) VALUES
    (1, 'The Dispossessed', 9.99, ARRAY['novel', 'utopia']),
    (1, 'The Lathe of Heaven', 7.50, ARRAY['novel']),
    (2, 'Invisible Cities', 11.00, NULL);
