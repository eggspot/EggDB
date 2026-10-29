import datetime
import decimal
import sys

import psycopg
import sqlalchemy as sa


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


def main():
    if len(sys.argv) != 3:
        print("usage: python app_check.py <libpq conninfo> <sqlalchemy url>", file=sys.stderr)
        sys.exit(2)
    conninfo, url = sys.argv[1], sys.argv[2]
    section("psycopg crud", psycopg_crud, conninfo)
    section("sqlalchemy reflection", sqlalchemy_reflect, url)


if __name__ == "__main__":
    main()
