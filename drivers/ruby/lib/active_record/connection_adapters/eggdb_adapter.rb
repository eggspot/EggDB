require "active_record"
require "active_record/connection_adapters/sqlite3_adapter"
require_relative "../../eggdb"

module ActiveRecord
  module ConnectionAdapters
    class EggDBAdapter < SQLite3Adapter
      ADAPTER_NAME = "EggDB"
      REPORTED_SQLITE_VERSION = "3.45.0"
      CHANGING_TAGS = %w[INSERT UPDATE DELETE].freeze

      class << self
        def new_client(config)
          ::EggDB::Database.open(config[:database].to_s, dialect: "sqlite")
        rescue ::EggDB::Error => error
          raise ActiveRecord::ConnectionNotEstablished, error.message
        end
      end

      def active?
        !@raw_connection.nil?
      end

      def disconnect!
        super
        @raw_connection&.close
        @raw_connection = nil
      end

      def get_database_version
        Version.new(REPORTED_SQLITE_VERSION)
      end

      def supports_concurrent_connections?
        true
      end

      private

      def configure_connection
        @raw_connection.execute("PRAGMA foreign_keys = ON")
      end

      def perform_query(raw_connection, sql, binds, type_casted_binds, prepare:, notification_payload:, batch: false)
        results = raw_connection.execute(sql, type_casted_binds || [])
        last = results.reverse.find { |result| !result[:columns].empty? } || results.last || { tag: "", columns: [], rows: [] }
        @last_affected_rows = changed_rows(results)
        notification_payload[:row_count] = last[:rows].length if notification_payload
        last
      end

      def cast_result(result)
        ActiveRecord::Result.new(result[:columns], result[:rows])
      end

      def affected_rows(_result)
        @last_affected_rows || 0
      end

      def last_inserted_id(_result)
        row = @raw_connection.execute("SELECT last_insert_rowid()").last
        row && row[:rows].first && row[:rows].first.first
      end

      def changed_rows(results)
        words = results.last ? results.last[:tag].to_s.split : []
        return 0 unless CHANGING_TAGS.include?(words.first)

        words.last.to_i
      end

      def translate_exception(exception, message:, sql:, binds:)
        return super unless exception.is_a?(::EggDB::Error)

        case exception.sqlstate
        when "23505" then RecordNotUnique.new(message, sql: sql, binds: binds, connection_pool: @pool)
        when "23503" then InvalidForeignKey.new(message, sql: sql, binds: binds, connection_pool: @pool)
        when "23502" then NotNullViolation.new(message, sql: sql, binds: binds, connection_pool: @pool)
        else StatementInvalid.new(message, sql: sql, binds: binds, connection_pool: @pool)
        end
      end
    end

    if respond_to?(:register)
      register("eggdb", "ActiveRecord::ConnectionAdapters::EggDBAdapter", "active_record/connection_adapters/eggdb_adapter")
    end
  end
end
