require "minitest/autorun"
require_relative "../lib/eggdb"

class EggDBTest < Minitest::Test
  def test_rows_parameters_and_errors
    db = EggDB::Database.open(":memory:")
    db.execute("CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT, score REAL)")
    db.execute("INSERT INTO people (name, score) VALUES (?, ?)", ["Ada", 1.5])
    db.execute("INSERT INTO people (name, score) VALUES (?, ?)", [nil, 2])
    rows = db.query("SELECT id, name, score FROM people ORDER BY id")
    assert_equal({ "id" => 1, "name" => "Ada", "score" => 1.5 }, rows[0])
    assert_nil rows[1]["name"]
    error = assert_raises(EggDB::Error) { db.query("SELECT * FROM nowhere") }
    assert_equal 5, error.sqlstate.size
    db.close
  end
end
