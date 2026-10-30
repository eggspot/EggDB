import datetime
import decimal
import sys
from typing import List, Optional

import psycopg
import sqlalchemy as sa
from sqlalchemy import orm


def shown(value):
    return "NULL" if value is None else str(value)


def line(*cells):
    print("\t".join(shown(cell) for cell in cells))


def section(name, body, *arguments):
    print("== " + name)
    try:
        body(*arguments)
    except (psycopg.Error, sa.exc.DBAPIError) as error:
        state = getattr(error, "sqlstate", None) or getattr(getattr(error, "orig", None), "sqlstate", None)
        line("error", state)


def psycopg_crud(conninfo):
    with psycopg.connect(conninfo) as connection:
        connection.isolation_level = psycopg.IsolationLevel.SERIALIZABLE
        line("isolation", connection.execute("SHOW TRANSACTION ISOLATION LEVEL").fetchone()[0])
        connection.execute("DROP TABLE IF EXISTS app_items")
        connection.execute(
            "CREATE TABLE app_items (id serial PRIMARY KEY, name text NOT NULL, "
            "price numeric(8,2), seen timestamptz, quantity integer)"
        )
        connection.commit()
        with connection.cursor() as cursor:
            cursor.executemany(
                "INSERT INTO app_items (name, price, seen, quantity) VALUES (%s, %s, %s, %s)",
                [
                    (
                        name,
                        decimal.Decimal(price),
                        datetime.datetime(2026, 10, day, 8, 30, tzinfo=datetime.timezone.utc),
                        quantity,
                    )
                    for name, price, day, quantity in [
                        ("a", "1.50", 1, 3),
                        ("b", "2.25", 2, None),
                        ("c", "10.00", 3, 7),
                    ]
                ],
            )
            line("inserted", cursor.rowcount)
            cursor.execute("INSERT INTO app_items (name) VALUES (%s) RETURNING id, price", ("returned",))
            line("returned", *cursor.fetchone())
            cursor.execute(
                "SELECT id, name, price, seen, quantity FROM app_items WHERE price > %s ORDER BY id",
                (decimal.Decimal("2.00"),),
            )
            for row in cursor:
                line(*row)
            cursor.execute("UPDATE app_items SET price = price * 2 WHERE name = %s", ("b",))
            line("updated", cursor.rowcount)
            cursor.execute("DELETE FROM app_items WHERE id = %s", (1,))
            line("deleted", cursor.rowcount)
        connection.commit()
        connection.execute("INSERT INTO app_items (name) VALUES ('rolled back')")
        connection.rollback()
        line("after rollback", connection.execute("SELECT count(*) FROM app_items").fetchone()[0])
        for row in connection.execute("SELECT id, name, price, quantity FROM app_items ORDER BY id"):
            line(*row)
        connection.commit()
        connection.execute("DROP TABLE app_items")
        connection.commit()


COPY_TYPES = ["int4", "text", "numeric", "timestamptz", "bytea", "text[]", "int4"]


def copied_bytes(cursor, statement):
    with cursor.copy(statement) as copy:
        return b"".join(bytes(piece) for piece in copy)


def copy_rows(connection):
    with connection.cursor() as cursor:
        with cursor.copy("COPY app_copies FROM STDIN (FORMAT BINARY)") as copy:
            copy.set_types(COPY_TYPES)
            copy.write_row(
                (
                    1,
                    "a",
                    decimal.Decimal("1.50"),
                    datetime.datetime(2026, 10, 1, 8, 30, tzinfo=datetime.timezone.utc),
                    b"\x00\xff",
                    ["x", "y z", None],
                    3,
                )
            )
            copy.write_row((2, "café €", None, None, None, [], None))
        line("binary in", cursor.rowcount)
        with cursor.copy("COPY (SELECT * FROM app_copies ORDER BY id) TO STDOUT (FORMAT BINARY)") as copy:
            copy.set_types(COPY_TYPES)
            for row in copy.rows():
                line("binary out", *row)
        line("binary bytes", copied_bytes(cursor, "COPY (SELECT id, name, tags FROM app_copies ORDER BY id) TO STDOUT (FORMAT BINARY)").hex())
        with cursor.copy(
            "COPY app_copies (id, name, quantity) FROM STDIN (ON_ERROR ignore, LOG_VERBOSITY verbose)"
        ) as copy:
            copy.write("3\tc\t7\n4\td\tlots\n5\te\t9\n")
        line("ignored", cursor.rowcount)
        with cursor.copy("COPY app_copies (id, name) FROM STDIN WHERE id % 2 = 0") as copy:
            copy.write("6\tf\n7\tg\n8\th\n")
        line("where", cursor.rowcount)
        with cursor.copy("COPY app_copies (id, name) FROM STDIN (ENCODING 'LATIN1')") as copy:
            copy.write(b"9\tna\xefve\n")
        line("latin1", cursor.rowcount)
        line(
            "win1252",
            copied_bytes(
                cursor,
                "COPY (SELECT name FROM app_copies WHERE id IN (2, 9) ORDER BY id) TO STDOUT (ENCODING 'WIN1252')",
            ).hex(),
        )
        for row in connection.execute("SELECT id, name, quantity FROM app_copies ORDER BY id"):
            line(*row)


def psycopg_copy(conninfo):
    with psycopg.connect(conninfo, autocommit=True) as connection:
        connection.add_notice_handler(
            lambda notice: line("notice", notice.severity, notice.message_primary)
        )
        connection.execute("DROP TABLE IF EXISTS app_copies")
        connection.execute(
            "CREATE TABLE app_copies (id integer PRIMARY KEY, name text, price numeric(8,2), "
            "seen timestamptz, raw bytea, tags text[], quantity integer)"
        )
        try:
            copy_rows(connection)
        finally:
            connection.execute("DROP TABLE app_copies")


def reflected_default(column):
    default = column.server_default
    return None if default is None else str(default.arg)


def show_table(table):
    line("table", table.schema, table.name, table.comment)
    for column in table.columns:
        line(
            "column",
            column.name,
            str(column.type),
            column.nullable,
            column.primary_key,
            reflected_default(column),
            column.comment,
        )
    primary_key = table.primary_key
    line("primary key", primary_key.name, *[column.name for column in primary_key.columns])
    for key in sorted(table.foreign_key_constraints, key=lambda key: key.name or ""):
        line(
            "foreign key",
            key.name,
            ",".join(column.name for column in key.columns),
            ",".join(element.target_fullname for element in key.elements),
            key.ondelete,
            key.onupdate,
        )
    unique = [
        constraint
        for constraint in table.constraints
        if isinstance(constraint, sa.UniqueConstraint)
    ]
    for constraint in sorted(unique, key=lambda constraint: constraint.name or ""):
        line("unique", constraint.name, ",".join(column.name for column in constraint.columns))
    checks = [
        constraint
        for constraint in table.constraints
        if isinstance(constraint, sa.CheckConstraint)
    ]
    for constraint in sorted(checks, key=lambda constraint: constraint.name or ""):
        line("check", constraint.name, str(constraint.sqltext))
    for index in sorted(table.indexes, key=lambda index: index.name or ""):
        line("index", index.name, index.unique, ",".join(column.name for column in index.columns))


def sqlalchemy_reflect(url):
    engine = sa.create_engine(url)
    try:
        for schema in [None, "shop"]:
            metadata = sa.MetaData()
            metadata.reflect(bind=engine, schema=schema)
            for name in sorted(metadata.tables):
                show_table(metadata.tables[name])
        inspector = sa.inspect(engine)
        line("schemas", *sorted(inspector.get_schema_names()))
        line("views", *sorted(inspector.get_view_names()))
        line("sequences", *sorted(inspector.get_sequence_names()))
        line("has orders", inspector.has_table("orders"), inspector.has_table("missing"))
        metadata = sa.MetaData()
        customers = sa.Table("customers", metadata, autoload_with=engine)
        orders = sa.Table("orders", metadata, autoload_with=engine)
        query = (
            sa.select(customers.c.id, customers.c.name, sa.func.count(orders.c.id).label("orders"))
            .select_from(customers.join(orders, orders.c.customer_id == customers.c.id))
            .where(customers.c.id <= 5)
            .group_by(customers.c.id, customers.c.name)
            .order_by(customers.c.id)
        )
        with engine.connect() as connection:
            for row in connection.execute(query):
                line(*row)
    finally:
        engine.dispose()


class OrmModel(orm.DeclarativeBase):
    pass


class Author(OrmModel):
    __tablename__ = "orm_authors"

    id: orm.Mapped[int] = orm.mapped_column(primary_key=True)
    name: orm.Mapped[str] = orm.mapped_column(sa.String(60), unique=True)
    born: orm.Mapped[Optional[datetime.date]]
    books: orm.Mapped[List["Book"]] = orm.relationship(
        back_populates="author", cascade="all, delete-orphan", order_by="Book.id"
    )


class Book(OrmModel):
    __tablename__ = "orm_books"

    id: orm.Mapped[int] = orm.mapped_column(primary_key=True)
    title: orm.Mapped[str] = orm.mapped_column(sa.String(120))
    price: orm.Mapped[decimal.Decimal] = orm.mapped_column(sa.Numeric(8, 2))
    stocked: orm.Mapped[bool] = orm.mapped_column(default=True)
    author_id: orm.Mapped[int] = orm.mapped_column(sa.ForeignKey("orm_authors.id"))
    author: orm.Mapped[Author] = orm.relationship(back_populates="books")


def show_author(author):
    line("author", author.id, author.name, author.born)
    for book in author.books:
        line("book", book.id, book.title, book.price, book.stocked)


def orm_create(engine):
    OrmModel.metadata.drop_all(engine)
    OrmModel.metadata.create_all(engine)
    inspector = sa.inspect(engine)
    line("created", inspector.has_table("orm_authors"), inspector.has_table("orm_books"))
    for column in inspector.get_columns("orm_books"):
        line("column", column["name"], column["type"], column["nullable"])
    for key in inspector.get_foreign_keys("orm_books"):
        line("foreign key", ",".join(key["constrained_columns"]), key["referred_table"])


def orm_insert(engine):
    with orm.Session(engine) as session:
        session.add_all(
            [
                Author(
                    name="Ada",
                    born=datetime.date(1815, 12, 10),
                    books=[
                        Book(title="Notes", price=decimal.Decimal("12.50")),
                        Book(title="Sketch", price=decimal.Decimal("7.25"), stocked=False),
                    ],
                ),
                Author(name="Grace", books=[Book(title="Compilers", price=decimal.Decimal("30.00"))]),
                Author(name="Edsger", born=datetime.date(1930, 5, 11)),
            ]
        )
        session.commit()
        for author in session.scalars(sa.select(Author).order_by(Author.id)):
            show_author(author)


def orm_query(engine):
    with orm.Session(engine) as session:
        books = session.scalars(
            sa.select(Book)
            .join(Book.author)
            .where(Author.name.in_(["Ada", "Grace"]), Book.price > decimal.Decimal("5"))
            .order_by(Book.price.desc())
        )
        for book in books:
            line("priced", book.title, book.price, book.author.name)
        authors = session.scalars(
            sa.select(Author).options(orm.selectinload(Author.books)).where(Author.born.is_not(None)).order_by(Author.name)
        )
        for author in authors:
            line("born", author.name, author.born, len(author.books))
        totals = session.execute(
            sa.select(Author.name, sa.func.count(Book.id), sa.func.sum(Book.price))
            .outerjoin(Author.books)
            .group_by(Author.name)
            .order_by(Author.name)
        )
        for name, count, total in totals:
            line("total", name, count, total)
        line("page", *session.scalars(sa.select(Book.title).order_by(Book.title).limit(2).offset(1)))
        line("first", session.get(Author, 2).name, session.get(Author, 99))


def orm_update(engine):
    with orm.Session(engine) as session:
        sketch = session.scalars(sa.select(Book).where(Book.title == "Sketch")).one()
        sketch.price = decimal.Decimal("8.00")
        sketch.stocked = True
        grace = session.scalars(sa.select(Author).where(Author.name == "Grace")).one()
        grace.born = datetime.date(1906, 12, 9)
        grace.books.append(Book(title="Nanoseconds", price=decimal.Decimal("1.00")))
        session.commit()
        line("updated", sketch.price, sketch.stocked, grace.born, len(grace.books))
        changed = session.execute(
            sa.update(Book).where(Book.price < decimal.Decimal("10")).values(stocked=False)
        )
        session.commit()
        line("bulk updated", changed.rowcount)


def orm_rollback(engine):
    with orm.Session(engine) as session:
        session.add(Author(name="Temporary", books=[Book(title="Draft", price=decimal.Decimal("0.50"))]))
        session.flush()
        line("flushed", session.scalar(sa.select(sa.func.count()).select_from(Author)))
        session.rollback()
        line("rolled back", session.scalar(sa.select(sa.func.count()).select_from(Author)))
        session.add(Author(name="Ada"))
        try:
            session.commit()
        except sa.exc.IntegrityError as error:
            line("duplicate", error.orig.sqlstate)
            session.rollback()
        line("after duplicate", session.scalar(sa.select(sa.func.count()).select_from(Author)))


def orm_delete(engine):
    with orm.Session(engine) as session:
        ada = session.scalars(sa.select(Author).where(Author.name == "Ada")).one()
        session.delete(ada)
        session.commit()
        line("authors", *session.scalars(sa.select(Author.name).order_by(Author.id)))
        line("books", *session.scalars(sa.select(Book.title).order_by(Book.id)))
    OrmModel.metadata.drop_all(engine)
    line("dropped", sa.inspect(engine).has_table("orm_authors"))


def sqlalchemy_orm(url):
    engine = sa.create_engine(url)
    try:
        for name, body in [
            ("create", orm_create),
            ("insert", orm_insert),
            ("query", orm_query),
            ("update", orm_update),
            ("rollback", orm_rollback),
            ("delete", orm_delete),
        ]:
            section("sqlalchemy orm " + name, body, engine)
    finally:
        engine.dispose()


def main():
    if len(sys.argv) != 3:
        print("usage: python app_check.py <libpq conninfo> <sqlalchemy url>", file=sys.stderr)
        sys.exit(2)
    conninfo, url = sys.argv[1], sys.argv[2]
    section("psycopg crud", psycopg_crud, conninfo)
    section("psycopg copy", psycopg_copy, conninfo)
    section("sqlalchemy reflection", sqlalchemy_reflect, url)
    sqlalchemy_orm(url)


if __name__ == "__main__":
    main()
