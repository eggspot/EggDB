import unittest

try:
    import sqlalchemy
except ImportError:
    sqlalchemy = None


@unittest.skipIf(sqlalchemy is None, "SQLAlchemy is not installed")
class SqlAlchemyTest(unittest.TestCase):
    def test_core_creates_inserts_selects_and_reflects(self):
        import eggdb.sqlalchemy  # noqa: F401
        from sqlalchemy import Column, Integer, MetaData, String, Table, create_engine, insert, inspect, select

        engine = create_engine("eggdb:///:memory:")
        metadata = MetaData()
        people = Table(
            "people",
            metadata,
            Column("id", Integer, primary_key=True),
            Column("name", String(50), nullable=False),
        )
        metadata.create_all(engine)
        with engine.begin() as connection:
            connection.execute(insert(people), [{"name": "Ada"}, {"name": "Grace"}])
        with engine.connect() as connection:
            names = connection.execute(select(people.c.name).order_by(people.c.id)).scalars().all()
        self.assertEqual(names, ["Ada", "Grace"])
        self.assertIn("people", inspect(engine).get_table_names())

    def test_the_orm_round_trips_an_object(self):
        import eggdb.sqlalchemy  # noqa: F401
        from sqlalchemy import Integer, String, create_engine, select
        from sqlalchemy.orm import DeclarativeBase, Mapped, Session, mapped_column

        class Base(DeclarativeBase):
            pass

        class Egg(Base):
            __tablename__ = "eggs"
            id: Mapped[int] = mapped_column(Integer, primary_key=True)
            colour: Mapped[str] = mapped_column(String(20))

        engine = create_engine("eggdb:///:memory:")
        Base.metadata.create_all(engine)
        with Session(engine) as session:
            session.add(Egg(colour="brown"))
            session.commit()
            found = session.scalars(select(Egg)).one()
        self.assertEqual(found.colour, "brown")


if __name__ == "__main__":
    unittest.main()
