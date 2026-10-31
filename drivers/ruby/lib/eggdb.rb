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
  end

  VALUE_BYTES = 40
  PARAMETER_BYTES = 16
  RESULT_BYTES = 40
  POINTER = Fiddle::SIZEOF_VOIDP

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
