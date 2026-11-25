CREATE TABLE authors (
    id integer PRIMARY KEY,
    name text NOT NULL,
    country text
);

CREATE TABLE books (
    id integer PRIMARY KEY,
    author_id integer NOT NULL REFERENCES authors (id),
    title text NOT NULL,
    published date,
    price numeric(8, 2) NOT NULL,
    summary text
);

CREATE TABLE orders (
    id integer PRIMARY KEY,
    book_id integer NOT NULL REFERENCES books (id),
    quantity integer NOT NULL CHECK (quantity > 0),
    ordered_at timestamptz NOT NULL
);

CREATE INDEX books_by_author ON books (author_id);

INSERT INTO authors VALUES
    (1, 'Ada Byron', 'United Kingdom'),
    (2, 'Grace Hopper', 'United States'),
    (3, 'Edsger Dijkstra', 'Netherlands');

INSERT INTO books VALUES
    (1, 1, 'Notes on the Analytical Engine', '1843-10-01', 19.90, 'The first published description of a program for a general-purpose computing machine.'),
    (2, 2, 'The Education of a Computer', '1952-05-02', 24.50, 'How a compiler lets a machine translate mathematical notation into its own code.'),
    (3, 3, 'A Discipline of Programming', '1976-01-01', 39.00, 'Weakest preconditions and the derivation of programs from their specifications.'),
    (4, 3, 'Go To Statement Considered Harmful', '1968-03-01', 9.99, 'A short letter on why structured control flow makes programs easier to reason about.');

INSERT INTO orders VALUES
    (1, 1, 2, '2026-11-01 09:30:00+00'),
    (2, 3, 1, '2026-11-02 14:05:00+00'),
    (3, 4, 5, '2026-11-02 16:45:00+00'),
    (4, 3, 2, '2026-11-03 08:10:00+00');
