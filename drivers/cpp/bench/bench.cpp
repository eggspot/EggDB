#include <sqlite3.h>

#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <string>

#include "eggdb.hpp"

namespace {

int setting(const char *name, int fallback) {
    const char *value = std::getenv(name);
    return value ? std::atoi(value) : fallback;
}

const int rows = setting("DRIVER_BENCH_ROWS", 20000);
const int lookups = setting("DRIVER_BENCH_LOOKUPS", 5000);
volatile std::size_t consumed = 0;

template <typename Work>
double timed(Work &&work) {
    auto started = std::chrono::steady_clock::now();
    work();
    return std::chrono::duration<double, std::milli>(std::chrono::steady_clock::now() - started).count();
}

struct times {
    double insert;
    double lookup;
    double scan;
};

times eggdb_side(const std::string &path) {
    auto db = eggdb::session::open(path);
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score REAL)");
    times out{};
    out.insert = timed([&] {
        db.execute("BEGIN");
        auto insert = db.prepare("INSERT INTO t VALUES (?, ?, ?)");
        for (int at = 1; at <= rows; ++at) {
            insert.bind_all({std::int64_t{at}, "name " + std::to_string(at), at * 0.25}).execute();
        }
        db.execute("COMMIT");
    });
    out.lookup = timed([&] {
        auto select = db.prepare("SELECT name FROM t WHERE id = ?");
        for (int at = 1; at <= lookups; ++at) {
            select.bind_all({std::int64_t{at * 7 % rows + 1}});
            while (auto row = select.step()) {
                consumed = std::get<std::string>((*row)[0]).size();
            }
        }
    });
    out.scan = timed([&] {
        auto select = db.prepare("SELECT name FROM t");
        std::size_t total = 0;
        while (auto row = select.step()) {
            total += std::get<std::string>((*row)[0]).size();
        }
        consumed = total;
    });
    return out;
}

void must(int status, sqlite3 *db) {
    if (status != SQLITE_OK && status != SQLITE_DONE && status != SQLITE_ROW) {
        std::fprintf(stderr, "sqlite: %s\n", sqlite3_errmsg(db));
        std::exit(1);
    }
}

times sqlite_side(const std::string &path) {
    sqlite3 *db = nullptr;
    must(sqlite3_open(path.c_str(), &db), db);
    must(sqlite3_exec(db, "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score REAL)", nullptr, nullptr, nullptr), db);
    times out{};
    out.insert = timed([&] {
        must(sqlite3_exec(db, "BEGIN", nullptr, nullptr, nullptr), db);
        sqlite3_stmt *insert = nullptr;
        must(sqlite3_prepare_v2(db, "INSERT INTO t VALUES (?, ?, ?)", -1, &insert, nullptr), db);
        for (int at = 1; at <= rows; ++at) {
            std::string name = "name " + std::to_string(at);
            sqlite3_bind_int64(insert, 1, at);
            sqlite3_bind_text(insert, 2, name.c_str(), static_cast<int>(name.size()), SQLITE_TRANSIENT);
            sqlite3_bind_double(insert, 3, at * 0.25);
            must(sqlite3_step(insert), db);
            sqlite3_reset(insert);
        }
        sqlite3_finalize(insert);
        must(sqlite3_exec(db, "COMMIT", nullptr, nullptr, nullptr), db);
    });
    out.lookup = timed([&] {
        sqlite3_stmt *select = nullptr;
        must(sqlite3_prepare_v2(db, "SELECT name FROM t WHERE id = ?", -1, &select, nullptr), db);
        for (int at = 1; at <= lookups; ++at) {
            sqlite3_bind_int64(select, 1, at * 7 % rows + 1);
            while (sqlite3_step(select) == SQLITE_ROW) {
                consumed = std::string(reinterpret_cast<const char *>(sqlite3_column_text(select, 0))).size();
            }
            sqlite3_reset(select);
        }
        sqlite3_finalize(select);
    });
    out.scan = timed([&] {
        sqlite3_stmt *select = nullptr;
        must(sqlite3_prepare_v2(db, "SELECT name FROM t", -1, &select, nullptr), db);
        std::size_t total = 0;
        while (sqlite3_step(select) == SQLITE_ROW) {
            total += std::string(reinterpret_cast<const char *>(sqlite3_column_text(select, 0))).size();
        }
        sqlite3_finalize(select);
        consumed = total;
    });
    sqlite3_close(db);
    return out;
}

}

int main(int argc, char **argv) {
    std::string directory = argc > 1 ? argv[1] : ".";
    times egg = eggdb_side(directory + "/bench.eggdb");
    times lite = sqlite_side(directory + "/bench.sqlite");
    std::printf("c++ insert %.2f %.2f\n", egg.insert, lite.insert);
    std::printf("c++ lookup %.2f %.2f\n", egg.lookup, lite.lookup);
    std::printf("c++ scan %.2f %.2f\n", egg.scan, lite.scan);
    return 0;
}
