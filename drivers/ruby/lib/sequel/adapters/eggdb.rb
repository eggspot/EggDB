require "sequel/adapters/shared/sqlite"
require_relative "../../eggdb"

module Sequel
  module EggDB
    class Database < Sequel::Database
      include Sequel::SQLite::DatabaseMethods

      set_adapter_scheme :eggdb

      def connect(server)
        options = server_opts(server)
        ::EggDB::Database.open(options[:database] || ":memory:", dialect: "sqlite")
      end

      def disconnect_connection(connection)
        connection.close
      end

      def execute(sql, opts = OPTS, &block)
        _execute(sql, opts) do |_connection, results|
          last = results.reverse.find { |result| !result[:columns].empty? }
          yield(last || { tag: "", columns: [], rows: [] }) if block
          results
        end
      end

      def execute_dui(sql, opts = OPTS)
        _execute(sql, opts) do |_connection, results|
          changed_rows(results)
        end
      end

      def execute_insert(sql, opts = OPTS)
        _execute(sql, opts) do |connection, _results|
          last = connection.execute("SELECT last_insert_rowid()").last
          last && last[:rows].first && last[:rows].first.first
        end
      end

      def execute_ddl(sql, opts = OPTS)
        _execute(sql, opts) { |_connection, _results| nil }
      end

      def database_type
        :sqlite
      end

      private

      def _execute(sql, opts)
        synchronize(opts[:server]) do |connection|
          results = log_connection_yield(sql, connection) { connection.execute(sql) }
          yield connection, results
        end
      rescue ::EggDB::Error => error
        raise_error(error)
      end

      def changed_rows(results)
        words = results.last ? results.last[:tag].split : []
        return 0 unless %w[INSERT UPDATE DELETE].include?(words.first)

        words.last.to_i
      end

      def database_error_classes
        [::EggDB::Error]
      end

      def dataset_class_default
        Dataset
      end
    end

    class Dataset < Sequel::Dataset
      include Sequel::SQLite::DatasetMethods

      def fetch_rows(sql)
        execute(sql) do |result|
          columns = result[:columns].map { |name| output_identifier(name) }
          self.columns = columns
          result[:rows].each { |row| yield columns.zip(row).to_h }
        end
      end
    end
  end
end
