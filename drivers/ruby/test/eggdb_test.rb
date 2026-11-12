require "minitest/autorun"
require "tmpdir"
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

  def test_key_value_transactions_and_collections
    path = File.join(Dir.mktmpdir, "kv.eggdb")
    EggDB::KeyValue.open(path) do |store|
      store.write do |txn|
        txn.put("a", "1")
        txn.put("b", "2")
        assert txn.create_collection("users")
        txn.put("u1", "Ada", collection: "users")
      end
      store.read do |txn|
        assert_equal "1", txn.get("a")
        assert_nil txn.get("missing")
        assert_equal [%w[a 1], %w[b 2]], txn.scan.to_a
        assert_equal [%w[b 2]], txn.scan(start: "b").to_a
        assert_equal ["users"], txn.collections
        assert_equal 1, txn.collection_stats("users").entries
      end
      store.write do |txn|
        assert txn.delete("a")
        txn.rename_collection("users", "people")
      end
      store.read do |txn|
        assert_nil txn.get("a")
        assert_equal "Ada", txn.get("u1", collection: "people")
      end
    end
  end
end
