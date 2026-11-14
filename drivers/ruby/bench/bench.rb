require "sqlite3"
require "tmpdir"
require_relative "../lib/eggdb"

ROWS = Integer(ENV.fetch("DRIVER_BENCH_ROWS", "20000"))
LOOKUPS = Integer(ENV.fetch("DRIVER_BENCH_LOOKUPS", "5000"))

def timed
  started = Process.clock_gettime(Process::CLOCK_MONOTONIC)
  yield
  (Process.clock_gettime(Process::CLOCK_MONOTONIC) - started) * 1000
end

def keys
  (1..LOOKUPS).map { |at| at * 7 % ROWS + 1 }
end

def eggdb_side(path)
  db = EggDB::Database.open(path)
  db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score REAL)")
  insert = timed do
    db.execute("BEGIN")
    db.prepare("INSERT INTO t VALUES (?, ?, ?)") do |statement|
      (1..ROWS).each { |at| statement.execute([at, "name #{at}", at * 0.25]) }
    end
    db.execute("COMMIT")
  end
  lookup = timed do
    db.prepare("SELECT name FROM t WHERE id = ?") do |statement|
      keys.each { |key| statement.each_row([key]) { |_row| } }
    end
  end
  scan = timed do
    total = 0
    db.each_row("SELECT name FROM t") { |row| total += row[0].bytesize }
  end
  db.close
  [insert, lookup, scan]
end

def sqlite_side(path)
  db = SQLite3::Database.new(path)
  db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score REAL)")
  insert = timed do
    db.transaction do
      statement = db.prepare("INSERT INTO t VALUES (?, ?, ?)")
      (1..ROWS).each { |at| statement.execute(at, "name #{at}", at * 0.25) }
      statement.close
    end
  end
  lookup = timed do
    statement = db.prepare("SELECT name FROM t WHERE id = ?")
    keys.each { |key| statement.execute(key).to_a }
    statement.close
  end
  scan = timed do
    total = 0
    db.execute("SELECT name FROM t") { |row| total += row[0].bytesize }
  end
  db.close
  [insert, lookup, scan]
end

Dir.mktmpdir do |directory|
  egg = eggdb_side(File.join(directory, "bench.eggdb"))
  lite = sqlite_side(File.join(directory, "bench.sqlite"))
  %w[insert lookup scan].each_with_index do |workload, at|
    puts format("ruby %s %.2f %.2f", workload, egg[at], lite[at])
  end
end
