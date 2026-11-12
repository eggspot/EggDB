require "minitest/autorun"

begin
  require "sequel"
  require_relative "../lib/sequel/adapters/eggdb"
  SEQUEL_LOADED = true
rescue LoadError
  SEQUEL_LOADED = false
end

class SequelAdapterTest < Minitest::Test
  def setup
    skip "the sequel gem is not installed" unless SEQUEL_LOADED
  end

  def test_datasets_insert_filter_update_and_delete
    db = Sequel.connect(adapter: :eggdb, database: ":memory:")
    db.create_table(:eggs) do
      primary_key :id
      String :colour, null: false
      Integer :weight
    end
    eggs = db[:eggs]
    first = eggs.insert(colour: "brown", weight: 61)
    eggs.insert(colour: "white", weight: 58)
    assert_equal 1, first
    assert_equal %w[brown white], eggs.order(:id).select_map(:colour)
    assert_equal 1, eggs.where(colour: "white").update(weight: 59)
    assert_equal 59, eggs.where(colour: "white").get(:weight)
    assert_equal 1, eggs.where(colour: "brown").delete
    assert_equal 1, eggs.count
    assert_includes db.tables, :eggs
    db.disconnect
  end
end
