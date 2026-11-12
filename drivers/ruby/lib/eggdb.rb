require "fiddle"
require "fiddle/import"

module EggDB
  class Error < StandardError
    attr_reader :sqlstate, :status

    def initialize(message, sqlstate, status)
      super(message)
      @sqlstate = sqlstate
      @status = status
    end
  end

  module Native
    extend Fiddle::Importer
    library = ENV.fetch("EGGDB_LIBRARY") { RUBY_PLATFORM.include?("darwin") ? "libeggdb_capi.dylib" : "libeggdb_capi.so" }
    dlload library
    extern "int eggdb_open(const char*, void*, void*)"
    extern "int eggdb_close(void*)"
    extern "int eggdb_sql_open(void*, const char*, void*)"
    extern "int eggdb_sql_connect(const char*, void*)"
    extern "int eggdb_sql_execute_with_parameters(void*, const char*, void*, size_t, void*, void*)"
    extern "int eggdb_sql_close(void*)"
    extern "const char* eggdb_last_error()"
    extern "const char* eggdb_last_sqlstate()"
    extern "void eggdb_free(void*, size_t)"
    extern "int eggdb_begin_read(void*, void*)"
    extern "int eggdb_begin_write(void*, void*)"
    extern "int eggdb_commit(void*, void*)"
    extern "int eggdb_rollback(void*)"
    extern "int eggdb_get(void*, const char*, void*, size_t, void*, void*)"
    extern "int eggdb_put(void*, const char*, void*, size_t, void*, size_t)"
    extern "int eggdb_put_with_expiry(void*, const char*, void*, size_t, void*, size_t, unsigned long long)"
    extern "int eggdb_delete(void*, const char*, void*, size_t, void*)"
    extern "int eggdb_scan_open(void*, const char*, void*, size_t, void*, size_t, unsigned int, void*)"
    extern "int eggdb_scan_next(void*, void*, size_t, void*, void*)"
    extern "void eggdb_scan_free(void*)"
    extern "int eggdb_create_collection(void*, const char*, void*)"
    extern "int eggdb_drop_collection(void*, const char*, void*)"
    extern "int eggdb_rename_collection(void*, const char*, const char*)"
    extern "int eggdb_collection_names(void*, void*, void*, void*)"
    extern "int eggdb_collection_stats(void*, const char*, void*)"
  end

  VALUE_BYTES = 40
  PARAMETER_BYTES = 16
  RESULT_BYTES = 40
  POINTER = Fiddle::SIZEOF_VOIDP
  ENTRY_BYTES = 4 * POINTER
  SCAN_REVERSE = 1
  SCAN_END_INCLUSIVE = 2
  SCAN_PREFIX = 4
  OPEN_READ_ONLY = 1

  def self.check(status)
    return if status.zero?

    message = Native.eggdb_last_error.to_s
    state = Native.eggdb_last_sqlstate.to_s
    raise Error.new(message.empty? ? "EggDB status #{status}" : message, state.empty? ? nil : state, status)
  end

  class Database
    def self.open(path, dialect: "sqlite")
      new(path, dialect)
    end

    def initialize(path, dialect)
      @database = nil
      out = Fiddle::Pointer.malloc(POINTER)
      if path.start_with?("postgresql://", "postgres://")
        EggDB.check(Native.eggdb_sql_connect(path, out))
        @dialect = "postgresql"
      else
        options = Fiddle::Pointer.malloc(16)
        options[0, 16] = "\0" * 16
        EggDB.check(Native.eggdb_open(path, options, out))
        @database = Fiddle::Pointer.new(out[0, POINTER].unpack1("J"))
        status = Native.eggdb_sql_open(@database, dialect, out)
        if status != 0
          Native.eggdb_close(@database)
          EggDB.check(status)
        end
        @dialect = dialect
      end
      @session = Fiddle::Pointer.new(out[0, POINTER].unpack1("J"))
    end

    def execute(sql, parameters = [])
      texts = parameters.map { |value| EggDB.text_of(value, @dialect) }
      array = Fiddle::Pointer.malloc(PARAMETER_BYTES * [texts.size, 1].max)
      keep = texts.map { |text| text && Fiddle::Pointer[text] }
      texts.each_with_index do |text, index|
        address = text ? keep[index].to_i : 0
        array[index * PARAMETER_BYTES, PARAMETER_BYTES] = [address, text ? text.bytesize : 0].pack("JJ")
      end
      results = Fiddle::Pointer.malloc(POINTER)
      count = Fiddle::Pointer.malloc(8)
      EggDB.check(Native.eggdb_sql_execute_with_parameters(@session, EggDB.numbered(sql, @dialect), array, texts.size, results, count))
      EggDB.gathered(Fiddle::Pointer.new(results[0, POINTER].unpack1("J")), count[0, 8].unpack1("J"))
    end

    def query(sql, parameters = [])
      last = execute(sql, parameters).reverse.find { |result| !result[:columns].empty? }
      return [] unless last

      last[:rows].map { |row| last[:columns].zip(row).to_h }
    end

    def close
      return unless @session

      Native.eggdb_sql_close(@session)
      Native.eggdb_close(@database) if @database
      @session = nil
    end
  end

  CollectionStats = Struct.new(:entries, :key_bytes, :value_bytes)

  class KeyValue
    def self.open(path, read_only: false)
      store = new(path, read_only)
      return store unless block_given?

      begin
        yield store
      ensure
        store.close
      end
    end

    def initialize(path, read_only)
      options = Fiddle::Pointer.malloc(16)
      options[0, 16] = [read_only ? OPEN_READ_ONLY : 0, 0, 0].pack("L<L<Q<")
      out = Fiddle::Pointer.malloc(POINTER)
      EggDB.check(Native.eggdb_open(path, options, out))
      @database = Fiddle::Pointer.new(out[0, POINTER].unpack1("J"))
      @read_only = read_only
    end

    attr_reader :read_only

    def read
      transaction = begin_transaction(false)
      begin
        yield transaction
      ensure
        transaction.rollback
      end
    end

    def write
      transaction = begin_transaction(true)
      begin
        result = yield transaction
        transaction.commit
        result
      ensure
        transaction.rollback
      end
    end

    def close
      return unless @database

      database = @database
      @database = nil
      EggDB.check(Native.eggdb_close(database))
    end

    private

    def begin_transaction(writing)
      raise Error.new("the store is closed", nil, nil) unless @database

      out = Fiddle::Pointer.malloc(POINTER)
      EggDB.check(writing ? Native.eggdb_begin_write(@database, out) : Native.eggdb_begin_read(@database, out))
      Transaction.new(Fiddle::Pointer.new(out[0, POINTER].unpack1("J")), writing)
    end
  end

  class Transaction
    attr_reader :writing

    def initialize(handle, writing)
      @handle = handle
      @writing = writing
    end

    def get(key, collection: nil)
      key = live_bytes(key)
      data = Fiddle::Pointer.malloc(POINTER)
      length = Fiddle::Pointer.malloc(8)
      EggDB.check(Native.eggdb_get(@handle, collection, key, key.bytesize, data, length))
      address = data[0, POINTER].unpack1("J")
      return nil if address.zero?

      size = length[0, 8].unpack1("Q<")
      value = Fiddle::Pointer.new(address)[0, size].b
      Native.eggdb_free(Fiddle::Pointer.new(address), size)
      value
    end

    def put(key, value, collection: nil, ttl_ms: nil)
      key = live_bytes(key)
      value = value.to_s.b
      status = if ttl_ms && ttl_ms.positive?
                 Native.eggdb_put_with_expiry(@handle, collection, key, key.bytesize, value, value.bytesize, ttl_ms)
               else
                 Native.eggdb_put(@handle, collection, key, key.bytesize, value, value.bytesize)
               end
      EggDB.check(status)
      nil
    end

    def delete(key, collection: nil)
      key = live_bytes(key)
      deleted = Fiddle::Pointer.malloc(4)
      EggDB.check(Native.eggdb_delete(@handle, collection, key, key.bytesize, deleted))
      !deleted[0, 4].unpack1("l<").zero?
    end

    def scan(start: nil, end_key: nil, prefix: nil, reverse: false, end_inclusive: false, collection: nil)
      return enum_for(:scan, start: start, end_key: end_key, prefix: prefix, reverse: reverse, end_inclusive: end_inclusive, collection: collection) unless block_given?

      live
      flags = 0
      flags |= SCAN_REVERSE if reverse
      flags |= SCAN_END_INCLUSIVE if end_inclusive
      if prefix
        flags |= SCAN_PREFIX
        start = prefix
        end_key = nil
      end
      start = start&.to_s&.b
      end_key = end_key&.to_s&.b
      out = Fiddle::Pointer.malloc(POINTER)
      EggDB.check(Native.eggdb_scan_open(@handle, collection, start, start ? start.bytesize : 0, end_key, end_key ? end_key.bytesize : 0, flags, out))
      scan = Fiddle::Pointer.new(out[0, POINTER].unpack1("J"))
      begin
        entries = Fiddle::Pointer.malloc(POINTER)
        count = Fiddle::Pointer.malloc(8)
        loop do
          EggDB.check(Native.eggdb_scan_next(@handle, scan, 0, entries, count))
          total = count[0, 8].unpack1("Q<")
          break if total.zero?

          batch = Fiddle::Pointer.new(entries[0, POINTER].unpack1("J"))
          pairs = (0...total).map do |index|
            key_address, key_length, value_address, value_length = batch[index * ENTRY_BYTES, ENTRY_BYTES].unpack("JJJJ")
            [Fiddle::Pointer.new(key_address)[0, key_length].b, Fiddle::Pointer.new(value_address)[0, value_length].b]
          end
          pairs.each { |key, value| yield key, value }
        end
      ensure
        Native.eggdb_scan_free(scan)
      end
      nil
    end

    def create_collection(name)
      live
      created = Fiddle::Pointer.malloc(4)
      EggDB.check(Native.eggdb_create_collection(@handle, name, created))
      !created[0, 4].unpack1("l<").zero?
    end

    def drop_collection(name)
      live
      dropped = Fiddle::Pointer.malloc(4)
      EggDB.check(Native.eggdb_drop_collection(@handle, name, dropped))
      !dropped[0, 4].unpack1("l<").zero?
    end

    def rename_collection(name, new_name)
      live
      EggDB.check(Native.eggdb_rename_collection(@handle, name, new_name))
      nil
    end

    def collections
      live
      data = Fiddle::Pointer.malloc(POINTER)
      length = Fiddle::Pointer.malloc(8)
      count = Fiddle::Pointer.malloc(8)
      EggDB.check(Native.eggdb_collection_names(@handle, data, length, count))
      address = data[0, POINTER].unpack1("J")
      return [] if address.zero?

      size = length[0, 8].unpack1("Q<")
      joined = Fiddle::Pointer.new(address)[0, size]
      Native.eggdb_free(Fiddle::Pointer.new(address), size)
      joined.split("\0").first(count[0, 8].unpack1("Q<")).map { |name| name.force_encoding("UTF-8") }
    end

    def collection_stats(name)
      live
      stats = Fiddle::Pointer.malloc(24)
      EggDB.check(Native.eggdb_collection_stats(@handle, name, stats))
      CollectionStats.new(*stats[0, 24].unpack("Q<Q<Q<"))
    end

    def commit
      live
      stamp = Fiddle::Pointer.malloc(8)
      handle = @handle
      @handle = nil
      EggDB.check(Native.eggdb_commit(handle, stamp))
      stamp[0, 8].unpack1("Q<")
    end

    def rollback
      return unless @handle

      handle = @handle
      @handle = nil
      Native.eggdb_rollback(handle)
      nil
    end

    private

    def live
      raise Error.new("the transaction has ended", nil, nil) unless @handle
    end

    def live_bytes(key)
      live
      key.to_s.b
    end
  end

  def self.text_of(value, dialect)
    case value
    when nil then nil
    when true then dialect == "postgresql" ? "true" : "1"
    when false then dialect == "postgresql" ? "false" : "0"
    else value.to_s
    end
  end

  def self.numbered(sql, dialect)
    return sql unless dialect == "postgresql"

    number = 0
    quote = nil
    sql.each_char.map do |character|
      if quote
        quote = nil if character == quote
        character
      elsif character == "'" || character == '"'
        quote = character
        character
      elsif character == "?"
        number += 1
        "$#{number}"
      else
        character
      end
    end.join
  end

  def self.gathered(results, count)
    (0...count).map do |index|
      base = results + index * RESULT_BYTES
      tag_address, column_count, names_address, row_count, values_address = base[0, RESULT_BYTES].unpack("JJJJJ")
      columns = (0...column_count).map do |column|
        Fiddle::Pointer.new(Fiddle::Pointer.new(names_address)[column * POINTER, POINTER].unpack1("J")).to_s
      end
      values = Fiddle::Pointer.new(values_address)
      rows = (0...row_count).map do |row|
        (0...column_count).map do |column|
          kind, integer, real, data, length = values[(row * column_count + column) * VALUE_BYTES, VALUE_BYTES].unpack("l<x4q<EJJ")
          case kind
          when 1 then integer
          when 2 then real
          when 3 then Fiddle::Pointer.new(data)[0, length].force_encoding("UTF-8")
          when 4 then Fiddle::Pointer.new(data)[0, length].b
          end
        end
      end
      { tag: Fiddle::Pointer.new(tag_address).to_s, columns: columns, rows: rows }
    end
  end
end
