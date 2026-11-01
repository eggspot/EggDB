#include <cstdio>
#include <cstdlib>
#include <string>

#include "eggdb.hpp"

static void require(bool condition, const char *what) {
    if (!condition) {
        std::fprintf(stderr, "c++ driver check failed: %s\n", what);
        std::exit(1);
    }
}

int main(int argc, char **argv) {
    std::string path = argc > 1 ? argv[1] : ":memory:";
    auto db = eggdb::session::open(path);
    db.execute("CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT, score REAL)");
    db.execute("INSERT INTO people (name, score) VALUES (?, ?)", {std::string("Ada"), std::string("1.5")});
    db.execute("INSERT INTO people (name, score) VALUES (?, ?)", {std::nullopt, std::string("2")});
    auto rows = db.execute("SELECT id, name, score FROM people ORDER BY id")[0].rows;
    require(rows.size() == 2, "two rows");
    require(std::get<std::int64_t>(rows[0][0]) == 1, "an integer id");
    require(std::get<std::string>(rows[0][1]) == "Ada", "a text name");
    require(std::get<double>(rows[0][2]) == 1.5, "a real score");
    require(std::holds_alternative<std::monostate>(rows[1][1]), "a NULL name");
    try {
        db.execute("SELECT * FROM nowhere");
        require(false, "a missing table fails");
    } catch (const eggdb::error &failure) {
        require(failure.sqlstate().size() == 5, "the error has a SQLSTATE");
    }
    std::puts("c++ driver check: passed");
    return 0;
}
