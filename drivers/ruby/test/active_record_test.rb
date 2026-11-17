require "minitest/autorun"

begin
  require "active_record"
  require_relative "../lib/active_record/connection_adapters/eggdb_adapter"
  ACTIVE_RECORD_LOADED = true
rescue LoadError
  ACTIVE_RECORD_LOADED = false
end

class ActiveRecordAdapterTest < Minitest::Test
  def setup
    skip "the activerecord gem is not installed" unless ACTIVE_RECORD_LOADED
    ActiveRecord::Base.establish_connection(adapter: "eggdb", database: ":memory:")
    ActiveRecord::Schema.verbose = false
    ActiveRecord::Schema.define do
      create_table :eggs, force: true do |table|
        table.string :colour, null: false
        table.integer :weight
      end
    end
  end

  def test_models_create_find_update_and_destroy
    egg_class = Class.new(ActiveRecord::Base) { self.table_name = "eggs" }
    brown = egg_class.create!(colour: "brown", weight: 61)
    egg_class.create!(colour: "white", weight: 58)
    assert_equal 2, egg_class.count
    assert_equal "brown", egg_class.find(brown.id).colour
    egg_class.where(colour: "white").update_all(weight: 60)
    assert_equal 60, egg_class.find_by(colour: "white").weight
    brown.destroy
    assert_equal 1, egg_class.count
  end
end
