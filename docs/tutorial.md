# A first tour of EggDB

This tour uses the sample bookshop in [samples/bookshop.sql](samples/bookshop.sql): three
tables of authors, books and orders. Every command is the `eggdb` binary of a release
(see the [README](../README.md) for installing it).

## 1. Load the sample

```sh
eggdb restore --create shop.eggdb docs/samples/bookshop.sql
```

`restore` reads the file as a PostgreSQL script and prints a report of the tables and rows it
loaded. The database is the single file `shop.eggdb`.

## 2. Ask it questions in SQL

```sh
eggdb sql shop.eggdb -c "SELECT a.name, b.title, b.price FROM books b JOIN authors a ON a.id = b.author_id ORDER BY b.price DESC"
eggdb sql shop.eggdb -c "SELECT b.title, sum(o.quantity) AS sold FROM orders o JOIN books b ON b.id = o.book_id GROUP BY b.title ORDER BY sold DESC"
```

The same data answers in another dialect, with that dialect's own syntax:

```sh
eggdb sql shop.eggdb --dialect mysql -c "SELECT title, DATE_FORMAT(published, '%Y') AS year FROM books LIMIT 2"
eggdb sql shop.eggdb --dialect sqlserver -c "SELECT TOP 1 title, price FROM books ORDER BY price"
```

## 3. Search the text

```sh
eggdb sql shop.eggdb -c "SELECT title FROM books WHERE to_tsvector('english', summary) @@ to_tsquery('english', 'program & reason')"
```

## 4. Keys and values beside the tables

```sh
eggdb put shop.eggdb greeting "Welcome to the bookshop"
eggdb get shop.eggdb greeting
eggdb scan shop.eggdb --prefix gr
```

The key-value entries live in the same file and the same transactions as the tables.

## 5. Explore interactively

```sh
eggdb shell shop.eggdb
```

The shell takes the key-value commands and SQL at one prompt; `eggdb ui shop.eggdb` browses the
entries full screen.

## 6. Serve it to existing tools

```sh
echo 'a long password' > pw.txt
eggdb serve shop.eggdb --password-file pw.txt --listen 127.0.0.1:5432
psql "host=127.0.0.1 port=5432 user=eggdb"
```

The server's one user is `eggdb` unless `--user` names another, and its password comes from the
file (or `EGGDB_PASSWORD`). PostgreSQL's `psql`, drivers and GUI clients connect as they would to
PostgreSQL. Add
`--mysql-listen 127.0.0.1:3306` for the `mysql` client and MySQL drivers, or `--tds-listen` for
SQL Server's. [cli.md](cli.md) lists every option, and [migrate.md](migrate.md) shows how to bring
an existing database over.
